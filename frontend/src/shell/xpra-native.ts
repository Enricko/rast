// Native Linux apps: one shared Xpra seamless session per RAST user.
//
// Every native app the person starts runs inside the same user-owned Xpra
// session (started lazily by the server). The session's HTML5 client is
// embedded once, off-screen, in a same-origin iframe proxied at
// `/linux-apps/session/<key>/`; the RAST desktop window manager is the window
// manager for that session:
//
//   remote Xpra window  ->  one RAST window (title, taskbar entry, focus,
//                            resize, minimize/maximize, close, transient
//                            dialogs attached to their parent)
//
// The shim inside the iframe (`src/linux_apps/xpra_shim.js`) exposes
// `window.RASTNative`; it adopts each remote window's DOM node into the RAST
// window body (one client connection, no per-window streams) and forwards
// window commands, keyboard, mouse and clipboard. If adoption fails the shim
// is still usable but that window shows an error.
//
// Docker-backed apps keep their existing behaviour: this module is only used
// for apps whose server-side `backend` is "native", and falls back to the
// legacy path when the server does not support native apps.

import { h, isElementNode } from "../core/dom";
import type { ApiClient } from "../core/api";
import type { AppDefinition } from "../core/app";
import { getRealtime, type RealtimeStore } from "../core/realtime";
import type { EventStream } from "../core/sse";
import type { ToastCenter } from "../core/toast";
import { copyText, errorMessage, modal } from "../core/ui";
import type { WindowHandle, WindowInfo, WindowManager, WindowRect } from "../core/wm";
import {
  LINUX_PREFIX,
  isNativeApp,
  linuxAppById,
  linuxApps,
  linuxNativeSession,
  linuxOverview,
  matchWindowApp,
  nativeWindowAppId,
  setLinuxNativeSession,
  type LinuxApp,
  type NativeSession,
} from "../apps/linuxapp/catalog";

/* ---------------------------------------------------------------- shim types */

interface NativeShimWindow {
  wid: string;
  title: string;
  x: number;
  y: number;
  width: number;
  height: number;
  minimized: boolean;
  maximized: boolean;
  focused: boolean;
  stacking: number;
  modal: boolean;
  transientFor: string | null;
  windowType: string[];
  classInstance: string[];
  overrideRedirect: boolean;
  resizable: boolean;
  iconUrl: string | null;
  adopted: boolean;
}

interface NativeShimState {
  version: string;
  key: string | null;
  ready: boolean;
  connected: boolean;
  separate: boolean;
  error: string | null;
  disconnectReason: string | null;
  size: [number, number];
  clipboardEnabled: boolean;
  /** `both`, `to-server`, `to-client` or empty when the client is disconnected. */
  clipboardDirection?: string;
  clipboardPending?: number;
  clipboardLastEventAt?: number;
  keyboardSwap?: boolean;
  captureKeyboard: boolean;
  windows: NativeShimWindow[];
}

export interface KeyboardInit {
  key?: string;
  code?: string;
  keyCode?: number;
  /** DOM_KEY_LOCATION_*: distinguishes left/right modifiers, numpad, ... */
  location?: number;
  ctrlKey?: boolean;
  shiftKey?: boolean;
  altKey?: boolean;
  metaKey?: boolean;
  repeat?: boolean;
}

/** Payload of the shim's `clipboard` event (remote copy landed in the client). */
export interface ShimClipboardPayload {
  text?: string;
  datatype?: string;
  selection?: string;
}

/**
 * Payload of the shim's `clipboard-request` event: the remote application wants
 * the current browser clipboard (a paste inside the app). The parent answers
 * with `respondClipboardRequest(key, text)`; `null` hands the request back to
 * the Xpra client's own fallback path.
 */
export interface ShimClipboardRequest {
  key?: string;
  requestId?: string;
  selection?: string;
}

/** Payload of the shim's `clipboard-format` event (non-text clipboard data). */
export interface ShimClipboardFormat {
  datatype?: string;
  selection?: string;
  supported?: boolean;
}

export interface NativeShimApi {
  version: string;
  key: string | null;
  ready: boolean;
  state: NativeShimState;
  windows(): NativeShimWindow[];
  snapshot(): NativeShimState;
  focusWindow(wid: string): boolean;
  closeWindow(wid: string): boolean;
  resizeWindow(wid: string, geometry: { width?: number; height?: number }): boolean;
  adoptWindow(wid: string, container: HTMLElement): boolean;
  detachWindow(wid: string): boolean;
  setMinimized(wid: string, minimized: boolean): boolean;
  requestRefresh(wid: string): boolean;
  unlockAudio?(): boolean;
  sendKey(pressed: boolean, init: KeyboardInit): boolean;
  sendClipboardText(text: string): boolean;
  clipboardText(): string;
  requestClipboard(): boolean;
  /** Answer a deferred `clipboard-request`; `null` declines (client fallback). */
  respondClipboardRequest?(key: string, text: string | null): boolean;
  /** Release every mouse button held by the remote window (focus loss). */
  releaseInput?(): boolean;
  reconnect(): boolean;
  disconnect(): boolean;
  on(event: string, callback: (payload?: any) => void): () => void;
  off(event: string, callback: (payload?: any) => void): void;
}

/* --------------------------------------------------------- keyboard model */
//
// The RAST desktop document owns the keyboard; the Xpra HTML5 client's own key
// handling runs in the hidden transport iframe. Every key event therefore
// reaches the parent first, which decides what the remote window receives:
//
//   `shell`     RAST keeps the key (no native window focused, shell control
//               focused, a modal is open, or an IME composition is running)
//   `reserved`  the browser or the operating system owns the combination; it is
//               never forwarded and never cancelled
//   `forward`   forwarded to the focused native window through the shim
//
// macOS: Command is the shortcut modifier while Linux applications expect
// Control. The pinned Xpra HTML5 client already maps this correctly through
// its `swap_keys` mode (it translates Meta to Control for key *and* mouse
// modifiers and delays a Ctrl/Cmd+V key packet behind the browser paste event
// so the clipboard token is pushed first). RAST therefore forwards the real
// Command event and only takes policy decisions here: rewriting modifiers in
// this file would be swapped a second time by the client. The shim pins
// `swap_keys` from the detected platform so this contract does not depend on a
// viewer URL parameter.

export type NativeKeyMode = "shell" | "reserved" | "forward";

/** Clipboard gestures the browser fires around a forwarded copy/cut/paste. */
export type NativeKeyGesture = "copy" | "cut" | "paste";

export interface NativeKeyContext {
  /** Client OS is macOS: Command is the shortcut modifier. */
  mac: boolean;
  /** A native window currently owns the remote focus. */
  nativeFocused: boolean;
  /** The event target is a RAST shell control or an editable field. */
  shellTarget: boolean;
  /** A RAST modal dialog is open (the shell keeps the keyboard). */
  modalOpen: boolean;
  /** The event belongs to an IME composition (Xpra HTML5 v21 cannot forward it). */
  composing: boolean;
  /** The transport viewer is connected (never forward into a dead session). */
  connected: boolean;
}

export interface NativeKeyDecision {
  mode: NativeKeyMode;
  init: KeyboardInit;
  /** True when the browser default action must be cancelled. */
  preventDefault: boolean;
  gesture: NativeKeyGesture | null;
}

/** How long the browser has to deliver the paste event of a paste gesture. */
export const PASTE_EVENT_GRACE_MS = 250;
/** How long a copied text may be reused to answer a remote paste request. */
export const CLIPBOARD_CACHE_TTL_MS = 120_000;

/**
 * macOS Command combinations RAST forwards to the native application. The
 * pinned client turns Command into Control, so these become the usual Linux
 * editing shortcuts (select-all, undo/redo, save). Copy/cut/paste are handled
 * before this set because they also drive the browser clipboard gesture.
 */
export const MAC_COMMAND_FORWARD_KEYS: ReadonlySet<string> = new Set(["a", "z", "y", "s"]) as ReadonlySet<string>;

/**
 * macOS Command combinations reserved by the browser or the operating system.
 * They are never forwarded and never cancelled: the browser keeps tab/window
 * management, navigation and the volume of its own UI, like it does for any
 * web page (a page cannot reliably override them anyway).
 */
export const MAC_COMMAND_RESERVED: ReadonlySet<string> = new Set([
  "w", "t", "n", "l", "r", "p", "m", "h", "f", "o", "q", ",", "1", "2", "3", "4", "5", "6", "7", "8", "9",
  "[", "]", "`", "\\", "Backspace", "Delete", "ArrowLeft", "ArrowRight", "ArrowUp", "ArrowDown", "Tab", "Escape",
]) as ReadonlySet<string>;

/** Command+Shift combinations owned by the browser (inspect/fullscreen/...). */
export const MAC_COMMAND_RESERVED_SHIFTED: ReadonlySet<string> = new Set([
  "w", "t", "n", "r", "q", "c", "3", "4", "5",
]) as ReadonlySet<string>;

/** Windows/Linux browser-reserved Control combinations (tab/window/navigation). */
export const OTHER_BROWSER_RESERVED: ReadonlySet<string> = new Set([
  "w", "t", "n", "l", "r", "p", "1", "2", "3", "4", "5", "6", "7", "8", "9",
]) as ReadonlySet<string>;

/** Windows/Linux browser-reserved Control+Shift combinations. */
export const OTHER_BROWSER_RESERVED_SHIFTED: ReadonlySet<string> = new Set(["w", "t", "n", "r", "p"]) as ReadonlySet<string>;

/** Function keys that stay with the browser (reload, fullscreen, devtools). */
export const RESERVED_FUNCTION_KEYS: ReadonlySet<string> = new Set(["F5", "F11", "F12"]) as ReadonlySet<string>;

/** Developer-tools letter shortcuts (Control+Shift on Windows/Linux). */
const DEVTOOLS_KEYS: ReadonlySet<string> = new Set(["i", "j", "c", "k"]) as ReadonlySet<string>;

const MODIFIER_KEY_NAMES: ReadonlySet<string> = new Set([
  "Shift", "Control", "Alt", "Meta", "AltGraph", "CapsLock", "OS", "Super", "Hyper",
]) as ReadonlySet<string>;

/**
 * Selector for RAST-owned keyboard contexts. Clicking any of these keeps the
 * keyboard in the desktop shell: typing in a form, operating the taskbar/start
 * menu/dialog buttons, or working inside a modal.
 */
