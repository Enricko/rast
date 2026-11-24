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
  /** Operator-selected performance profile at session start. */
  profile?: string | null;
  /** Display backend the session uses (`xvfb` software or `custom` GPU). */
  displayBackend?: string | null;
  /** True only when a real application was verified to render on the GPU. */
  gpuAccelerationVerified?: boolean;
  /** Live cgroup usage of this person's session unit (missing = unknown). */
  metrics?: {
    memoryBytes?: number | null;
    tasks?: number | null;
    cpuUsec?: number | null;
  } | null;
}

/** Display-backend capability detection (read-only, from the server). */
export interface NativeDisplayReport {
  xvfb: boolean;
  xvfbPath: string | null;
  customConfigured: boolean;
  customUsable: boolean;
  /** `xvfb` or `custom`. */
  active: string;
  customCommand: string | null;
  nvidiaXorgDriver: boolean;
  xorg: boolean;
  gpuDisplayConfigured: boolean;
}

/** GPU capability detection (read-only; never an acceleration claim). */
export interface NativeGpuReport {
  vendor: string;
  model: string | null;
  driverVersion: string | null;
  nvidiaDeviceNodes: boolean;
  nvidiaUvm: boolean;
  nvidiaSmi: boolean;
  nvencLibrary: boolean;
  nvdecLibrary: boolean;
  xpraNvidiaCodecs: boolean;
  mesaEgl: boolean;
  renderNodes: string[];
}

/** Effective native runtime policy and host capabilities. */
export interface NativeRuntimeReport {
  profile: string;
  quality: number | null;
  minQuality: number | null;
  speed: number | null;
  minSpeed: number | null;
  videoScaling: number | null;
  avSync: boolean | null;
  encoding: string | null;
  display: NativeDisplayReport;
  gpu: NativeGpuReport;
  gpuAccelerationVerified: boolean;
  notes: string[];
}

export interface LinuxOverview {
  apps: LinuxApp[];
  sessions: LinuxSession[];
  limits: { minFreeMemoryBytes: number; idleMinutes: number; maxCpus: number };
  dockerEnabled: boolean;
  /** Effective runtime profile, display backend and GPU detection. */
  runtime?: NativeRuntimeReport | null;
  /** The user's shared native session, when the server supports native apps. */
  session?: NativeSession | null;
  /** False when this server has no native (host install) support. */
  nativeEnabled?: boolean;
  /** False when the whole Linux apps workflow is disabled server-side. */
  linuxAppsEnabled?: boolean;
}

/** One application installed outside the App Center ("discovered"). */
export interface DiscoveredApp {
  id: string;
  name: string;
  genericName: string | null;
  comment: string | null;
  /** Parsed desktop-entry argv (never a shell string). */
  exec: string[];
  tryExec: string | null;
  icon: string | null;
  /** Same-origin URL of the entry's stored icon; null = use the generic icon. */
  iconUrl: string | null;
  categories: string[];
  terminal: boolean;
  startupWmClass: string | null;
  desktopPath: string;
  desktopId: string | null;
  /** NULL = system-wide entry. */
  ownerUserId: number | null;
  scope: "user" | "system" | "managed-profile";
  source: "xdg" | "appimage" | "portable" | "manual" | "external-installer" | string;
  installDir: string | null;
  version: string | null;
  /** Optional working directory the entry was launched with. */
  workingDir: string | null;
  status: "discovered" | "pending_approval" | "ready" | "unavailable" | "failed";
  approved: boolean;
  hidden: boolean;
  lastSeenMs: number;
  lastLaunchedMs: number | null;
  createdAt: number;
  updatedAt: number;
  canLaunch: boolean;
}

export interface DiscoveredOverview {
  apps: DiscoveredApp[];
  enabled: boolean;
  policy: "approval" | "auto" | "hidden" | string;
  scanIntervalSeconds: number;
  systemRoots: string[];
  extraRoots: string[];
}

/** The server reports a newer AppImage build without a version number this way. */
export const NEWER_BUILD = "newer build";

/**
 * Human-readable runtime rows for the App Center details panel. Pure so the
 * formatting (and the "never claim unverified acceleration" rule) is unit
 * tested without a DOM.
 */
