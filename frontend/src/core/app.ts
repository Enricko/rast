// Contracts between the desktop shell and the individual applications.

import type { ApiClient } from "./api";
import type { EventStream } from "./sse";
import type { Store } from "./store";
import type { ToastCenter } from "./toast";
import type { WindowHandle } from "./wm";

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
  /** Wrap a promise with a busy overlay on the window content. */
  withBusy<T>(promise: Promise<T>, label?: string): Promise<T>;
  onResize(callback: () => void): () => void;
  onDispose(callback: () => void): () => void;
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
  mount(ctx: AppContext): AppInstance | void | Promise<AppInstance | void>;
}