const SHELL_KEYBOARD_SELECTOR =
  "input, textarea, select, [contenteditable='true'], [contenteditable=''], " +
  ".taskbar, .start-menu, .context-menu, .panel, .tray, .modal, .auth";

/** Element of the RAST-owned window body for an adopted native surface. */
export function nativeSurfaceFor(target: EventTarget | null): Element | null {
  if (!isElementNode(target)) return null;
  try {
    return target.closest(".linux-native-window");
  } catch {
    return null;
  }
}

/** True when the event target belongs to a RAST shell control / editable. */
export function isShellKeyboardTarget(target: EventTarget | null): boolean {
  if (!isElementNode(target)) return false;
  try {
    if (target.closest(SHELL_KEYBOARD_SELECTOR)) return true;
  } catch {
    /* detached or cross-realm node: fall through to the cheap checks */
  }
  if ((target as HTMLElement).isContentEditable === true) return true;
  const tag = target.tagName;
  return tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT";
}

/** Detect macOS without relying on a deprecated-only field. */
export function detectMacPlatform(
  nav: { userAgentData?: { platform?: string }; platform?: string; userAgent?: string } | null | undefined = globalThis.navigator,
): boolean {
  if (!nav) return false;
  const platform = nav.userAgentData?.platform || nav.platform || "";
  if (/mac/i.test(platform)) return true;
  return /Macintosh|Mac OS X/i.test(nav.userAgent ?? "");
}

function normalizedKeyName(key: string | undefined): string {
  if (!key) return "";
  return key.length === 1 ? key.toLowerCase() : key;
}

function isModifierKey(event: KeyboardInit): boolean {
  const key = event.key ?? "";
  if (MODIFIER_KEY_NAMES.has(key)) return true;
  const code = event.code ?? "";
  return (
    code.startsWith("Shift") ||
    code.startsWith("Control") ||
    code.startsWith("Alt") ||
    code.startsWith("Meta") ||
    code === "CapsLock"
  );
}

/**
 * Decide what happens to one keydown/keyup. Pure so the complete policy is unit
 * tested without a browser: see `frontend/tests/input-clipboard.test.ts`.
 */
export function decideNativeKey(
  event: KeyboardInit & { isComposing?: boolean },
  context: NativeKeyContext,
): NativeKeyDecision {
  const init: KeyboardInit = {
    key: event.key,
    code: event.code,
    keyCode: event.keyCode,
    location: event.location,
    ctrlKey: event.ctrlKey,
    shiftKey: event.shiftKey,
    altKey: event.altKey,
    metaKey: event.metaKey,
    repeat: event.repeat,
  };
  const shell: NativeKeyDecision = { mode: "shell", init, preventDefault: false, gesture: null };
  const reserved: NativeKeyDecision = { mode: "reserved", init, preventDefault: false, gesture: null };
  const forward = (preventDefault: boolean, gesture: NativeKeyGesture | null = null): NativeKeyDecision => ({
    mode: "forward",
    init,
    preventDefault,
    gesture,
  });

  if (!context.connected) return shell;
  if (!context.nativeFocused || context.shellTarget || context.modalOpen || context.composing) return shell;
  if (event.isComposing === true) return shell;
  const key = normalizedKeyName(event.key);
  if (RESERVED_FUNCTION_KEYS.has(event.key ?? "")) return reserved;

  // A modifier press on its own is never a command: forward it so remote
  // chords work (the client swaps Meta/Control on macOS by itself).
  if (isModifierKey(event)) return forward(false);

  if (context.mac) {
    // Command+Option / Command+Control belong to the OS or the browser
    // (devtools, fullscreen, tab switching); never forward them.
    if (event.metaKey && (event.ctrlKey || event.altKey)) return reserved;
    if (event.metaKey) {
      if (event.shiftKey) {
        if (key === "z") return forward(true); // redo
        if (key === "v") return forward(false, "paste"); // paste special
        return reserved;
      }
      if (key === "v") return forward(false, "paste");
      if (key === "c") return forward(true, "copy");
      if (key === "x") return forward(true, "cut");
      if (MAC_COMMAND_FORWARD_KEYS.has(key)) return forward(true);
      return reserved;
    }
    // Control on macOS is not the Command modifier; forward it (the client
    // swaps it to Meta for the Linux application, which is its documented
    // `swap_keys` behaviour). Option/Alt is forwarded as Alt.
    return forward(true);
  }

  // Windows/Linux: Control is the primary shortcut modifier; Meta (Windows /
  // Super) belongs to the OS.
  if (event.metaKey) return reserved;
  if (event.ctrlKey) {
    if (event.shiftKey && DEVTOOLS_KEYS.has(key)) return reserved;
    if (!event.shiftKey && OTHER_BROWSER_RESERVED.has(key)) return reserved;
    if (event.shiftKey && OTHER_BROWSER_RESERVED_SHIFTED.has(key)) return reserved;
    if (!event.shiftKey && key === "c") return forward(true, "copy");
    if (!event.shiftKey && key === "x") return forward(true, "cut");
    if (key === "v") return forward(false, "paste");
    return forward(true);
  }
  if (event.shiftKey && (event.key === "Insert" || key === "Insert")) return forward(false, "paste");
  return forward(true);
}

/* --------------------------------------------------------- clipboard model */
//
// Xpra's clipboard is a best-effort token protocol: the server pushes text
// tokens, the client writes them to the browser clipboard, and the client
// answers the server's clipboard requests with browser clipboard text. The
// pinned client does that itself *inside its own document*, which is hidden
// and unfocused here — browsers reject clipboard API calls from an unfocused
// document, which is why copies used to fail silently. RAST therefore runs
// the clipboard in the focused desktop document:
//
//   remote copy  -> shim `clipboard` event -> manager writes navigator.clipboard
//   browser copy -> captured text (copy event / paste event / quiet read)
//   remote paste -> shim `clipboard-request` -> manager answers with that text
//
// Every refusal is surfaced to the person (toast with an action, or an explicit
// copy/paste fallback dialog). Nothing is ignored silently. Text/plain only:
// the negotiated Xpra protocol in this deployment is text-first, so HTML and
// images trigger an honest notice instead of a partial/failed transfer.

export interface NativeClipboardEnv {
  /** Write text to the browser clipboard; resolves false when refused. */
  writeText(text: string): Promise<boolean>;
  /** Read the browser clipboard only when no permission prompt can appear. */
  readTextQuietly(): Promise<string | null>;
  notify(
    kind: "info" | "success" | "warning" | "error",
    message: string,
    action?: { label: string; run: () => void },
  ): void;
  /** Explicit fallback showing text the person can select and copy by hand. */
  copyFallback(text: string, reason: string): void;
  /** Explicit fallback where the person pastes text to send to the application. */
  pasteFallback(reason: string, suggestedText: string | null): void;
}

interface ClipboardShim {
  sendClipboardText(text: string): boolean;
  requestClipboard(): boolean;
  clipboardText(): string;
  respondClipboardRequest?(key: string, text: string | null): boolean;
}

/**
 * Two-way text clipboard between the focused desktop document and the shared
 * native session. All browser APIs are injected (`NativeClipboardEnv`) so the
 * complete policy is unit tested without a DOM.
 */
export class NativeClipboardManager {
  readonly env: NativeClipboardEnv;
  private shim: ClipboardShim | null = null;
  private connected = false;
  /** Last browser clipboard text seen by a copy/paste event (best effort). */
  private browserText: string | null = null;
  private browserTextAt = 0;
  private lastWritten = "";
  private lastRemote = "";
  private pendingWrite: string | null = null;
  private writing = false;
  private failureText: string | null = null;
  private failureAt = 0;
  private readNoticeShown = false;
  private sessionNoticeShown = false;
  private readonly unsupportedFormats = new Set<string>();

  constructor(env: NativeClipboardEnv) {
    this.env = env;
  }

  attach(shim: ClipboardShim): void {
    this.shim = shim;
  }

  detach(): void {
    this.shim = null;
    this.pendingWrite = null;
    this.failureText = null;
    this.connected = false;
  }

  setConnected(connected: boolean): void {
    if (this.connected === connected) return;
    this.connected = connected;
    if (!connected) {
      this.pendingWrite = null;
      this.failureText = null;
      return;
    }
    void this.reconnect();
  }

  /** Last text the remote desktop sent us (debugging/status). */
  remoteText(): string {
    return this.lastRemote;
  }

  /** Text copied inside RAST, reusable for a remote paste request. */
  cachedBrowserText(): string | null {
    if (this.browserText === null) return null;
    if (Date.now() - this.browserTextAt > CLIPBOARD_CACHE_TTL_MS) return null;
    return this.browserText;
  }

  /**
   * The remote application copied text (Xpra clipboard token). Writes it to the
   * browser clipboard, coalescing bursts and never clobbering a newer token.
   */
  remoteClipboard(payload: ShimClipboardPayload): void {
    const text = typeof payload.text === "string" ? payload.text : "";
    if (!text) return;
    this.lastRemote = text;
    this.queueWrite(text);
  }

  /** The application copied something that is not plain text. Tell the person. */
  unsupportedFormat(payload: ShimClipboardFormat): void {
    const datatype = (payload.datatype || "unknown").toLowerCase();
    const key = datatype.split(";")[0] ?? datatype;
    if (this.unsupportedFormats.has(key)) return;
    const firstNotice = this.unsupportedFormats.size === 0;
    this.unsupportedFormats.add(key);
    if (!firstNotice) return;
    this.env.notify(
      "info",
      "The native application copied non-text data (images/HTML). RAST transfers plain text only, so it was not placed on your clipboard.",
    );
  }

  /** Text the person copied inside the RAST desktop (copy/cut event). */
  browserCopied(text: string): void {
    if (!text) return;
    this.browserText = text;
    this.browserTextAt = Date.now();
  }

  /** Paste event text: the reliable browser -> remote path. */
  pasteEventText(text: string): boolean {
    if (!text) return false;
    this.browserCopied(text);
    return this.pushToRemote(text);
  }

  /**
   * A paste gesture (Cmd/Ctrl+V) was forwarded but the browser never delivered
   * the paste event. Try a quiet clipboard read, otherwise show the explicit
   * fallback dialog. Called by the host after `PASTE_EVENT_GRACE_MS`.
   */
  async pasteGestureMissed(): Promise<void> {
    const text = await this.env.readTextQuietly();
    if (text) {
      this.browserCopied(text);
      if (this.pushToRemote(text)) return;
    }
    this.env.pasteFallback(
      "The browser did not deliver the paste event, so RAST could not read your clipboard for this paste.",
      this.cachedBrowserText(),
    );
  }

