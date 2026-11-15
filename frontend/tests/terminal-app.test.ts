// Unit tests for the pure Terminal application helpers (no DOM needed).
import assert from "node:assert/strict";
import test from "node:test";

import {
  addTab,
  activateTab,
  buildTerminalStartMessage,
  closeTab,
  createTabSet,
  effectiveRuntimeVersion,
  folderMenuTarget,
  initialTerminalState,
  isExpiredSessionError,
  normalizeFontSize,
  normalizeLogicalFolderPath,
  normalizeRuntimeOverrides,
  normalizeTerminalEnv,
  normalizeTerminalFolder,
  normalizeTerminalParams,
  parseTerminalServerMessage,
  parseTerminalSessions,
  readFontSize,
  reduceTerminalState,
  runtimeSourceLabel,
  runtimeSummary,
  runtimeVersionChoices,
  stepFontSize,
  terminalExitNotice,
  terminalFolderFromLogicalPath,
  terminalFolderLabel,
  terminalPhaseLabel,
  terminalShortcut,
  terminalTabTitle,
  terminalWebSocketUrl,
  validateRuntimeOverrides,
  writeFontSize,
  type TerminalState,
} from "../src/apps/terminal/model";

// ---------------------------------------------------------------- WebSocket

test("terminal address follows the page's scheme and host", () => {
  assert.equal(
    terminalWebSocketUrl({ protocol: "http:", host: "192.168.1.20:8720" }),
    "ws://192.168.1.20:8720/api/terminal/ws",
  );
  assert.equal(
    terminalWebSocketUrl({ protocol: "https:", host: "rast.example.com" }),
    "wss://rast.example.com/api/terminal/ws",
  );
});

// ------------------------------------------------------------ logical paths

test("logical folder paths collapse slashes and stay absolute", () => {
  assert.equal(normalizeLogicalFolderPath("/"), "/");
  assert.equal(normalizeLogicalFolderPath("//"), "/");
  assert.equal(normalizeLogicalFolderPath("/a//b/"), "/a/b");
  assert.equal(normalizeLogicalFolderPath(" /srv /../ "), null);
  assert.equal(normalizeLogicalFolderPath("/shared/team docs/report 2026.pdf"), "/shared/team docs/report 2026.pdf");
});

test("logical folder paths reject escapes", () => {
  for (const bad of ["", "relative", "./x", "C:/x", "/a/../b", "/a\\b", "/a\u0000b", 42, null, undefined]) {
    assert.equal(normalizeLogicalFolderPath(bad), null, String(bad));
  }
});

test("File Station directory paths map to terminal folder references", () => {
  assert.deepEqual(terminalFolderFromLogicalPath("/"), { path: "/" });
  assert.deepEqual(terminalFolderFromLogicalPath("/docs/report"), { path: "/docs/report" });
  assert.deepEqual(terminalFolderFromLogicalPath("/shared/team/docs"), { path: "/shared/team/docs" });
  assert.equal(terminalFolderFromLogicalPath("/a/../b"), null);
  assert.deepEqual(normalizeTerminalFolder({ path: "/shared/team" }), { path: "/shared/team" });
  assert.equal(normalizeTerminalFolder({}), null);
  assert.equal(normalizeTerminalFolder({ path: "relative" }), null);
  assert.equal(normalizeTerminalFolder("/absolute"), null);
});

test("folder labels and tab titles are human readable", () => {
  assert.equal(terminalFolderLabel({ path: "/" }), "Home");
  assert.equal(terminalFolderLabel({ path: "/docs" }), "~/docs");
  assert.equal(terminalFolderLabel({ path: "/shared/team" }), "team");
  assert.equal(terminalFolderLabel({ path: "/shared/team/docs" }), "team/docs");
  assert.equal(terminalTabTitle(null, 2), "Terminal 2");
  assert.equal(terminalTabTitle({ path: "/shared/team/docs" }, 1), "Terminal — team/docs");
  assert.equal(terminalTabTitle({ path: "/" }, 1), "Terminal — Home");
});

