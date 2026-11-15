// Pure helpers for the RAST Terminal application: WebSocket address and
// message shapes, the session phase machine, the tab model, runtime
// environment normalisation and File Station folder hand-off.
//
// Nothing in this module touches the DOM, `window`, `WebSocket` or storage
// directly (a storage object is passed in), so `tests/terminal-app.test.ts`
// exercises it under Node without a browser.

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/** Same-origin WebSocket endpoint of the native PTY backend. */
export const TERMINAL_WS_PATH = "/api/terminal/ws";
/** Runtime environment endpoint (GET / PATCH). */
export const TERMINAL_ENV_PATH = "/api/terminal/env";
export const TERMINAL_SESSIONS_PATH = "/api/terminal/sessions";
export const TERMINAL_FONT_SIZE_KEY = "rast.terminal.fontSize";
export const TERMINAL_FONT_SIZE_MIN = 12;
export const TERMINAL_FONT_SIZE_MAX = 20;
export const TERMINAL_FONT_SIZE_DEFAULT = 13;
/** Lines kept above the viewport; the server keeps sending output regardless. */
export const TERMINAL_SCROLLBACK = 5000;
/** Rows/columns the protocol accepts (mirrors the Docker terminal contract). */
export const TERMINAL_COLS_MIN = 10;
export const TERMINAL_COLS_MAX = 500;
export const TERMINAL_ROWS_MIN = 2;
export const TERMINAL_ROWS_MAX = 200;
/** One reconnect attempt, shortly after the socket drops (contract: ≤ 5 s). */
export const TERMINAL_RECONNECT_ATTEMPTS = 1;
export const TERMINAL_RECONNECT_DELAY_MS = 1500;
/** Resize frames are coalesced; xterm emits one per fit() call. */
export const TERMINAL_RESIZE_DEBOUNCE_MS = 100;
/** The backend caps live sessions (8 by default); surfaced in the UI. */
export const TERMINAL_DEFAULT_MAX_SESSIONS = 8;
export const TERMINAL_DEFAULT_GRACE_SECONDS = 30;

// ---------------------------------------------------------------------------
// Folders
// ---------------------------------------------------------------------------

/**
 * A RAST logical folder reference handed to the PTY backend.
 *
 * File Station's virtual namespace is `/` for the caller's home and
 * `/shared/<shareName>/...` for shared folders (see src/storage/safe_path.rs).
 * The server resolves the same logical path with the same `storage.resolve`
 * access checks (share ACLs, directory-only, symlink containment), so the
 * frontend only ever passes a logical path — never an absolute POSIX path.
 */
export interface TerminalFolder {
  /** Logical absolute path: `/`, `/sub/dir` or `/shared/<share>/<rest>`. */
  path: string;
}

/** WebSocket address of the native terminal, on the page's own host. */
export function terminalWebSocketUrl(location: Pick<Location, "protocol" | "host">): string {
  const scheme = location.protocol === "https:" ? "wss:" : "ws:";
  return `${scheme}//${location.host}${TERMINAL_WS_PATH}`;
}

/**
 * Validate and normalise a logical File Station path: must be absolute, `/`
 * and duplicate slashes collapse, `.` segments drop and `..`, backslashes and
 * control characters are rejected.
 */
export function normalizeLogicalFolderPath(value: unknown): string | null {
  if (typeof value !== "string") return null;
  const raw = value.trim();
  if (!raw.startsWith("/")) return null;
  if (raw.includes("\\")) return null;
  if (/[\u0000-\u001f\u007f]/.test(raw)) return null;
  const parts: string[] = [];
  for (const part of raw.split("/")) {
    if (part === "" || part === ".") continue;
    if (part === "..") return null;
    parts.push(part);
  }
  return parts.length === 0 ? "/" : `/${parts.join("/")}`;
}

/** Validate a `{path}` folder reference from untrusted input. */
export function normalizeTerminalFolder(value: unknown): TerminalFolder | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const path = normalizeLogicalFolderPath((value as Record<string, unknown>).path);
  return path === null ? null : { path };
}