  /**
   * The remote application asked for the clipboard (a paste inside the app).
   * Answer from the freshest source available, or hand the request back to the
   * Xpra client's fallback path so the application never hangs.
   */
  clipboardRequest(payload: ShimClipboardRequest): void {
    const shim = this.shim;
    const key = typeof payload.key === "string" ? payload.key : "";
    if (!shim?.respondClipboardRequest || !key) return;
    const cached = this.cachedBrowserText();
    if (cached !== null) {
      shim.respondClipboardRequest(key, cached);
      return;
    }
    void (async () => {
      const text = await this.env.readTextQuietly();
      if (text) {
        this.browserCopied(text);
        shim.respondClipboardRequest?.(key, text);
        return;
      }
      // Decline: the shim runs the Xpra client's own fallback immediately.
      shim.respondClipboardRequest?.(key, null);
      if (!this.readNoticeShown) {
        this.readNoticeShown = true;
        this.env.notify(
          "warning",
          "RAST could not read your browser clipboard for the application's paste. Copy the text inside RAST, or use the paste fallback.",
          { label: "Paste manually…", run: () => this.env.pasteFallback("Clipboard read permission was refused.", null) },
        );
      }
    })();
  }

  /** The session (re)connected: re-establish clipboard ownership both ways. */
  async reconnect(): Promise<void> {
    if (!this.connected) return;
    this.lastWritten = "";
    this.failureText = null;
    this.pendingWrite = null;
    try {
      this.shim?.requestClipboard();
    } catch {
      /* the client may still be starting; the next event retries */
    }
    const text = await this.env.readTextQuietly();
    if (text) {
      this.browserCopied(text);
      this.pushToRemote(text);
    }
  }

  /** The browser window regained focus: retry a write the browser refused. */
  onWindowFocus(): void {
    if (this.failureText === null) return;
    const text = this.failureText;
    this.failureText = null;
    this.queueWrite(text);
  }

  /** Push text to the remote selection; surfaces a disabled/closed session. */
  pushToRemote(text: string): boolean {
    if (!text) return false;
    const shim = this.shim;
    if (!shim) return false;
    const ok = shim.sendClipboardText(text);
    if (ok) return true;
    if (!this.sessionNoticeShown) {
      this.sessionNoticeShown = true;
      this.env.notify(
        "error",
        "The native session did not accept clipboard data. Clipboard sharing may be disabled by the server or the session is reconnecting.",
      );
    }
    return false;
  }

  /** Queue a browser clipboard write, coalescing bursts of remote tokens. */
  private queueWrite(text: string): void {
    if (!this.connected) return;
    if (text === this.lastWritten) return;
    this.pendingWrite = text;
    this.pumpWrite();
  }

  private pumpWrite(): void {
    if (this.writing) return;
    const text = this.pendingWrite;
    if (text === null) return;
    this.pendingWrite = null;
    this.writing = true;
    void this.env.writeText(text).then((ok) => {
      this.writing = false;
      if (ok) {
        this.lastWritten = text;
        const recovered = this.failureText !== null && this.failureAt > 0;
        this.failureText = null;
        if (recovered) this.env.notify("info", "Copied the application's text to your browser clipboard.");
      } else if (this.pendingWrite === null) {
        // A newer token is queued: let it win instead of reporting a stale failure.
        this.failureText = text;
        this.failureAt = Date.now();
        this.surfaceCopyFailure(text);
      }
      this.pumpWrite();
    });
  }

  private surfaceCopyFailure(text: string): void {
    this.env.notify(
      "warning",
      "The native application copied text, but the browser blocked access to your clipboard.",
      { label: "Copy now", run: () => void this.retryCopy(text) },
    );
  }

  private async retryCopy(text: string): Promise<void> {
    if (await this.env.writeText(text)) {
      this.lastWritten = text;
      this.failureText = null;
      this.env.notify("success", "Copied the application's text to your clipboard.");
      return;
    }
    this.env.copyFallback(text, "The browser still refuses programmatic clipboard access.");
  }
}

/* -------------------------------------------------------------------- deps */

export interface NativeLinuxDeps {
  api: ApiClient;
  events: EventStream;
  toast: ToastCenter;
  wm: WindowManager;
  /** Shell element the hidden session iframe is attached to. */
  host: HTMLElement;
  /** Desktop app definition for a RAST app id (legacy Docker fallback). */
  findApp: (id: string) => AppDefinition | undefined;
  /** Show the whole-app window used by Docker-backed apps. */
  openLegacy: (app: AppDefinition) => void;
}

/* ---------------------------------------------------------------- constants */

const SESSION_POLL_MS = 5000;
const DISCONNECT_GRACE_MS = 8000;
// A first native launch can include creating the person's graphical session and
// a cold browser start (the profile may be large); 30 s was far too optimistic.
const START_TIMEOUT_MS = 240_000;
/** How often the shell re-reads the shim snapshot as a safety net. */
const SNAPSHOT_POLL_MS = 1_000;
/** Reload the embedded viewer if a launched app's window has not appeared. */
const LAUNCH_VIEWER_RECOVERY_MS = 8_000;
const RESIZE_DEBOUNCE_MS = 90;
const ATTACH_DELAYS = [40, 120, 300, 700, 1500, 3000, 6000];
const FORCE_FORGET_MS = 12_000;
const MIN_W = 260;
const MIN_H = 160;
const MAX_W = 4096;
const MAX_H = 3072;

/** Window types we never turn into RAST windows (converted to popups). */
const POPUP_TYPES = new Set(["MENU", "POPUP_MENU", "TOOLTIP", "DROPDOWN", "COMBO"]);

interface FrameState {
  wid: string;
  slug: string;
  appId: string;
  handle: WindowHandle | null;
  dialogParent: string | null;
  dialogOffset: { x: number; y: number };
  modal: boolean;
  popup: boolean;
  adopted: boolean;
  closing: boolean;
  remoteGone: boolean;
  autoMinimized: boolean;
  closeTimer: number;
  resizeTimer: number;
  remoteX: number;
  remoteY: number;
  /** Last maximize state reported by the *remote* window (never by RAST). */
  remoteMaximized: boolean;
  last: {
    minimized: boolean;
    maximized: boolean;
    focused: boolean;
    title: string;
    rect: WindowRect;
  };
}

interface StartingWindow {
  slug: string;
  handle: WindowHandle | null;
  titleEl: HTMLElement;
  detailEl: HTMLElement;
  timer: number;
}

const clamp = (value: number, min: number, max: number): number => {
  if (!Number.isFinite(value) || value <= 0) return min;
  return Math.min(max, Math.max(min, Math.round(value)));
};

const isHttpStatus = (error: unknown, status: number): boolean =>
  (error as { status?: number })?.status === status;

/** An X11 popup/menu/override-redirect window (floats over its parent). */
export function isPopupWindow(window: NativeShimWindow): boolean {
  if (window.windowType.some((type) => POPUP_TYPES.has(type.toUpperCase()))) return true;
  return window.overrideRedirect && !window.transientFor;
}

/* -------------------------------------------------------------------- host */

export class NativeLinuxHost {
  private readonly deps: NativeLinuxDeps;
  private readonly realtime: RealtimeStore;
  private readonly frames = new Map<string, FrameState>();
  private readonly starting = new Map<string, StartingWindow>();
  private readonly unsubscribers: Array<() => void> = [];
  private iframe: HTMLIFrameElement | null = null;
  private shim: NativeShimApi | null = null;
  private session: NativeSession | null = null;
  /** URL of a session that reported a non-seamless (desktop/shadow) mode.
   * Such a session is never rendered in a RAST window (see
   * onNonSeamlessSnapshot); reset when the session URL changes or it ends. */
  private nonSeamlessNotice = "";
  private lastSnapshot: NativeShimState | null = null;
  private pollTimer = 0;
  private reconnectTimer = 0;
  private attachTimer = 0;
  /** Safety poll: the shim may publish its snapshot before this page attaches
   * its listeners (a session created during a first launch does exactly that),
   * in which case no later window event would ever arrive. */
  private snapshotTimer = 0;
  /** One-shot recovery when a fresh session's viewer connected before the
   * Xpra web server was ready (first UI launch): reload the viewer once. */
  private launchRecoveryTimer = 0;
  private lastLaunchSlug: string | null = null;
  private lastLaunchAt = 0;
  private disposed = false;
  private quiet = false;
  private nativeUnavailable = false;
  private lastRemoteClipboard = "";
  private focusedWid: string | null = null;
  private lastRemoteFocus: string | null = null;
  /** Client OS uses Command as the shortcut modifier (macOS). */
  private readonly mac = detectMacPlatform();
  private readonly clipboard: NativeClipboardManager;
  /** Last connection state reported by the shim (never forward into a dead link). */
  private shimConnected = false;
  /** Keys currently held down in the remote session, for focus-loss releases. */
  private readonly pressedKeys = new Map<string, KeyboardInit>();
  private composing = false;
  /** Paste gesture bookkeeping: the browser has PASTE_EVENT_GRACE_MS to fire. */
  private pasteGestureSeq = 0;
  private pasteGestureSeen = true;
  private pasteFallbackTimer = 0;
  private keyForwardNoticeShown = false;
  private readonly onPasteHandler: (event: ClipboardEvent) => void;
  private readonly onCopyHandler: (event: ClipboardEvent) => void;
  private readonly onCutHandler: (event: ClipboardEvent) => void;
  private readonly onKeyDownHandler: (event: KeyboardEvent) => void;
  private readonly onKeyUpHandler: (event: KeyboardEvent) => void;
  private readonly onWindowBlurHandler: () => void;
  private readonly onWindowFocusHandler: () => void;
  private readonly onVisibilityHandler: () => void;
  private readonly onCompositionStartHandler: () => void;
  private readonly onCompositionEndHandler: () => void;

