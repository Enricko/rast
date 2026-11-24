import assert from "node:assert/strict";
import test from "node:test";
import { readFileSync } from "node:fs";
import vm from "node:vm";

// Execute the actual injected bridge, not a second implementation of its math.
const source = readFileSync("../src/linux_apps/xpra_shim.js", "utf8");
function bridge() {
  const content = {
    getBoundingClientRect: () => ({ left: 140, top: 90, width: 500, height: 300 }),
  };
  const adopted = { getAttribute: () => "7" };
  const target = { closest: () => adopted };
  const win = { wid: 7, x: 400, y: 200, w: 1000, h: 600, __rastContentEl: content };
  const client = {
    container: {}, id_to_window: { 7: win }, connected: true,
    getMouse: () => ({ x: -1, y: -1, button: 0 }),
  };
  const window = {
    client, parent: null, addEventListener() {}, setInterval() { return 1; },
    clearInterval() {}, setTimeout() { return 1; }, clearTimeout() {},
    location: { pathname: "/linux-apps/session/0123456789abcdef0123456789abcdef/" },
    RASTNative: undefined as any,
  };
  vm.runInNewContext(source, { window, console: { info() {}, warn() {} }, document: {} });
  return { client, win, target, api: window.RASTNative };
}

test("adopted pointer motion and buttons use identical remote coordinates", () => {
  const { client, win, target } = bridge();
  const event = { clientX: 240, clientY: 150, button: 0, target };
  // Xpra on_mousemove calls getMouse(e) without win; buttons pass win.
  const motion = client.getMouse(event);
  const click = client.getMouse(event, win);
  assert.deepEqual(motion, click);
  assert.equal(click.x, 600);
  assert.equal(click.y, 320);
  assert.equal(click.button, 1);
  assert.equal(click.x - win.x, 200);
  assert.equal(click.y - win.y, 120);
});

test("jQuery wrapped buttons use the native event target and button", () => {
  const { client, target } = bridge();
  const result = client.getMouse({ originalEvent: { clientX: 140, clientY: 90, which: 3, target } });
  assert.equal(result.x, 400);
  assert.equal(result.y, 200);
  assert.equal(result.button, 3);
});

test("non-adopted events retain upstream pointer behavior", () => {
  const { client } = bridge();
  assert.equal(client.getMouse({ clientX: 1 }).x, -1);
});

test("trusted audio unlock resumes only output and retries a blocked player", async () => {
  const { client, api } = bridge();
  let resumed = 0, played = 0;
  Object.assign(client, {
    audio_context: { state: "suspended", resume: async () => { resumed++; } },
    audio: { paused: true, play: async () => { played++; } },
  });
  assert.equal(api.unlockAudio(), true);
  assert.equal(resumed, 1);
  assert.equal(played, 1);
});

/* --------------------------------------------- shim: clipboard and policy */

interface BridgeOptions {
  navigator?: { platform?: string; userAgent?: string; userAgentData?: { platform?: string } } | undefined;
  utilities?: any;
}