/** The terminal folder that corresponds to a File Station logical path. */
export function terminalFolderFromLogicalPath(logicalPath: unknown): TerminalFolder | null {
  const path = normalizeLogicalFolderPath(logicalPath);
  return path === null ? null : { path };
}

/** Human label for a folder reference: `Home`, `~/docs`, `team/docs`. */
export function terminalFolderLabel(folder: TerminalFolder): string {
  const path = folder.path;
  if (path === "/") return "Home";
  if (path.startsWith("/shared/")) {
    const rest = path.slice("/shared/".length);
    const slash = rest.indexOf("/");
    return slash < 0 ? rest : `${rest.slice(0, slash)}/${rest.slice(slash + 1)}`;
  }
  return `~${path}`;
}

/**
 * Menu eligibility for File Station's "Open in Terminal" item.
 *
 * Mirrors the state the station itself needs before it may open a folder:
 * the files section must be showing a real listing (never while access is
 * blocked) and the terminal app must be allowed for this account.
 */
export function folderMenuTarget(options: {
  terminalAllowed: boolean;
  section: "files" | "trash";
  accessBlocked: boolean;
  entryKind: "file" | "dir" | string;
  path: string;
}): TerminalFolder | null {
  if (!options.terminalAllowed) return null;
  if (options.section !== "files" || options.accessBlocked) return null;
  if (options.entryKind !== "dir") return null;
  const folder = terminalFolderFromLogicalPath(options.path);
  return folder;
}

/**
 * Validate the launcher params of a Terminal window.
 *
 * File Station opens the app with `{folder: {path: "/shared/team/docs"}}`;
 * a plain launch has no params and starts in the private per-user home.
 */
export function normalizeTerminalParams(params: unknown): TerminalFolder | null {
  if (typeof params === "string") return terminalFolderFromLogicalPath(params);
  if (!params || typeof params !== "object" || Array.isArray(params)) return null;
  const record = params as Record<string, unknown>;
  if ("folder" in record) {
    return typeof record.folder === "string"
      ? terminalFolderFromLogicalPath(record.folder)
      : normalizeTerminalFolder(record.folder);
  }
  return normalizeTerminalFolder(record);
}

/** Title of a tab: the folder it starts in, else its ordinal. */
export function terminalTabTitle(folder: TerminalFolder | null, ordinal: number): string {
  if (folder) return `Terminal — ${terminalFolderLabel(folder)}`;
  return `Terminal ${Math.max(1, Math.trunc(ordinal))}`;
}

// ---------------------------------------------------------------------------
// Client -> server messages
// ---------------------------------------------------------------------------

export interface TerminalStartMessage {
  type: "start";
  csrf: string;
  cols: number;
  rows: number;
  folder?: TerminalFolder;
  runtimes?: Record<string, string>;
  session?: string;
}

function clampInt(value: number, min: number, max: number, fallback: number): number {
  if (!Number.isFinite(value)) return fallback;
  return Math.min(max, Math.max(min, Math.trunc(value)));
}

/** Build the first text frame sent after the WebSocket opens. */
export function buildTerminalStartMessage(input: {
  csrf: string;
  cols: number;
  rows: number;
  folder?: TerminalFolder | null;
  runtimes?: Record<string, string> | null;
  session?: string | null;
}): TerminalStartMessage {
  const message: TerminalStartMessage = {
    type: "start",
    csrf: typeof input.csrf === "string" ? input.csrf : "",
    cols: clampInt(input.cols, TERMINAL_COLS_MIN, TERMINAL_COLS_MAX, 80),
    rows: clampInt(input.rows, TERMINAL_ROWS_MIN, TERMINAL_ROWS_MAX, 24),
  };
  const folder = input.folder ? normalizeTerminalFolder(input.folder) : null;
  if (folder) message.folder = folder;
  const runtimes = normalizeRuntimeOverrides(input.runtimes);
  if (Object.keys(runtimes).length > 0) message.runtimes = runtimes;
  if (input.session) message.session = input.session;
  return message;
}