  constructor(deps: NativeLinuxDeps) {
    this.deps = deps;
    this.realtime = getRealtime(deps.events);
    this.clipboard = new NativeClipboardManager({
      writeText: (text) => this.writeBrowserClipboard(text),
      readTextQuietly: () => this.readBrowserClipboardQuietly(),
      notify: (kind, message, action) => {
        this.deps.toast.show(message, {
          kind,
          ...(action ? { action: { label: action.label, onClick: action.run } } : {}),
        });
      },
      copyFallback: (text, reason) => this.showCopyFallback(text, reason),
      pasteFallback: (reason, suggested) => this.showPasteFallback(reason, suggested),
    });
    this.onPasteHandler = (event) => this.forwardPaste(event);
    this.onCopyHandler = (event) => this.rememberCopiedText(event);
    this.onCutHandler = (event) => this.rememberCopiedText(event);
    this.onKeyDownHandler = (event) => this.forwardKey(event, true);
    this.onKeyUpHandler = (event) => this.forwardKey(event, false);
    this.onWindowBlurHandler = () => this.flushNativeInput("window-blur");
    this.onWindowFocusHandler = () => {
      this.clipboard.onWindowFocus();
      this.ensureNativeKeyboardFocus();
    };
    this.onVisibilityHandler = () => {
      if (document.visibilityState === "hidden") this.flushNativeInput("document-hidden");
    };
    this.onCompositionStartHandler = () => {
      this.composing = true;
    };
    this.onCompositionEndHandler = () => {
      this.composing = false;
    };
  }

  /* ------------------------------------------------------------- lifecycle */

  start(): void {
    if (this.disposed) return;
    const api = this.deps.api;
    this.unsubscribers.push(
      this.realtime.on<NativeSession | { session?: NativeSession | null }>("linuxapps.session", (data) => {
        // The backend sends `{"session": NativeSession}`; older/mock servers
        // send the session object directly.
        const session =
          data && typeof data === "object" && "session" in data
            ? ((data as { session?: NativeSession | null }).session ?? null)
            : (data as NativeSession | null);
        if (!session || session.id !== "native") return;
        this.applySession(session);
      }),
    );
    this.unsubscribers.push(
      this.realtime.on("linuxapps.changed", () => {
        void this.syncApps();
      }),
    );
    this.unsubscribers.push(
      this.realtime.onResync(() => {
        void this.refreshSession();
      }),
    );
    this.unsubscribers.push(
      this.realtime.onStatus((state) => {
        // The RAST event stream came back: the native session may have changed.
        if (state === "connected") void this.refreshSession();
      }),
    );
    this.unsubscribers.push(
      this.deps.wm.subscribe((windows) => this.onRastWindowsChanged(windows)),
    );
    // The RAST document owns the keyboard and the clipboard: the transport
    // iframe is hidden and must never receive focus (the shim releases the
    // Xpra page's clipboard autofocus for the same reason).
    document.addEventListener("paste", this.onPasteHandler, true);
    document.addEventListener("copy", this.onCopyHandler, true);
    document.addEventListener("cut", this.onCutHandler, true);
    document.addEventListener("keydown", this.onKeyDownHandler, true);
    document.addEventListener("keyup", this.onKeyUpHandler, true);
    document.addEventListener("compositionstart", this.onCompositionStartHandler, true);
    document.addEventListener("compositionend", this.onCompositionEndHandler, true);
    window.addEventListener("blur", this.onWindowBlurHandler);
    window.addEventListener("focus", this.onWindowFocusHandler);
    document.addEventListener("visibilitychange", this.onVisibilityHandler);
    this.unsubscribers.push(() => {
      document.removeEventListener("paste", this.onPasteHandler, true);
      document.removeEventListener("copy", this.onCopyHandler, true);
      document.removeEventListener("cut", this.onCutHandler, true);
      document.removeEventListener("keydown", this.onKeyDownHandler, true);
      document.removeEventListener("keyup", this.onKeyUpHandler, true);
      document.removeEventListener("compositionstart", this.onCompositionStartHandler, true);
      document.removeEventListener("compositionend", this.onCompositionEndHandler, true);
      window.removeEventListener("blur", this.onWindowBlurHandler);
      window.removeEventListener("focus", this.onWindowFocusHandler);
      document.removeEventListener("visibilitychange", this.onVisibilityHandler);
    });
    this.pollTimer = window.setInterval(() => {
      if (this.disposed || this.nativeUnavailable) return;
      void this.refreshSession();
    }, SESSION_POLL_MS);
    this.syncFromOverview();
    void this.refreshSession();
    void api;
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    window.clearInterval(this.pollTimer);
    window.clearInterval(this.snapshotTimer);
    window.clearTimeout(this.launchRecoveryTimer);
    window.clearTimeout(this.reconnectTimer);
    window.clearTimeout(this.attachTimer);
    window.clearTimeout(this.pasteFallbackTimer);
    this.pasteFallbackTimer = 0;
    this.pressedKeys.clear();
    this.clipboard.detach();
    for (const unsubscribe of this.unsubscribers.splice(0)) {
      try {
        unsubscribe();
      } catch {
        /* ignore */
      }
    }
    this.closeAllWindows(true);
    this.removeIframe();
    setLinuxNativeSession(null);
  }

  /* ---------------------------------------------------------------- session */

  /** Called when the account's app list is refreshed (`linuxapps.changed`). */
  private async syncApps(): Promise<void> {
    if (this.disposed) return;
    this.syncFromOverview();
    await this.refreshSession();
  }

  /** Adopt the session snapshot that came with `/api/linux-apps`. */
  syncFromOverview(): void {
    if (this.disposed) return;
    const overview = linuxOverview();
    if (!overview) return;
    if (overview.nativeEnabled === false) {
      // Older server: no native support at all. Never touch the Docker path.
      this.nativeUnavailable = true;
      return;
    }
    const session = linuxNativeSession();
    if (session) this.applySession(session);
  }

  private async refreshSession(): Promise<void> {
    if (this.disposed || this.nativeUnavailable) return;
    try {
      const res = await this.deps.api.get<{ session: NativeSession | null }>("/api/linux-apps/session");
      if (this.disposed) return;
      this.applySession(res?.session ?? null);
    } catch (error) {
      if (this.disposed) return;
      const status = (error as { status?: number }).status;
      if (status === 404 || status === 405 || status === 501) {
        // The server predates native apps: keep Docker-backed apps working.
        this.nativeUnavailable = true;
        return;
      }
      console.warn("could not read the native session", error);
    }
  }

  private applySession(session: NativeSession | null): void {
    if (this.disposed) return;
    const previous = this.session;
    this.session = session;
    // Only notify the app list when the session actually changed: the poll
    // runs every few seconds and re-setting an identical snapshot would make
    // every Linux-apps listener repaint (and re-enter syncFromOverview).
    const next = session && session.id === "native" ? session : null;
    if (sameSession(linuxNativeSession(), next)) {
      // keep the previous module snapshot (same content, new object)
    } else {
      setLinuxNativeSession(next);
    }

    if (!session || session.status === "ended") {
      const wasAlive = this.frames.size > 0 || this.starting.size > 0;
      this.closeAllWindows(false);
      this.nonSeamlessNotice = "";
      this.removeIframe();
      this.shim = null;
      this.lastSnapshot = null;
      this.resetInputState("session-ended");
      if (session?.status === "ended" && (wasAlive || previous?.status === "ready")) this.announceEnded(session);
      return;
    }
    // Starting sessions already have a proxy URL, but Xpra is not listening
    // yet. Loading now would leave an HTML 502 page in the iframe indefinitely.
    if (session.status !== "ready") {
      if (this.iframe && !this.shim) this.removeIframe();
      return;
    }
    if (previous?.url !== session.url) {
      this.nonSeamlessNotice = "";
      if (this.iframe) {
        this.removeIframe();
        this.shim = null;
        this.resetInputState("viewer-replaced");
      }
    }
    if (session.url && !this.iframe) this.createIframe(session.url);
    else if (this.iframe && !this.shim) this.attachShim(0);
  }

  private announceEnded(session: NativeSession): void {
    switch (session.reason) {
      case "removed":
        this.deps.toast.warning("Your native desktop session was stopped because its app was removed.");
        return;
      case "idle":
        this.deps.toast.info("Your native desktop session was stopped because it was not used for a while.");
        return;
      case "error":
        this.deps.toast.error(session.error || "Your native desktop session could not start.");
        return;
      case "closed":
        this.deps.toast.info("Your native desktop session was closed.");
        return;
      default:
        this.deps.toast.info("Your native desktop session ended.");
    }
  }

  /** Explicit "End session" action (App Center): stops every native app. */
  async endSession(): Promise<void> {
    try {
      await this.deps.api.post("/api/linux-apps/session/close");
      this.applySession({ ...(this.session ?? emptySession()), status: "ended", reason: "closed", error: null });
    } catch (error) {
      this.deps.toast.error(errorMessage(error));
    }
  }

  /** Stop one app group inside the shared session (explicit "End application"). */
  async endApplication(slug: string): Promise<void> {
    const session = this.session;
    const run = session?.apps.find((app) => app.appId === slug);
    try {
      if (run) await this.deps.api.post(`/api/linux-apps/session/runs/${encodeURIComponent(run.runId)}/close`);
      for (const frame of [...this.frames.values()]) {
        if (frame.slug === slug) this.remoteCloseFrame(frame);
      }
    } catch (error) {
      this.deps.toast.error(errorMessage(error));
    }
  }

  /* ------------------------------------------------------------------- iframe */

  private createIframe(url: string): void {
    if (this.iframe || !url) return;
    const iframe = document.createElement("iframe");
    iframe.className = "linux-native-session-frame";
    iframe.title = "Native desktop session";
    // The transport frame is hidden and must never be reachable with Tab: all
    // keyboard input is routed through the desktop document (see forwardKey).
    iframe.tabIndex = -1;
    iframe.setAttribute("aria-hidden", "true");
    // The Xpra client must stay same-origin and unsandboxed: the RAST desktop
    // adopts the remote windows' DOM nodes and forwards input.
    iframe.setAttribute("allow", "autoplay; clipboard-read; clipboard-write");
    iframe.addEventListener("load", () => {
      this.attachShim(0);
    });
    const viewerUrl = new URL(url, window.location.href);
    viewerUrl.searchParams.set("sound", "true");
    iframe.src = viewerUrl.href;
    this.deps.host.appendChild(iframe);
    this.iframe = iframe;
    this.attachShim(0);
  }

  private removeIframe(): void {
    window.clearTimeout(this.attachTimer);
    window.clearInterval(this.snapshotTimer);
    if (!this.iframe) return;
    const iframe = this.iframe;
    this.iframe = null;
    try {
      iframe.src = "about:blank";
    } catch {
      /* ignore */
    }
    iframe.remove();
  }

