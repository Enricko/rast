// Interactive terminal into one container: xterm.js (loaded on first use)
// over a WebSocket to /api/docker/containers/{id}/terminal.

import type { AppContext } from "../../core/app";
import { h } from "../../core/dom";
import { iconSvg } from "../../core/icons";
import { copyText, errorMessage } from "../../core/ui";

export interface TerminalView {
  el: HTMLElement;
  dispose(): void;
}

export type TerminalMessage =
  | { type: "ready"; container: string }
  | { type: "exit"; code: number | null }
  | { type: "error"; message: string };

type Phase = "loading" | "connecting" | "ready" | "ended" | "failed";

/** WebSocket address of a container's terminal, on the page's own host. */
export function terminalUrl(location: Pick<Location, "protocol" | "host">, id: string): string {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  return `${scheme}//${location.host}/api/docker/containers/${encodeURIComponent(id)}/terminal`;
}

/** Parse a text message from the server; anything unexpected is ignored. */
export function parseTerminalMessage(text: string): TerminalMessage | null {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    return null;
  }
  if (!value || typeof value !== "object") return null;
  const message = value as Record<string, unknown>;
  switch (message.type) {
    case "ready":
      return { type: "ready", container: typeof message.container === "string" ? message.container : "" };
    case "exit":
      return { type: "exit", code: typeof message.code === "number" ? message.code : null };
    case "error":
      return { type: "error", message: typeof message.message === "string" && message.message ? message.message : "The terminal is not available." };
    default:
      return null;
  }
}

/** Line written into the terminal when the shell ends. A shell that cannot
 * start ends within seconds with 126/127: the image has no shell. */
export function exitNotice(code: number | null, quick = false): string {
  if (code === null) return "The terminal session ended.";
  if (quick && (code === 126 || code === 127)) return "This container has no shell (sh), so a terminal cannot run in it.";
  if (code === 0) return "The shell exited.";
  return `The shell exited with code ${code}.`;
}

// Colours of the terminal itself; it stays dark in both themes, like the
// log viewer next to it.
const THEME = {
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
};