export function nativeRuntimeSummary(
  report: NativeRuntimeReport | null | undefined,
): Array<{ label: string; value: string }> {
  if (!report) return [];
  const rows: Array<{ label: string; value: string }> = [];
  rows.push({ label: "Runtime profile", value: report.profile });
  const display = report.display;
  rows.push({
    label: "Display backend",
    value:
      display.active === "custom"
        ? `custom GPU X server (${display.customCommand ?? "configured"})`
        : "Xvfb (software rendering)",
  });
  const gpu = report.gpu;
  if (gpu.model) {
    rows.push({
      label: "GPU",
      value: gpu.driverVersion ? `${gpu.model} (driver ${gpu.driverVersion})` : gpu.model,
    });
  }
  rows.push({
    label: "Stream encoding",
    value: report.encoding
      ? `forced ${report.encoding}`
      : gpu.xpraNvidiaCodecs && display.gpuDisplayConfigured
        ? "NVENC-capable (hardware video encoding)"
        : "software (x264 / webp / jpeg)",
  });
  rows.push({
    label: "GPU acceleration",
    value: report.gpuAccelerationVerified
      ? "verified by a real application test"
      : "not verified (software fallback is in use unless tested)",
  });
  for (const note of report.notes.slice(0, 2)) {
    rows.push({ label: "Runtime note", value: note });
  }
  return rows;
}

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
 * Does this server offer the Linux apps workflow at all? `linux_apps_enabled =
 * false` quarantines the whole feature (native and legacy Docker backends)
 * while the implementation stays in the binary.
 */
export function linuxAppsOffered(state: LinuxOverview | null): boolean {
  return state?.linuxAppsEnabled !== false;
}

/**
 * Can administrators install a Linux app on this server right now? Requires
 * the feature to be offered and at least one execution backend to exist.
 * Until the overview arrives, assume yes so the button does not flicker.
 */
export function linuxAppsInstallAvailable(state: LinuxOverview | null): boolean {
  if (!linuxAppsOffered(state)) return false;
  if (!state) return true;
  return state.dockerEnabled || state.nativeEnabled === true;
}

/**
 * Guess which installed app an Xpra window belongs to, from its X11
 * class-instance metadata (e.g. ["google-chrome", "Google-chrome"]). Only a
 * heuristic: several apps can share a toolkit class. Returns the best match.
 */