/** Drop unknown ids / versions from a runtime override map (defence in depth). */
export function normalizeRuntimeOverrides(value: unknown): Record<string, string> {
  if (!value || typeof value !== "object" || Array.isArray(value)) return {};
  const out: Record<string, string> = {};
  for (const [id, version] of Object.entries(value as Record<string, unknown>)) {
    if (!id || typeof version !== "string" || !version) continue;
    out[id] = version;
  }
  return out;
}

// ---------------------------------------------------------------------------
// Server -> client messages
// ---------------------------------------------------------------------------

export interface TerminalReadyMessage {
  type: "ready";
  sessionId: string;
  shell: string;
  cwd: string;
  /** Administrator shell running as root (full server access). */
  root: boolean;
  reconnected: boolean;
  runtimes: Record<string, string>;
}

export type TerminalServerMessage =
  | TerminalReadyMessage
  | { type: "exit"; code: number | null }
  | { type: "error"; message: string };

function stringValue(value: unknown, fallback = ""): string {
  return typeof value === "string" ? value : fallback;
}

/** Parse one text frame from the terminal socket; unknown shapes are ignored. */
export function parseTerminalServerMessage(text: string): TerminalServerMessage | null {
  let value: unknown;
  try {
    value = JSON.parse(text);
  } catch {
    return null;
  }
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const message = value as Record<string, unknown>;
  switch (message.type) {
    case "ready":
      if (typeof message.sessionId !== "string" || !message.sessionId) return null;
      return {
        type: "ready",
        sessionId: message.sessionId,
        shell: stringValue(message.shell),
        cwd: stringValue(message.cwd),
        root: message.root === true,
        reconnected: message.reconnected === true,
        runtimes: normalizeRuntimeOverrides(message.runtimes),
      };
    case "exit": {
      // The contract always carries a code: a number, or null when unknown.
      if (!("code" in message)) return null;
      const code = message.code;
      if (code === null) return { type: "exit", code: null };
      if (typeof code === "number" && Number.isFinite(code)) return { type: "exit", code };
      return null;
    }
    case "error":
      return {
        type: "error",
        message: typeof message.message === "string" && message.message ? message.message : "The terminal is not available.",
      };
    default:
      return null;
  }
}

/**
 * "session expired" is an ending, not an error: offer a new tab afterwards.
 *
 * The backend words this as "That terminal is no longer running." / "That
 * terminal has ended." when a reattach fails, and as "…the terminal was
 * closed." when the signed-in session is revoked.
 */
export function isExpiredSessionError(message: string): boolean {
  return /session (?:has )?expired|session (?:was )?closed|no longer (?:active|available|running)|has ended|terminal was closed|terminal (?:is )?not running/i.test(message);
}

/** Line written into the terminal when the shell ends. */
export function terminalExitNotice(code: number | null): string {
  if (code === null) return "The terminal session ended.";
  if (code === 0) return "The shell exited.";
  return `The shell exited with code ${code}.`;
}

// ---------------------------------------------------------------------------
// Session phase machine
// ---------------------------------------------------------------------------

export type TerminalPhase =
  | "loading"
  | "connecting"
  | "connected"
  | "reconnecting"
  | "disconnected"
  | "exited"
  | "failed";

export interface TerminalState {
  phase: TerminalPhase;
  sessionId: string | null;
  shell: string;
  cwd: string;
  /** Administrator root shell (see `terminal_admin_root` on the server). */
  root: boolean;
  reconnected: boolean;
  runtimes: Record<string, string>;
  exitCode: number | null;
  error: string | null;
  /** Automatic reconnects already used by the current session. */
  reconnectAttempts: number;
  /** True once a `ready` was received (a session existed on the server). */
  everConnected: boolean;
}

export type TerminalEvent =
  | { type: "loaded" }
  | { type: "connecting" }
  | { type: "ready"; ready: TerminalReadyMessage }
  | { type: "socket-closed" }
  | { type: "exit"; code: number | null }
  | { type: "error"; message: string }
  | { type: "retry" }
  | { type: "new-session" };

export function initialTerminalState(): TerminalState {
  return {
    phase: "loading",
    sessionId: null,
    shell: "",
    cwd: "",
    root: false,
    reconnected: false,
    runtimes: {},
    exitCode: null,
    error: null,
    reconnectAttempts: 0,
    everConnected: false,
  };
}

