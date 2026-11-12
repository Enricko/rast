// Contracts between the desktop shell and the individual applications.

import type { ApiClient } from "./api";
import type { EventStream } from "./sse";
import type { Store } from "./store";
import type { ToastCenter } from "./toast";
import type { WindowHandle } from "./wm";

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
  /**
   * No desktop title bar: the app draws its own window controls (streamed
   * Linux app windows do). The taskbar still minimizes, maximizes and closes
   * the window; it cannot be dragged, and the edges resize it.
   */
  frameless?: boolean;
  mount(ctx: AppContext): AppInstance | void | Promise<AppInstance | void>;
}
