// Linux apps installed through App Center (managing them is for
// administrators; every signed-in user can open the ones shared with users).
//
// The desktop loads the list once and keeps it current from the
// `linuxapps.changed` event; the registry turns each ready app into a desktop
// app with the id `linux:<id>`. App Center and the app windows read the same
// list. The server sends each account only the apps it may see.

import type { ApiClient } from "../../core/api";
import { setAppImageIcon } from "../../core/icons";

export const LINUX_PREFIX = "linux:";

export type LinuxStatus = "installing" | "ready" | "updating" | "failed";
export type LinuxFormat = "deb" | "appimage" | "tarball";
export type UpdateSource = "apt" | "github" | "zsync";

/** How an app is packaged: a Docker image (legacy) or a real install on the host. */
export type LinuxBackend = "docker" | "native";

/** Native install details reported by the server for a host-installed app. */
export interface NativeAppInfo {
  installPath: string | null;
  previousPath: string | null;
  launcher: { argv?: string[]; name?: string | null; desktopEntry?: string | null } | null;
  architecture: string | null;
  packageVersion: string | null;
  preexistingPackage: boolean;
  dependencies: string[];
  packagesAdded: string[];
  compatibility: { ok: boolean; reasons: string[] };
  installSource: { kind: "path" | "url" | string; path?: string; url?: string } | null;
  migration: {
    state: "none" | "pending" | "running" | "done" | "failed" | "rolled-back" | string;
    error: string | null;
    backup: string | null;
    migratedAt?: number | null;
    dockerVolume?: string | null;
  } | null;
  /** Newer version the app can update to from a remote source, when known. */
  updateSource?: { kind: "url" | "github" | "path" | string; url?: string; version?: string | null } | null;
}

export interface LinuxApp {
  id: string;
  desktopId: string;
  name: string;
  format: LinuxFormat;
  fileName: string;
  version: string | null;
  package: string | null;
  description: string | null;
  command: string[];
  launchGuess: boolean;
  iconUrl: string | null;
  status: LinuxStatus;
  error: string | null;
  jobId: string | null;
  update: {
    source: UpdateSource | null;
    latestVersion: string | null;
    available: boolean;
    checkedAt: number | null;
    error: string | null;
  };
  memoryMb: number;
  cpus: number;
  sizeBytes: number | null;
  /** Who can see and open the app: every signed-in user, or administrators. */
  visibility: "users" | "admins";
  running: number;
  createdAt: number;
  updatedAt: number;
  /** Missing on older servers; treat as "docker". */
  backend?: LinuxBackend;
  native?: NativeAppInfo | null;
}

export interface LinuxSession {
  id: string;
  appId: string;
  appName: string;
  status: "starting" | "ready" | "ended";
  url: string | null;
  reason: "closed" | "idle" | "exited" | "error" | "removed" | null;
  error: string | null;
  startedAt: number;
  viewers: number;
  /** The app image can stream each app window on its own. */
  separateWindows?: boolean;
}

/** One app started inside the shared native session. */
export interface NativeSessionApp {
  runId: string;
  appId: string;
  name: string;
  status: string;
  error?: string | null;
  startedAt?: number | null;
}

/**
 * The user's single shared native desktop session: every native app they start
 * runs inside it, and every top-level Xpra window becomes one RAST window.
 */
export interface NativeSession {
  id: "native" | string;
  status: "starting" | "ready" | "ended";
  /** Same-origin URL of the proxied Xpra HTML5 page (contains rast-shim.js). */
  url: string;
  startedAt: number;
  viewers: number;
  separateWindows: boolean;
  display?: string | null;
  reason: null | "closed" | "idle" | "error" | "removed";
  error: null | string;
  apps: NativeSessionApp[];
}

export interface LinuxOverview {
  apps: LinuxApp[];
  sessions: LinuxSession[];
  limits: { minFreeMemoryBytes: number; idleMinutes: number; maxCpus: number };
  dockerEnabled: boolean;
  /** The user's shared native session, when the server supports native apps. */
  session?: NativeSession | null;
  /** False when this server has no native (host install) support. */
  nativeEnabled?: boolean;
}

/** The server reports a newer AppImage build without a version number this way. */
export const NEWER_BUILD = "newer build";

/** File names App Center can install (same list as the server). */
export const LINUX_FILE_PATTERN = /\.(deb|appimage|tar\.gz|tgz|tar\.xz|txz|tar\.bz2|tbz2|tar)$/i;

export function isLinuxPackageName(name: string): boolean {
  return LINUX_FILE_PATTERN.test(name);
}