/** Pure transition table for one terminal tab. */
export function reduceTerminalState(state: TerminalState, event: TerminalEvent): TerminalState {
  switch (event.type) {
    case "loaded":
      return state.phase === "loading" ? { ...state, phase: "connecting", error: null } : state;
    case "connecting":
    case "retry":
      // A retry may reuse the previous session id (reattach) when one exists.
      return {
        ...state,
        phase: state.sessionId ? "reconnecting" : "connecting",
        error: null,
        exitCode: null,
        reconnectAttempts: 0,
        reconnected: false,
      };
    case "ready":
      return {
        ...state,
        phase: "connected",
        sessionId: event.ready.sessionId,
        shell: event.ready.shell,
        cwd: event.ready.cwd,
        root: event.ready.root,
        reconnected: event.ready.reconnected,
        runtimes: event.ready.runtimes,
        exitCode: null,
        error: null,
        reconnectAttempts: 0,
        everConnected: true,
      };
    case "socket-closed": {
      if (state.phase === "connected" && state.sessionId && state.reconnectAttempts < TERMINAL_RECONNECT_ATTEMPTS) {
        return {
          ...state,
          phase: "reconnecting",
          reconnectAttempts: state.reconnectAttempts + 1,
          error: null,
        };
      }
      if (state.phase === "connecting" || state.phase === "reconnecting") {
        return {
          ...state,
          phase: "disconnected",
          error: state.phase === "connecting" ? "Could not connect to the terminal." : "Lost connection to the terminal session.",
        };
      }
      return state;
    }
    case "exit":
      return { ...state, phase: "exited", exitCode: event.code, error: null };
    case "error": {
      if (isExpiredSessionError(event.message)) {
        return { ...state, phase: "exited", exitCode: null, error: event.message };
      }
      return { ...state, phase: "failed", error: event.message };
    }
    case "new-session":
      return {
        ...initialTerminalState(),
        phase: "connecting",
      };
    default:
      return state;
  }
}

/** Status text shown next to the status dot. */
export function terminalPhaseLabel(state: TerminalState): string {
  switch (state.phase) {
    case "loading":
      return "Loading terminal…";
    case "connecting":
      return "Connecting…";
    case "connected": {
      if (state.reconnected) return "Reconnected";
      const short = state.sessionId ? state.sessionId.slice(0, 8) : "";
      return short ? `Connected · ${short}` : "Connected";
    }
    case "reconnecting":
      return "Reconnecting…";
    case "disconnected":
      return "Disconnected";
    case "exited":
      return state.exitCode === null ? "Session ended" : `Session ended (code ${state.exitCode})`;
    case "failed":
      return "Terminal error";
    default:
      return state.phase;
  }
}

/** Whether the session has to be (re)started before keyboard input flows. */
export function isTerminalInteractive(state: TerminalState): boolean {
  return state.phase === "connected";
}

// ---------------------------------------------------------------------------
// Tabs
// ---------------------------------------------------------------------------

export interface TerminalTabLike {
  id: string;
}

export interface TerminalTabSet<T extends TerminalTabLike> {
  tabs: T[];
  activeId: string | null;
}

export function createTabSet<T extends TerminalTabLike>(tabs: T[] = []): TerminalTabSet<T> {
  return { tabs: [...tabs], activeId: tabs[0]?.id ?? null };
}

export function addTab<T extends TerminalTabLike>(set: TerminalTabSet<T>, tab: T): TerminalTabSet<T> {
  return { tabs: [...set.tabs, tab], activeId: tab.id };
}

export function activateTab<T extends TerminalTabLike>(set: TerminalTabSet<T>, id: string): TerminalTabSet<T> {
  if (!set.tabs.some((tab) => tab.id === id)) return set;
  return { tabs: set.tabs, activeId: id };
}

export interface CloseTabResult<T extends TerminalTabLike> {
  set: TerminalTabSet<T>;
  closed: T | null;
  /** True when the last tab was closed (the window can close). */
  closedLast: boolean;
}

