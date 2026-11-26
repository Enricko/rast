// One shell of the Terminal app: xterm.js over a WebSocket to
// /api/terminal/ws. The shell lives on the server (src/terminal): closing the
// window or losing the network only detaches, and the view reattaches by the
// shell's id, getting its recent output first.

import type { AppContext } from "../../core/app";
import { h } from "../../core/dom";
import { copyText, errorMessage } from "../../core/ui";
import { TERMINAL_THEME } from "../applications/terminal";

export interface ShellInfo {
  id: string;
  number: number;
  createdAt: number;
  attached: boolean;
  program: string;
}

export type ShellMessage =
  | { type: "ready"; session: ShellInfo; created: boolean }
  | { type: "exit"; code: number | null }
  | { type: "detached" }
  | { type: "error"; message: string };

export type ShellPhase = "loading" | "connecting" | "ready" | "reconnecting" | "detached" | "ended" | "failed";

/** What the account badge says about a sandbox (its limits). */
export function sandboxSummary(limits?: { memoryMb?: number; cpus?: string; network?: boolean }): string {
  const parts = ["Your own sandbox: only your home folder"];
  if (limits?.network === false) parts.push("no network");
  if (limits?.memoryMb) parts.push(`${limits.memoryMb >= 1024 ? `${limits.memoryMb / 1024} GB` : `${limits.memoryMb} MB`} memory`);
  if (limits?.cpus) parts.push(`${limits.cpus} CPU${limits.cpus === "1" ? "" : "s"}`);
  return parts.join(", ");
}

/** WebSocket address of the Terminal, on the page's own host. */
export function shellSocketUrl(location: Pick<Location, "protocol" | "host">): string {
  return `${location.protocol === "https:" ? "wss:" : "ws:"}//${location.host}/api/terminal/ws`;
}

function asShellInfo(value: unknown): ShellInfo | null {
  if (!value || typeof value !== "object") return null;
  const v = value as Record<string, unknown>;
  if (typeof v.id !== "string" || !/^[0-9a-f]{16}$/.test(v.id)) return null;
  return {
    id: v.id,
    number: typeof v.number === "number" && v.number > 0 ? v.number : 1,
    createdAt: typeof v.createdAt === "number" ? v.createdAt : 0,
    attached: v.attached === true,
    program: typeof v.program === "string" ? v.program.slice(0, 32) : "",
  };
}

/** Parse a text message from the server; anything unexpected is ignored. */
export function parseShellMessage(text: string): ShellMessage | null {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    return null;
  }
  if (!value || typeof value !== "object") return null;
  const m = value as Record<string, unknown>;
  switch (m.type) {
    case "ready": {
      const session = asShellInfo(m.session);
      return session ? { type: "ready", session, created: m.created === true } : null;
    }
    case "exit":
      return { type: "exit", code: typeof m.code === "number" ? m.code : null };
    case "detached":
      return { type: "detached" };
    case "error":
      return { type: "error", message: typeof m.message === "string" && m.message ? m.message : "The terminal is not available." };
    default:
      return null;
  }
}

/** Sessions the server listed (`GET /api/terminal`), valid ones only. */
export function parseShellList(value: unknown): ShellInfo[] {
  if (!Array.isArray(value)) return [];
  return value.map(asShellInfo).filter((s): s is ShellInfo => s !== null);
}

/** Wait before reconnect attempt `attempt` (0-based): 1 s, 2 s, 4 s … 15 s. */
export function reconnectDelay(attempt: number): number {
  return Math.min(15_000, 1000 * 2 ** Math.max(0, attempt));
}

/** Give up reconnecting after this many attempts (about two minutes). */
export const MAX_RECONNECTS = 10;

const SHELLS = new Set(["bash", "sh", "dash", "zsh", "fish", "ksh", "tcsh", "csh", "-bash", "-sh", "-zsh"]);

/** Whether a program other than the shell itself is in the foreground. */
export function isBusy(program: string): boolean {
  return Boolean(program) && !SHELLS.has(program);
}

/** Tab label: "1", or "2 · htop" while a program runs. */
export function tabLabel(info: Pick<ShellInfo, "number" | "program">): string {
  return isBusy(info.program) ? `${info.number} · ${info.program}` : String(info.number);
}