test("launcher params accept the File Station shape, a path string or a folder object", () => {
  assert.deepEqual(normalizeTerminalParams({ folder: { path: "/shared/team/docs" } }), { path: "/shared/team/docs" });
  assert.deepEqual(normalizeTerminalParams({ folder: "/docs" }), { path: "/docs" });
  assert.deepEqual(normalizeTerminalParams({ path: "/docs" }), { path: "/docs" });
  assert.deepEqual(normalizeTerminalParams("/docs"), { path: "/docs" });
  assert.equal(normalizeTerminalParams(undefined), null);
  assert.equal(normalizeTerminalParams({ folder: { path: "../etc" } }), null);
});

test("File Station menu eligibility needs an allowed app and a browsable directory", () => {
  const base = { terminalAllowed: true, section: "files" as const, accessBlocked: false, entryKind: "dir" as const, path: "/shared/team/docs" };
  assert.deepEqual(folderMenuTarget(base), { path: "/shared/team/docs" });
  assert.equal(folderMenuTarget({ ...base, terminalAllowed: false }), null);
  assert.equal(folderMenuTarget({ ...base, entryKind: "file" }), null);
  assert.equal(folderMenuTarget({ ...base, section: "trash" }), null);
  assert.equal(folderMenuTarget({ ...base, accessBlocked: true }), null);
  assert.equal(folderMenuTarget({ ...base, path: "/a/../b" }), null);
});

// ------------------------------------------------------------ start message

test("start message carries the logical folder only when launched with one", () => {
  const plain = buildTerminalStartMessage({ csrf: "tok", cols: 120, rows: 32 });
  assert.deepEqual(plain, { type: "start", csrf: "tok", cols: 120, rows: 32 });
  assert.equal("folder" in plain, false);

  const fromFolder = buildTerminalStartMessage({
    csrf: "tok",
    cols: 120,
    rows: 32,
    folder: { path: "/shared/team/docs" },
    runtimes: { rust: "1.89.0", ignored: "" },
  });
  assert.deepEqual(fromFolder, {
    type: "start",
    csrf: "tok",
    cols: 120,
    rows: 32,
    folder: { path: "/shared/team/docs" },
    runtimes: { rust: "1.89.0" },
  });

  const reattach = buildTerminalStartMessage({ csrf: "tok", cols: 80, rows: 24, session: "s-1" });
  assert.equal(reattach.session, "s-1");
  assert.equal("folder" in reattach, false);
});

test("start message clamps terminal geometry and drops malformed folders", () => {
  const message = buildTerminalStartMessage({ csrf: "tok", cols: 2, rows: 9999, folder: { path: "../etc" } });
  assert.equal(message.cols, 10);
  assert.equal(message.rows, 200);
  assert.equal("folder" in message, false);
  const nan = buildTerminalStartMessage({ csrf: "tok", cols: Number.NaN, rows: Number.NaN });
  assert.equal(nan.cols, 80);
  assert.equal(nan.rows, 24);
});

test("runtime overrides only contain non-empty string versions", () => {
  assert.deepEqual(normalizeRuntimeOverrides({ rust: "1.90.0", go: "", 3: "x", bad: null }), {
    rust: "1.90.0",
    3: "x",
  });
  assert.deepEqual(normalizeRuntimeOverrides(null), {});
});

// ----------------------------------------------------------- server frames