/** Close one tab, activating its neighbour when it was active. */
export function closeTab<T extends TerminalTabLike>(set: TerminalTabSet<T>, id: string): CloseTabResult<T> {
  const index = set.tabs.findIndex((tab) => tab.id === id);
  if (index < 0) return { set, closed: null, closedLast: false };
  const tabs = set.tabs.filter((tab) => tab.id !== id);
  let activeId = set.activeId;
  if (activeId === id) {
    const next = tabs[Math.min(index, tabs.length - 1)] ?? null;
    activeId = next?.id ?? null;
  }
  return { set: { tabs, activeId }, closed: set.tabs[index] ?? null, closedLast: tabs.length === 0 };
}

export function updateTab<T extends TerminalTabLike>(set: TerminalTabSet<T>, id: string, patch: Partial<T>): TerminalTabSet<T> {
  let changed = false;
  const tabs = set.tabs.map((tab) => {
    if (tab.id !== id) return tab;
    changed = true;
    return { ...tab, ...patch };
  });
  return changed ? { tabs, activeId: set.activeId } : set;
}

export function activeTab<T extends TerminalTabLike>(set: TerminalTabSet<T>): T | null {
  return set.tabs.find((tab) => tab.id === set.activeId) ?? null;
}

// ---------------------------------------------------------------------------
// Runtime environment DTO
// ---------------------------------------------------------------------------

export type RuntimeSource = "rast" | "system" | string;

export interface TerminalRuntime {
  id: string;
  name: string;
  source: RuntimeSource;
  /** Version a freshly started shell sees today. */
  current: string | null;
  versions: string[];
  /** RAST-wide default version. */
  default: string | null;
  home: string | null;
  binDirs: string[];
  requirements: string[];
  missing: string[];
  env: Record<string, string>;
}

export interface TerminalEnv {
  enabled: boolean;
  /** Administrators get a root shell on this server (server config). */
  adminRoot: boolean;
  shell: string | null;
  fallbackShell: string | null;
  maxSessions: number;
  graceSeconds: number;
  runtimes: TerminalRuntime[];
  /** Per-user default versions (`PATCH /api/terminal/env`). */
  defaults: Record<string, string>;
  pathPreview: string[];
}

export function emptyTerminalEnv(): TerminalEnv {
  return {
    enabled: true,
    adminRoot: false,
    shell: null,
    fallbackShell: null,
    maxSessions: TERMINAL_DEFAULT_MAX_SESSIONS,
    graceSeconds: TERMINAL_DEFAULT_GRACE_SECONDS,
    runtimes: [],
    defaults: {},
    pathPreview: [],
  };
}

function stringArray(value: unknown): string[] {
  if (!Array.isArray(value)) return [];
  const out: string[] = [];
  for (const item of value) {
    if (typeof item === "string" && item && !out.includes(item)) out.push(item);
  }
  return out;
}

function stringRecord(value: unknown): Record<string, string> {
  if (!value || typeof value !== "object" || Array.isArray(value)) return {};
  const out: Record<string, string> = {};
  for (const [key, item] of Object.entries(value as Record<string, unknown>)) {
    if (key && typeof item === "string") out[key] = item;
  }
  return out;
}

function normalizeRuntime(value: unknown): TerminalRuntime | null {
  if (!value || typeof value !== "object" || Array.isArray(value)) return null;
  const record = value as Record<string, unknown>;
  if (typeof record.id !== "string" || !record.id) return null;
  const versions = stringArray(record.versions);
  const current = typeof record.current === "string" && record.current ? record.current : null;
  if (current && !versions.includes(current)) versions.unshift(current);
  return {
    id: record.id,
    name: typeof record.name === "string" && record.name ? record.name : record.id,
    source: typeof record.source === "string" && record.source ? record.source : "rast",
    current,
    versions,
    default: typeof record.default === "string" && record.default ? record.default : null,
    home: typeof record.home === "string" && record.home ? record.home : null,
    binDirs: stringArray(record.binDirs),
    requirements: stringArray(record.requirements),
    missing: stringArray(record.missing),
    env: stringRecord(record.env),
  };
}