export interface ShellView {
  el: HTMLElement;
  readonly phase: ShellPhase;
  readonly info: ShellInfo | null;
  setVisible(visible: boolean): void;
  focus(): void;
  copySelection(): Promise<void>;
  /** Show the shell here again (after it was opened in another window). */
  reattach(): void;
  /** Close the connection; the shell keeps running on the server. */
  detach(): void;
}

export interface ShellOptions {
  ctx: AppContext;
  /** Reattach to this shell; without it a new shell is started. */
  session?: ShellInfo;
  onChange(view: ShellView): void;
}

export function mountShell(options: ShellOptions): ShellView {
  const { ctx } = options;
  let info: ShellInfo | null = options.session ?? null;
  let phase: ShellPhase = "loading";
  let socket: WebSocket | null = null;
  let term: import("../applications/xterm-bundle").Terminal | null = null;
  let fit: import("../applications/xterm-bundle").FitAddon | null = null;
  let observer: ResizeObserver | null = null;
  let fitFrame = 0;
  let retryTimer = 0;
  let attempts = 0;
  /** The view was removed (detach). */
  let disposed = false;
  /** The shell is gone: nothing to reattach to. */
  let finished = false;
  let visible = true;
  const encoder = new TextEncoder();

  const screen = h("div", { class: "term-screen", ariaLabel: "Terminal" });
  const overlayTitle = h("p", { class: "term-overlay-title" });
  const overlayText = h("p", { class: "term-overlay-text muted" });
  const overlayButton = h("button", { class: "btn btn-primary btn-sm", type: "button" });
  const overlay = h("div", { class: "term-overlay hidden", role: "status" }, overlayTitle, overlayText, overlayButton);
  const el = h("div", { class: "term-shell" }, screen, overlay);
  overlayButton.addEventListener("click", () => view.reattach());

  const view: ShellView = {
    el,
    get phase() {
      return phase;
    },
    get info() {
      return info;
    },
    setVisible(next) {
      visible = next;
      el.classList.toggle("hidden", !next);
      if (next) fitSoon();
    },
    focus() {
      term?.focus();
    },
    copySelection,
    reattach() {
      if (phase !== "detached" && phase !== "failed") return;
      attempts = 0;
      connect();
    },
    detach() {
      disposed = true;
      window.clearTimeout(retryTimer);
      if (fitFrame) cancelAnimationFrame(fitFrame);
      observer?.disconnect();
      closeSocket();
      term?.dispose();
      term = null;
      el.remove();
    },
  };

  function setPhase(next: ShellPhase, overlayInfo?: { title: string; text: string; action?: string }): void {
    phase = next;
    el.dataset.phase = next;
    overlay.classList.toggle("hidden", !overlayInfo);
    if (overlayInfo) {
      overlayTitle.textContent = overlayInfo.title;
      overlayText.textContent = overlayInfo.text;
      overlayButton.textContent = overlayInfo.action ?? "";
      overlayButton.classList.toggle("hidden", !overlayInfo.action);
    }
    options.onChange(view);
  }

  async function copySelection(): Promise<void> {
    const text = term?.getSelection() ?? "";
    if (!text) {
      ctx.toast.info("Select text in the terminal first");
      return;
    }
    if (await copyText(text)) ctx.toast.success("Copied");
    else ctx.toast.error("Could not copy");
  }

  function send(data: Uint8Array | string): void {
    if (socket?.readyState === WebSocket.OPEN && phase === "ready") socket.send(data);
  }

  function fitSoon(): void {
    if (fitFrame) return;
    fitFrame = requestAnimationFrame(() => {
      fitFrame = 0;
      if (disposed || !visible || !fit || !screen.isConnected || screen.clientWidth === 0) return;
      try {
        fit.fit();
      } catch {
        /* not measurable yet */
      }
    });
  }

  async function start(): Promise<void> {
    try {
      const xterm = await import("../applications/xterm-bundle");
      if (disposed) return;
      const mono = getComputedStyle(document.documentElement).getPropertyValue("--font-mono").trim();
      term = new xterm.Terminal({
        cursorBlink: true,
        fontFamily: mono || "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
        fontSize: 13,
        lineHeight: 1.15,
        scrollback: 10000,
        theme: TERMINAL_THEME,
      });
      fit = new xterm.FitAddon();
      term.loadAddon(fit);
      term.open(screen);
      fitSoon();
      term.onData((data) => send(encoder.encode(data)));
      term.onBinary((data) => send(Uint8Array.from(data, (c) => c.charCodeAt(0) & 0xff)));
      term.onResize(({ cols, rows }) => send(JSON.stringify({ type: "resize", cols, rows })));
      term.attachCustomKeyEventHandler((event) => {
        // Ctrl+Shift+C copies; plain Ctrl+C stays an interrupt for the shell.
        if (event.type === "keydown" && event.ctrlKey && event.shiftKey && event.code === "KeyC") {
          void copySelection();
          return false;
        }
        return true;
      });
      observer = new ResizeObserver(() => fitSoon());
      observer.observe(screen);
      connect();
    } catch (error) {
      if (disposed) return;
      setPhase("failed", { title: "Terminal unavailable", text: `The terminal could not be loaded: ${errorMessage(error)}` });
    }
  }

  function connect(): void {
    if (disposed || finished || !term) return;
    window.clearTimeout(retryTimer);
    closeSocket();
    setPhase(attempts > 0 ? "reconnecting" : "connecting", attempts > 0 ? { title: "Reconnecting…", text: "The connection to the server was lost. Your shell is still running." } : undefined);
    const ws = new WebSocket(shellSocketUrl(window.location));
    ws.binaryType = "arraybuffer";
    socket = ws;
    let answered = false;
    ws.addEventListener("open", () => {
      if (socket !== ws || !term) return;
      const start: Record<string, unknown> = { type: "start", csrf: ctx.api.getCsrfToken(), cols: term.cols, rows: term.rows };
      if (info) start.session = info.id;
      ws.send(JSON.stringify(start));
    });
    ws.addEventListener("message", (event) => {
      if (socket !== ws || !term) return;
      if (typeof event.data !== "string") {
        term.write(new Uint8Array(event.data as ArrayBuffer));
        return;
      }
      const message = parseShellMessage(event.data);
      if (!message) return;
      answered = true;
      switch (message.type) {
        case "ready":
          // A reattached shell's recent output follows: start from a clean screen.
          term.reset();
          info = message.session;
          attempts = 0;
          setPhase("ready");
          fitSoon();
          if (visible) term.focus();
          break;
        case "exit":
          term.write(`\r\n\x1b[2m[${message.code === null || message.code === 0 ? "The shell exited." : `The shell exited with code ${message.code}.`}]\x1b[0m\r\n`);
          finished = true;
          setPhase("ended");
          break;
        case "detached":
          setPhase("detached", { title: "Open in another window", text: "This shell is shown in another browser window or tab now.", action: "Show it here" });
          break;
        case "error":
          if (/no longer open/i.test(message.message)) {
            finished = true;
            setPhase("ended", { title: "This shell has ended", text: "It was closed on the server, or it exited." });
          } else {
            setPhase("failed", { title: "Terminal unavailable", text: message.message, action: "Try again" });
          }
          break;
      }
    });
    ws.addEventListener("close", () => {
      if (socket !== ws) return;
      socket = null;
      if (disposed || finished || phase === "detached" || phase === "failed") return;
      // Dropped (network, server restart, sleep): try again with backoff.
      if (!answered && attempts === 0 && !info) {
        setPhase("failed", { title: "Terminal unavailable", text: "Could not connect to the terminal. Check that you are still signed in.", action: "Try again" });
        return;
      }
      if (attempts >= MAX_RECONNECTS) {
        setPhase("failed", { title: "Disconnected", text: "The server could not be reached. Your shell may still be running.", action: "Reconnect" });
        return;
      }
      const delay = reconnectDelay(attempts);
      attempts += 1;
      setPhase("reconnecting", { title: "Reconnecting…", text: "The connection to the server was lost. Your shell is still running." });
      retryTimer = window.setTimeout(connect, delay);
    });
  }

  function closeSocket(): void {
    const ws = socket;
    socket = null;
    if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) ws.close();
  }

  setPhase("loading");
  void start();
  return view;
}
