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
}

export interface LinuxOverview {
  apps: LinuxApp[];
  sessions: LinuxSession[];
  limits: { minFreeMemoryBytes: number; idleMinutes: number; maxCpus: number };
  dockerEnabled: boolean;
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
const listeners = new Set<() => void>();

export function linuxApps(): LinuxApp[] {
  return current;
}

export function linuxOverview(): LinuxOverview | null {
  return overview;
}

export function linuxAppById(id: string): LinuxApp | undefined {
  return current.find((app) => app.id === id);
}

export function setLinuxOverview(next: LinuxOverview): void {
  overview = next;
  current = Array.isArray(next.apps) ? next.apps : [];
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
