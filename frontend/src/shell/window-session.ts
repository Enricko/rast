// Desktop session: the account's open windows, saved on the server.
//
// Windows keep their app, place, size, state (minimized, maximized, snapped),
// stacking order and where the app was (`params`, reported by the app with
// `ctx.saveState`). The desktop saves the session shortly after every change
// and right away when the page is hidden or closed, and opens the same
// windows when it starts — after a reload, in another tab or on another
// device. Phones show every window full screen but save the desktop places.
//
// Live: every save reaches the account's other open pages (`desktop.session`
// event), which show it at once. Sessions carry the server's version
// (`updatedAt`, which only grows); a page shows a session only when it is
// newer than the one it has, and never while it has a change of its own on
// the way (its save is newer and the other pages show it instead).

import type { ApiClient } from "../core/api";
import type { WindowPlacement, WindowRect, WindowState } from "../core/wm";
import type { SavedDialog } from "./window-dialogs";
import type { WindowView } from "./window-view";

export const SESSION_PATH = "/api/desktop/session";
/** Header naming the page that saved, echoed in the realtime event. */
export const ORIGIN_HEADER = "X-RAST-Desktop";
/** Sessions larger than this are not sent with `keepalive` (browsers refuse). */
const KEEPALIVE_MAX_CHARS = 60_000;
/** Windows saved and restored (the most recent ones when there are more). */
export const MAX_SESSION_WINDOWS = 12;
const SAVE_DELAY_MS = 700;
/** While a window is being dragged, look again for a session to show. */
const BUSY_RETRY_MS = 1_000;

export interface SavedWindow {
  key: string;
  appId: string;
  params?: unknown;
  rect: WindowRect | null;
  state: WindowState;
  minimized: boolean;
  /** Scroll positions and typed text inside the window. */
  view?: WindowView;
  /** Dialogs open in the window, with what was typed in them. */
  dialogs?: SavedDialog[];
}

export interface SavedSession {
  version: 1;
  /** Bottom of the stack first. */
  windows: SavedWindow[];
  /** Key of the focused window. */
  focused: string | null;
  stage: { w: number; h: number } | null;
}

/** A saved session and its version, from a load or the realtime event. */
export interface SessionUpdate {
  session: SavedSession | null;
  updatedAt: number | null;
  /** The page that saved it (realtime event only). */
  origin?: string | null;
}

export function placementOf(window: SavedWindow): WindowPlacement {
  return { rect: window.rect, state: window.state, minimized: window.minimized };
}

function readableSession(value: unknown): SavedSession | null {
  const session = value as SavedSession | null | undefined;
  if (!session || session.version !== 1 || !Array.isArray(session.windows)) return null;
  return session;
}

/** Load the account's saved session (`session: null` when none or unreadable). */
export async function loadSession(api: ApiClient): Promise<SessionUpdate> {
  const response = await api.get<{ session: SavedSession | null; updatedAt: number | null }>(SESSION_PATH);
  const updatedAt = typeof response?.updatedAt === "number" ? response.updatedAt : null;
  return { session: readableSession(response?.session), updatedAt };
}

export function newWindowKey(): string {
  const random =
    typeof crypto !== "undefined" && "randomUUID" in crypto
      ? crypto.randomUUID().replace(/-/g, "").slice(0, 16)
      : Math.random().toString(36).slice(2, 18);
  return `w-${random}`;
}

/** JSON-safe copy of an app's state, or `undefined` when it cannot be saved. */
export function storableParams(params: unknown): unknown {
  if (params === undefined || params === null) return undefined;
  try {
    const text = JSON.stringify(params);
    if (text === undefined || text.length > 4000) return undefined;
    return JSON.parse(text) as unknown;
  } catch {
    return undefined;
  }
}

export interface SessionSyncOptions {
  /** Show a session another page saved (open, move and close windows). */
  apply?: (session: SavedSession) => void;
  /** A window is being dragged or resized: show other pages' changes after. */
  busy?: () => boolean;
}

export interface SessionSaver {
  /** Random id of this page, sent with every save. */
  readonly pageId: string;
  /** Version of the newest session this page has shown or saved. */
  readonly version: number;
  /** Save soon (changes in the meantime are folded in). */
  schedule(): void;
  /** Save now if anything changed. */
  flush(keepalive?: boolean): Promise<void>;
  /**
   * The desktop shows its starting windows: the session of `version` when
   * `saved` (nothing to save until they change), otherwise windows the
   * server does not have yet. Sessions from other pages are shown from now.
   */
  started(version: number | null, saved: boolean): void;
  /** A session a page saved (realtime event, or a load after a gap). */
  receive(update: SessionUpdate): void;
  /** Save and show nothing more (before the desktop closes its windows). */
  stop(): void;
}