/** Richer harness: a live XpraClient stand-in with the clipboard methods. */
function clipboardBridge(options: BridgeOptions = {}) {
  const calls = {
    strings: [] as unknown[][],
    tokenSends: [] as unknown[],
    polls: 0,
    requests: [] as unknown[][],
    releases: [] as unknown[][],
  };
  const content = {
    getBoundingClientRect: () => ({ left: 10, top: 20, width: 250, height: 150 }),
  };
  const adopted = { getAttribute: () => "7" };
  const target = { closest: () => adopted };
  const win = {
    wid: 7, x: 400, y: 200, w: 500, h: 300,
    __rastContentEl: content,
    __rastLastPointerEvent: { clientX: 20, clientY: 30 },
  } as any;
  const client: any = {
    container: {},
    id_to_window: { 7: win },
    connected: true,
    clipboard_enabled: true,
    clipboard_direction: "both",
    clipboard_buffer: "",
    swap_keys: false,
    capture_keyboard: false,
    buttons_pressed: new Set<number>([1]),
    packet_handlers: {},
    getMouse: () => ({ x: -1, y: -1, button: 0 }),
    _process_clipboard_token(packet: unknown[]) {
      const datatype = String(packet[4] ?? "");
      if (datatype.includes("text") || datatype.includes("string")) {
        const bytes = packet[7] as ArrayLike<number>;
        this.clipboard_buffer = Array.from(bytes, (byte) => String.fromCharCode(Number(byte))).join("");
      }
    },
    _process_clipboard_request(packet: unknown[]) {
      calls.requests.push(packet);
    },
    send_clipboard_string(id: number, selection: string, text: string) {
      calls.strings.push([id, selection, text]);
    },
    send_clipboard_token(bytes: unknown) {
      calls.tokenSends.push(bytes);
    },
    _poll_clipboard() {
      calls.polls += 1;
    },
    release_buttons(event: unknown, window: unknown) {
      calls.releases.push([event, window]);
    },
  };
  const window = {
    client,
    parent: null,
    navigator: options.navigator,
    Utilities: options.utilities,
    addEventListener() {},
    setInterval() { return 1; },
    clearInterval() {},
    setTimeout: (fn: () => void, ms: number) => setTimeout(fn, ms),
    clearTimeout: (timer: any) => clearTimeout(timer),
    location: { pathname: "/linux-apps/session/0123456789abcdef0123456789abcdef/" },
    RASTNative: undefined as any,
  };
  vm.runInNewContext(source, {
    window,
    console: { info() {}, warn() {} },
    document: {},
    TextEncoder,
  });
  const events: Record<string, unknown[]> = {};
  const api = window.RASTNative;
  for (const name of ["clipboard", "clipboard-format", "clipboard-request", "focus-request"]) {
    events[name] = [];
    api.on(name, (payload: unknown) => events[name].push(payload));
  }
  return { client, win, api, calls, events, window };
}

const textToken = (datatype: unknown, text: string): unknown[] => [
  1, "CLIPBOARD", [], "UTF8_STRING", datatype, 8, "bytes",
  new TextEncoder().encode(text),
];

test("remote text tokens are forwarded to the desktop exactly once", () => {
  const { client, api, events } = clipboardBridge();
  client._process_clipboard_token(textToken("text/plain", "hello remote"));
  const clipboards = events.clipboard as Array<{ text: string; datatype: string; selection: string }>;
  assert.equal(clipboards.length, 1);
  assert.equal(clipboards[0]?.text, "hello remote");
  assert.equal(clipboards[0]?.selection, "CLIPBOARD");
  assert.equal(api.clipboardText(), "hello remote");
  // A duplicated token does not re-write the browser clipboard.
  client._process_clipboard_token(textToken("text/plain", "hello remote"));
  assert.equal((events.clipboard as unknown[]).length, 1);
});

test("image tokens are reported as unsupported and never replace text", () => {
  const { client, events } = clipboardBridge();
  client._process_clipboard_token(textToken("text/plain", "old text"));
  client._process_clipboard_token(textToken("image/png", "\u0089PNG"));
  const formats = events["clipboard-format"] as Array<{ datatype: string; supported: boolean }>;
  assert.equal(formats.length, 1);
  assert.equal(formats[0]?.datatype, "image/png");
  assert.equal(formats[0]?.supported, false);
  assert.equal((events.clipboard as unknown[]).length, 1, "stale text is never re-emitted");
});

test("the client answers a clipboard request from the desktop, not from the unfocused iframe", () => {
  const { client, api, calls, events } = clipboardBridge();
  client._process_clipboard_request([9, 42, "CLIPBOARD"]);
  assert.equal(calls.requests.length, 0, "the Xpra client fallback is deferred");
  const request = (events["clipboard-request"] as Array<{ key: string; requestId: string; selection: string }>)[0];
  assert.ok(request?.key);
  assert.equal(request?.requestId, "42");
  assert.equal(api.respondClipboardRequest(request.key, "from the browser"), true);
  assert.deepEqual(calls.strings, [[42, "CLIPBOARD", "from the browser"]]);
  assert.equal(calls.requests.length, 0, "the client fallback never ran");
});

test("an unanswered clipboard request falls back to the Xpra client path", async () => {
  const { client, calls } = clipboardBridge();
  client._process_clipboard_request([9, 43, "PRIMARY"]);
  assert.equal(calls.requests.length, 0);
  await new Promise((resolve) => setTimeout(resolve, 900));
  assert.equal(calls.requests.length, 1, "the application's paste never hangs");
});