export function mountTerminal(options: { ctx: AppContext; id: string; name: string }): TerminalView {
  const { ctx } = options;
  let disposed = false;
  let phase: Phase = "loading";
  let socket: WebSocket | null = null;
  let term: import("./xterm-bundle").Terminal | null = null;
  let fit: import("./xterm-bundle").FitAddon | null = null;
  let observer: ResizeObserver | null = null;
  let fitFrame = 0;
  let readyAt = 0;
  const encoder = new TextEncoder();

  const dot = h("span", { class: "dk-term-dot", ariaHidden: true });
  const statusText = h("span", {}, "Loading terminal…");
  const status = h("span", { class: "dk-term-status small", role: "status" }, dot, statusText);
  const copyButton = h("button", { class: "btn btn-ghost btn-icon btn-sm", type: "button", title: "Copy selection (Ctrl+Shift+C)", ariaLabel: "Copy selection" }, iconSvg("copy", 14));
  const againButton = h("button", { class: "btn btn-ghost btn-sm hidden", type: "button" }, iconSvg("refresh", 13), h("span", {}, "New session"));
  const screen = h("div", { class: "dk-term", ariaLabel: `Terminal of ${options.name}` });
  const notice = h("div", { class: "dk-term-notice hidden", role: "alert" });
  const el = h("div", { class: "dk-term-wrap" }, h("div", { class: "dk-log-toolbar dk-term-toolbar" }, status, h("div", { class: "app-toolbar-spacer" }), againButton, copyButton), notice, screen);

  copyButton.addEventListener("click", () => void copySelection());
  againButton.addEventListener("click", () => {
    if (!term) return;
    term.reset();
    connect();
  });

  function setPhase(next: Phase, text: string): void {
    phase = next;
    statusText.textContent = text;
    el.dataset.phase = next;
    againButton.classList.toggle("hidden", next !== "ended" && next !== "failed");
  }

  function showNotice(text: string | null): void {
    notice.textContent = text ?? "";
    notice.classList.toggle("hidden", !text);
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
    if (!socket || socket.readyState !== WebSocket.OPEN || phase !== "ready") return;
    socket.send(data);
  }

  function fitSoon(): void {
    if (fitFrame) return;
    fitFrame = requestAnimationFrame(() => {
      fitFrame = 0;
      if (disposed || !fit || !screen.isConnected || screen.clientWidth === 0) return;
      try {
        fit.fit();
      } catch {
        /* not measurable yet */
      }
    });
  }

  async function start(): Promise<void> {
    try {
      const xterm = await import("./xterm-bundle");
      if (disposed) return;
      const mono = getComputedStyle(document.documentElement).getPropertyValue("--font-mono").trim();
      term = new xterm.Terminal({
        cursorBlink: true,
        fontFamily: mono || "ui-monospace, SFMono-Regular, Menlo, Consolas, monospace",
        fontSize: 13,
        lineHeight: 1.15,
        scrollback: 5000,
        theme: THEME,
        convertEol: false,
      });
      fit = new xterm.FitAddon();
      term.loadAddon(fit);
      term.open(screen);
      fit.fit();
      term.onData((data) => send(encoder.encode(data)));
      term.onBinary((data) => send(Uint8Array.from(data, (char) => char.charCodeAt(0) & 0xff)));
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
      setPhase("failed", "Terminal unavailable");
      showNotice(`The terminal could not be loaded: ${errorMessage(error)}`);
    }
  }

  function connect(): void {
    if (disposed || !term) return;
    closeSocket();
    showNotice(null);
    setPhase("connecting", "Connecting…");
    const ws = new WebSocket(terminalUrl(window.location, options.id));
    ws.binaryType = "arraybuffer";
    socket = ws;
    ws.addEventListener("open", () => {
      if (socket !== ws || !term) return;
      ws.send(JSON.stringify({ type: "start", csrf: ctx.api.getCsrfToken(), cols: term.cols, rows: term.rows }));
    });
    ws.addEventListener("message", (event) => {
      if (socket !== ws || !term) return;
      if (typeof event.data !== "string") {
        term.write(new Uint8Array(event.data as ArrayBuffer));
        return;
      }
      const message = parseTerminalMessage(event.data);
      if (!message) return;
      if (message.type === "ready") {
        readyAt = Date.now();
        setPhase("ready", `Connected to ${message.container || options.name}`);
        fitSoon();
        term.focus();
      } else if (message.type === "exit") {
        term.write(`\r\n\x1b[2m${exitNotice(message.code, Date.now() - readyAt < 3000)}\x1b[0m\r\n`);
        setPhase("ended", "Session ended");
      } else {
        showNotice(message.message);
        setPhase("failed", phase === "ready" ? "Session closed" : "Terminal unavailable");
      }
    });
    ws.addEventListener("close", () => {
      if (socket !== ws) return;
      socket = null;
      if (phase === "connecting") {
        setPhase("failed", "Terminal unavailable");
        if (notice.classList.contains("hidden")) showNotice("Could not connect to the terminal. Check that you are still signed in and try again.");
      } else if (phase === "ready") {
        term?.write(`\r\n\x1b[2m${exitNotice(null)}\x1b[0m\r\n`);
        setPhase("ended", "Disconnected");
      }
    });
  }

  function closeSocket(): void {
    const ws = socket;
    socket = null;
    if (ws && (ws.readyState === WebSocket.OPEN || ws.readyState === WebSocket.CONNECTING)) ws.close();
  }

  setPhase("loading", "Loading terminal…");
  void start();

  return {
    el,
    dispose: () => {
      disposed = true;
      if (fitFrame) cancelAnimationFrame(fitFrame);
      observer?.disconnect();
      closeSocket();
      term?.dispose();
      term = null;
      el.remove();
    },
  };
}