/** Normalise `GET /api/terminal/env`; malformed fields fall back sensibly. */
export function normalizeTerminalEnv(raw: unknown): TerminalEnv {
  const env = emptyTerminalEnv();
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return env;
  const record = raw as Record<string, unknown>;
  env.enabled = record.enabled !== false;
  env.adminRoot = record.adminRoot === true;
  env.shell = typeof record.shell === "string" && record.shell ? record.shell : null;
  env.fallbackShell = typeof record.fallbackShell === "string" && record.fallbackShell ? record.fallbackShell : null;
  env.maxSessions =
    typeof record.maxSessions === "number" && Number.isFinite(record.maxSessions) && record.maxSessions > 0
      ? Math.trunc(record.maxSessions)
      : TERMINAL_DEFAULT_MAX_SESSIONS;
  env.graceSeconds =
    typeof record.graceSeconds === "number" && Number.isFinite(record.graceSeconds) && record.graceSeconds >= 0
      ? Math.trunc(record.graceSeconds)
      : TERMINAL_DEFAULT_GRACE_SECONDS;
  const seen = new Set<string>();
  if (Array.isArray(record.runtimes)) {
    for (const item of record.runtimes) {
      const runtime = normalizeRuntime(item);
      if (!runtime || seen.has(runtime.id)) continue;
      seen.add(runtime.id);
      env.runtimes.push(runtime);
    }
  }
  env.defaults = stringRecord(record.defaults);
  env.pathPreview = stringArray(record.pathPreview);
  return env;
}

/** RAST/OS label for the runtime source badge. */
export function runtimeSourceLabel(source: RuntimeSource): string {
  if (source === "system") return "OS";
  if (source === "rast") return "RAST";
  return source ? source.toUpperCase() : "RAST";
}

/** Effective version for a runtime: the user's default wins over the RAST one. */
export function effectiveRuntimeVersion(env: TerminalEnv, runtime: TerminalRuntime): string | null {
  const chosen = env.defaults[runtime.id];
  if (chosen && runtime.versions.includes(chosen)) return chosen;
  return runtime.default ?? runtime.current;
}

/** The selection a new tab starts with (empty = whatever the server defaults to). */
export function defaultRuntimeOverrides(env: TerminalEnv): Record<string, string> {
  const out: Record<string, string> = {};
  for (const runtime of env.runtimes) {
    const version = effectiveRuntimeVersion(env, runtime);
    if (version) out[runtime.id] = version;
  }
  return out;
}

/** Keep only overrides that select an actually available version. */
export function validateRuntimeOverrides(env: TerminalEnv, overrides: Record<string, string>): Record<string, string> {
  const out: Record<string, string> = {};
  for (const runtime of env.runtimes) {
    const chosen = overrides[runtime.id];
    if (!chosen) continue;
    if (!runtime.versions.includes(chosen)) continue;
    // Equal to the effective default: no need to pin it in the start message.
    if (chosen === effectiveRuntimeVersion(env, runtime)) continue;
    out[runtime.id] = chosen;
  }
  return out;
}

/** "Rust 1.90.0" lines for the status bar / ready banner. */
export function runtimeSummary(env: TerminalEnv, overrides: Record<string, string>): string[] {
  const out: string[] = [];
  for (const runtime of env.runtimes) {
    const version = overrides[runtime.id] ?? effectiveRuntimeVersion(env, runtime);
    if (version) out.push(`${runtime.name} ${version}`);
    else out.push(runtime.name);
  }
  return out;
}

/** Versions offered by the environment panel (current first, deduplicated). */
export function runtimeVersionChoices(runtime: TerminalRuntime): string[] {
  const out = [...runtime.versions];
  if (runtime.current && !out.includes(runtime.current)) out.unshift(runtime.current);
  return out;
}

/** True when a runtime cannot be used safely (missing system dependency). */
export function runtimeHasProblems(runtime: TerminalRuntime): boolean {
  return runtime.missing.length > 0;
}

export interface TerminalSessionInfo {
  id: string;
  shell: string;
  cwd: string;
  state: string;
  startedAt: number;
  viewers: number;
}

