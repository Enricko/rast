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

import { h } from "../core/dom";
import type { ApiClient } from "../core/api";
import type { AppDefinition } from "../core/app";
import { getRealtime, type RealtimeStore } from "../core/realtime";
import type { EventStream } from "../core/sse";
import type { ToastCenter } from "../core/toast";
import { errorMessage } from "../core/ui";
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
  captureKeyboard: boolean;
  windows: NativeShimWindow[];
}

interface KeyboardInit {
  key?: string;
  code?: string;
  keyCode?: number;
  ctrlKey?: boolean;
  shiftKey?: boolean;
  altKey?: boolean;
  metaKey?: boolean;
  repeat?: boolean;
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
  sendKey(pressed: boolean, init: KeyboardInit): boolean;
  sendClipboardText(text: string): boolean;
  clipboardText(): string;
  requestClipboard(): boolean;
  reconnect(): boolean;
  disconnect(): boolean;
  on(event: string, callback: (payload?: never) => void): () => void;
  off(event: string, callback: (payload?: never) => void): void;
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

interface WholeSessionWindow {
  handle: WindowHandle | null;
  separate: boolean;
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
  private wholeSession: WholeSessionWindow | null = null;
  private lastSnapshot: NativeShimState | null = null;
  private pollTimer = 0;
  private reconnectTimer = 0;
  private attachTimer = 0;
  /** Safety poll: the shim may publish its snapshot before this page attaches
   * its listeners (a session created during a first launch does exactly that),
   * in which case no later window event would ever arrive. */
  private snapshotTimer = 0;
  private lastLaunchSlug: string | null = null;
  private lastLaunchAt = 0;
  private disposed = false;
  private quiet = false;
  private nativeUnavailable = false;
  private lastRemoteClipboard = "";
  private focusedWid: string | null = null;
  private readonly onPasteHandler: (event: ClipboardEvent) => void;
  private readonly onKeyDownHandler: (event: KeyboardEvent) => void;
  private readonly onKeyUpHandler: (event: KeyboardEvent) => void;

