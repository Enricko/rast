// Dialogs that stay with their window (`AppContext.dialog`): kept in the
// desktop session with what was typed in them, opened again after a reload
// or on another device, and shown and closed live on the account's other
// open pages. The app names each dialog and what it is about (`params`) and
// registers how to open it again (`AppContext.onDialog`); the text typed in
// it is restored like a window's (see window-view.ts).

import type { DialogSession } from "../core/ui";
import { ViewTracker, type WindowView } from "./window-view";

export interface SavedDialog {
  name: string;
  params?: unknown;
  view?: WindowView;
}

/** Dialogs kept per window (a dialog may open another on top). */
export const MAX_DIALOGS = 4;
/**
 * How long a saved dialog waits for its app after a restore. One that
 * another page shows (live) waits until that page closes it: this page may
 * not open it (a "Revoke session" for this very device) but must not drop it
 * from the session, which would close it there.
 */
const REOPEN_MS = 15_000;
/** How long the next saved dialog waits for the one before to open. */
const REOPEN_STEP_MS = 5_000;
const MAX_PARAMS_CHARS = 4_000;

function plainParams(params: unknown): unknown {
  if (params === undefined || params === null) return undefined;
  try {
    const text = JSON.stringify(params);
    if (text === undefined || text.length > MAX_PARAMS_CHARS) return undefined;
    return JSON.parse(text) as unknown;
  } catch {
    return undefined;
  }
}

function dialogKey(name: string, params: unknown): string {
  return `${name}\u0000${JSON.stringify(params ?? null)}`;
}

/** The dialogs of a saved window that this version can use. */
export function readableDialogs(value: unknown): SavedDialog[] {
  if (!Array.isArray(value)) return [];
  return value
    .filter(
      (d): d is SavedDialog =>
        Boolean(d) && typeof (d as SavedDialog).name === "string" && /^[A-Za-z0-9_-]{1,64}$/.test((d as SavedDialog).name),
    )
    .slice(0, MAX_DIALOGS);
}

interface OpenDialog {
  key: string;
  name: string;
  params: unknown;
  close: () => void;
  view: ViewTracker;
}

interface PendingDialog {
  key: string;
  name: string;
  params: unknown;
  view: WindowView | undefined;
  requested: boolean;
  expires: number;
}

export class WindowDialogs {
  private readonly open: OpenDialog[] = [];
  private readonly openers = new Map<string, (params: unknown) => void | Promise<unknown>>();
  private pending: PendingDialog[] = [];
  private ready = false;
  private reopening = false;
  private disposed = false;

  /**
   * `host`: something inside the window (its content), so that its dialogs
   * open over that window; none for the desktop's own dialogs.
   */
  constructor(
    private readonly onChange: () => void,
    private readonly host?: HTMLElement,
  ) {}

  /** The `session` for a dialog `name` about `params` (AppContext.dialog). */
  session(name: string, params?: unknown): DialogSession {
    let stored = plainParams(params);
    let key = dialogKey(name, stored);
    let entry: OpenDialog | null = null;
    return {
      ...(this.host ? { host: this.host } : {}),
      update: (next) => {
        stored = plainParams(next);
        key = dialogKey(name, stored);
        if (!entry) return;
        entry.params = stored;
        entry.key = key;
        if (!this.disposed) this.onChange();
      },
      opened: (el, close) => {
        if (this.disposed || entry) return;
        const view = new ViewTracker(el, this.onChange);
        // Opened again for a saved dialog: what was typed in it comes back.
        const index = this.pending.findIndex((p) => p.key === key);
        if (index >= 0) {
          const [restored] = this.pending.splice(index, 1);
          if (restored?.view) view.apply(restored.view, false);
        }
        entry = { key, name, params: stored, close, view };
        this.open.push(entry);
        this.onChange();
      },
      closed: () => {
        if (!entry) return;
        entry.view.dispose();
        const index = this.open.indexOf(entry);
        if (index >= 0) this.open.splice(index, 1);
        entry = null;
        if (!this.disposed) this.onChange();
      },
    };
  }

