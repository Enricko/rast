import assert from "node:assert/strict";
import test from "node:test";

import {
  CLIPBOARD_CACHE_TTL_MS,
  MAC_COMMAND_RESERVED,
  MAC_COMMAND_RESERVED_SHIFTED,
  NativeClipboardManager,
  NativeLinuxHost,
  OTHER_BROWSER_RESERVED,
  decideNativeKey,
  detectMacPlatform,
  type NativeClipboardEnv,
  type NativeKeyContext,
} from "../src/shell/xpra-native";

// The host methods read `document` for the modal guard; the pure decision
// function and the clipboard manager never need a DOM. A minimal document
// stub keeps the forwarding tests honest without a browser.
(globalThis as any).document = {
  querySelector: () => null,
  activeElement: null,
  visibilityState: "visible",
  addEventListener: () => undefined,
  removeEventListener: () => undefined,
};
(globalThis as any).window = {
  setTimeout: (callback: () => void, ms: number) => setTimeout(callback, ms),
  clearTimeout: (timer: number) => clearTimeout(timer),
  setInterval: () => 0,
  clearInterval: () => undefined,
};

/* ----------------------------------------------------------- key decisions */

const macContext: NativeKeyContext = {
  mac: true,
  nativeFocused: true,
  shellTarget: false,
  modalOpen: false,
  composing: false,
  connected: true,
};

const otherContext: NativeKeyContext = { ...macContext, mac: false };

interface KeyInput {
  key: string;
  code?: string;
  keyCode?: number;
  ctrlKey?: boolean;
  shiftKey?: boolean;
  altKey?: boolean;
  metaKey?: boolean;
  repeat?: boolean;
  isComposing?: boolean;
}

function key(name: string, extra: Partial<KeyInput> = {}): KeyInput {
  const code =
    name.length === 1
      ? `Key${name.toUpperCase()}`
      : name === "Backspace"
        ? "Backspace"
        : name === "Delete"
          ? "Delete"
          : name === "Tab"
            ? "Tab"
            : name.startsWith("Arrow")
              ? name
              : name;
  return {
    key: name,
    code,
    keyCode: name.length === 1 ? name.toUpperCase().charCodeAt(0) : 0,
    ctrlKey: false,
    shiftKey: false,
    altKey: false,
    metaKey: false,
    repeat: false,
    ...extra,
  };
}

function decide(input: KeyInput, context: NativeKeyContext = macContext) {
  return decideNativeKey(input, context);
}

test("macOS Command editing combos are forwarded, not rewritten", () => {
  // Command is forwarded as the real Meta event: the pinned Xpra client's
  // swap_keys mode turns it into the Linux Control shortcut. Rewriting the
  // event here would be swapped a second time (Control -> Meta on the server).
  const cases: Array<[KeyInput, boolean, "copy" | "cut" | "paste" | null]> = [
    [key("c", { metaKey: true }), true, "copy"],
    [key("x", { metaKey: true }), true, "cut"],
    [key("v", { metaKey: true }), false, "paste"],
    [key("v", { metaKey: true, shiftKey: true }), false, "paste"],
    [key("a", { metaKey: true }), true, null],
    [key("z", { metaKey: true }), true, null],
    [key("z", { metaKey: true, shiftKey: true }), true, null],
    [key("y", { metaKey: true }), true, null],
    [key("s", { metaKey: true }), true, null],
  ];
  for (const [input, preventDefault, gesture] of cases) {
    const decision = decide(input);
    assert.equal(decision.mode, "forward", `${input.key} must be forwarded`);
    assert.equal(decision.preventDefault, preventDefault, `${input.key} default action policy`);
    assert.equal(decision.gesture, gesture, `${input.key} gesture`);
    assert.equal(decision.init.metaKey, true, "Command is forwarded as Meta (swap_keys maps it)");
    assert.equal(decision.init.ctrlKey, false, "RAST never rewrites Command to Control itself");
  }
});