  constructor(deps: NativeLinuxDeps) {
    this.deps = deps;
    this.realtime = getRealtime(deps.events);
    this.onPasteHandler = (event) => this.forwardPaste(event);
    this.onKeyDownHandler = (event) => this.forwardKey(event, true);
    this.onKeyUpHandler = (event) => this.forwardKey(event, false);
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
    document.addEventListener("paste", this.onPasteHandler, true);
    this.unsubscribers.push(() => document.removeEventListener("paste", this.onPasteHandler, true));
    // The RAST document owns the keyboard: forward keys to the focused remote
    // window (the Xpra client's own document handlers only see its iframe).
    document.addEventListener("keydown", this.onKeyDownHandler, true);
    document.addEventListener("keyup", this.onKeyUpHandler, true);
    this.unsubscribers.push(() => document.removeEventListener("keydown", this.onKeyDownHandler, true));
    this.unsubscribers.push(() => document.removeEventListener("keyup", this.onKeyUpHandler, true));
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
    window.clearTimeout(this.reconnectTimer);
    window.clearTimeout(this.attachTimer);
    for (const unsubscribe of this.unsubscribers.splice(0)) {
      try {
        unsubscribe();
      } catch {
        /* ignore */
      }
    }
    this.closeAllWindows(true);
    this.closeWholeSession(true);
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
    const sameStatus = previous?.status === session?.status;
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
      const wasAlive = this.frames.size > 0 || this.starting.size > 0 || this.wholeSession !== null;
      this.closeAllWindows(false);
      this.closeWholeSession(false);
      this.removeIframe();
      this.shim = null;
      this.lastSnapshot = null;
      if (session?.status === "ended" && (wasAlive || previous?.status === "ready")) this.announceEnded(session);
      return;
    }
    if (session.url && !this.iframe) this.createIframe(session.url);
    if (this.iframe && !sameStatus) this.attachShim(0);
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
    // The Xpra client must stay same-origin and unsandboxed: the RAST desktop
    // adopts the remote windows' DOM nodes and forwards input.
    iframe.setAttribute("allow", "clipboard-read; clipboard-write");
    iframe.addEventListener("load", () => {
      this.attachShim(0);
    });
    iframe.src = url;
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
    this.quiet = true;
    this.closeAllWindows(true);
    this.quiet = false;
    shim.on("state", (state?: NativeShimState) => {
      if (state) this.applySnapshot(state);
    });
    shim.on("windows", (state?: NativeShimState) => {
      if (state) this.applySnapshot(state);
    });
    shim.on("connected", (state?: NativeShimState) => {
      window.clearTimeout(this.reconnectTimer);
      this.reconnectTimer = 0;
      if (state) this.applySnapshot(state);
    });
    shim.on("disconnected", () => this.scheduleReconnect());
    shim.on("error", () => {
      /* surfaced through the session state */
    });
    shim.on("clipboard", (payload?: { text?: string }) => {
      const text = typeof payload?.text === "string" ? payload.text : "";
      this.lastRemoteClipboard = text;
      if (!text || !navigator.clipboard?.writeText) return;
      navigator.clipboard.writeText(text).catch(() => undefined);
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
    if (snapshot.separate === false) {
      this.openWholeSession();
      return;
    }
    if (this.wholeSession) this.closeWholeSession(false);
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
      const parent = window.transientFor ? this.frames.get(window.transientFor) : undefined;
      if (window.transientFor && !parent) {
        deferred.push(window);
        continue;
      }
      this.createFrame(window, parent ?? null);
    }
    for (const window of deferred) {
      if (this.frames.has(window.wid)) continue;
      const parent = window.transientFor ? this.frames.get(window.transientFor) : undefined;
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

  private resolveSlug(window: NativeShimWindow): string {
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

  private createFrame(info: NativeShimWindow, parent: FrameState | null): void {
    const slug = this.resolveSlug(info);
    const app = linuxAppById(slug);
    const title = info.title?.trim() || app?.name || "Window";
    const popup = isPopupWindow(info);
    const dialog = Boolean(parent) && !popup;
    const appId = nativeWindowAppId(slug, info.wid);
    const width = clamp(info.width, MIN_W, Math.min(MAX_W, globalThis.innerWidth - 8));
    const height = clamp(info.height, MIN_H, Math.min(MAX_H, globalThis.innerHeight - 8));
    const area = globalThis.innerWidth;

    let initialRect: WindowRect | undefined;
    let dialogOffset = { x: 0, y: 0 };
    if (parent?.handle) {
      const parentRect = parent.handle.getRect();
      if (popup) {
        dialogOffset = {
          x: info.x - parent.remoteX,
          y: info.y - parent.remoteY,
        };
        initialRect = {
          x: Math.max(0, parentRect.x + dialogOffset.x),
          y: Math.max(0, parentRect.y + dialogOffset.y),
          w: width,
          h: height,
        };
      } else {
        dialogOffset = { x: 40, y: 40 };
        initialRect = {
          x: Math.min(Math.max(8, parentRect.x + 40), Math.max(8, area - width - 8)),
          y: Math.min(Math.max(8, parentRect.y + 40), Math.max(8, globalThis.innerHeight - height - 8)),
          w: width,
          h: height,
        };
      }
    }

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
          taskbar: !dialog && !popup,
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
      }, retryMs);
    }
    if (!frame.popup) shim?.focusWindow(frame.wid);
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
    frame.last.maximized = window.maximized;
  }

  private syncRemoteFocus(snapshot: NativeShimState): void {
    const focused = snapshot.windows.find((window) => window.focused);
    if (!focused) return;
    const frame = this.frames.get(focused.wid);
    if (!frame?.handle || frame.closing) return;
    // Popup/menu windows must not take the RAST focus away from the app.
    const target = frame.popup ? this.frames.get(frame.dialogParent ?? "") : frame;
    if (!target?.handle) return;
    if (!target.handle.isMinimized()) target.handle.focus();
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
      if (info.focused) this.focusedWid = frame.wid;
      else if (this.focusedWid === frame.wid) this.focusedWid = null;
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
      if (ownerInfo.minimized || frame.autoMinimized || frame.popup) continue;
      const dx = ownerInfo.rect.x + frame.dialogOffset.x;
      const dy = ownerInfo.rect.y + frame.dialogOffset.y;
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
    if (this.wholeSession) {
      this.wholeSession.handle?.focus();
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
    } catch (error) {
      this.closeStarting(app.id);
      if (isHttpStatus(error, 404) || isHttpStatus(error, 405) || isHttpStatus(error, 501)) {
        // Partial backend: keep the old Docker-backed behaviour.
        this.nativeUnavailable = true;
        this.fallbackToLegacy(app);
        return;
      }
      this.deps.toast.error(errorMessage(error));
    }
  }