export function createSessionSaver(
  api: ApiClient,
  capture: () => SavedSession,
  options: SessionSyncOptions = {},
): SessionSaver {
  const pageId = newWindowKey().replace(/^w-/, "p-");
  let timer: number | undefined;
  let retry: number | undefined;
  let stopped = false;
  let ready = false;
  let applying = false;
  /** What the server has; what is on its way there. */
  let confirmed: string | null = null;
  let inFlight: string | null = null;
  let queue: Promise<void> = Promise.resolve();
  let version = 0;
  /** The newest session from another page, waiting for this page's own save. */
  let pending: { session: SavedSession; version: number } | null = null;

  const dirty = (): boolean => timer !== undefined || inFlight !== null;

  const send = async (text: string, session: SavedSession, keepalive: boolean): Promise<void> => {
    inFlight = text;
    try {
      const response = await api.request<{ updatedAt?: unknown } | undefined>("PUT", SESSION_PATH, session, {
        keepalive: keepalive && text.length <= KEEPALIVE_MAX_CHARS,
        headers: { [ORIGIN_HEADER]: pageId },
      });
      confirmed = text;
      const saved = response?.updatedAt;
      if (typeof saved === "number" && Number.isFinite(saved)) version = Math.max(version, saved);
    } catch {
      // Offline or signed out: the next change tries again.
    } finally {
      if (inFlight === text) inFlight = null;
    }
  };

  const take = (): { text: string; session: SavedSession } | null => {
    if (stopped) return null;
    if (timer !== undefined) {
      window.clearTimeout(timer);
      timer = undefined;
    }
    const session = capture();
    const text = JSON.stringify(session);
    return text === confirmed || text === inFlight ? null : { text, session };
  };

  const show = (session: SavedSession, shownVersion: number): void => {
    applying = true;
    try {
      options.apply?.(session);
    } catch (error) {
      console.error("could not show the desktop session", error);
    } finally {
      applying = false;
    }
    version = Math.max(version, shownVersion);
    // What the page shows now is what the server has: nothing to save back.
    confirmed = JSON.stringify(capture());
  };

  /** Show the waiting session once this page has nothing of its own on the way. */
  const settle = (): void => {
    if (stopped || !ready || !pending) return;
    if (pending.version <= version) {
      pending = null; // this page saved after it: the server has this page's
      return;
    }
    if (dirty()) return; // the save (or its answer) settles again
    if (options.busy?.()) {
      if (retry === undefined) {
        retry = window.setTimeout(() => {
          retry = undefined;
          settle();
        }, BUSY_RETRY_MS);
      }
      return;
    }
    const next = pending;
    pending = null;
    show(next.session, next.version);
  };

  function flush(keepalive = false): Promise<void> {
    if (keepalive) {
      // The page is going away: send now, without waiting for a save that
      // is still on its way (it may never finish).
      const next = take();
      return next ? send(next.text, next.session, true) : Promise.resolve();
    }
    queue = queue.then(async () => {
      const next = take();
      if (next) await send(next.text, next.session, false);
      settle();
    });
    return queue;
  }

  const onHidden = (): void => {
    if (document.visibilityState === "hidden") void flush(true);
  };
  const onPageHide = (): void => void flush(true);
  document.addEventListener("visibilitychange", onHidden);
  window.addEventListener("pagehide", onPageHide);

  return {
    pageId,
    get version() {
      return version;
    },
    schedule() {
      // Showing another page's session is not a change of this page's.
      if (stopped || applying) return;
      if (timer !== undefined) window.clearTimeout(timer);
      timer = window.setTimeout(() => {
        timer = undefined;
        void flush();
      }, SAVE_DELAY_MS);
    },
    flush,
    started(startVersion, saved) {
      if (stopped) return;
      ready = true;
      if (typeof startVersion === "number" && Number.isFinite(startVersion)) {
        version = Math.max(version, startVersion);
      }
      if (saved) confirmed = JSON.stringify(capture());
      settle();
    },
    receive(update) {
      if (stopped) return;
      const updateVersion = update?.updatedAt;
      if (typeof updateVersion !== "number" || !Number.isFinite(updateVersion)) return;
      if (update.origin === pageId) {
        version = Math.max(version, updateVersion);
        return;
      }
      if (updateVersion <= version) return;
      const session = readableSession(update.session);
      if (!session) return;
      if (!pending || updateVersion > pending.version) pending = { session, version: updateVersion };
      settle();
    },
    stop() {
      stopped = true;
      pending = null;
      if (timer !== undefined) window.clearTimeout(timer);
      if (retry !== undefined) window.clearTimeout(retry);
      timer = undefined;
      retry = undefined;
      document.removeEventListener("visibilitychange", onHidden);
      window.removeEventListener("pagehide", onPageHide);
    },
  };
}