test("declining a clipboard request runs the client fallback immediately", () => {
  const { client, api, calls, events } = clipboardBridge();
  client._process_clipboard_request([9, 44, "CLIPBOARD"]);
  const request = (events["clipboard-request"] as Array<{ key: string }>)[0];
  assert.ok(request);
  // The desktop cannot read the clipboard: the client serves its own buffer.
  client.clipboard_buffer = "server buffer";
  assert.equal(api.respondClipboardRequest(request.key, null), true);
  assert.equal(calls.requests.length, 1);
});

test("push to the application is refused when the session disables clipboard", () => {
  const { client, api } = clipboardBridge();
  assert.equal(api.sendClipboardText("hello"), true);
  client.clipboard_enabled = false;
  assert.equal(api.sendClipboardText("hello"), false);
  client.clipboard_enabled = true;
  client.connected = false;
  assert.equal(api.sendClipboardText("hello"), false);
});

test("an echoed token for visited clipboard text does not loop", () => {
  const { client, api, events } = clipboardBridge();
  api.sendClipboardText("browser to remote");
  client._process_clipboard_token(textToken("text/plain", "browser to remote"));
  assert.equal((events.clipboard as unknown[]).length, 0, "the browser clipboard is not overwritten");
});

test("the keyboard/clipboard contract is pinned from the desktop platform", () => {
  const generic = clipboardBridge();
  assert.equal(generic.client.capture_keyboard, true, "key capture is always enabled for the desktop");
  assert.equal(generic.client.swap_keys, false, "no Meta/Control swap on non-macOS clients");

  const mac = clipboardBridge({
    navigator: { platform: "MacIntel", userAgent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 14_0)" },
  });
  assert.equal(mac.client.swap_keys, true, "Command becomes Control for the Linux application");
  assert.equal(mac.client.capture_keyboard, true);
});

test("shift+wheel becomes horizontal scrolling; real horizontal deltas are untouched", () => {
  const utilities = {
    normalizeWheel(event: any) {
      return {
        spinX: 0,
        spinY: event.deltaY > 0 ? -1 : 1,
        pixelX: event.deltaX || 0,
        pixelY: event.deltaY || 0,
        deltaMode: 0,
      };
    },
  };
  clipboardBridge({ utilities });
  const vertical = utilities.normalizeWheel({ shiftKey: true, deltaY: 120 });
  assert.equal(vertical.pixelX, 120);
  assert.equal(vertical.pixelY, 0);
  assert.equal(vertical.spinX, 1);
  const plain = utilities.normalizeWheel({ shiftKey: false, deltaY: -120 });
  assert.equal(plain.pixelY, -120);
  assert.equal(plain.pixelX, 0);
  const horizontal = utilities.normalizeWheel({ shiftKey: true, deltaX: 80 });
  assert.equal(horizontal.pixelX, 80);
  assert.equal(horizontal.pixelY, 0);
});

test("pointer mapping follows the RAST window scale (zoom/DPI independent)", () => {
  const { client, win, win: _win } = clipboardBridge();
  // The adopted canvas is shown at 250 CSS px while the remote window is
  // 500 px wide (e.g. a 200% browser zoom): local pixels double.
  const result = client.getMouse({ clientX: 140, clientY: 90, button: 0, target: { closest: () => ({ getAttribute: () => "7" }) } }, win);
  assert.equal(result.x, 400 + (140 - 10) * 2);
  assert.equal(result.y, 200 + (90 - 20) * 2);
  assert.equal(result.x - win.x, 260);
  assert.equal(result.y - win.y, 140);
});

test("right and middle buttons keep their X11 button numbers", () => {
  const { client, win } = clipboardBridge();
  const right = client.getMouse({ originalEvent: { clientX: 10, clientY: 20, which: 3, target: { closest: () => ({ getAttribute: () => "7" }) } } });
  assert.equal(right.button, 3);
  const middle = client.getMouse({
    originalEvent: { clientX: 10, clientY: 20, which: 2, target: { closest: () => ({ getAttribute: () => "7" }) } },
  });
  assert.equal(middle.button, 2);
  assert.ok(win);
});

test("releaseInput releases every adopted remote window and clears button state", () => {
  const { client, api, calls } = clipboardBridge();
  assert.equal(api.releaseInput(), true);
  assert.equal(calls.releases.length, 1, "the held button is released on the server");
  assert.equal(client.buttons_pressed.size, 0);
});
