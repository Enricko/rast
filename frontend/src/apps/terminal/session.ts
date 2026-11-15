// One terminal tab: an xterm.js screen bound to one PTY session over a
// WebSocket. xterm and its addons are imported dynamically (see
// `applications/xterm-bundle.ts`) so the desktop bundle stays small.
//
// Keyboard policy (see also `terminalShortcut` in ./model):
//   * Alt/Ctrl/arrow/Tab/completion keys are left entirely to xterm → the
//     shell receives Ctrl+C (^C), Ctrl+D, Ctrl+Z, arrows and UTF-8 input.
//   * Only Ctrl+Shift / Cmd+Shift shortcuts (copy, paste, search, new tab) are
//     intercepted; they are handled by the app's capture listener and xterm's
//     custom handler declines them so no escape bytes reach the shell.
//   * Browser-reserved chords are NOT intercepted and remain browser-controlled:
//     Ctrl+W / Ctrl+Shift+W (close tab/window), Ctrl+T / Ctrl+Shift+N (new
//     browser tab/window), Ctrl+Shift+I / J / C (devtools), Ctrl+L (address
//     bar), F5/F11 and the OS-level shortcuts. Every action has a toolbar
//     button as the guaranteed path, so nothing is unreachable when the
//     browser eats a chord.

import type { IDisposable, Terminal as XtermTerminal } from "@xterm/xterm";
import type { FitAddon as XtermFitAddon } from "@xterm/addon-fit";
import type { SearchAddon as XtermSearchAddon } from "@xterm/addon-search";
import type { ApiClient } from "../../core/api";
import { h } from "../../core/dom";
import { copyText } from "../../core/ui";
import {
  buildTerminalStartMessage,
  initialTerminalState,
  parseTerminalServerMessage,
  reduceTerminalState,
  terminalExitNotice,
  terminalShortcut,
  terminalWebSocketUrl,
  TERMINAL_RESIZE_DEBOUNCE_MS,
  TERMINAL_RECONNECT_DELAY_MS,
  TERMINAL_SCROLLBACK,
  type TerminalEvent,
  type TerminalFolder,
  type TerminalReadyMessage,
  type TerminalState,
} from "./model";

type XtermModule = typeof import("../applications/xterm-bundle");

// One dynamic import per page: xterm + styles live in a lazy chunk.
let xtermModule: Promise<XtermModule> | null = null;

export function loadXterm(): Promise<XtermModule> {
  xtermModule ??= import("../applications/xterm-bundle");
  return xtermModule;
}

// The terminal itself stays dark in both themes, like the Docker container
// terminal and the log viewer.
const TERMINAL_THEME = {
  background: "#0f1419",
  foreground: "#d5dde6",
  cursor: "#d5dde6",
  cursorAccent: "#0f1419",
  selectionBackground: "#33506e",
  black: "#1c232b",
  red: "#ef6b73",
  green: "#8fd18b",
  yellow: "#e6c07b",
  blue: "#6cb6ff",
  magenta: "#c79bf2",
  cyan: "#5fd4d4",
  white: "#d5dde6",
  brightBlack: "#5c6773",
  brightRed: "#ff8f96",
  brightGreen: "#aee8a9",
  brightYellow: "#f2d59a",
  brightBlue: "#99ccff",
  brightMagenta: "#ddbdfa",
  brightCyan: "#8ce6e6",
  brightWhite: "#ffffff",
} as const;

const SEARCH_DECORATIONS = {
  matchBackground: "#33506e",
  matchOverviewRuler: "#33506e",
  activeMatchBackground: "#4d7ab0",
  activeMatchColorOverviewRuler: "#6cb6ff",
} as const;

export interface TerminalSessionOptions {
  api: ApiClient;
  /** Logical folder the session starts in; null opens the private home. */
  folder: TerminalFolder | null;
  /** Runtime overrides for a NEW session (empty = server/user defaults). */
  runtimes: Record<string, string>;
  /**
   * A shell that is already running on the server (the page was reloaded or
   * Terminal was reopened): the tab attaches to it instead of starting one.
   */
  attach?: string | null;
  fontSize: number;
  onState(state: TerminalState): void;
  onReady?(ready: TerminalReadyMessage): void;
  onSearchResults?(resultIndex: number, resultCount: number): void;
}

