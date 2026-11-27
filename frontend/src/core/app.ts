// Contracts between the desktop shell and the individual applications.

import type { ApiClient } from "./api";
import type { EventStream } from "./sse";
import type { Store } from "./store";
import type { ToastCenter } from "./toast";
import type { WindowHandle } from "./wm";
import type { DialogSession } from "./ui";

/** Drag-and-drop type carried by app tiles (Applications folder, desktop icons). */
export const APP_DRAG_TYPE = "application/x-rast-app";

/** Desktop and taskbar shortcuts an app may change (the Applications folder). */
export interface DesktopActions {
  /** Every app this account may open now (installed and allowed), in order. */
  apps(): AppDefinition[];
  isOnDesktop(appId: string): boolean;
  /** Put the app's icon in the first free desktop cell. */
  addToDesktop(appId: string): void;
  removeFromDesktop(appId: string): void;
  isPinned(appId: string): boolean;
  pin(appId: string): void;
  unpin(appId: string): void;
}

export interface AppContext {
  api: ApiClient;
  store: Store;
  events: EventStream;
  toast: ToastCenter;
  window: WindowHandle;
  /** Content root inside the window (already scrollable). */
  root: HTMLElement;
  /** Parameters passed by the launcher (e.g. a path to open). */
  params: unknown;
  setTitle(title: string): void;
  refreshUser(): Promise<void>;
  openApp(id: string, params?: unknown): void;
  desktop: DesktopActions;
  /** Wrap a promise with a busy overlay on the window content. */
  withBusy<T>(promise: Promise<T>, label?: string): Promise<T>;
  onResize(callback: () => void): () => void;
  onDispose(callback: () => void): () => void;
  /** A singleton window was opened again with new parameters (e.g. a page to show). */
  onReopen(callback: (params: unknown) => void): void;
  /**
   * Where the app is now (e.g. `{ path }` in File Station), in the shape its
   * `params` accept: the desktop saves it with the window and passes it back
   * as `params` when the window opens again (reload, another device).
   */
  saveState(state: unknown): void;
  /**
   * Another of the account's pages (a tab, another device) moved this
   * window's app elsewhere: show `params` (the shape `saveState` saves).
   */
  onPlace(callback: (params: unknown) => void): void;
  /**
   * A dialog that stays with the window: pass the result as `session` to
   * `modal()`, `promptDialog()` or `confirmDialog()`. A kept confirmation
   * never acts by itself: answering it on one page closes it (as cancelled)
   * on the others, and its `onDialog` handler should open it again only
   * while what it is about still exists. After a reload, in another tab or on
   * another device the dialog opens again, with what was typed in it, by
   * the handler registered with `onDialog(name, …)` called with `params`
   * (plain JSON naming what the dialog is about, e.g. `{ path }`). The
   * account's other open pages show it too, and close it when it closes.
   */
  dialog(name: string, params?: unknown): DialogSession;
  /**
   * Open the dialog `name` again for `params` (see `dialog`). Saved dialogs
   * open in the order they were opened; `reopen` may return a promise that
   * settles once its dialog is open (not when it closes) so that a dialog
   * opened over it comes after. Returns a function that stops it (e.g. when
   * a tab that opens the dialog closes).
   */
  onDialog(name: string, reopen: (params: unknown) => void | Promise<unknown>): () => void;
}

export interface AppInstance {
  destroy?(): void;
  onResize?(): void;
}

export interface AppDefinition {
  id: string;
  name: string;
  icon: string;
  description: string;
  adminOnly?: boolean;
  singleton?: boolean;
  defaultSize?: { w: number; h: number };
  minSize?: { w: number; h: number };
  /** Set for Package Center apps: the installed version. */
  packageVersion?: string;
  mount(ctx: AppContext): AppInstance | void | Promise<AppInstance | void>;
}