export function matchWindowApp(
  apps: readonly LinuxApp[],
  classInstance: readonly string[],
  discovered: readonly DiscoveredApp[] = [],
): LinuxApp | null {
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
  // Discovered applications match through their StartupWMClass first (the
  // desktop entry's declaration), then the usual name/exec heuristics.
  for (const app of discovered) {
    const candidates: Array<{ value: string; score: number }> = [];
    if (app.startupWmClass) candidates.push({ value: app.startupWmClass.toLowerCase(), score: 100 });
    if (app.desktopId) {
      const base = app.desktopId.toLowerCase().replace(/\.desktop$/, "");
      candidates.push({ value: base, score: 90 });
      candidates.push({ value: base.split(".").pop() ?? base, score: 70 });
    }
    const command = app.exec[0];
    if (command) {
      const base = command.split("/").pop() ?? command;
      candidates.push({ value: base.toLowerCase(), score: 60 });
    }
    candidates.push({ value: app.name.toLowerCase().replace(/\s+/g, ""), score: 80 });
    for (const candidate of candidates) {
      for (const klass of classes) {
        let score = 0;
        if (klass === candidate.value) score = candidate.score;
        else if (klass.startsWith(candidate.value) || candidate.value.startsWith(klass)) score = Math.round(candidate.score * 0.6);
        if (score > bestScore) {
          bestScore = score;
          best = discoveredAsLinuxApp(app);
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
  return current.find((app) => app.id === id) ?? discoveredSynthetic.get(id);
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

// ---------------------------------------------------------------------------
// Discovered applications (installed outside the App Center)
// ---------------------------------------------------------------------------

let discoveredCurrent: DiscoveredApp[] = [];
let discoveredState: DiscoveredOverview | null = null;
let discoveredSynthetic = new Map<string, LinuxApp>();
const discoveredListeners = new Set<() => void>();

/** Entries the server reports as visible to this account. */
export function discoveredApps(): DiscoveredApp[] {
  return discoveredCurrent;
}

/** The last discovery overview (policy, scan interval, feature state). */
export function discoveredOverview(): DiscoveredOverview | null {
  return discoveredState;
}

export function discoveredAppById(id: string): DiscoveredApp | undefined {
  return discoveredCurrent.find((app) => app.id === id);
}

// ---------------------------------------------------------------------------
// Discovery ↔ launcher reconciliation
// ---------------------------------------------------------------------------

/**
 * Identity keys of one application: the base of its desktop id and the
 * basenames of its executable tokens. Two entries sharing any key describe the
 * same application (a managed app and its leftover system desktop entry, or a
 * manual import of what a `.desktop` file already declares).
 */
function discoveredIdentityKeys(app: { desktopId?: string | null; exec: readonly string[] }): string[] {
  const keys: string[] = [];
  if (app.desktopId) {
    const base = app.desktopId.toLowerCase().replace(/\.desktop$/, "").split(".").pop() ?? "";
    if (base) keys.push(`desktop:${base}`);
  }
  const seenExec = new Set<string>();
  for (const token of app.exec) {
    const base = (token.split("/").pop() ?? "").toLowerCase();
    // System wrapper scripts never identify an application.
    if (!base || base === "sh" || base === "env" || base === "dbus-run-session") continue;
    if (seenExec.has(base)) continue;
    seenExec.add(base);
    keys.push(`exec:${base}`);
  }
  return keys;
}

function managedIdentityKeys(): Set<string> {
  const keys = new Set<string>();
  // `current` holds the managed apps only; discovered entries live in
  // `discoveredSynthetic` and never appear there.
  for (const app of current) {
    for (const key of discoveredIdentityKeys({ desktopId: app.desktopId, exec: app.command })) keys.add(key);
  }
  return keys;
}

/**
 * A discovered entry that a managed application already provides. It must not
 * duplicate the managed app in the Applications menu, on the desktop or in
 * App Center's launcher surface (the managed app stays canonical).
 */
export function discoveredIsShadowed(app: { desktopId?: string | null; exec: readonly string[] }): boolean {
  const managed = managedIdentityKeys();
  if (managed.size === 0 || current.length === 0) return false;
  return discoveredIdentityKeys(app).some((key) => managed.has(key));
}

/**
 * One canonical discovered entry per executable, ordered for the launcher.
 * Managed-shadowed entries are removed; everything else is grouped by the
 * argv[0] basename so a manual import next to the equivalent system desktop
 * entry shows a single row (highest-ranked entry first: pending approval over
 * ready, approved, then xdg source, then name).
 */
export function reconciledDiscoveredApps(): DiscoveredApp[] {
  const managed = managedIdentityKeys();
  const byExec = new Map<string, DiscoveredApp[]>();
  for (const app of discoveredCurrent) {
    if (managed.size > 0 && current.length > 0) {
      if (discoveredIdentityKeys(app).some((key) => managed.has(key))) continue;
    }
    const base = (app.exec[0]?.split("/").pop() ?? `id:${app.id}`).toLowerCase();
    const group = byExec.get(base);
    if (group) group.push(app);
    else byExec.set(base, [app]);
  }
  const rank = (app: DiscoveredApp): number =>
    (app.status === "pending_approval" ? 4 : 0) + (app.approved ? 2 : 0) + (app.source === "xdg" ? 1 : 0);
  const representative = (group: DiscoveredApp[]): DiscoveredApp =>
    group.reduce((best, app) =>
      rank(app) > rank(best) || (rank(app) === rank(best) && app.name.localeCompare(best.name) < 0) ? app : best);
  return [...byExec.values()].map(representative);
}

/** Is `appId` (`linux:<discovered id>`) one of the externally discovered apps? */
export function isDiscoveredDesktopId(appId: string): boolean {
  return appId.startsWith(LINUX_PREFIX) && discoveredSynthetic.has(appId.slice(LINUX_PREFIX.length));
}

/**
 * A discovered entry as the synthetic [`LinuxApp`] the shared window/launch
 * machinery already understands (native backend + discovered id). It is never
 * part of [`linuxApps()`]: App Center lists discovered entries separately.
 */
export function discoveredAsLinuxApp(app: DiscoveredApp): LinuxApp {
  return {
    id: app.id,
    desktopId: app.desktopId ?? `discovered-${app.id}`,
    name: app.name,
    format: "appimage",
    fileName: app.desktopPath,
    version: app.version,
    package: null,
    description: app.comment,
    command: app.exec,
    launchGuess: false,
    iconUrl: app.iconUrl,
    status: "ready",
    error: null,
    jobId: null,
    update: { source: null, latestVersion: null, available: false, checkedAt: null, error: null },
    memoryMb: 0,
    cpus: 0,
    sizeBytes: null,
    visibility: app.scope === "system" ? "users" : "admins",
    running: 0,
    createdAt: app.createdAt,
    updatedAt: app.updatedAt,
    backend: "native",
  };
}

/** Discovered entries the current account may open now, as LinuxApps. */
export function discoveredOpenableLinuxApps(): LinuxApp[] {
  // Reconciled first: entries a managed app already provides (for example a
  // leftover system desktop entry for a managed browser) never produce a
  // second launcher item.
  return reconciledDiscoveredApps()
    .filter((app) => app.canLaunch)
    .map(discoveredAsLinuxApp);
}

/**
 * Every app the native session may adopt windows for: managed native apps plus
 * the discovered apps this account can launch.
 */
export function nativeWindowApps(): LinuxApp[] {
  return [...linuxApps().filter(isNativeApp), ...discoveredOpenableLinuxApps()];
}

/** Look up a managed or discovered app id (windows adopt discovered ids too). */
export function anyLinuxAppById(id: string): LinuxApp | undefined {
  return linuxAppById(id) ?? discoveredSynthetic.get(id);
}

export function setDiscoveredOverview(next: DiscoveredOverview): void {
  discoveredState = next;
  discoveredCurrent = Array.isArray(next.apps) ? next.apps : [];
  discoveredSynthetic = new Map(
    discoveredCurrent.map((app) => [app.id, discoveredAsLinuxApp(app)] as const),
  );
  // The desktop identity of a discovered app is `linux:<id>`, like a managed
  // app; without an icon URL the existing generic icon is used.
  for (const app of discoveredCurrent) {
    setAppImageIcon(`${LINUX_PREFIX}${app.id}`, app.iconUrl ?? null);
  }
  for (const listener of [...discoveredListeners]) {
    try {
      listener();
    } catch (error) {
      console.warn("discovered applications listener failed", error);
    }
  }
}

export function onDiscoveredApps(listener: () => void): () => void {
  discoveredListeners.add(listener);
  return () => discoveredListeners.delete(listener);
}

let discoveredLoading: Promise<void> | null = null;
let discoveredAgain = false;

/** Fetch the discovered inventory the server allows this account to see. */
export function loadDiscoveredApps(api: ApiClient): Promise<void> {
  if (discoveredLoading) {
    discoveredAgain = true;
    return discoveredLoading;
  }
  discoveredLoading = (async () => {
    try {
      do {
        discoveredAgain = false;
        setDiscoveredOverview(await api.get<DiscoveredOverview>("/api/linux-apps/discovered"));
      } while (discoveredAgain);
    } finally {
      discoveredLoading = null;
    }
  })();
  return discoveredLoading;
}

// ---------------------------------------------------------------------------
// "Import existing application" draft (pure validation, unit tested)
// ---------------------------------------------------------------------------

/** The raw values of the App Center import dialog. */
export interface DiscoveredImportDraft {
  name: string;
  path: string;
  args: string;
  workingDir: string;
  icon: string;
}

/** A validated import request (`POST /api/linux-apps/discovered/import`). */
export interface DiscoveredImportBody {
  name: string;
  path: string;
  args: string[];
  workingDir?: string;
  icon?: string;
}

/** Per-field messages the dialog shows inline; empty = valid. */
export interface DiscoveredImportProblems {
  name?: string;
  path?: string;
  args?: string;
  workingDir?: string;
  icon?: string;
}

const CONTROL_CHARS = /[\u0000-\u001f\u007f]/;
const ICON_THEME_NAME = /^[A-Za-z0-9][A-Za-z0-9@+._-]*$/;

function hasPathTraversal(value: string): boolean {
  return value.split("/").some((part) => part === "." || part === "..");
}

/**
 * Split the optional arguments field into an argv array. Quotes group spaces,
 * a backslash escapes the next character and empty tokens are dropped. RAST
 * always starts applications with an explicit argv, never through a shell.
 */
export function splitDiscoveredImportArgs(value: string): { args: string[]; error: string | null } {
  const args: string[] = [];
  let current = "";
  let started = false;
  let quote: '"' | "'" | null = null;
  let escaped = false;
  for (const char of value) {
    if (char === "\0") return { args: [], error: "Arguments cannot contain NUL characters." };
    if (escaped) {
      current += char;
      started = true;
      escaped = false;
      continue;
    }
    if (char === "\\") {
      escaped = true;
      started = true;
      continue;
    }
    if (quote) {
      if (char === quote) quote = null;
      else current += char;
      continue;
    }
    if (char === '"' || char === "'") {
      quote = char;
      started = true;
      continue;
    }
    if (/\s/.test(char)) {
      if (started) {
        args.push(current);
        current = "";
        started = false;
      }
      continue;
    }
    current += char;
    started = true;
  }
  if (escaped) current += "\\";
  if (quote) return { args: [], error: "Close the quoted argument (unbalanced quotes)." };
  if (started) args.push(current);
  if (args.length > 64) return { args: [], error: "At most 64 arguments are supported." };
  if (args.some((arg) => arg.length > 4096)) {
    return { args: [], error: "Each argument must be at most 4096 characters." };
  }
  return { args, error: null };
}

/**
 * Validate the import dialog client-side (the server validates again). Returns
 * the request body only when every field is acceptable, so an invalid dialog
 * never sends a request.
 */
export function validateDiscoveredImport(draft: DiscoveredImportDraft): {
  problems: DiscoveredImportProblems;
  body: DiscoveredImportBody | null;
} {
  const problems: DiscoveredImportProblems = {};
  const name = draft.name.trim();
  if (!name) problems.name = "Enter a name for the application.";
  else if (name.length > 256) problems.name = "The name must be at most 256 characters.";
  else if (CONTROL_CHARS.test(name)) problems.name = "The name contains unsupported characters.";

  const path = draft.path.trim();
  if (!path) problems.path = "Enter the executable's absolute path.";
  else if (!path.startsWith("/")) problems.path = "The executable path must be absolute (start with /).";
  else if (path.length > 4096) problems.path = "The executable path is too long.";
  else if (CONTROL_CHARS.test(path)) problems.path = "The executable path contains unsupported characters.";

  const parsed = splitDiscoveredImportArgs(draft.args);
  if (parsed.error) problems.args = parsed.error;

  const workingDir = draft.workingDir.trim();
  if (workingDir) {
    if (!workingDir.startsWith("/")) problems.workingDir = "The working directory must be absolute (start with /).";
    else if (hasPathTraversal(workingDir)) problems.workingDir = "The working directory must not contain . or .. segments.";
    else if (workingDir.length > 4096) problems.workingDir = "The working directory is too long.";
    else if (CONTROL_CHARS.test(workingDir)) problems.workingDir = "The working directory contains unsupported characters.";
  }

  const icon = draft.icon.trim();
  if (icon) {
    if (icon.length > 1024) problems.icon = "The icon must be at most 1024 characters.";
    else if (CONTROL_CHARS.test(icon) || icon.includes("\\")) problems.icon = "The icon contains unsupported characters.";
    else if (icon.startsWith("/")) {
      if (hasPathTraversal(icon)) problems.icon = "The icon path must not contain . or .. segments.";
      else if (!/\.(png|svg)$/i.test(icon)) problems.icon = "An icon file path must end in .png or .svg.";
    } else if (icon.includes("/") || !ICON_THEME_NAME.test(icon) || icon.includes("..")) {
      problems.icon = "Use an icon-theme name (for example utilities-terminal) or an absolute .png/.svg path.";
    }
  }

  if (Object.keys(problems).length > 0) return { problems, body: null };
  const body: DiscoveredImportBody = { name, path, args: parsed.args };
  if (workingDir) body.workingDir = workingDir;
  if (icon) body.icon = icon;
  return { problems, body };
}