/** Apps that can be opened (they have a finished build). */
export function isOpenable(app: LinuxApp): boolean {
  return app.status === "ready" || app.status === "updating";
}

let current: LinuxApp[] = [];
let overview: LinuxOverview | null = null;
let currentSession: NativeSession | null = null;
const listeners = new Set<() => void>();

export function linuxApps(): LinuxApp[] {
  return current;
}

export function linuxOverview(): LinuxOverview | null {
  return overview;
}

/** The user's shared native session, if one is starting/running. */
export function linuxNativeSession(): NativeSession | null {
  return currentSession;
}

/** Update the shared native session state (realtime events, polls). */
export function setLinuxNativeSession(session: NativeSession | null): void {
  currentSession = session;
  for (const listener of [...listeners]) {
    try {
      listener();
    } catch (error) {
      console.warn("linux apps listener failed", error);
    }
  }
}

/** Native (host-installed) apps are opened through the shared Xpra session. */
export function isNativeApp(app: LinuxApp | undefined | null): boolean {
  return app?.backend === "native";
}

/** Does this server support native apps at all? (older servers: no) */
export function nativeAppsEnabled(): boolean {
  return overview?.nativeEnabled === true;
}

/**
 * Guess which installed app an Xpra window belongs to, from its X11
 * class-instance metadata (e.g. ["google-chrome", "Google-chrome"]). Only a
 * heuristic: several apps can share a toolkit class. Returns the best match.
 */
export function matchWindowApp(apps: readonly LinuxApp[], classInstance: readonly string[]): LinuxApp | null {
  const classes = classInstance.map((value) => value.toLowerCase()).filter(Boolean);
  if (classes.length === 0) return null;
  let best: LinuxApp | null = null;
  let bestScore = 0;
  for (const app of apps) {
    const candidates = new Set<string>();
    if (app.package) candidates.add(app.package.toLowerCase());
    if (app.desktopId) {
      candidates.add(app.desktopId.toLowerCase());
      const base = app.desktopId.toLowerCase().replace(/\.desktop$/, "");
      candidates.add(base);
      candidates.add(base.split(".").pop() ?? base);
    }
    const command = app.command[0];
    if (command) {
      const base = command.split("/").pop() ?? command;
      candidates.add(base.toLowerCase());
    }
    candidates.add(app.name.toLowerCase().replace(/\s+/g, ""));
    for (const mode of ["prefix", "prefix-two"] as const) {
      for (const candidate of candidates) {
        if (!candidate) continue;
        for (const klass of classes) {
          let score = 0;
          if (klass === candidate) score = 100;
          else if (mode === "prefix" && (klass.startsWith(candidate) || candidate.startsWith(klass))) score = 60;
          else if (mode === "prefix-two") {
            const prefix = candidate.slice(0, Math.min(4, candidate.length));
            if (prefix.length >= 3 && klass.startsWith(prefix)) score = 30;
          }
          if (score > bestScore) {
            bestScore = score;
            best = app;
          }
        }
      }
    }
  }
  return best;
}

/** Stable RAST window app id for one remote Xpra window. */
export function nativeWindowAppId(appId: string, wid: string | number): string {
  return `${LINUX_PREFIX}${appId}:${wid}`;
}

export function linuxAppById(id: string): LinuxApp | undefined {
  return current.find((app) => app.id === id);
}

export function setLinuxOverview(next: LinuxOverview): void {
  overview = next;
  current = Array.isArray(next.apps) ? next.apps : [];
  if ("session" in next) currentSession = (next as LinuxOverview).session ?? null;
  for (const app of current) setAppImageIcon(app.desktopId, app.iconUrl);
  for (const listener of [...listeners]) {
    try {
      listener();
    } catch (error) {
      console.warn("linux apps listener failed", error);
    }
  }
}

export function onLinuxApps(listener: () => void): () => void {
  listeners.add(listener);
  return () => listeners.delete(listener);
}

let loading: Promise<void> | null = null;
let again = false;

/**
 * Fetch the list of apps this account may see. Calls made while a fetch runs
 * are folded into one more fetch afterwards, so the newest state always wins.
 */
export function loadLinuxApps(api: ApiClient): Promise<void> {
  if (loading) {
    again = true;
    return loading;
  }
  loading = (async () => {
    try {
      do {
        again = false;
        setLinuxOverview(await api.get<LinuxOverview>("/api/linux-apps"));
      } while (again);
    } finally {
      loading = null;
    }
  })();
  return loading;
}