export class TerminalSession {
  /** The element xterm renders into; the app positions and wraps it. */
  readonly el: HTMLElement;
  private readonly options: TerminalSessionOptions;
  private readonly encoder = new TextEncoder();
  private readonly folder: TerminalFolder | null;
  private readonly runtimes: Record<string, string>;
  private currentFontSize: number;
  private state: TerminalState = initialTerminalState();
  private term: XtermTerminal | null = null;
  private fit: XtermFitAddon | null = null;
  private search: XtermSearchAddon | null = null;
  private searchResults: IDisposable | null = null;
  private socket: WebSocket | null = null;
  private observer: ResizeObserver | null = null;
  private disposed = false;
  private reconnectTimer: number | undefined;
  private resizeTimer: number | undefined;
  private fitFrame: number | undefined;
  private started = false;

  constructor(options: TerminalSessionOptions) {
    this.options = options;
    this.folder = options.folder ? { path: options.folder.path } : null;
    this.runtimes = { ...options.runtimes };
    this.currentFontSize = options.fontSize;
    if (options.attach) this.state = { ...this.state, sessionId: options.attach };
    this.el = h("div", { class: "term-screen-host" });
  }

  get status(): TerminalState {
    return this.state;
  }

  /** Load xterm lazily, open the screen and connect the WebSocket. */
  async start(): Promise<void> {
    if (this.started || this.disposed) return;
    this.started = true;
    let xterm: XtermModule;
    try {
      xterm = await loadXterm();
    } catch (error) {
      this.transition({ type: "error", message: `Could not load the terminal renderer: ${errorMessage(error)}` });
      return;
    }
    if (this.disposed) return;
    this.createTerminal(xterm);
    this.transition({ type: "loaded" });
    this.openSocket(Boolean(this.state.sessionId));
  }

  private createTerminal(xterm: XtermModule): void {
    const mono = getComputedStyle(document.documentElement).getPropertyValue("--font-mono").trim();
    const terminal = new xterm.Terminal({
      cursorBlink: true,
      fontFamily: mono || "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
      fontSize: this.currentFontSize,
      lineHeight: 1.15,
      scrollback: TERMINAL_SCROLLBACK,
      theme: { ...TERMINAL_THEME },
      convertEol: false,
    });
    this.term = terminal;
    this.fit = new xterm.FitAddon();
    terminal.loadAddon(this.fit);
    const search = new xterm.SearchAddon();
    this.search = search;
    terminal.loadAddon(search);
    this.searchResults = search.onDidChangeResults(({ resultIndex, resultCount }) => {
      this.options.onSearchResults?.(resultIndex, resultCount);
    });
    terminal.open(this.el);
    terminal.onData((data) => {
      if (this.state.phase !== "connected") return;
      this.sendBytes(this.encoder.encode(data));
    });
    terminal.onBinary((data) => {
      if (this.state.phase !== "connected") return;
      this.sendBytes(Uint8Array.from(data, (char) => char.charCodeAt(0) & 0xff));
    });
    terminal.onResize(() => this.scheduleResize());
    // Declines only the app shortcuts; everything else (including plain
    // Ctrl+C/D/Z, Ctrl+V, Tab, arrows and Unicode input) reaches the shell.
    terminal.attachCustomKeyEventHandler((event) => {
      if (event.type !== "keydown") return true;
      if (terminalShortcut(event)) {
        event.preventDefault();
        return false;
      }
      return true;
    });
    this.observer = new ResizeObserver(() => this.fitSoon());
    this.observer.observe(this.el);
    this.fitNow();
    terminal.focus();
  }

  focus(): void {
    this.term?.focus();
  }

  fitSoon(): void {
    if (this.disposed || this.fitFrame !== undefined) return;
    this.fitFrame = window.requestAnimationFrame(() => {
      this.fitFrame = undefined;
      this.fitNow();
    });
  }