/** Normalise `GET /api/terminal/sessions`; null when the payload is unusable. */
export function parseTerminalSessions(raw: unknown): TerminalSessionInfo[] | null {
  if (!raw || typeof raw !== "object" || Array.isArray(raw)) return null;
  const sessions = (raw as Record<string, unknown>).sessions;
  if (!Array.isArray(sessions)) return null;
  const out: TerminalSessionInfo[] = [];
  for (const item of sessions) {
    if (!item || typeof item !== "object" || Array.isArray(item)) continue;
    const record = item as Record<string, unknown>;
    if (typeof record.id !== "string" || !record.id) continue;
    out.push({
      id: record.id,
      shell: stringValue(record.shell),
      cwd: stringValue(record.cwd),
      state: stringValue(record.state, "running"),
      startedAt: typeof record.startedAt === "number" && Number.isFinite(record.startedAt) ? record.startedAt : 0,
      viewers: typeof record.viewers === "number" && Number.isFinite(record.viewers) ? record.viewers : 0,
    });
  }
  return out;
}

// ---------------------------------------------------------------------------
// Font size preference
// ---------------------------------------------------------------------------

export interface StorageLike {
  getItem(key: string): string | null;
  setItem(key: string, value: string): void;
}

export function normalizeFontSize(value: unknown): number {
  const numeric = typeof value === "number" ? value : Number.parseInt(String(value ?? ""), 10);
  if (!Number.isFinite(numeric)) return TERMINAL_FONT_SIZE_DEFAULT;
  return Math.min(TERMINAL_FONT_SIZE_MAX, Math.max(TERMINAL_FONT_SIZE_MIN, Math.round(numeric)));
}

/** Clamp a font size to the supported range (12–20 px). */
export function stepFontSize(current: number, direction: 1 | -1): number {
  return normalizeFontSize(normalizeFontSize(current) + direction);
}

export function readFontSize(storage: StorageLike | null | undefined): number {
  if (!storage) return TERMINAL_FONT_SIZE_DEFAULT;
  try {
    return normalizeFontSize(storage.getItem(TERMINAL_FONT_SIZE_KEY));
  } catch {
    return TERMINAL_FONT_SIZE_DEFAULT;
  }
}

/** Persist a font size; returns the value actually stored (clamped). */
export function writeFontSize(storage: StorageLike | null | undefined, size: number): number {
  const value = normalizeFontSize(size);
  if (storage) {
    try {
      storage.setItem(TERMINAL_FONT_SIZE_KEY, String(value));
    } catch {
      /* storage disabled (private mode / quota) */
    }
  }
  return value;
}

// ---------------------------------------------------------------------------
// Keyboard shortcuts
// ---------------------------------------------------------------------------

export type TerminalShortcut = "copy" | "paste" | "search" | "new-tab" | null;

export interface TerminalKeyChord {
  key?: string;
  code?: string;
  ctrlKey?: boolean;
  metaKey?: boolean;
  shiftKey?: boolean;
  altKey?: boolean;
}

/**
 * Classify the app shortcuts that may be taken from the browser/xterm.
 *
 * Plain Ctrl+C / Ctrl+V are NOT classified: they stay shell interrupts and
 * readline pastes. Only Ctrl+Shift (or Cmd+Shift) combinations are handled;
 * browser-reserved chords that cannot be intercepted (Ctrl+W, Ctrl+Shift+W,
 * Ctrl+T, Ctrl+Shift+I/J/C …) are deliberately not classified — see the
 * comment in `index.ts` and the toolbar buttons as the guaranteed path.
 */
export function terminalShortcut(chord: TerminalKeyChord): TerminalShortcut {
  if (!(chord.ctrlKey || chord.metaKey) || chord.altKey) return null;
  const key = chord.key ?? "";
  switch (key.toLowerCase()) {
    case "c":
      return chord.shiftKey ? "copy" : null;
    case "v":
      return chord.shiftKey ? "paste" : null;
    case "f":
      return chord.shiftKey ? "search" : null;
    case "t":
      return chord.shiftKey ? "new-tab" : null;
    default:
      return null;
  }
}