test("server ready/exit/error frames are parsed strictly", () => {
  assert.deepEqual(
    parseTerminalServerMessage(
      '{"type":"ready","sessionId":"s1","shell":"/bin/bash","cwd":"/srv/rast/homes/alice","reconnected":false,"runtimes":{"rust":"1.90.0"}}',
    ),
    {
      type: "ready",
      sessionId: "s1",
      shell: "/bin/bash",
      cwd: "/srv/rast/homes/alice",
      root: false,
      reconnected: false,
      runtimes: { rust: "1.90.0" },
    },
  );
  assert.deepEqual(parseTerminalServerMessage('{"type":"ready","sessionId":"s1"}'), {
    type: "ready",
    sessionId: "s1",
    shell: "",
    cwd: "",
    root: false,
    reconnected: false,
    runtimes: {},
  });
  assert.deepEqual(parseTerminalServerMessage('{"type":"exit","code":0}'), { type: "exit", code: 0 });
  assert.deepEqual(parseTerminalServerMessage('{"type":"exit","code":null}'), { type: "exit", code: null });
  assert.deepEqual(parseTerminalServerMessage('{"type":"error","message":"boom"}'), { type: "error", message: "boom" });
  assert.deepEqual(parseTerminalServerMessage('{"type":"error"}'), { type: "error", message: "The terminal is not available." });
});

test("unknown or malformed server frames are ignored", () => {
  for (const bad of ["", "not json", "42", "null", "[]", '{"type":"ready"}', '{"type":"login"}', '{"type":"exit"}']) {
    assert.equal(parseTerminalServerMessage(bad), null, bad);
  }
});

test("exit notices and session-expiry messages are plain language", () => {
  assert.equal(terminalExitNotice(0), "The shell exited.");
  assert.equal(terminalExitNotice(130), "The shell exited with code 130.");
  assert.equal(terminalExitNotice(null), "The terminal session ended.");
  assert.equal(isExpiredSessionError("session expired"), true);
  assert.equal(isExpiredSessionError("The session expired after 30 seconds."), true);
  assert.equal(isExpiredSessionError("Session closed"), true);
  assert.equal(isExpiredSessionError("That terminal is no longer running."), true);
  assert.equal(isExpiredSessionError("That terminal has ended."), true);
  assert.equal(isExpiredSessionError("Your session ended, so the terminal was closed."), true);
  assert.equal(isExpiredSessionError("cannot start shell"), false);
  assert.equal(isExpiredSessionError("You already have 8 terminal sessions open."), false);
});

// --------------------------------------------------------- phase machine

function ready(sessionId = "s1", reconnected = false, root = false): ReturnType<typeof parseTerminalServerMessage> {
  return { type: "ready", sessionId, shell: "/bin/bash", cwd: "/home/alice", root, reconnected, runtimes: {} };
}

test("administrator root shells are surfaced by the ready frame", () => {
  const parsed = parseTerminalServerMessage(
    '{"type":"ready","sessionId":"root1","shell":"/bin/bash","cwd":"/root","root":true,"reconnected":false,"runtimes":{}}',
  );
  assert.equal(parsed?.type, "ready");
  assert.equal((parsed as { root?: boolean }).root, true);
  let state = initialTerminalState();
  state = reduceTerminalState(state, { type: "ready", ready: parsed as never });
  assert.equal(state.root, true);
  assert.equal(state.cwd, "/root");
  const unprivileged = reduceTerminalState(initialTerminalState(), { type: "ready", ready: ready() as never });
  assert.equal(unprivileged.root, false);
});

test("a session connects, reconnects once, then gives up", () => {
  let state = initialTerminalState();
  assert.equal(state.phase, "loading");
  state = reduceTerminalState(state, { type: "loaded" });
  assert.equal(state.phase, "connecting");
  state = reduceTerminalState(state, { type: "ready", ready: ready() as never });
  assert.equal(state.phase, "connected");
  assert.equal(state.sessionId, "s1");
  assert.equal(state.everConnected, true);

  state = reduceTerminalState(state, { type: "socket-closed" });
  assert.equal(state.phase, "reconnecting");
  assert.equal(state.reconnectAttempts, 1);

  state = reduceTerminalState(state, { type: "ready", ready: ready("s1", true) as never });
  assert.equal(state.phase, "connected");
  assert.equal(state.reconnected, true);
  assert.equal(state.reconnectAttempts, 0);

  state = reduceTerminalState(state, { type: "socket-closed" });
  assert.equal(state.phase, "reconnecting");
  const second = reduceTerminalState(state, { type: "socket-closed" });
  assert.equal(second.phase, "disconnected");
  assert.match(second.error ?? "", /Lost connection/);
});