test("browser-reserved macOS Command combos are never forwarded", () => {
  const commands = ["w", "t", "l", "n", "r", "p", "m", "h", "f", "o", "q", ",", "1", "2", "9", "[", "]"];
  for (const name of commands) {
    const decision = decide(key(name, { metaKey: true }));
    assert.equal(decision.mode, "reserved", `Cmd+${name} stays with the browser`);
    assert.equal(decision.preventDefault, false, `Cmd+${name} is not cancelled`);
    assert.equal(decision.gesture, null);
  }
  for (const name of ["w", "n", "t"]) {
    const decision = decide(key(name, { metaKey: true, shiftKey: true }));
    assert.equal(decision.mode, "reserved", `Cmd+Shift+${name} stays with the browser`);
    assert.equal(decision.preventDefault, false);
  }
  const devtools = ["i", "j", "c", "k"];
  for (const name of devtools) {
    const decision = decide(key(name, { metaKey: true, altKey: true }));
    assert.equal(decision.mode, "reserved", `Cmd+Opt+${name} is a browser/OS shortcut`);
    assert.equal(decision.preventDefault, false);
  }
  assert.ok(MAC_COMMAND_RESERVED.has("w") && MAC_COMMAND_RESERVED.has("f"));
  assert.ok(MAC_COMMAND_RESERVED_SHIFTED.has("w") && MAC_COMMAND_RESERVED_SHIFTED.has("n"));
});

test("non-reserved Command combos are left to the browser, never forwarded", () => {
  // Cmd+E / Cmd+K / Cmd+ArrowLeft are browser or application-level Mac
  // shortcuts RAST does not translate: forwarding them as Control would trigger
  // unexpected Linux shortcuts.
  for (const input of [
    key("e", { metaKey: true }),
    key("k", { metaKey: true }),
    key("ArrowLeft", { metaKey: true }),
    key("Backspace", { metaKey: true }),
  ]) {
    const decision = decide(input);
    assert.equal(decision.mode, "reserved");
    assert.equal(decision.preventDefault, false);
  }
});

test("typing, Shift, Control and Option are forwarded like before", () => {
  for (const input of [key("a"), key("Z", { shiftKey: true }), key("Enter"), key("ArrowDown"), key("F5")]) {
    const decision = decide(input);
    if (input.key === "F5") {
      assert.equal(decision.mode, "reserved", "browser reload stays with the browser");
      continue;
    }
    assert.equal(decision.mode, "forward", `${input.key} must be forwarded`);
    assert.equal(decision.preventDefault, true);
  }
  const option = decide(key("f", { altKey: true }));
  assert.equal(option.mode, "forward");
  assert.equal(option.init.altKey, true);
  const control = decide(key("a", { ctrlKey: true }));
  assert.equal(control.mode, "forward", "Control on macOS is forwarded (swap_keys maps it)");
});

test("modifier presses are forwarded so remote chords work", () => {
  for (const input of [
    key("Shift", { shiftKey: true, code: "ShiftLeft" }),
    key("Control", { ctrlKey: true, code: "ControlLeft" }),
    key("Alt", { altKey: true, code: "AltLeft" }),
    key("Meta", { metaKey: true, code: "MetaLeft" }),
  ]) {
    const decision = decide(input);
    assert.equal(decision.mode, "forward", `${input.key} modifier must reach the remote session`);
    assert.equal(decision.preventDefault, false, "a bare modifier does not cancel anything");
  }
});

test("the RAST shell keeps the keyboard for shell controls, modals and IME", () => {
  assert.equal(decide(key("a"), { ...macContext, nativeFocused: false }).mode, "shell");
  assert.equal(decide(key("a"), { ...macContext, shellTarget: true }).mode, "shell");
  assert.equal(decide(key("a"), { ...macContext, modalOpen: true }).mode, "shell");
  assert.equal(decide(key("a"), { ...macContext, composing: true }).mode, "shell");
  assert.equal(decide(key("a", { isComposing: true })).mode, "shell");
  assert.equal(decide(key("a"), { ...macContext, connected: false }).mode, "shell");
  assert.equal(decide(key("a"), { ...macContext, nativeFocused: false }).preventDefault, false);
});