  private attachShim(attempt: number): void {
    if (this.disposed || !this.iframe) return;
    const win = this.iframe.contentWindow as (Window & { RASTNative?: NativeShimApi }) | null;
    const shim = win?.RASTNative;
    if (shim) {
      this.connectShim(shim);
      return;
    }
    if (attempt >= ATTACH_DELAYS.length) {
      this.deps.toast.error("The native desktop session could not be loaded.");
      return;
    }
    window.clearTimeout(this.attachTimer);
    this.attachTimer = window.setTimeout(
      () => this.attachShim(attempt + 1),
      ATTACH_DELAYS[attempt] ?? 3000,
    );
  }

  private connectShim(shim: NativeShimApi): void {
    if (this.shim === shim) return;
    this.shim = shim;
    this.lastRemoteFocus = null;
    this.quiet = true;
    this.closeAllWindows(true);
    this.quiet = false;
    this.clipboard.attach(shim);
    shim.on("state", (state?: NativeShimState) => {
      if (state) this.applySnapshot(state);
    });
    shim.on("windows", (state?: NativeShimState) => {
      if (state) this.applySnapshot(state);
    });
    shim.on("connected", (state?: NativeShimState) => {
      window.clearTimeout(this.reconnectTimer);
      this.reconnectTimer = 0;
      this.shimConnected = true;
      this.clipboard.setConnected(true);
      if (state) this.applySnapshot(state);
      this.ensureNativeKeyboardFocus();
    });
    shim.on("disconnected", (state?: NativeShimState) => {
      this.shimConnected = false;
      this.clipboard.setConnected(false);
      this.flushNativeInput("viewer-disconnected");
      if (state) this.applySnapshot(state);
      this.scheduleReconnect();
    });
    shim.on("error", () => {
      /* surfaced through the session state */
    });
    shim.on("focus-request", () => this.ensureNativeKeyboardFocus());
    shim.on("clipboard", (payload?: ShimClipboardPayload) => {
      if (!payload) return;
      const text = typeof payload.text === "string" ? payload.text : "";
      if (!text) return;
      this.lastRemoteClipboard = text;
      this.clipboard.remoteClipboard(payload);
    });
    shim.on("clipboard-format", (payload?: ShimClipboardFormat) => {
      if (payload) this.clipboard.unsupportedFormat(payload);
    });
    shim.on("clipboard-request", (payload?: ShimClipboardRequest) => {
      if (payload) this.clipboard.clipboardRequest(payload);
    });
    const snapshot = shim.snapshot();
    if (snapshot) this.applySnapshot(snapshot);
    window.clearInterval(this.snapshotTimer);
    this.snapshotTimer = window.setInterval(() => {
      if (this.disposed || !this.shim) return;
      const state = this.shim.snapshot?.();
      if (state?.ready) this.applySnapshot(state);
    }, SNAPSHOT_POLL_MS);
  }

  private scheduleReconnect(): void {
    if (this.disposed || this.reconnectTimer) return;
    this.reconnectTimer = window.setTimeout(() => {
      this.reconnectTimer = 0;
      void this.reconnect();
    }, DISCONNECT_GRACE_MS);
  }

  /** Reload only the embedded viewer (remote apps/session stay untouched). */
  private reloadViewer(): void {
    if (this.disposed || !this.iframe) return;
    this.quiet = true;
    this.closeAllWindows(true);
    this.quiet = false;
    this.lastSnapshot = null;
    this.shim = null;
    this.resetInputState("viewer-reload");
    window.clearInterval(this.snapshotTimer);
    try {
      this.iframe.contentWindow?.location.reload();
    } catch {
      if (this.session?.url) this.iframe.src = this.session.url;
    }
    this.attachShim(0);
  }

  private async reconnect(): Promise<void> {
    if (this.disposed) return;
    await this.refreshSession();
    const session = this.session;
    if (!session || session.status !== "ready") return;
    // The session is alive but our viewer lost its link: reload only the viewer.
    // Remote apps keep running; the RAST windows are rebuilt from its windows.
    this.quiet = true;
    this.closeAllWindows(true);
    this.quiet = false;
    this.lastSnapshot = null;
    this.shim = null;
    this.resetInputState("viewer-reconnect");
    try {
      this.iframe?.contentWindow?.location.reload();
    } catch {
      if (this.iframe && session.url) {
        this.iframe.src = session.url;
      }
    }
    this.attachShim(0);
    this.deps.toast.info("Reconnecting to your native desktop session…");
  }

  /* ---------------------------------------------------------------- windows */

  private applySnapshot(snapshot: NativeShimState): void {
    if (this.disposed || !snapshot?.ready) return;
    this.lastSnapshot = snapshot;
    if (snapshot.connected !== this.shimConnected) {
      this.shimConnected = snapshot.connected === true;
      this.clipboard.setConnected(this.shimConnected);
      if (!this.shimConnected) this.flushNativeInput("viewer-disconnected");
    }
    if (snapshot.separate === false) {
      this.onNonSeamlessSnapshot(snapshot);
      return;
    }
    this.nonSeamlessNotice = "";
    const seen = new Set<string>();
    const deferred: NativeShimWindow[] = [];
    const ordered = [...snapshot.windows].sort((a, b) => a.stacking - b.stacking);
    for (const window of ordered) {
      seen.add(window.wid);
      const existing = this.frames.get(window.wid);
      if (existing) {
        if (!existing.closing) this.updateFrame(existing, window);
        continue;
      }
      const anchorWid = window.transientFor ?? (isPopupWindow(window) ? this.focusedWid : null);
      const parent = anchorWid ? this.frames.get(anchorWid) : undefined;
      if (window.transientFor && !parent) {
        deferred.push(window);
        continue;
      }
      this.createFrame(window, parent ?? null);
    }
    for (const window of deferred) {
      if (this.frames.has(window.wid)) continue;
      const anchorWid = window.transientFor ?? (isPopupWindow(window) ? this.focusedWid : null);
      const parent = anchorWid ? this.frames.get(anchorWid) : undefined;
      this.createFrame(window, parent ?? null);
    }
    for (const frame of [...this.frames.values()]) {
      if (!seen.has(frame.wid) && !frame.remoteGone) this.destroyFrame(frame, true);
    }
    // A starting window closes as soon as its app has a remote window.
    for (const slug of [...this.starting.keys()]) {
      if (this.framesFor(slug).length > 0) this.closeStarting(slug);
    }
    this.syncRemoteFocus(snapshot);
  }

  private resolveSlug(window: NativeShimWindow, parent?: FrameState | null): string {
    // A transient window belongs to the app of the window it is attached to;
    // this is more reliable than class matching (dialogs and file choosers
    // often have no useful WM_CLASS, and every transient must keep the parent
    // app's identity for icons, grouping and access checks).
    if (parent) return parent.slug;
    const nativeApps = linuxApps().filter(isNativeApp);
    const matched = matchWindowApp(nativeApps, window.classInstance);
    if (matched) return matched.id;
    // Reuse the app of another window with the same X11 class (e.g. a second
    // Chrome window).
    const klass = (window.classInstance[0] ?? "").toLowerCase();
    if (klass) {
      for (const frame of this.frames.values()) {
        for (const candidate of nativeApps) {
          if (candidate.id === frame.slug) {
            const candidateClass = window.classInstance.some((value) => value.toLowerCase() === klass);
            if (candidateClass) return frame.slug;
          }
        }
      }
    }
    // A window that appeared right after a launch belongs to that app.
    if (this.lastLaunchSlug && Date.now() - this.lastLaunchAt < 120_000) {
      const launched = nativeApps.find((app) => app.id === this.lastLaunchSlug);
      if (launched) return launched.id;
    }
    return nativeApps[0]?.id ?? "unknown";
  }

  private framesFor(slug: string): FrameState[] {
    return [...this.frames.values()].filter((frame) => frame.slug === slug && !frame.closing);
  }

  /**
   * Offset of the window's content area inside its frame. RAST draws its own
   * title bar above the adopted remote window, so transient surfaces placed
   * from remote coordinates must add this offset or they float one title bar
   * too high.
   */
  private contentOrigin(handle: WindowHandle): { x: number; y: number } {
    const content = handle.contentEl;
    const root = content.parentElement;
    if (!root) return { x: 0, y: 0 };
    const contentRect = content.getBoundingClientRect();
    const rootRect = root.getBoundingClientRect();
    return {
      x: Math.round(contentRect.x - rootRect.x),
      y: Math.round(contentRect.y - rootRect.y),
    };
  }

  private createFrame(info: NativeShimWindow, parent: FrameState | null): void {
    const slug = this.resolveSlug(info, parent);
    const app = linuxAppById(slug);
    const title = info.title?.trim() || app?.name || "Window";
    const popup = isPopupWindow(info);
    const appId = nativeWindowAppId(slug, info.wid);
    const width = clamp(info.width, MIN_W, Math.min(MAX_W, globalThis.innerWidth - 8));
    const height = clamp(info.height, MIN_H, Math.min(MAX_H, globalThis.innerHeight - 8));
    const area = globalThis.innerWidth;

    let initialRect: WindowRect | undefined;
    let dialogOffset = { x: 0, y: 0 };
    if (parent?.handle) {
      const parentRect = parent.handle.getRect();
      const origin = this.contentOrigin(parent.handle);
      if (popup) {
        dialogOffset = {
          x: info.x - parent.remoteX,
          y: info.y - parent.remoteY,
        };
        initialRect = {
          x: Math.max(0, parentRect.x + origin.x + dialogOffset.x),
          y: Math.max(0, parentRect.y + origin.y + dialogOffset.y),
          w: width,
          h: height,
        };
      } else {
        dialogOffset = { x: 40, y: 40 };
        initialRect = {
          x: Math.min(Math.max(8, parentRect.x + origin.x + 40), Math.max(8, area - width - 8)),
          y: Math.min(Math.max(8, parentRect.y + origin.y + 40), Math.max(8, globalThis.innerHeight - height - 8)),
          w: width,
          h: height,
        };
      }
    }

    // Transient surfaces (dialogs, menus, tooltips) never get their own
    // taskbar entry, even if their parent window is not known (yet).
    const transient = popup || Boolean(info.transientFor) || Boolean(parent);

    const frame: FrameState = {
      wid: info.wid,
      slug,
      appId,
      handle: null,
      dialogParent: parent?.wid ?? null,
      dialogOffset,
      modal: info.modal,
      popup,
      adopted: false,
      closing: false,
      remoteGone: false,
      autoMinimized: false,
      closeTimer: 0,
      resizeTimer: 0,
      remoteX: info.x,
      remoteY: info.y,
      remoteMaximized: info.maximized === true,
      last: {
        minimized: false,
        maximized: false,
        focused: false,
        title,
        rect: { x: 0, y: 0, w: width, h: height },
      },
    };
    this.frames.set(info.wid, frame);

    const definition: AppDefinition = {
      id: appId,
      name: title,
      icon: "box",
      description: app?.description || "Native Linux app window",
      defaultSize: { w: width, h: height },
      minSize: { w: popup ? 80 : 220, h: popup ? 40 : 120 },
      frameless: popup,
      mount: () => undefined,
    };

    try {
      const handle = this.deps.wm.open(
        definition,
        undefined,
        (ctx) => this.mountFrame(frame, ctx),
        {
          taskbar: !transient,
          ownerId: parent?.handle?.id,
          ...(initialRect ? { initialRect } : {}),
        },
      );
      frame.handle = handle;
      handle.setIcon("box");
      frame.last.rect = handle.getRect();
      // wm.open focuses the new window; remember it for keyboard forwarding.
      this.focusedWid = info.wid;
      if (parent?.handle) parent.handle.setIcon("box");
    } catch (error) {
      this.frames.delete(info.wid);
      console.warn("could not open a native window", error);
      this.deps.toast.error(`A native window could not be shown: ${errorMessage(error)}`);
    }
  }