test("a failed first connection is retryable without a session", () => {
  let state = reduceTerminalState(initialTerminalState(), { type: "loaded" });
  state = reduceTerminalState(state, { type: "socket-closed" });
  assert.equal(state.phase, "disconnected");
  assert.match(state.error ?? "", /Could not connect/);
  state = reduceTerminalState(state, { type: "retry" });
  assert.equal(state.phase, "connecting");
  assert.equal(state.error, null);
});

test("an ended shell stays ended; expired sessions become an ending", () => {
  let state = reduceTerminalState(initialTerminalState(), { type: "loaded" });
  state = reduceTerminalState(state, { type: "ready", ready: ready() as never });
  state = reduceTerminalState(state, { type: "exit", code: 130 });
  assert.equal(state.phase, "exited");
  assert.equal(state.exitCode, 130);
  assert.equal(reduceTerminalState(state, { type: "socket-closed" }).phase, "exited");

  let reconnect = reduceTerminalState({ ...state, phase: "connected" }, { type: "socket-closed" });
  reconnect = reduceTerminalState(reconnect, { type: "error", message: "session expired" });
  assert.equal(reconnect.phase, "exited");
  assert.equal(reconnect.exitCode, null);
  assert.match(reconnect.error ?? "", /expired/);
});

test("a real error fails the tab; a new session resets it", () => {
  let state = reduceTerminalState(initialTerminalState(), { type: "loaded" });
  state = reduceTerminalState(state, { type: "error", message: "Terminal is disabled by the administrator" });
  assert.equal(state.phase, "failed");
  assert.match(state.error ?? "", /disabled/);
  const fresh = reduceTerminalState(state, { type: "new-session" });
  assert.equal(fresh.phase, "connecting");
  assert.equal(fresh.sessionId, null);
  assert.equal(fresh.error, null);
});

test("status labels describe every phase", () => {
  const label = (patch: Partial<TerminalState>): string =>
    terminalPhaseLabel({ ...initialTerminalState(), ...patch });
  assert.equal(label({ phase: "loading" }), "Loading terminal…");
  assert.equal(label({ phase: "connecting" }), "Connecting…");
  assert.equal(label({ phase: "connected", sessionId: "0123456789" }), "Connected · 01234567");
  assert.equal(label({ phase: "connected", sessionId: "s1", reconnected: true }), "Reconnected");
  assert.equal(label({ phase: "reconnecting" }), "Reconnecting…");
  assert.equal(label({ phase: "disconnected" }), "Disconnected");
  assert.equal(label({ phase: "exited", exitCode: 0 }), "Session ended (code 0)");
  assert.equal(label({ phase: "failed" }), "Terminal error");
});

// --------------------------------------------------------------- tab model

test("tabs add, activate and close with neighbour focus", () => {
  let set = createTabSet<{ id: string }>();
  assert.equal(set.activeId, null);
  set = addTab(set, { id: "a" });
  assert.equal(set.activeId, "a");
  set = addTab(set, { id: "b" });
  assert.equal(set.activeId, "b");
  set = addTab(set, { id: "c" });
  assert.deepEqual(set.tabs.map((tab) => tab.id), ["a", "b", "c"]);

  set = activateTab(set, "a");
  assert.equal(set.activeId, "a");
  const middle = closeTab(set, "a");
  assert.deepEqual(middle.set.tabs.map((tab) => tab.id), ["b", "c"]);
  assert.equal(middle.set.activeId, "b");
  assert.equal(middle.closedLast, false);

  set = closeTab(set, "zzz").set;
  assert.deepEqual(set.tabs.map((tab) => tab.id), ["a", "b", "c"]);

  const last = closeTab(createTabSet([{ id: "only" }]), "only");
  assert.equal(last.closedLast, true);
  assert.equal(last.set.activeId, null);
  assert.equal(last.closed?.id, "only");
});

