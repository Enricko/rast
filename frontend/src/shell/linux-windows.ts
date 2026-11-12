// Separate windows for Linux apps.
//
// An app that can stream each of its own windows (the base image runs a VNC
// server per app window) is shown as one RAST desktop window per app window:
// the RAST desktop is the window manager, each window has its own taskbar
// entry, and closing a RAST window closes that window inside the app. Apps
// whose image does not provide this fall back to the single whole-desktop
// window (`openLegacy`).

import type RFB from "@novnc/novnc";
import type { AppDefinition } from "../core/app";
import type { ApiClient } from "../core/api";
import { h } from "../core/dom";
import type { ToastCenter } from "../core/toast";
import { errorMessage } from "../core/ui";
import type { WindowHandle, WindowManager } from "../core/wm";
import type { LinuxSession } from "../apps/linuxapp/catalog";

/** One window of a running app, as the app container reports it. */
export interface AppWindow {
  id: string;
  title: string;
  x: number;
  y: number;
  width: number;
  height: number;
  state: "normal" | "maximized" | "minimized";
  type: string;
  transient: boolean;
  streaming: boolean;
}

interface WindowsResponse {
  windows: AppWindow[];
}

interface StreamResponse {
  url: string;
  /** VNC password of this window's stream (the stream is not open). */
  password: string;
}

interface OpenWindow {
  handle: WindowHandle;
  title: string;
}

interface AppState {
  app: AppDefinition;
  /** The app id without the `linux:` prefix. */
  slug: string;
  sessionId: string;
  separate: boolean;
  windows: Map<string, OpenWindow>;
  pending: Set<string>;
  timer: number;
  busy: boolean;
  disposed: boolean;
}

export interface LinuxWindowsDeps {
  api: ApiClient;
  toast: ToastCenter;
  wm: WindowManager;
  /** Show the app the old way: one window with the whole streamed desktop. */
  openLegacy: (app: AppDefinition) => void;
}

const POLL_MS = 1000;
const START_TIMEOUT_MS = 30_000;
const MIN_W = 420;
const MAX_W = 1600;
const MIN_H = 280;
const MAX_H = 1200;

const sleep = (ms: number): Promise<void> =>
  new Promise((resolve) => window.setTimeout(resolve, ms));

const isNotFound = (error: unknown): boolean =>
  (error as { status?: number })?.status === 404;

function clamp(value: number, min: number, max: number): number {
  if (!Number.isFinite(value) || value <= 0) return min;
  return Math.min(max, Math.max(min, Math.round(value)));
}

export class LinuxWindows {
  private readonly deps: LinuxWindowsDeps;
  private readonly apps = new Map<string, AppState>();

  constructor(deps: LinuxWindowsDeps) {
    this.deps = deps;
  }

  /** The app's icon or taskbar entry was used. */
  open(app: AppDefinition): void {
    const existing = this.apps.get(app.id);
    if (existing) {
      const first = existing.windows.values().next().value as OpenWindow | undefined;
      if (first) this.deps.wm.focus(first.handle.id);
      return;
    }
    void this.start(app);
  }

  /** Close every RAST window of every app and stop polling (desktop teardown). */
  dispose(): void {
    for (const appId of [...this.apps.keys()]) this.teardown(appId);
  }

  private async start(app: AppDefinition): Promise<void> {
    const slug = app.id.slice("linux:".length);
    const state: AppState = {
      app,
      slug,
      sessionId: "",
      separate: false,
      windows: new Map(),
      pending: new Set(),
      timer: 0,
      busy: false,
      disposed: false,
    };
    this.apps.set(app.id, state);
    this.deps.toast.info(`Starting ${app.name}…`);
    try {
      const session = await this.deps.api.post<LinuxSession>(
        `/api/linux-apps/${encodeURIComponent(slug)}/open`,
      );
      if (state.disposed) return;
      state.sessionId = session.id;
      const ready = await this.waitUntilReady(state, session);
      if (!ready) return;
      if (!state.separate) {
        // The image only streams the whole desktop: use the old window.
        this.apps.delete(app.id);
        this.deps.openLegacy(app);
        return;
      }
      await this.sync(state);
      if (state.disposed) return;
      state.timer = window.setInterval(() => void this.sync(state), POLL_MS);
    } catch (error) {
      this.teardown(app.id);
      this.deps.toast.error(errorMessage(error));
    }
  }

  private async waitUntilReady(
    state: AppState,
    first: LinuxSession,
  ): Promise<boolean> {
    let session = first;
    const deadline = Date.now() + START_TIMEOUT_MS;
    while (!state.disposed) {
      if (session.status === "ready") {
        state.separate = session.separateWindows === true;
        return true;
      }
      if (session.status === "ended") {
        this.fail(state, session);
        return false;
      }
      if (Date.now() > deadline) {
        this.teardown(state.app.id);
        this.deps.toast.error(`${state.app.name} did not start in time`);
        return false;
      }
      await sleep(500);
      session = await this.deps.api.get<LinuxSession>(
        `/api/linux-apps/sessions/${encodeURIComponent(state.sessionId)}`,
      );
    }
    return false;
  }