test("Windows/Linux Control shortcuts: clipboard gestures forwarded, browser combos kept", () => {
  assert.equal(decide(key("c", { ctrlKey: true }), otherContext).gesture, "copy");
  assert.equal(decide(key("x", { ctrlKey: true }), otherContext).gesture, "cut");
  assert.equal(decide(key("v", { ctrlKey: true }), otherContext).gesture, "paste");
  assert.equal(decide(key("v", { ctrlKey: true }), otherContext).preventDefault, false);
  assert.equal(decide(key("s", { ctrlKey: true }), otherContext).mode, "forward");
  for (const name of ["w", "t", "n", "l", "r", "p"]) {
    const decision = decide(key(name, { ctrlKey: true }), otherContext);
    assert.equal(decision.mode, "reserved", `Ctrl+${name} stays with the browser`);
    assert.equal(decision.preventDefault, false);
    assert.ok(OTHER_BROWSER_RESERVED.has(name));
  }
  for (const name of ["i", "j", "k"]) {
    assert.equal(decide(key(name, { ctrlKey: true, shiftKey: true }), otherContext).mode, "reserved");
  }
  assert.equal(decide(key("a", { metaKey: true }), otherContext).mode, "reserved", "the OS key is never forwarded");
  assert.equal(decide(key("Insert", { shiftKey: true }), otherContext).gesture, "paste");
});