// ----------------------------------------------------------- runtime env

const ENV_RAW = {
  enabled: true,
  adminRoot: true,
  shell: "/bin/bash",
  fallbackShell: "sh",
  maxSessions: 8,
  graceSeconds: 30,
  runtimes: [
    {
      id: "rust",
      name: "Rust",
      source: "rast",
      current: "1.90.0",
      versions: ["1.90.0", "1.89.0"],
      default: "1.90.0",
      home: "/var/lib/rast/packages/runtimes/rust/1.90.0",
      binDirs: ["/var/lib/rast/packages/runtimes/rust/1.90.0/bin"],
      requirements: ["glibc >= 2.17"],
      missing: [],
      env: { CARGO_HOME: "~/.cargo" },
    },
    { id: "go", name: "Go", source: "system", current: "1.27.1", versions: [], default: null },
  ],
  defaults: { rust: "1.89.0" },
  pathPreview: ["/usr/local/bin", "/var/lib/rast/packages/bin"],
};

test("terminal env DTO is normalised and defaults are respected", () => {
  const env = normalizeTerminalEnv(ENV_RAW);
  assert.equal(env.enabled, true);
  assert.equal(env.adminRoot, true);
  assert.equal(env.shell, "/bin/bash");
  assert.equal(env.fallbackShell, "sh");
  assert.equal(env.maxSessions, 8);
  assert.equal(env.graceSeconds, 30);
  assert.equal(env.runtimes.length, 2);
  const rust = env.runtimes[0]!;
  assert.equal(rust.name, "Rust");
  assert.equal(rust.source, "rast");
  assert.equal(rust.current, "1.90.0");
  assert.deepEqual(rust.requirements, ["glibc >= 2.17"]);
  assert.deepEqual(rust.binDirs, ["/var/lib/rast/packages/runtimes/rust/1.90.0/bin"]);
  assert.deepEqual(rust.env, { CARGO_HOME: "~/.cargo" });
  const go = env.runtimes[1]!;
  assert.deepEqual(runtimeVersionChoices(go), ["1.27.1"]);
  assert.equal(effectiveRuntimeVersion(env, rust), "1.89.0");
  assert.deepEqual(runtimeSummary(env, {}), ["Rust 1.89.0", "Go 1.27.1"]);
  assert.equal(runtimeSourceLabel("rast"), "RAST");
  assert.equal(runtimeSourceLabel("system"), "OS");
});

test("malformed env payloads fall back to safe defaults", () => {
  const empty = normalizeTerminalEnv(null);
  assert.equal(empty.enabled, true);
  assert.equal(empty.maxSessions, 8);
  assert.deepEqual(empty.runtimes, []);
  const disabled = normalizeTerminalEnv({ enabled: false });
  assert.equal(disabled.enabled, false);
  const junk = normalizeTerminalEnv({
    enabled: true,
    shell: 42,
    maxSessions: -3,
    graceSeconds: "x",
    runtimes: [null, { id: "" }, { id: "node", versions: ["24.22.2", 7], default: "24.22.2" }],
    defaults: { node: "24.22.2", bogus: 1 },
    pathPreview: ["/bin", 3],
  });
  assert.equal(junk.shell, null);
  assert.equal(junk.maxSessions, 8);
  assert.equal(junk.graceSeconds, 30);
  assert.equal(junk.runtimes.length, 1);
  assert.deepEqual(junk.runtimes[0]!.versions, ["24.22.2"]);
  assert.deepEqual(junk.defaults, { node: "24.22.2" });
  assert.deepEqual(junk.pathPreview, ["/bin"]);
});

