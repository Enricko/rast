// Desktop session: the account's open windows, saved on the server.
//
// Windows keep their app, place, size, state (minimized, maximized, snapped),
// stacking order and where the app was (`params`, reported by the app with
// `ctx.saveState`). The desktop saves the session shortly after every change
// and right away when the page is hidden or closed, and opens the same
// windows when it starts — after a reload, in another tab or on another
// device. Phones show every window full screen but save the desktop places.

import type { ApiClient } from "../core/api";
import type { WindowPlacement, WindowRect, WindowState } from "../core/wm";

export const SESSION_PATH = "/api/desktop/session";
/** Windows saved and restored (the most recent ones when there are more). */
export const MAX_SESSION_WINDOWS = 12;
const SAVE_DELAY_MS = 700;

export interface SavedWindow {
  key: string;
  appId: string;
  params?: unknown;
  rect: WindowRect | null;
  state: WindowState;
  minimized: boolean;
}

export interface SavedSession {
  version: 1;
  /** Bottom of the stack first. */
  windows: SavedWindow[];
  /** Key of the focused window. */
  focused: string | null;
  stage: { w: number; h: number } | null;
}

export function placementOf(window: SavedWindow): WindowPlacement {
  return { rect: window.rect, state: window.state, minimized: window.minimized };
}

/** Load the account's saved session (`null` when none or unreadable). */
export async function loadSession(api: ApiClient): Promise<SavedSession | null> {
  const response = await api.get<{ session: SavedSession | null }>(SESSION_PATH);
  const session = response?.session;
  if (!session || session.version !== 1 || !Array.isArray(session.windows)) return null;
  return session;
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

export interface SessionSaver {
  /** Save soon (changes in the meantime are folded in). */
  schedule(): void;
  /** Save now if anything changed. */
  flush(keepalive?: boolean): Promise<void>;
  /** Save nothing more (before the desktop closes its windows). */
  stop(): void;
}

export function createSessionSaver(api: ApiClient, capture: () => SavedSession): SessionSaver {
  let timer: number | undefined;
  let stopped = false;
  /** What the server has; what is on its way there. */
  let confirmed: string | null = null;
  let inFlight: string | null = null;
  let queue: Promise<void> = Promise.resolve();

  const send = async (text: string, session: SavedSession, keepalive: boolean): Promise<void> => {
    inFlight = text;
    try {
      await api.request("PUT", SESSION_PATH, session, { keepalive });
      confirmed = text;
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

  function flush(keepalive = false): Promise<void> {
    if (keepalive) {
      // The page is going away: send now, without waiting for a save that
      // is still on its way (it may never finish).
      const next = take();
      return next ? send(next.text, next.session, true) : Promise.resolve();
    }
    queue = queue.then(() => {
      const next = take();
      return next ? send(next.text, next.session, false) : undefined;
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
    schedule() {
      if (stopped) return;
      if (timer !== undefined) window.clearTimeout(timer);
      timer = window.setTimeout(() => {
        timer = undefined;
        void flush();
      }, SAVE_DELAY_MS);
    },
    flush,
    stop() {
      stopped = true;
      if (timer !== undefined) window.clearTimeout(timer);
      timer = undefined;
      document.removeEventListener("visibilitychange", onHidden);
      window.removeEventListener("pagehide", onPageHide);
    },
  };
}
