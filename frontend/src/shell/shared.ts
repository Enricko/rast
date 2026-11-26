// Types shared by the shell modules (boot, auth screens, desktop).

import type { ApiClient } from "../core/api";
import type { MeResponse } from "../core/types";
import type { EventStream } from "../core/sse";
import type { Store } from "../core/store";
import type { SavedSession } from "./window-session";

export interface ShellDeps {
  api: ApiClient;
  store: Store;
  events: EventStream;
  root: HTMLElement;
}

/** The windows of a desktop whose session expired, to reopen after sign-in. */
export interface SessionSnapshot {
  userId: number;
  session: SavedSession;
}

export interface DesktopController {
  snapshot(): SessionSnapshot;
  dispose(): void;
}

export interface DesktopOptions {
  /** Applications to reopen after re-authentication. */
  restore?: SessionSnapshot | null;
  /** Called after the user signs out (desktop is already disposed). */
  onSignedOut(notice: string): void;
  /**
   * The server revoked this session (expired, revoked elsewhere, disabled or
   * role change). The desktop is already disposed; show the login screen.
   */
  onSessionRevoked?(reason: string): void;
}

export interface AuthOptions {
  /** Optional notice shown above the login form (e.g. session expired). */
  notice?: string;
  onAuthenticated(me: MeResponse): void;
}