  private mountFrame(
    frame: FrameState,
    ctx: {
      contentEl: HTMLElement;
      window: WindowHandle;
      onResize: (callback: () => void) => () => void;
      onDispose: (callback: () => void) => () => void;
    },
  ): void {
    ctx.contentEl.classList.add("linux-native-window");
    if (frame.popup) ctx.contentEl.classList.add("is-popup");
    // Pointer focus for the adopted surface: clicking the application must
    // move DOM focus off any previously used RAST control (title-bar buttons,
    // taskbar, ...) so keyboard events keep reaching the native window.
    ctx.contentEl.tabIndex = -1;
    const focusContent = (): void => {
      if (frame.closing) return;
      if (document.activeElement === ctx.contentEl) return;
      ctx.contentEl.focus({ preventScroll: true });
      // The browser may have kept focus in the hidden transport iframe (the
      // Xpra page's clipboard autofocus does exactly that): take it back.
      window.setTimeout(() => this.ensureNativeKeyboardFocus(), 0);
    };
    ctx.contentEl.addEventListener("pointerdown", focusContent, true);
    ctx.onDispose(() => ctx.contentEl.removeEventListener("pointerdown", focusContent, true));
    // The remote application draws and handles its own context menu, popup
    // menus and drags. The browser's page-level defaults (context menu,
    // middle-click autoscroll, native image/canvas dragging) would appear on
    // top of the application's own UI, so they are suppressed inside the
    // adopted surface only.
    const suppressDefaults = (event: Event): void => {
      event.preventDefault();
    };
    const suppressed = ["contextmenu", "auxclick", "dragstart"];
    for (const type of suppressed) ctx.contentEl.addEventListener(type, suppressDefaults, true);
    const suppressMiddleDown = (event: MouseEvent): void => {
      if (event.button === 1) event.preventDefault();
    };
    ctx.contentEl.addEventListener("mousedown", suppressMiddleDown, true);
    ctx.onDispose(() => {
      for (const type of suppressed) ctx.contentEl.removeEventListener(type, suppressDefaults, true);
      ctx.contentEl.removeEventListener("mousedown", suppressMiddleDown, true);
    });
    ctx.onResize(() => this.scheduleRemoteResize(frame, ctx.contentEl));
    ctx.onDispose(() => this.onFrameDisposed(frame));
    const shim = this.shim;
    const adopted = shim ? shim.adoptWindow(frame.wid, ctx.contentEl) : false;
    frame.adopted = adopted;
    if (!adopted) {
      ctx.contentEl.replaceChildren(
        h(
          "div",
          { class: "linux-native-problem" },
          h("div", { class: "linux-native-problem-title" }, "This window could not be shown"),
          h("button", {
            class: "btn btn-sm btn-secondary",
            type: "button",
            onClick: () => {
              this.destroyFrame(frame, true);
              if (this.lastSnapshot) this.applySnapshot(this.lastSnapshot);
            },
          }, "Try again"),
        ),
      );
      return;
    }
    // The first remote configure can race the Xpra window becoming mapped
    // (especially for a browser window that starts at the virtual-screen size),
    // leaving the picture at its old scale in the top-left corner. Re-apply the
    // RAST content size a couple of times so the remote window always matches
    // the window the person sees.
    window.requestAnimationFrame(() => this.scheduleRemoteResize(frame, ctx.contentEl, 0));
    for (const retryMs of [350, 1200]) {
      window.setTimeout(() => {
        if (frame.closing || frame.remoteGone) return;
        this.scheduleRemoteResize(frame, ctx.contentEl, 0);
        // The first paint after adoption can be an all-black buffer until a
        // damage event arrives; ask the client to pull the window contents.
        shim?.requestRefresh(frame.wid);
      }, retryMs);
    }
    if (!frame.popup) {
      shim?.focusWindow(frame.wid);
      // A new window takes the keyboard: make sure the desktop document (not
      // the hidden transport iframe) holds DOM focus.
      focusContent();
    }
  }

  private scheduleRemoteResize(frame: FrameState, contentEl: HTMLElement, delay = RESIZE_DEBOUNCE_MS): void {
    if (frame.resizeTimer) window.clearTimeout(frame.resizeTimer);
    frame.resizeTimer = window.setTimeout(() => {
      frame.resizeTimer = 0;
      const shim = this.shim;
      if (!shim || frame.closing || frame.remoteGone) return;
      const width = Math.round(contentEl.clientWidth);
      const height = Math.round(contentEl.clientHeight);
      if (width < 8 || height < 8) return;
      shim.resizeWindow(frame.wid, { width, height });
    }, delay);
  }

  private updateFrame(frame: FrameState, window: NativeShimWindow): void {
    frame.remoteX = window.x;
    frame.remoteY = window.y;
    frame.modal = window.modal;
    const handle = frame.handle;
    if (!handle || handle.isClosed()) return;
    const title = window.title?.trim() || "Window";
    if (title !== frame.last.title) {
      frame.last.title = title;
      handle.setTitle(title);
    }
    // A window the application iconified itself: minimize the RAST window.
    if (window.minimized && !frame.last.minimized && !handle.isMinimized()) {
      this.deps.wm.minimize(handle.id);
    } else if (!window.minimized && frame.last.minimized && handle.isMinimized() && !frame.closing) {
      this.deps.wm.toggleMinimize(handle.id);
    }
    frame.last.minimized = window.minimized;
    // Only follow a *remote* maximize. Reacting to a remote "not maximized"
    // as well would undo a RAST-side maximize on the next snapshot (the X11
    // window itself is not maximized by the RAST window manager), creating a
    // toggle feedback loop. A RAST-side restore stays local.
    if (window.maximized && !frame.remoteMaximized && !handle.isMaximized() && !frame.closing) {
      this.deps.wm.toggleMaximize(handle.id);
    }
    frame.remoteMaximized = window.maximized === true;
    frame.last.maximized = window.maximized;
  }

  private syncRemoteFocus(snapshot: NativeShimState): void {
    const focused = snapshot.windows.find((window) => window.focused);
    const wid = focused?.wid ?? null;
    // Snapshots are repeated as a recovery fallback, not a request to raise
    // Chrome every second (which stole focus from other desktop applications).
    if (wid === this.lastRemoteFocus) return;
    this.lastRemoteFocus = wid;
    if (!focused) return;
    const frame = this.frames.get(focused.wid);
    if (!frame?.handle || frame.closing) return;
    // Popup/menu windows must not take the RAST focus away from the app.
    const target = frame.popup ? this.frames.get(frame.dialogParent ?? "") : frame;
    if (!target?.handle) return;
    if (!target.handle.isMinimized()) target.handle.focus();
    this.focusNativeContent(target);
  }

  private onRastWindowsChanged(windows: WindowInfo[]): void {
    if (this.disposed || this.quiet) return;
    const shim = this.shim;
    const byId = new Map(windows.map((info) => [info.id, info]));
    for (const frame of [...this.frames.values()]) {
      if (!frame.handle || frame.handle.isClosed()) continue;
      const info = byId.get(frame.handle.id);
      if (!info) continue;
      if (shim && !frame.closing && !frame.remoteGone) {
        if (info.minimized !== frame.last.minimized) {
          shim.setMinimized(frame.wid, info.minimized);
        }
        if (info.focused && !frame.last.focused) {
          shim.focusWindow(frame.wid);
        }
      }
      if (info.focused) {
        this.focusedWid = frame.wid;
        this.focusNativeContent(frame);
      } else if (this.focusedWid === frame.wid) {
        this.focusedWid = null;
      }
      frame.last.minimized = info.minimized;
      frame.last.maximized = info.maximized;
      frame.last.focused = info.focused;
      frame.last.rect = info.rect;
    }
    // Transient dialogs: follow their parent and hide while it is minimized.
    for (const frame of this.frames.values()) {
      if (!frame.dialogParent || !frame.handle || frame.handle.isClosed()) continue;
      const owner = this.frames.get(frame.dialogParent);
      if (!owner?.handle || owner.handle.isClosed()) continue;
      const ownerInfo = byId.get(owner.handle.id);
      if (!ownerInfo) continue;
      if (ownerInfo.minimized && !frame.autoMinimized) {
        frame.autoMinimized = true;
        this.deps.wm.minimize(frame.handle.id);
      } else if (!ownerInfo.minimized && frame.autoMinimized) {
        frame.autoMinimized = false;
        this.deps.wm.toggleMinimize(frame.handle.id);
      }
      if (ownerInfo.minimized || frame.autoMinimized) continue;
      const origin = this.contentOrigin(owner.handle);
      const dx = ownerInfo.rect.x + origin.x + frame.dialogOffset.x;
      const dy = ownerInfo.rect.y + origin.y + frame.dialogOffset.y;
      const rect = frame.handle.getRect();
      if (Math.abs(rect.x - dx) > 1 || Math.abs(rect.y - dy) > 1) {
        frame.handle.moveTo(Math.max(0, dx), Math.max(0, dy));
      }
    }
  }

  /* ------------------------------------------------------------ frame close */