  private fallbackToLegacy(app: LinuxApp): void {
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

  /* ---------------------------------------------------------- whole session */

  /**
   * Fallback for a session that is not seamless (desktop/shadow Xpra mode):
   * show the one whole-desktop stream in a single RAST window.
   */
  private openWholeSession(): void {
    if (this.wholeSession) return;
    for (const frame of [...this.frames.values()]) this.destroyFrame(frame, true);
    const state: WholeSessionWindow = { handle: null, separate: false };
    this.wholeSession = state;
    state.handle = this.deps.wm.open(
      {
        id: "linuxnative-session",
        name: "Native desktop",
        icon: "box",
        defaultSize: { w: 1100, h: 720 },
        minSize: { w: 480, h: 320 },
      },
      undefined,
      (ctx) => {
        const iframe = this.iframe;
        if (!iframe) return;
        iframe.classList.remove("linux-native-session-frame");
        iframe.style.cssText = "position:absolute;inset:0;width:100%;height:100%;border:0;background:#000;";
        ctx.contentEl.appendChild(iframe);
        ctx.onDispose(() => {
          const whole = this.wholeSession;
          this.wholeSession = null;
          if (iframe.parentElement) {
            iframe.classList.add("linux-native-session-frame");
            iframe.style.cssText = "";
            this.deps.host.appendChild(iframe);
          }
          if (whole && !this.quiet && !this.disposed) void this.endSession();
        });
      },
      { taskbar: true },
    );
  }

  private closeWholeSession(quiet: boolean): void {
    const whole = this.wholeSession;
    if (!whole) return;
    this.wholeSession = null;
    if (whole.handle && !whole.handle.isClosed()) {
      const previous = this.quiet;
      this.quiet = quiet;
      whole.handle.close();
      this.quiet = previous;
    }
  }

  /* -------------------------------------------------------------- clipboard */

  private forwardPaste(event: ClipboardEvent): void {
    const target = event.target;
    if (!(target instanceof Node)) return;
    const container = target instanceof Element ? target.closest(".linux-native-window") : null;
    if (!container) return;
    const shim = this.shim;
    if (!shim) return;
    const text = event.clipboardData?.getData("text/plain") ?? "";
    if (!text) return;
    event.preventDefault();
    event.stopPropagation();
    shim.sendClipboardText(text);
  }

  /* ----------------------------------------------------------------- keyboard */

  private forwardKey(event: KeyboardEvent, pressed: boolean): void {
    if (this.disposed) return;
    const shim = this.shim;
    if (!shim) return;
    const wid = this.focusedWid;
    if (!wid) return;
    if (document.querySelector(".modal-backdrop")) return;
    const target = event.target;
    if (target instanceof HTMLElement) {
      if (target.isContentEditable) return;
      const tag = target.tagName;
      if (tag === "INPUT" || tag === "TEXTAREA" || tag === "SELECT") return;
    }
    // Keep browser/reload/devtools shortcuts working.
    if (event.key === "F5" || event.key === "F11" || event.key === "F12") return;
    if (event.ctrlKey && event.shiftKey && ["I", "J", "C", "K"].includes(event.key)) return;
    if (event.metaKey) return;
    const forwarded = shim.sendKey(pressed, {
      key: event.key,
      code: event.code,
      keyCode: event.keyCode || event.which || 0,
      ctrlKey: event.ctrlKey,
      shiftKey: event.shiftKey,
      altKey: event.altKey,
      metaKey: event.metaKey,
      repeat: event.repeat,
    });
    if (forwarded) {
      event.preventDefault();
      event.stopPropagation();
    }
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