test("per-tab runtime overrides only pin available non-default versions", () => {
  const env = normalizeTerminalEnv(ENV_RAW);
  // ENV_RAW's user default for Rust is 1.89.0.
  assert.deepEqual(validateRuntimeOverrides(env, { rust: "1.89.0" }), {}, "equal to the user default");
  assert.deepEqual(validateRuntimeOverrides(env, { rust: "1.90.0" }), { rust: "1.90.0" });
  assert.deepEqual(validateRuntimeOverrides(env, { rust: "1.90.0", go: "9.9.9" }), { rust: "1.90.0" });
  assert.deepEqual(validateRuntimeOverrides(env, { unknown: "1.0" }), {});
  const withDefault = normalizeTerminalEnv({ ...ENV_RAW, defaults: {} });
  assert.deepEqual(validateRuntimeOverrides(withDefault, { rust: "1.89.0" }), { rust: "1.89.0" });
  assert.deepEqual(validateRuntimeOverrides(withDefault, { rust: "1.90.0" }), {}, "equal to the RAST default");
});

test("terminal session lists are normalised", () => {
  assert.deepEqual(parseTerminalSessions({ sessions: [{ id: "s1", shell: "/bin/bash", cwd: "/home/a", state: "running", startedAt: 5, viewers: 1 }] }), [
    { id: "s1", shell: "/bin/bash", cwd: "/home/a", state: "running", startedAt: 5, viewers: 1 },
  ]);
  assert.equal(parseTerminalSessions({ sessions: [] } as unknown)?.length, 0);
  assert.equal(parseTerminalSessions(null), null);
  assert.equal(parseTerminalSessions({ sessions: "nope" }), null);
  assert.deepEqual(parseTerminalSessions({ sessions: [{ nope: true }, null] }), []);
});

// ------------------------------------------------------------- font size

class MemoryStorage {
  private readonly data = new Map<string, string>();
  getItem(key: string): string | null {
    return this.data.get(key) ?? null;
  }
  setItem(key: string, value: string): void {
    this.data.set(key, value);
  }
}

test("font size is clamped to 12–20 px and persisted", () => {
  assert.equal(normalizeFontSize(2), 12);
  assert.equal(normalizeFontSize(99), 20);
  assert.equal(normalizeFontSize("15"), 15);
  assert.equal(normalizeFontSize("nope"), 13);
  assert.equal(normalizeFontSize(undefined), 13);
  assert.equal(stepFontSize(19, 1), 20);
  assert.equal(stepFontSize(20, 1), 20);
  assert.equal(stepFontSize(13, -1), 12);

  const storage = new MemoryStorage();
  assert.equal(readFontSize(storage), 13);
  assert.equal(writeFontSize(storage, 17), 17);
  assert.equal(readFontSize(storage), 17);
  assert.equal(storage.getItem("rast.terminal.fontSize"), "17");
  assert.equal(writeFontSize(storage, 42), 20);
  assert.equal(readFontSize(storage), 20);
  assert.equal(readFontSize(null), 13);
  assert.equal(writeFontSize(undefined, 15), 15);
});

// --------------------------------------------------------------- shortcuts

test("app shortcuts steal only ctrl/cmd+shift combos from the shell", () => {
  assert.equal(terminalShortcut({ key: "c", ctrlKey: true, shiftKey: true }), "copy");
  assert.equal(terminalShortcut({ key: "V", ctrlKey: true, shiftKey: true }), "paste");
  assert.equal(terminalShortcut({ key: "f", metaKey: true, shiftKey: true }), "search");
  assert.equal(terminalShortcut({ key: "t", ctrlKey: true, shiftKey: true }), "new-tab");
  assert.equal(terminalShortcut({ key: "c", ctrlKey: true }), null);
  assert.equal(terminalShortcut({ key: "v", ctrlKey: true }), null);
  assert.equal(terminalShortcut({ key: "c", ctrlKey: true, shiftKey: true, altKey: true }), null);
  assert.equal(terminalShortcut({ key: "w", ctrlKey: true, shiftKey: true }), null);
  assert.equal(terminalShortcut({ key: "Escape", ctrlKey: true, shiftKey: true }), null);
});