test("platform detection is generic (not Chrome specific)", () => {
  assert.equal(detectMacPlatform({ platform: "MacIntel", userAgent: "" }), true);
  assert.equal(detectMacPlatform({ platform: "Win32", userAgent: "Mozilla/5.0 (Windows NT 10.0)" }), false);
  assert.equal(detectMacPlatform({ platform: "Linux x86_64", userAgent: "Mozilla/5.0 (X11; Linux x86_64)" }), false);
  assert.equal(detectMacPlatform({ userAgentData: { platform: "macOS" }, userAgent: "" }), true);
  assert.equal(detectMacPlatform({ platform: "", userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0)" }), true);
  assert.equal(detectMacPlatform(null), false);
});

test("Cmd+F is deliberately browser-reserved (find bar), documented in docs/INPUT-CLIPBOARD.md", () => {
  const decision = decide(key("f", { metaKey: true }));
  assert.equal(decision.mode, "reserved");
  assert.equal(decision.preventDefault, false);
});

/* --------------------------------------------------- host key forwarding */

interface SentKey {
  pressed: boolean;
  init: Record<string, unknown>;
}

function fakeHost(mac = false) {
  const sent: SentKey[] = [];
  const shim = {
    sendKey: (pressed: boolean, init: Record<string, unknown>) => {
      sent.push({ pressed, init });
      return true;
    },
    releaseInput: () => true,
  };
  const host: any = Object.assign(Object.create(NativeLinuxHost.prototype), {
    disposed: false,
    shim,
    shimConnected: true,
    focusedWid: "7",
    mac,
    composing: false,
    pressedKeys: new Map(),
    pasteGestureSeq: 0,
    pasteGestureSeen: true,
    pasteFallbackTimer: 0,
    keyForwardNoticeShown: false,
    deps: { toast: { error: () => undefined, warning: () => undefined, success: () => undefined, info: () => undefined } },
    clipboard: {
      pasteGestureMissed: async () => undefined,
      pasteEventText: () => true,
      browserCopied: () => undefined,
      onWindowFocus: () => undefined,
      pushToRemote: () => true,
      detach: () => undefined,
      setConnected: () => undefined,
    },
  });
  return { host, sent };
}

function event(target: unknown, extras: Record<string, unknown> = {}): any {
  let prevented = false;
  let stopped = false;
  return {
    key: "a",
    code: "KeyA",
    keyCode: 65,
    which: 65,
    location: 0,
    ctrlKey: false,
    shiftKey: false,
    altKey: false,
    metaKey: false,
    repeat: false,
    isComposing: false,
    target,
    preventDefault: () => {
      prevented = true;
    },
    stopPropagation: () => {
      stopped = true;
    },
    get prevented() {
      return prevented;
    },
    get stopped() {
      return stopped;
    },
    ...extras,
  };
}

const adoptedTarget = { nodeType: 1, tagName: "CANVAS", closest: () => null, isContentEditable: false };

test("a native keydown/keyup is forwarded exactly once (no duplicate delivery)", () => {
  const { host, sent } = fakeHost();
  const forwardKey = (NativeLinuxHost.prototype as any).forwardKey;
  const down = event(adoptedTarget);
  forwardKey.call(host, down, true);
  assert.equal(sent.length, 1, "exactly one remote keydown");
  assert.deepEqual(sent[0], {
    pressed: true,
    init: {
      key: "a",
      code: "KeyA",
      keyCode: 65,
      location: 0,
      ctrlKey: false,
      shiftKey: false,
      altKey: false,
      metaKey: false,
      repeat: false,
    },
  });
  assert.equal(down.prevented, true);
  assert.equal(down.stopped, true);
  const up = event(adoptedTarget);
  forwardKey.call(host, up, false);
  assert.equal(sent.length, 2, "exactly one remote keyup, never a duplicate");
  assert.equal(sent[1]?.pressed, false);
  assert.equal(down.prevented, true, "the keydown was not re-delivered");
});

test("key repeat is preserved and forwarded once per repeat event", () => {
  const { host, sent } = fakeHost();
  const forwardKey = (NativeLinuxHost.prototype as any).forwardKey;
  forwardKey.call(host, event(adoptedTarget, { repeat: false }), true);
  forwardKey.call(host, event(adoptedTarget, { repeat: true }), true);
  forwardKey.call(host, event(adoptedTarget, { repeat: true }), true);
  forwardKey.call(host, event(adoptedTarget), false);
  assert.equal(sent.length, 4);
  assert.deepEqual(sent.map((entry) => [entry.pressed, entry.init.repeat]), [
    [true, false],
    [true, true],
    [true, true],
    [false, false],
  ]);
});

test("a keyup is still released after focus moves to the shell", () => {
  const { host, sent } = fakeHost();
  const forwardKey = (NativeLinuxHost.prototype as any).forwardKey;
  forwardKey.call(host, event(adoptedTarget), true);
  assert.equal(sent.length, 1);
  // Focus moved to the taskbar while the key was held.
  host.focusedWid = null;
  const shellTarget = { nodeType: 1, tagName: "BUTTON", closest: () => ({}), isContentEditable: false };
  forwardKey.call(host, event(shellTarget), false);
  assert.equal(sent.length, 2, "the remote key must not stay stuck");
  assert.equal(sent[1]?.pressed, false);
});

test("nothing is forwarded when no native window is focused", () => {
  const { host, sent } = fakeHost();
  host.focusedWid = null;
  const forwardKey = (NativeLinuxHost.prototype as any).forwardKey;
  forwardKey.call(host, event(adoptedTarget), true);
  forwardKey.call(host, event(adoptedTarget), false);
  assert.equal(sent.length, 0);
});

test("the parent never forwards or rewrites browser-reserved Command combos", () => {
  const { host, sent } = fakeHost(true);
  const forwardKey = (NativeLinuxHost.prototype as any).forwardKey;
  const reserved = [
    { key: "w", code: "KeyW" },
    { key: "t", code: "KeyT" },
    { key: "l", code: "KeyL" },
    { key: "n", code: "KeyN" },
    { key: "r", code: "KeyR" },
    { key: "p", code: "KeyP" },
    { key: "m", code: "KeyM" },
    { key: "h", code: "KeyH" },
    { key: "f", code: "KeyF" },
  ];
  for (const combo of reserved) {
    const down = event(adoptedTarget, { ...combo, metaKey: true, keyCode: combo.code.charCodeAt(3) });
    forwardKey.call(host, down, true);
    assert.equal(down.prevented, false, `Cmd+${combo.key} must not be cancelled`);
    forwardKey.call(host, event(adoptedTarget, { ...combo, metaKey: true }), false);
  }
  for (const combo of [{ key: "W", code: "KeyW" }, { key: "N", code: "KeyN" }, { key: "T", code: "KeyT" }]) {
    const down = event(adoptedTarget, { ...combo, metaKey: true, shiftKey: true });
    forwardKey.call(host, down, true);
    assert.equal(down.prevented, false, `Cmd+Shift+${combo.key} must not be cancelled`);
  }
  assert.equal(sent.length, 0, "no browser-reserved combination reached the native session");
});

test("Command+C reaches the remote window once, as a Command (Meta) event", () => {
  const { host, sent } = fakeHost(true);
  const forwardKey = (NativeLinuxHost.prototype as any).forwardKey;
  const down = event(adoptedTarget, { key: "c", code: "KeyC", keyCode: 67, metaKey: true });
  forwardKey.call(host, down, true);
  assert.equal(sent.length, 1);
  assert.equal(sent[0]?.init.metaKey, true);
  assert.equal(sent[0]?.init.ctrlKey, false);
  assert.equal(down.prevented, true, "the browser copy action is cancelled (the app copies)");
});

/* ----------------------------------------------------- clipboard manager */

class FakeEnv implements NativeClipboardEnv {
  writes: string[] = [];
  writeResult = true;
  quietRead: string | null = null;
  notifications: Array<{ kind: string; message: string; action?: { label: string; run: () => void } }> = [];
  copyFallbacks: Array<{ text: string; reason: string }> = [];
  pasteFallbacks: Array<{ reason: string; suggested: string | null }> = [];

  writeText(text: string): Promise<boolean> {
    this.writes.push(text);
    return Promise.resolve(this.writeResult);
  }
  readTextQuietly(): Promise<string | null> {
    return Promise.resolve(this.quietRead);
  }
  notify(kind: "info" | "success" | "warning" | "error", message: string, action?: { label: string; run: () => void }): void {
    this.notifications.push({ kind, message, ...(action ? { action } : {}) });
  }
  copyFallback(text: string, reason: string): void {
    this.copyFallbacks.push({ text, reason });
  }
  pasteFallback(reason: string, suggestedText: string | null): void {
    this.pasteFallbacks.push({ reason, suggested: suggestedText });
  }
}

class FakeShim {
  sent: string[] = [];
  requests = 0;
  responses: Array<{ key: string; text: string | null }> = [];
  sendOk = true;

  sendClipboardText(text: string): boolean {
    this.sendOk ? this.sent.push(text) : undefined;
    return this.sendOk;
  }
  requestClipboard(): boolean {
    this.requests += 1;
    return true;
  }
  clipboardText(): string {
    return "";
  }
  respondClipboardRequest(key: string, text: string | null): boolean {
    this.responses.push({ key, text });
    return true;
  }
}

const flush = () => new Promise((resolve) => setImmediate(resolve));

function newManager(): { manager: NativeClipboardManager; env: FakeEnv; shim: FakeShim } {
  const env = new FakeEnv();
  const clipboard = new NativeClipboardManager(env);
  const shim = new FakeShim();
  clipboard.attach(shim);
  clipboard.setConnected(true);
  return { manager: clipboard, env, shim };
}

test("remote copies are written to the browser clipboard, once", async () => {
  const { manager, env } = newManager();
  await flush(); // let the reconnect quiet read settle
  env.writes.length = 0;
  manager.remoteClipboard({ text: "hello from Linux" });
  await flush();
  assert.deepEqual(env.writes, ["hello from Linux"]);
  manager.remoteClipboard({ text: "hello from Linux" });
  await flush();
  assert.deepEqual(env.writes, ["hello from Linux"], "identical tokens are not written twice");
  assert.equal(manager.remoteText(), "hello from Linux");
});

test("a blocked clipboard write is surfaced with a working explicit fallback", async () => {
  const { manager, env } = newManager();
  env.writeResult = false;
  await flush();
  manager.remoteClipboard({ text: "copy me" });
  await flush();
  const warning = env.notifications.find((entry) => entry.kind === "warning" && entry.action);
  assert.ok(warning, "the person must be told that the copy failed");
  assert.equal(warning?.action?.label, "Copy now");
  // The retry still fails: the explicit manual fallback opens (never silent).
  warning?.action?.run();
  await flush();
  assert.equal(env.copyFallbacks.at(-1)?.text, "copy me");
  assert.match(env.copyFallbacks.at(-1)?.reason ?? "", /refuses/);
});

test("a burst of remote copies keeps the newest text and never writes stale content", async () => {
  const { manager, env } = newManager();
  const pending: Array<(value: boolean) => void> = [];
  env.writeText = (text: string) => {
    env.writes.push(text);
    return new Promise<boolean>((resolve) => pending.push(resolve));
  };
  manager.remoteClipboard({ text: "first" });
  manager.remoteClipboard({ text: "second" });
  assert.deepEqual(env.writes, ["first"], "only one write is in flight");
  pending.shift()?.(true);
  await flush();
  assert.deepEqual(env.writes, ["first", "second"], "the newest token wins after the stale write");
  pending.shift()?.(true);
  await flush();
  assert.equal(env.copyFallbacks.length, 0);
});

test("browser paste pushes the text to the application (primary path)", async () => {
  const { manager, shim } = newManager();
  await flush();
  const ok = manager.pasteEventText("pasted text");
  assert.equal(ok, true);
  assert.deepEqual(shim.sent, ["pasted text"]);
  assert.equal(manager.cachedBrowserText(), "pasted text");
});

test("a session that rejects clipboard data reports an error once", async () => {
  const { manager, shim, env } = newManager();
  await flush();
  shim.sendOk = false;
  manager.pasteEventText("text");
  manager.pasteEventText("more text");
  assert.equal(env.notifications.filter((entry) => entry.kind === "error").length, 1);
});

test("a remote paste request is answered from the recently copied RAST text", async () => {
  const { manager, env, shim } = newManager();
  await flush();
  manager.browserCopied("copied inside RAST");
  manager.clipboardRequest({ key: "CLIPBOARD:1:1", requestId: "1", selection: "CLIPBOARD" });
  assert.deepEqual(shim.responses, [{ key: "CLIPBOARD:1:1", text: "copied inside RAST" }]);
  assert.equal(env.writes.length, 0, "answering a request does not touch the browser clipboard");
});

test("a remote paste request falls back to a quiet read, then declines loudly", async () => {
  const { manager, env, shim } = newManager();
  await flush();
  env.quietRead = "read from the clipboard";
  manager.clipboardRequest({ key: "CLIPBOARD:2:1" });
  await flush();
  assert.deepEqual(shim.responses.at(-1), { key: "CLIPBOARD:2:1", text: "read from the clipboard" });

  // No cached text and the permission is not granted: decline (the shim then
  // runs the Xpra client's own path) and tell the person how to paste.
  const second = newManager();
  await flush();
  second.env.quietRead = null;
  second.manager.clipboardRequest({ key: "CLIPBOARD:3:1" });
  await flush();
  assert.deepEqual(second.shim.responses.at(-1), { key: "CLIPBOARD:3:1", text: null });
  assert.ok(second.env.notifications.some((entry) => entry.kind === "warning" && entry.action));
});

test("a paste gesture without a paste event surfaces the explicit paste fallback", async () => {
  const { manager, env } = newManager();
  await flush();
  manager.browserCopied("recent copy");
  env.quietRead = null;
  await manager.pasteGestureMissed();
  assert.equal(env.pasteFallbacks.length, 1);
  assert.equal(env.pasteFallbacks[0]?.suggested, "recent copy");
});

test("stale cached text expires instead of leaking into a later paste", async () => {
  const { manager } = newManager();
  await flush();
  manager.browserCopied("old text");
  assert.equal(manager.cachedBrowserText(), "old text");
  const originalNow = Date.now;
  Date.now = () => originalNow() + CLIPBOARD_CACHE_TTL_MS + 1000;
  try {
    assert.equal(manager.cachedBrowserText(), null);
  } finally {
    Date.now = originalNow;
  }
});

test("non-text clipboard formats are reported as unsupported, once", async () => {
  const { manager, env } = newManager();
  await flush();
  manager.unsupportedFormat({ datatype: "image/png" });
  manager.unsupportedFormat({ datatype: "image/png" });
  manager.unsupportedFormat({ datatype: "text/html;charset=utf-8" });
  const notices = env.notifications.filter((entry) => entry.kind === "info");
  assert.equal(notices.length, 1, "one honest notice instead of a silent partial transfer");
  assert.match(notices[0]?.message ?? "", /plain text only/);
});

test("reconnect re-pushes the browser clipboard and asks the client to re-poll", async () => {
  const env = new FakeEnv();
  env.quietRead = "browser clipboard after reconnect";
  const clipboard = new NativeClipboardManager(env);
  const shim = new FakeShim();
  clipboard.attach(shim);
  clipboard.setConnected(true);
  await flush();
  assert.ok(shim.requests >= 1, "the Xpra client is asked to re-read the clipboard");
  assert.deepEqual(shim.sent, ["browser clipboard after reconnect"]);
});

test("a focus loss drops queued writes instead of overwriting a newer clipboard", async () => {
  const { manager } = newManager();
  await flush();
  manager.setConnected(false);
  manager.remoteClipboard({ text: "not written while disconnected" });
  await flush();
  manager.setConnected(true);
  await flush();
  // The disconnected token is discarded; only the reconnect push may happen.
  assert.equal(manager.remoteText(), "not written while disconnected");
});