  private fitNow(): void {
    if (this.disposed || !this.fit || !this.term) return;
    if (!this.el.isConnected || this.el.clientWidth === 0) return;
    try {
      this.fit.fit();
    } catch {
      /* not measurable yet */
    }
  }

  private scheduleResize(): void {
    if (this.resizeTimer !== undefined) window.clearTimeout(this.resizeTimer);
    this.resizeTimer = window.setTimeout(() => {
      this.resizeTimer = undefined;
      this.sendResizeNow();
    }, TERMINAL_RESIZE_DEBOUNCE_MS);
  }

  private sendResizeNow(): void {
    if (!this.socket || this.socket.readyState !== WebSocket.OPEN) return;
    if (this.state.phase !== "connected" || !this.term) return;
    this.socket.send(JSON.stringify({ type: "resize", cols: this.term.cols, rows: this.term.rows }));
  }

  private sendBytes(data: Uint8Array): void {
    if (!this.socket || this.socket.readyState !== WebSocket.OPEN) return;
    this.socket.send(data);
  }

  /** Send literal text (clipboard paste) as keyboard bytes. */
  sendText(text: string): boolean {
    if (!text || this.state.phase !== "connected" || !this.socket || this.socket.readyState !== WebSocket.OPEN) {
      return false;
    }
    this.socket.send(this.encoder.encode(text));
    this.focus();
    return true;
  }

  private openSocket(useSession: boolean): void {
    if (this.disposed || !this.term) return;
    this.cancelReconnect();
    this.closeSocket();
    const sessionId = useSession ? this.state.sessionId : null;
    const ws = new WebSocket(terminalWebSocketUrl(window.location));
    ws.binaryType = "arraybuffer";
    this.socket = ws;
    ws.addEventListener("open", () => {
      if (this.socket !== ws || this.disposed || !this.term) return;
      const message = buildTerminalStartMessage({
        csrf: this.options.api.getCsrfToken(),
        cols: this.term.cols,
        rows: this.term.rows,
        folder: sessionId ? null : this.folder,
        runtimes: sessionId ? null : this.runtimes,
        session: sessionId,
      });
      ws.send(JSON.stringify(message));
    });
    ws.addEventListener("message", (event) => this.onMessage(ws, event));
    ws.addEventListener("close", () => this.onSocketClosed(ws));
    ws.addEventListener("error", () => {
      /* a close event always follows */
    });
  }

  private onMessage(ws: WebSocket, event: MessageEvent): void {
    if (this.socket !== ws || this.disposed) return;
    if (typeof event.data !== "string") {
      const data = event.data as ArrayBuffer;
      if (data instanceof ArrayBuffer) this.term?.write(new Uint8Array(data));
      return;
    }
    const message = parseTerminalServerMessage(event.data);
    if (!message) return;
    switch (message.type) {
      case "ready":
        this.transition({ type: "ready", ready: message });
        this.fitSoon();
        this.term?.focus();
        // Send the fitted geometry explicitly: the server may have started
        // with the pre-fit defaults if the first ready arrived very fast.
        this.sendResizeNow();
        this.options.onReady?.(message);
        break;
      case "exit":
        if (this.state.phase === "exited") return;
        this.term?.write(`\r\n\x1b[2m${terminalExitNotice(message.code)}\x1b[0m\r\n`);
        this.transition({ type: "exit", code: message.code });
        this.closeSocket();
        break;
      case "error":
        this.transition({ type: "error", message: message.message });
        this.closeSocket();
        break;
      default:
        break;
    }
  }

  private onSocketClosed(ws: WebSocket): void {
    if (this.socket !== ws) return;
    this.socket = null;
    if (this.disposed) return;
    const previous = this.state.phase;
    this.apply(reduceTerminalState(this.state, { type: "socket-closed" }));
    if (this.state.phase === "reconnecting" && previous === "connected") {
      // Reconnect once, shortly after an unexpected drop.
      this.reconnectTimer = window.setTimeout(() => {
        this.reconnectTimer = undefined;
        if (!this.disposed && this.state.phase === "reconnecting") this.openSocket(true);
      }, TERMINAL_RECONNECT_DELAY_MS);
    }
  }