  /** RAST user closed a native window: WM_DELETE, keep the app's own prompts. */
  private onFrameDisposed(frame: FrameState): void {
    if (frame.remoteGone) return;
    if (frame.closing) return;
    frame.closing = true;
    const shim = this.shim;
    const owner = frame.dialogParent ? this.frames.get(frame.dialogParent) : undefined;
    if (!owner?.closing) shim?.closeWindow(frame.wid);
    window.clearTimeout(frame.closeTimer);
    frame.closeTimer = window.setTimeout(() => {
      if (!this.frames.has(frame.wid)) return;
      // The app did not close (e.g. it opened its own prompt): the remote
      // window still exists, so let the snapshot re-create a RAST window for it.
      const current = this.frames.get(frame.wid);
      if (!current) return;
      this.frames.delete(frame.wid);
      if (this.lastSnapshot) this.applySnapshot(this.lastSnapshot);
    }, FORCE_FORGET_MS);
  }

  private remoteCloseFrame(frame: FrameState): void {
    frame.remoteGone = true;
    window.clearTimeout(frame.closeTimer);
    const handle = frame.handle;
    this.frames.delete(frame.wid);
    if (this.focusedWid === frame.wid) this.focusedWid = null;
    if (handle && !handle.isClosed()) handle.close();
  }

  private destroyFrame(frame: FrameState, remoteGone: boolean): void {
    window.clearTimeout(frame.closeTimer);
    window.clearTimeout(frame.resizeTimer);
    const handle = frame.handle;
    if (remoteGone) {
      this.remoteCloseFrame(frame);
      return;
    }
    this.frames.delete(frame.wid);
    if (this.focusedWid === frame.wid) this.focusedWid = null;
    if (handle && !handle.isClosed()) {
      frame.remoteGone = true;
      handle.close();
    }
  }

  private closeAllWindows(quiet: boolean): void {
    const previous = this.quiet;
    this.quiet = quiet;
    for (const frame of [...this.frames.values()]) {
      window.clearTimeout(frame.closeTimer);
      window.clearTimeout(frame.resizeTimer);
      frame.remoteGone = true;
      this.frames.delete(frame.wid);
      if (this.focusedWid === frame.wid) this.focusedWid = null;
      if (frame.handle && !frame.handle.isClosed()) frame.handle.close();
    }
    for (const slug of [...this.starting.keys()]) this.closeStarting(slug);
    this.quiet = previous;
  }

  /* --------------------------------------------------------------- launching */

  /** The app's desktop icon or taskbar entry was used. */
  open(slug: string): void {
    if (this.disposed) return;
    const app = linuxAppById(slug);
    if (!app) {
      this.deps.toast.error("This app is no longer installed.");
      return;
    }
    if (!isNativeApp(app)) {
      this.fallbackToLegacy(app);
      return;
    }
    const open = this.framesFor(slug);
    const last = open.at(-1);
    if (last) {
      if (last.handle) {
        if (last.handle.isMinimized()) this.deps.wm.toggleMinimize(last.handle.id);
        else last.handle.focus();
      }
      return;
    }
    void this.launch(app);
  }

  private async launch(app: LinuxApp): Promise<void> {
    const starting = this.showStarting(app);
    try {
      const res = await this.deps.api.post<{
        run: { id: string; appId: string; appName: string; status: string };
        session: NativeSession;
      }>(`/api/linux-apps/${encodeURIComponent(app.id)}/launch`);
      if (this.disposed) return;
      this.lastLaunchSlug = app.id;
      this.lastLaunchAt = Date.now();
      if (res?.session) this.applySession(res.session);
      const run = res?.run;
      if (run && (run.status === "running" || run.status === "ready")) {
        starting.detailEl.textContent = "Opening its window…";
      }
      // A session created by this launch can take a moment before the Xpra web
      // server accepts the embedded viewer; if no window shows up, reload the
      // viewer once (the session and app keep running).
      window.clearTimeout(this.launchRecoveryTimer);
      this.launchRecoveryTimer = window.setTimeout(() => {
        if (this.disposed || !this.starting.has(app.id)) return;
        if (this.framesFor(app.id).length > 0) return;
        this.reloadViewer();
      }, LAUNCH_VIEWER_RECOVERY_MS);
    } catch (error) {
      this.closeStarting(app.id);
      if (isHttpStatus(error, 404) || isHttpStatus(error, 405) || isHttpStatus(error, 501)) {
        // This server does not serve the native launch API. A native app is
        // never downgraded to the legacy viewer: state the misconfiguration.
        this.nativeUnavailable = true;
        this.deps.toast.error(
          `${app.name} needs native window support, which this server does not offer. Ask an administrator to enable it.`,
        );
        return;
      }
      this.deps.toast.error(errorMessage(error));
    }
  }

  private fallbackToLegacy(app: LinuxApp): void {
    // Never downgrade a native app to the legacy whole-session viewer: that
    // path embeds a remote desktop (and Xpra's own chrome) inside a RAST
    // window, which this desktop never allows. Refuse with a clear message.
    if (isNativeApp(app)) {
      this.deps.toast.error(
        `${app.name} could not be opened as an individual window. Check that the native helper is running, then try again.`,
      );
      return;
    }
    const definition = this.deps.findApp(`${LINUX_PREFIX}${app.id}`);
    if (!definition) {
      this.deps.toast.error("This app cannot be started: native apps are not available on this server.");
      return;
    }
    this.deps.openLegacy(definition);
  }

  private showStarting(app: LinuxApp): StartingWindow {
    const existing = this.starting.get(app.id);
    if (existing) return existing;
    const titleEl = h("div", { class: "linux-native-starting-title" }, `Starting ${app.name}…`);
    const detailEl = h("div", { class: "linux-native-starting-detail" }, "Preparing your desktop session…");
    const body = h(
      "div",
      { class: "linux-native-starting" },
      h("span", { class: "spinner" }),
      h("div", { class: "linux-native-starting-text" }, titleEl, detailEl),
    );
    const state: StartingWindow = {
      slug: app.id,
      handle: null,
      titleEl,
      detailEl,
      timer: 0,
    };
    state.handle = this.deps.wm.open(
      {
        id: `linuxnative-starting:${app.id}`,
        name: `Starting ${app.name}…`,
        icon: "box",
        defaultSize: { w: 420, h: 168 },
        minSize: { w: 320, h: 148 },
      },
      undefined,
      (ctx) => {
        ctx.contentEl.classList.add("linux-native-starting-host");
        ctx.contentEl.replaceChildren(body);
      },
      { taskbar: false },
    );
    state.timer = window.setTimeout(() => {
      if (!this.starting.has(app.id)) return;
      this.closeStarting(app.id);
      this.deps.toast.error(`${app.name} did not open in time. Check App Center for details.`);
    }, START_TIMEOUT_MS);
    this.starting.set(app.id, state);
    return state;
  }

  private closeStarting(slug: string): void {
    const state = this.starting.get(slug);
    if (!state) return;
    this.starting.delete(slug);
    window.clearTimeout(state.timer);
    if (state.handle && !state.handle.isClosed()) {
      this.quiet = true;
      state.handle.close();
      this.quiet = false;
    }
  }

  /* --------------------------------------------------- non-seamless guard */

  /**
   * The Xpra session runs in seamless mode, so every normal X11 top-level
   * window is adopted as its own RAST window. A desktop/shadow session
   * (`separate === false`) would need a whole remote desktop, including
   * Xpra's own toolbar and remote decorations, inside a RAST window. RAST
   * never renders that: the RAST window manager stays the single visible
   * window-manager authority. Explain the anomaly once per session instead.
   */
  private onNonSeamlessSnapshot(snapshot: NativeShimState): void {
    const key = this.session?.url || "native";
    if (this.nonSeamlessNotice === key) return;
    this.nonSeamlessNotice = key;
    // The hidden transport viewer is kept off-screen; no RAST window is
    // created for it and no remote window is adopted from a desktop session.
    for (const slug of [...this.starting.keys()]) this.closeStarting(slug);
    console.error("native session is not seamless; the remote desktop will not be rendered", snapshot.size);
    this.deps.toast.error(
      "The native session started in desktop mode, so it has no individual windows. RAST will not show the remote desktop. Restart the session from App Center, then open the app again.",
    );
  }

  /* --------------------------------------------------- keyboard & clipboard */

  private isModalOpen(): boolean {
    return document.querySelector(".modal-backdrop") !== null;
  }

  /**
   * The single keyboard path: every keydown/keyup in the desktop document is
   * classified once (see `decideNativeKey`) and either kept by the shell, left
   * to the browser, or forwarded exactly once to the focused native window.
   */
  private forwardKey(event: KeyboardEvent, pressed: boolean): void {
    if (this.disposed) return;
    const shim = this.shim;
    if (!shim) return;
    const code = event.code || event.key || "";
    const decision = decideNativeKey(
      {
        key: event.key,
        code: event.code,
        keyCode: event.keyCode || event.which || 0,
        location: event.location,
        ctrlKey: event.ctrlKey,
        shiftKey: event.shiftKey,
        altKey: event.altKey,
        metaKey: event.metaKey,
        repeat: event.repeat,
        isComposing: event.isComposing === true || event.keyCode === 229,
      },
      {
        mac: this.mac,
        nativeFocused: this.focusedWid !== null,
        shellTarget: isShellKeyboardTarget(event.target),
        modalOpen: this.isModalOpen(),
        composing: this.composing,
        connected: this.shimConnected,
      },
    );

    // Releasing a key earlier forwarded must always reach the remote window,
    // even if focus moved to the shell meanwhile: a stuck modifier in the
    // remote session is worse than one extra keyup.
    if (!pressed) {
      const tracked = code ? this.pressedKeys.get(code) : undefined;
      if (tracked) {
        this.pressedKeys.delete(code);
        shim.sendKey(false, { ...tracked, repeat: false });
        return;
      }
      if (decision.mode === "forward") shim.sendKey(false, decision.init);
      return;
    }

    if (decision.mode !== "forward") return;
    if (!shim.sendKey(true, decision.init)) {
      if (!this.keyForwardNoticeShown) {
        this.keyForwardNoticeShown = true;
        this.deps.toast.error(
          "Keyboard input could not be sent to the native session. Reload the page or restart the application.",
        );
      }
      return;
    }
    if (code) this.pressedKeys.set(code, decision.init);
    if (decision.preventDefault) {
      event.preventDefault();
      event.stopPropagation();
    } else if (decision.gesture === "paste") {
      // Keep the browser's paste default action (it delivers the paste event
      // with the clipboard data) but never let RAST shell shortcuts treat the
      // gesture as their own.
      event.stopPropagation();
    }
    if (decision.gesture === "paste") this.armPasteGesture();
  }