  /** One refresh: session state, then the app's window list. */
  private async sync(state: AppState): Promise<void> {
    if (state.disposed || state.busy) return;
    state.busy = true;
    try {
      const session = await this.deps.api.get<LinuxSession>(
        `/api/linux-apps/sessions/${encodeURIComponent(state.sessionId)}`,
      );
      if (state.disposed) return;
      if (session.status === "ended") {
        this.fail(state, session);
        return;
      }
      if (session.status !== "ready") return;
      const data = await this.deps.api.get<WindowsResponse>(
        `/api/linux-apps/sessions/${encodeURIComponent(state.sessionId)}/windows`,
      );
      if (state.disposed) return;
      this.applyWindows(state, data.windows ?? []);
    } catch (error) {
      if (state.disposed) return;
      if (isNotFound(error)) {
        this.fail(state);
        return;
      }
      // Keep trying: a window may be starting or the app may be restarting.
      console.warn("linux windows: could not refresh", error);
    } finally {
      state.busy = false;
    }
  }

  private applyWindows(state: AppState, windows: AppWindow[]): void {
    const seen = new Set<string>();
    for (const info of windows) {
      if (info.type === "dock") continue;
      // A minimized window stays out of the desktop until it is restored.
      if (info.state === "minimized") continue;
      seen.add(info.id);
      const title = info.title?.trim() || state.app.name;
      const open = state.windows.get(info.id);
      if (open) {
        if (open.title !== title) {
          open.title = title;
          open.handle.setTitle(title);
        }
        continue;
      }
      if (state.pending.has(info.id)) continue;
      void this.openWindow(state, info, title);
    }
    for (const [id, open] of [...state.windows]) {
      if (seen.has(id)) continue;
      // The window is gone from the app: close its RAST window quietly.
      state.windows.delete(id);
      open.handle.close();
    }
  }

  private async openWindow(state: AppState, info: AppWindow, title: string): Promise<void> {
    state.pending.add(info.id);
    try {
      const stream = await this.deps.api.post<StreamResponse>(
        `/api/linux-apps/sessions/${encodeURIComponent(state.sessionId)}/windows/${encodeURIComponent(info.id)}/stream`,
      );
      if (state.disposed || state.windows.has(info.id)) return;
      const size = {
        w: clamp(info.width, MIN_W, MAX_W),
        h: clamp(info.height, MIN_H, MAX_H),
      };
      const handle = this.deps.wm.open(
        {
          id: `${state.app.id}:${info.id}`,
          name: title,
          icon: state.app.icon,
          defaultSize: size,
          minSize: { w: 320, h: 220 },
        },
        undefined,
        (ctx) => this.mountWindow(state, info.id, stream.url, stream.password, ctx),
      );
      state.windows.set(info.id, { handle, title });
    } catch (error) {
      if (!state.disposed) {
        console.warn("linux windows: could not show a window", error);
        this.deps.toast.warning(
          `A ${state.app.name} window could not be shown: ${errorMessage(error)}`,
        );
      }
    } finally {
      state.pending.delete(info.id);
    }
  }

  private async mountWindow(
    state: AppState,
    windowId: string,
    path: string,
    password: string,
    ctx: {
      contentEl: HTMLElement;
      window: WindowHandle;
      onDispose: (callback: () => void) => () => void;
    },
  ): Promise<void> {
    const host = h("div", { class: "linux-window-stream", tabIndex: 0 });
    ctx.contentEl.appendChild(host);
    const scheme = location.protocol === "https:" ? "wss" : "ws";
    const url = `${scheme}://${location.host}${path}`;
    let rfb: RFB | null = null;
    let disposed = false;
    ctx.onDispose(() => {
      disposed = true;
      try {
        rfb?.disconnect();
      } catch {
        /* already disconnected */
      }
    });
    try {
      // The viewer is only loaded when an app actually uses separate windows.
      const { default: Client } = await import("@novnc/novnc");
      if (disposed || ctx.window.isClosed()) return;
      rfb = new Client(host, url, { shared: true, credentials: { password } });
      rfb.scaleViewport = true;
      rfb.resizeSession = false;
      rfb.focusOnClick = true;
      rfb.clipViewport = false;
      rfb.background = "#000";
      rfb.addEventListener("connect", () => host.focus());
    } catch (error) {
      console.warn("linux windows: could not open the stream", error);
      host.replaceChildren(
        h("div", { class: "linux-window-stream-problem" }, "This window could not be shown."),
      );
    }
    // Closing a RAST window closes the window inside the app, unless the app's
    // window disappeared by itself and the manager is closing ours.
    ctx.onDispose(() => {
      const open = state.windows.get(windowId);
      if (!open || open.handle.id !== ctx.window.id) return;
      state.windows.delete(windowId);
      void this.deps.api
        .post(
          `/api/linux-apps/sessions/${encodeURIComponent(state.sessionId)}/windows/${encodeURIComponent(windowId)}/close`,
        )
        .catch(() => undefined);
    });
  }

  private fail(state: AppState, session?: LinuxSession): void {
    const wasClosed = session?.reason === "exited";
    this.teardown(state.app.id);
    if (wasClosed) this.deps.toast.info(`${state.app.name} was closed`);
  }

  private teardown(appId: string): void {
    const state = this.apps.get(appId);
    if (!state) return;
    state.disposed = true;
    this.apps.delete(appId);
    if (state.timer) window.clearInterval(state.timer);
    for (const open of state.windows.values()) open.handle.close();
    state.windows.clear();
  }
}