  /** How to open the dialog `name` again (AppContext.onDialog). */
  register(name: string, reopen: (params: unknown) => void | Promise<unknown>): () => void {
    this.openers.set(name, reopen);
    this.reopenPending();
    return () => {
      if (this.openers.get(name) === reopen) this.openers.delete(name);
    };
  }

  /** The app has started: dialogs waiting for it may open. */
  mounted(): void {
    this.ready = true;
    this.reopenPending();
  }

  /**
   * Show these dialogs (a restored window, or another page's change).
   * `live`: dialogs not listed close, and open ones get the other page's text.
   */
  apply(value: unknown, live: boolean): void {
    if (this.disposed) return;
    const wanted = readableDialogs(value).map((d) => {
      const params = plainParams(d.params);
      return { ...d, params, key: dialogKey(d.name, params) };
    });
    const keys = new Set(wanted.map((d) => d.key));
    if (live) {
      for (const dialog of [...this.open]) if (!keys.has(dialog.key)) dialog.close();
      this.pending = this.pending.filter((p) => keys.has(p.key));
    }
    for (const dialog of wanted) {
      const open = this.open.find((o) => o.key === dialog.key);
      if (open) {
        open.view.apply(dialog.view, live);
        continue;
      }
      const expires = live ? Number.POSITIVE_INFINITY : Date.now() + REOPEN_MS;
      const pending = this.pending.find((p) => p.key === dialog.key);
      if (pending) {
        pending.view = dialog.view;
        if (live) pending.expires = expires;
        continue;
      }
      this.pending.push({
        key: dialog.key,
        name: dialog.name,
        params: dialog.params,
        view: dialog.view,
        requested: false,
        expires,
      });
    }
    this.reopenPending();
  }

  /** The window's dialogs for the session (undefined: none). */
  capture(): SavedDialog[] | undefined {
    const now = Date.now();
    this.pending = this.pending.filter((p) => p.expires > now);
    const saved: SavedDialog[] = [];
    const seen = new Set<string>();
    const add = (key: string, name: string, params: unknown, view: WindowView | undefined): void => {
      if (seen.has(key)) return;
      seen.add(key);
      saved.push({ name, ...(params !== undefined ? { params } : {}), ...(view ? { view } : {}) });
    };
    for (const dialog of this.open) add(dialog.key, dialog.name, dialog.params, dialog.view.capture());
    for (const p of this.pending) add(p.key, p.name, p.params, p.view);
    return saved.length ? saved.slice(0, MAX_DIALOGS) : undefined;
  }

  /** The window closes: so do its dialogs. */
  dispose(): void {
    if (this.disposed) return;
    this.disposed = true;
    this.pending = [];
    for (const dialog of [...this.open]) dialog.close();
  }

  /**
   * Ask the app to open waiting dialogs, in order: a dialog opened from
   * another one (a confirmation over a list) opens after it, when its opener
   * returns a promise that settles once the dialog is open.
   */
  private reopenPending(): void {
    if (!this.ready || this.disposed || this.reopening) return;
    this.reopening = true;
    void (async () => {
      try {
        for (;;) {
          if (this.disposed) return;
          const next = this.pending.find((p) => !p.requested && this.openers.has(p.name));
          if (!next) return;
          next.requested = true;
          try {
            const opening = this.openers.get(next.name)?.(next.params);
            if (opening && typeof (opening as Promise<unknown>).then === "function") {
              let timer: number | undefined;
              await Promise.race([
                Promise.resolve(opening).catch(() => undefined),
                new Promise((resolve) => (timer = window.setTimeout(resolve, REOPEN_STEP_MS))),
              ]);
              window.clearTimeout(timer);
            }
          } catch (error) {
            console.error(`could not open the ${next.name} dialog again`, error);
          }
        }
      } finally {
        this.reopening = false;
      }
    })();
  }
}