  /** Arm the "paste event never arrived" fallback after a paste gesture. */
  private armPasteGesture(): void {
    this.pasteGestureSeq += 1;
    const seq = this.pasteGestureSeq;
    this.pasteGestureSeen = false;
    window.clearTimeout(this.pasteFallbackTimer);
    this.pasteFallbackTimer = window.setTimeout(() => {
      this.pasteFallbackTimer = 0;
      if (seq !== this.pasteGestureSeq || this.pasteGestureSeen) return;
      this.pasteGestureSeen = true;
      void this.clipboard.pasteGestureMissed();
    }, PASTE_EVENT_GRACE_MS);
  }

  /**
   * Browser paste into a native window. The paste event is a trusted user
   * gesture, so `clipboardData` is available without a permission prompt: this
   * is the primary browser -> application path. It also answers a Cmd/Ctrl+V
   * gesture whose paste event was consumed by a browser UI element.
   */
  private forwardPaste(event: ClipboardEvent): void {
    if (this.disposed) return;
    const shim = this.shim;
    if (!shim || !this.shimConnected) return;
    if (this.focusedWid === null) return;
    if (this.isModalOpen()) return;
    const onSurface = nativeSurfaceFor(event.target) !== null;
    const pendingGesture = !this.pasteGestureSeen;
    if (!onSurface && !pendingGesture) return;
    this.pasteGestureSeen = true;
    const text = event.clipboardData?.getData("text/plain") ?? "";
    event.preventDefault();
    event.stopPropagation();
    if (text) this.clipboard.pasteEventText(text);
  }

  /** Remember text copied inside RAST for remote paste requests (best effort). */
  private rememberCopiedText(event: ClipboardEvent): void {
    if (this.disposed) return;
    // A copy performed by the application itself is handled by the shim.
    if (nativeSurfaceFor(event.target) !== null) return;
    const text = event.clipboardData?.getData("text/plain") ?? "";
    if (text) this.clipboard.browserCopied(text);
  }

  /** Release everything RAST forwarded (focus loss / disconnect / reload). */
  private flushNativeInput(_reason: string): void {
    const shim = this.shim;
    for (const init of this.pressedKeys.values()) {
      try {
        shim?.sendKey(false, init);
      } catch {
        /* the session may already be gone */
      }
    }
    this.pressedKeys.clear();
    try {
      shim?.releaseInput?.();
    } catch {
      /* ignore */
    }
    window.clearTimeout(this.pasteFallbackTimer);
    this.pasteFallbackTimer = 0;
    this.pasteGestureSeen = true;
    this.composing = false;
  }

  /** Full input reset when the viewer/session is replaced or ended. */
  private resetInputState(reason: string): void {
    this.flushNativeInput(reason);
    this.clipboard.detach();
    this.shimConnected = false;
    this.focusedWid = null;
    this.lastRemoteClipboard = "";
  }

  /** Focus the RAST window body that hosts an adopted surface. */
  private focusNativeContent(frame: FrameState | null | undefined): void {
    const content = frame?.handle?.contentEl;
    if (!content || frame?.closing) return;
    if (document.activeElement === content) return;
    try {
      content.focus({ preventScroll: true });
    } catch {
      /* the window may be closing */
    }
  }

  /**
   * The hidden transport iframe must never keep keyboard focus: give it back to
   * the focused native window body so the desktop document sees every key.
   */
  private ensureNativeKeyboardFocus(): void {
    if (this.disposed) return;
    if (this.isModalOpen()) return;
    if (this.iframe && document.activeElement === this.iframe) {
      this.focusNativeContent(this.focusedWid ? this.frames.get(this.focusedWid) : null);
      return;
    }
    if (this.focusedWid) this.focusNativeContent(this.frames.get(this.focusedWid));
  }

  /* ---------------------------------------------------- browser clipboard api */

  private async writeBrowserClipboard(text: string): Promise<boolean> {
    try {
      const clipboard = navigator.clipboard;
      if (!clipboard || typeof clipboard.writeText !== "function") return false;
      await clipboard.writeText(text);
      return true;
    } catch {
      return false;
    }
  }

  /**
   * Read the browser clipboard only when the permission is already granted:
   * a clipboard read must never pop a permission prompt from an event the
   * person did not directly trigger.
   */
  private async readBrowserClipboardQuietly(): Promise<string | null> {
    try {
      const clipboard = navigator.clipboard;
      if (!clipboard || typeof clipboard.readText !== "function") return null;
      const permissions = navigator.permissions;
      if (!permissions || typeof permissions.query !== "function") return null;
      let state = "prompt";
      try {
        const status = await permissions.query({ name: "clipboard-read" as PermissionName });
        state = status.state;
      } catch {
        return null;
      }
      if (state !== "granted") return null;
      const text = await clipboard.readText();
      return typeof text === "string" ? text : null;
    } catch {
      return null;
    }
  }

  /** Retry writing from a real user gesture, with the legacy copy fallback. */
  private async retryClipboardWrite(text: string): Promise<boolean> {
    if (await this.writeBrowserClipboard(text)) return true;
    return copyText(text);
  }

  /* ------------------------------------------------------- explicit fallbacks */

  private showCopyFallback(text: string, reason: string): void {
    const area = h("textarea", { class: "textarea" });
    area.value = text;
    area.readOnly = true;
    area.rows = 6;
    area.style.cssText = "width: 100%; min-height: 140px; margin-top: 10px; white-space: pre-wrap;";
    area.addEventListener("focus", () => area.select());
    modal({
      title: "Copy from the native application",
      wide: true,
      body: h(
        "div",
        {},
        h("p", {}, "The browser refused to let RAST write to your clipboard."),
        h("p", {}, reason),
        h("p", {}, "Select the text below and press Cmd+C (macOS) or Ctrl+C to copy it manually."),
        area,
      ),
      actions: [
        {
          label: "Try copy again",
          variant: "primary",
          keepOpen: true,
          onClick: async (close) => {
            if (await this.retryClipboardWrite(text)) {
              this.deps.toast.success("Copied to your browser clipboard.");
              close();
              this.ensureNativeKeyboardFocus();
              return;
            }
            this.deps.toast.warning("The browser still refuses clipboard access. Select the text and copy it manually.");
            area.focus();
            area.select();
          },
        },
        { label: "Close", variant: "ghost", onClick: (close) => close() },
      ],
    });
    window.setTimeout(() => {
      area.focus();
      area.select();
    }, 0);
  }

  private showPasteFallback(reason: string, suggestedText: string | null): void {
    const area = h("textarea", { class: "textarea" });
    area.value = suggestedText ?? "";
    area.rows = 6;
    area.placeholder = "Paste the text here (Cmd+V / Ctrl+V), then send it to the application.";
    area.style.cssText = "width: 100%; min-height: 140px; margin-top: 10px;";
    modal({
      title: "Paste into the native application",
      wide: true,
      body: h(
        "div",
        {},
        h("p", {}, "RAST could not read your clipboard directly for the application's paste."),
        h("p", {}, reason),
        h("p", {}, "Paste the text below, then send it to the focused application."),
        area,
      ),
      actions: [
        {
          label: "Send to application",
          variant: "primary",
          keepOpen: true,
          onClick: () => {
            const text = area.value;
            if (!text) {
              this.deps.toast.warning("Paste some text first.");
              area.focus();
              return;
            }
            if (!this.clipboard.pushToRemote(text)) {
              this.deps.toast.error("The native session did not accept the pasted text.");
              return;
            }
            this.sendPasteShortcut();
            this.deps.toast.success("Sent to the native application.");
          },
        },
        { label: "Close", variant: "ghost", onClick: (close) => close() },
      ],
    });
    window.setTimeout(() => {
      area.focus();
      if (area.value) area.select();
    }, 0);
  }

  /** Send a Ctrl+V (Command+V on macOS) to the focused remote window. */
  private sendPasteShortcut(): void {
    const shim = this.shim;
    if (!shim || !this.shimConnected || this.focusedWid === null) return;
    const modifier = this.mac ? { metaKey: true } : { ctrlKey: true };
    const init: KeyboardInit = { key: "v", code: "KeyV", keyCode: 86, ...modifier };
    shim.sendKey(true, init);
    window.setTimeout(() => shim.sendKey(false, init), 30);
  }

  /** Last clipboard text the remote desktop sent us (debugging/status). */
  lastClipboard(): string {
    return this.lastRemoteClipboard;
  }

  /* ------------------------------------------------------------- accessors */

  /** RAST app ids owned by the native session (used by access enforcement). */
  isManagedAppId(appId: string): boolean {
    for (const frame of this.frames.values()) if (frame.appId === appId) return true;
    return false;
  }

  /** The app was uninstalled or lost access: close its RAST windows. */
  isWindowAllowed(appId: string): boolean {
    for (const frame of this.frames.values()) {
      if (frame.appId !== appId) continue;
      if (frame.slug === "unknown") return true;
      return linuxApps().some((app) => app.id === frame.slug);
    }
    return true;
  }

  closeManagedWindow(appId: string): void {
    for (const frame of [...this.frames.values()]) {
      if (frame.appId === appId && frame.handle && !frame.handle.isClosed()) frame.handle.close();
    }
  }

  /** Session status for the App Center banner / window titles. */
  status(): { session: NativeSession | null; connected: boolean; windows: number; unsupported: boolean } {
    return {
      session: this.session,
      connected: !!this.shim?.state?.connected,
      windows: this.frames.size,
      unsupported: this.nativeUnavailable,
    };
  }
}

/** Stable comparison of the session fields the UI reacts to. */
function sessionSignature(session: NativeSession | null): string {
  if (!session) return "";
  const apps = (session.apps ?? []).map((app) => `${app.runId}:${app.appId}:${app.status}:${app.error ?? ""}`);
  return [session.id, session.status, session.url, session.reason ?? "", session.error ?? "", session.separateWindows ? 1 : 0, ...apps].join("|");
}

function sameSession(a: NativeSession | null, b: NativeSession | null): boolean {
  if (a === b) return true;
  return sessionSignature(a) === sessionSignature(b);
}

function emptySession(): NativeSession {
  return {
    id: "native",
    status: "ended",
    url: "",
    startedAt: 0,
    viewers: 0,
    separateWindows: true,
    reason: "closed",
    error: null,
    apps: [],
  };
}