  /** Retry after a disconnect (reattaches the previous session when possible). */
  retry(): void {
    if (this.disposed) return;
    if (!this.term) {
      // The lazy xterm chunk failed to load (offline, stale asset): load again.
      this.started = false;
      this.apply(initialTerminalState());
      void this.start();
      return;
    }
    this.transition({ type: "retry" });
    this.openSocket(Boolean(this.state.sessionId));
  }

  /** Replace an ended/failed session with a fresh shell in the same tab. */
  newSession(): void {
    if (this.disposed) return;
    if (!this.term) {
      this.started = false;
      this.apply(initialTerminalState());
      void this.start();
      return;
    }
    this.transition({ type: "new-session" });
    this.term.reset();
    this.term.clear();
    this.openSocket(false);
  }

  private transition(event: TerminalEvent): void {
    this.apply(reduceTerminalState(this.state, event));
  }

  private apply(next: TerminalState): void {
    this.state = next;
    this.options.onState(next);
  }

  private closeSocket(): void {
    const ws = this.socket;
    this.socket = null;
    if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) ws.close();
  }

  private cancelReconnect(): void {
    if (this.reconnectTimer !== undefined) {
      window.clearTimeout(this.reconnectTimer);
      this.reconnectTimer = undefined;
    }
  }

  // ------------------------------------------------------------- toolbar API

  async copySelection(): Promise<boolean> {
    const selection = this.term?.getSelection() ?? "";
    if (!selection) return false;
    const copied = await copyText(selection);
    this.focus();
    return copied;
  }

  /** Read the clipboard and write it to the shell; false when unavailable. */
  async pasteFromClipboard(): Promise<boolean> {
    try {
      if (!navigator.clipboard?.readText) return false;
      const text = await navigator.clipboard.readText();
      return this.sendText(text);
    } catch {
      return false;
    }
  }

  hasSelection(): boolean {
    return Boolean(this.term?.getSelection());
  }

  clear(): void {
    this.term?.clear();
  }

  searchNext(query: string, caseSensitive: boolean): boolean {
    if (!query || !this.search) return false;
    return this.search.findNext(query, { caseSensitive, decorations: { ...SEARCH_DECORATIONS } });
  }

  searchPrevious(query: string, caseSensitive: boolean): boolean {
    if (!query || !this.search) return false;
    return this.search.findPrevious(query, { caseSensitive, decorations: { ...SEARCH_DECORATIONS } });
  }

  clearSearch(): void {
    this.search?.clearDecorations();
  }

  setFontSize(size: number): void {
    this.currentFontSize = size;
    if (this.term) this.term.options.fontSize = size;
    this.fitSoon();
  }

  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    // Tell the backend this tab is gone so it closes the PTY. (A plain socket
    // close, as on a page reload, is a detach: the shell keeps running and
    // Terminal attaches to it again.)
    if (
      this.socket &&
      this.socket.readyState === WebSocket.OPEN &&
      this.state.phase !== "exited" &&
      this.state.phase !== "failed"
    ) {
      try {
        this.socket.send(JSON.stringify({ type: "close" }));
      } catch {
        /* the socket may be closing */
      }
    }
    this.cancelReconnect();
    if (this.resizeTimer !== undefined) window.clearTimeout(this.resizeTimer);
    if (this.fitFrame !== undefined) window.cancelAnimationFrame(this.fitFrame);
    this.resizeTimer = undefined;
    this.fitFrame = undefined;
    this.observer?.disconnect();
    this.observer = null;
    this.searchResults?.dispose();
    this.searchResults = null;
    this.closeSocket();
    this.term?.dispose();
    this.term = null;
    this.fit = null;
    this.search = null;
    this.el.remove();
  }
}

function errorMessage(error: unknown): string {
  return error instanceof Error ? error.message : String(error);
}
