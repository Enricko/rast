// Pure parts of the "app matrix" scenario (task 0.6): option parsing, the
// production guard, the API allow-list, statistics, budget evaluation, the
// per-app flow table, result JSON and the markdown report.
//
// Nothing here touches the network, the filesystem (except an injected file
// reader), Playwright or the clock, so `node --test tests/e2e/lib/` covers it.

import { inflateRawSync } from "node:zlib";

export const SCHEMA = "rast-app-matrix/1";

// ---------------------------------------------------------------------------
// Budgets: ONE table. Change a limit here and nowhere else.
// ---------------------------------------------------------------------------
export const BUDGETS = [
  {
    id: "typing.p95",
    label: "typing p95, keydown to first changed pixel",
    unit: "ms",
    op: "<=",
    limit: 150,
    pick: (r) => r?.typing?.stats?.p95 ?? null,
  },
  {
    id: "scroll.p95",
    label: "scroll p95, wheel to first changed frame",
    unit: "ms",
    op: "<=",
    limit: 200,
    pick: (r) => r?.scroll?.latency?.p95 ?? null,
  },
  {
    id: "scroll.fps",
    label: "changed frames per second while scrolling",
    unit: "fps",
    op: ">=",
    limit: 15,
    pick: (r) => r?.scroll?.fps ?? null,
  },
  {
    id: "drag.fps",
    label: "RAST window frames per second while dragging",
    unit: "fps",
    op: ">=",
    limit: 30,
    pick: (r) => r?.drag?.fps ?? null,
  },
  {
    id: "drag.repaint",
    label: "app content repaint after the drop",
    unit: "ms",
    op: "<=",
    limit: 250,
    // "none-needed": the content did not change after the drop, so nothing
    // had to be repainted; that counts as 0 ms.
    pick: (r) => (r?.drag?.repaint === "none-needed" ? 0 : (r?.drag?.repaintMs ?? null)),
  },
];

export function evaluateBudgets(responsiveness, table = BUDGETS) {
  return table.map((budget) => {
    const value = responsiveness ? budget.pick(responsiveness) : null;
    let status = "unmeasured";
    if (typeof value === "number" && Number.isFinite(value)) {
      const ok = budget.op === "<=" ? value <= budget.limit : value >= budget.limit;
      status = ok ? "pass" : "fail";
    }
    return {
      id: budget.id,
      label: budget.label,
      unit: budget.unit,
      op: budget.op,
      limit: budget.limit,
      value: typeof value === "number" ? round1(value) : null,
      status,
    };
  });
}

// ---------------------------------------------------------------------------
// Statistics
// ---------------------------------------------------------------------------
export function round1(value) {
  return Math.round(value * 10) / 10;
}

/** Nearest-rank percentile of a list of numbers; null for an empty list. */
export function percentile(values, p) {
  const sorted = values.filter((v) => Number.isFinite(v)).sort((a, b) => a - b);
  if (sorted.length === 0) return null;
  const rank = Math.max(1, Math.ceil((p / 100) * sorted.length));
  return sorted[Math.min(sorted.length, rank) - 1];
}

export function stats(values) {
  const list = values.filter((v) => Number.isFinite(v));
  if (list.length === 0) return { n: 0, min: null, p50: null, p95: null, max: null, mean: null };
  const sum = list.reduce((a, b) => a + b, 0);
  return {
    n: list.length,
    min: round1(Math.min(...list)),
    p50: round1(percentile(list, 50)),
    p95: round1(percentile(list, 95)),
    max: round1(Math.max(...list)),
    mean: round1(sum / list.length),
  };
}

/**
 * Latency of each trigger (keydown, wheel) to the first changed frame after
 * it. A change counts for trigger i only before the next trigger and within
 * `capMs`; a trigger without such a change is "missed" and is counted with
 * the length of its window, so a stuck app cannot hide behind the p95.
 */
export function latenciesFromEvents(triggerTimes, changeTimes, capMs = 1000) {
  const triggers = [...triggerTimes].sort((a, b) => a - b);
  const changes = [...changeTimes].sort((a, b) => a - b);
  const samples = [];
  let missed = 0;
  for (let i = 0; i < triggers.length; i += 1) {
    const start = triggers[i];
    const bound = Math.min(i + 1 < triggers.length ? triggers[i + 1] : Infinity, start + capMs);
    const hit = changes.find((c) => c >= start && c < bound);
    if (hit === undefined) {
      missed += 1;
      samples.push(bound - start);
    } else {
      samples.push(hit - start);
    }
  }
  return { samples, missed };
}

/** Changes per second inside [from, to] (timestamps in ms). */
export function rateInWindow(times, from, to) {
  const span = to - from;
  if (!(span > 0)) return 0;
  const count = times.filter((t) => t >= from && t <= to).length;
  return round1((count * 1000) / span);
}

/**
 * Frame rate of a moving window from per-rAF samples [{t,x,y}]: frames in
 * which the position differs from the previous sample, over the time from the
 * first to the last such frame.
 */
export function movesPerSecond(rects) {
  const moves = [];
  for (let i = 1; i < rects.length; i += 1) {
    if (rects[i].x !== rects[i - 1].x || rects[i].y !== rects[i - 1].y) moves.push(rects[i].t);
  }
  if (moves.length < 2) return { fps: 0, frames: moves.length, durationMs: 0 };
  const durationMs = moves[moves.length - 1] - moves[0];
  return { fps: durationMs > 0 ? round1(((moves.length - 1) * 1000) / durationMs) : 0, frames: moves.length, durationMs: round1(durationMs) };
}

// ---------------------------------------------------------------------------
// Targets and safety
// ---------------------------------------------------------------------------
const PRODUCTION_PORTS = new Set(["8720", "8721"]);
const PRODUCTION_HOSTS = [/^desktop\.lufria\.tech$/i, /(^|\.)lufria\.tech$/i];
const LOCAL_HOSTS = new Set(["127.0.0.1", "localhost", "::1", "[::1]"]);

/**
 * True for the production RAST (port 8720, its nginx on 8721, desktop.lufria
 * .tech) and, to be safe, for any host that is not loopback and for an
 * unparsable URL.
 */
export function isProductionTarget(baseUrl) {
  let url;
  try {
    url = new URL(baseUrl);
  } catch {
    return true;
  }
  if (PRODUCTION_PORTS.has(url.port)) return true;
  if (PRODUCTION_HOSTS.some((re) => re.test(url.hostname))) return true;
  return !LOCAL_HOSTS.has(url.hostname);
}

/** Throws unless the username may be used against the target. */
export function assertTargetGuard(baseUrl, username) {
  if (!isProductionTarget(baseUrl)) return { production: false };
  if (typeof username !== "string" || !username.startsWith("t-")) {
    throw new Error(
      `refusing to run against production (${redactUrl(baseUrl)}) as "${username}": only test accounts (t-*) are allowed`,
    );
  }
  if (username === "t-admin") {
    throw new Error("refusing to run against production as t-admin: the matrix uses a plain test account (t-alice)");
  }
  return { production: true };
}

export function redactUrl(baseUrl) {
  try {
    const url = new URL(baseUrl);
    return `${url.protocol}//${url.host}`;
  } catch {
    return "<invalid url>";
  }
}

/**
 * The matrix may only call these endpoints: reads, File Station mkdir (one
 * folder in the test account's HOME) and, with cleanup, File Station delete.
 * Nothing admin-only, nothing that installs, updates or uninstalls.
 */
const ALLOWED_CALLS = [
  ["GET", /^\/healthz$/],
  ["GET", /^\/api\/bootstrap$/],
  ["GET", /^\/api\/auth\/me$/],
  ["POST", /^\/api\/auth\/login$/],
  ["POST", /^\/api\/auth\/logout$/],
  ["GET", /^\/api\/linux-apps$/],
  ["GET", /^\/api\/linux-apps\/session$/],
  ["GET", /^\/api\/linux-apps\/(?!install$|preview$)[A-Za-z0-9._-]+$/],
  // End application for one run of the caller's own session; only used to
  // clean up an app a failed check left running.
  ["POST", /^\/api\/linux-apps\/session\/runs\/[A-Za-z0-9._-]+\/close$/],
  ["GET", /^\/api\/files$/],
  ["GET", /^\/api\/files\/(roots|stat|text|download|usage)$/],
  ["POST", /^\/api\/files\/mkdir$/],
];
const CLEANUP_CALLS = [["POST", /^\/api\/files\/delete$/]];

export function isAllowedApiCall(method, path, { cleanup = false } = {}) {
  const pathname = String(path).split("?")[0];
  const table = cleanup ? [...ALLOWED_CALLS, ...CLEANUP_CALLS] : ALLOWED_CALLS;
  return table.some(([m, re]) => m === String(method).toUpperCase() && re.test(pathname));
}

export function assertAllowedApiCall(method, path, options) {
  if (!isAllowedApiCall(method, path, options)) {
    throw new Error(`app matrix: API call not allowed: ${String(method).toUpperCase()} ${String(path).split("?")[0]}`);
  }
}

/** `KEY=VALUE` lines (key = username); comments and blank lines ignored. */
export function parseCredsFile(text) {
  const values = {};
  for (const line of String(text).split("\n")) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const index = trimmed.indexOf("=");
    if (index <= 0) continue;
    values[trimmed.slice(0, index).trim()] = trimmed.slice(index + 1).trim();
  }
  return values;
}

/**
 * Resolve the account to use. The guard runs before any password is read.
 * `readFile` is injected (fs.readFileSync(path, "utf8")).
 */
export function resolveCredentials(options, config, readFile) {
  const username = options.user ?? config.users?.[0]?.username;
  if (!username) throw new Error("no --user and no configured test account");
  const guard = assertTargetGuard(config.baseUrl, username);
  let password;
  if (options.passwordFile) {
    const creds = parseCredsFile(readFile(options.passwordFile));
    password = creds[username];
    if (!password) throw new Error(`no entry for "${username}" in ${options.passwordFile}`);
  } else {
    password = config.users?.find((user) => user.username === username)?.password;
    if (!password) throw new Error(`no password for "${username}": pass --password-file=<KEY=VALUE file>`);
  }
  return { username, password, production: guard.production };
}

// ---------------------------------------------------------------------------
// Options
// ---------------------------------------------------------------------------
export const MATRIX_OPTION_NAMES = [
  "--apps",
  "--user",
  "--password-file",
  "--home-mode",
  "--home-path",
  "--budgets",
  "--keys",
  "--launch-timeout",
  "--cleanup",
  "--plan",
  "--no-responsiveness",
];

export const HOME_MODES = ["auto", "legacy", "one-home"];

export function parseMatrixOptions(argv, appKeys = APPS.map((app) => app.key)) {
  const options = {
    apps: null,
    user: null,
    passwordFile: null,
    homeMode: "auto",
    homePath: null,
    budgets: "enforce",
    keys: 30,
    launchTimeoutMs: 180_000,
    cleanup: false,
    plan: false,
    responsiveness: true,
  };
  for (const item of argv) {
    const [name, ...rest] = item.split("=");
    const value = rest.join("=");
    switch (name) {
      case "--apps": {
        const list = value.split(",").map((s) => s.trim().toLowerCase()).filter(Boolean);
        const unknown = list.filter((key) => !appKeys.includes(key));
        if (list.length === 0 || unknown.length > 0) {
          throw new Error(`--apps: unknown app(s) ${unknown.join(",") || "(none given)"}; known: ${appKeys.join(",")}`);
        }
        options.apps = list;
        break;
      }
      case "--user":
        if (!value) throw new Error("--user needs a value");
        options.user = value;
        break;
      case "--password-file":
        if (!value) throw new Error("--password-file needs a path");
        options.passwordFile = value;
        break;
      case "--home-mode":
        if (!HOME_MODES.includes(value)) throw new Error(`--home-mode must be one of ${HOME_MODES.join("|")}`);
        options.homeMode = value;
        break;
      case "--home-path":
        if (!value.startsWith("/") && value !== "~") throw new Error("--home-path must be absolute (or ~)");
        options.homePath = value.replace(/\/$/, "");
        break;
      case "--budgets":
        if (!["enforce", "report"].includes(value)) throw new Error("--budgets must be enforce|report");
        options.budgets = value;
        break;
      case "--keys": {
        const n = Number(value);
        if (!Number.isInteger(n) || n < 5 || n > 200) throw new Error("--keys must be an integer 5..200");
        options.keys = n;
        break;
      }
      case "--launch-timeout": {
        const n = Number(value);
        if (!(n >= 10 && n <= 900)) throw new Error("--launch-timeout is in seconds, 10..900");
        options.launchTimeoutMs = n * 1000;
        break;
      }
      case "--cleanup":
        options.cleanup = true;
        break;
      case "--plan":
        options.plan = true;
        break;
      case "--no-responsiveness":
        options.responsiveness = false;
        break;
      default:
        throw new Error(`unknown app matrix option: ${name}`);
    }
  }
  return options;
}

// ---------------------------------------------------------------------------
// Home mode, paths, expected failures
// ---------------------------------------------------------------------------
/**
 * `GET /api/files/usage`: legacy accounts report only `files` and `trash` in
 * `byLocation`; one-HOME accounts also report `apps`, `caches` (and `shared`).
 */
export function detectHomeMode(usage) {
  const keys = Object.keys(usage?.byLocation ?? {});
  return keys.includes("apps") || keys.includes("caches") || keys.includes("shared") ? "one-home" : "legacy";
}

/** Where the app saves (as the app sees it) and where File Station looks. */
export function planSavePaths({ homeMode, homePath, username, fileName }) {
  if (homeMode === "one-home") {
    const home = homePath ?? `/srv/rast/homes/${username}`;
    return { appPath: `${home}/rast-matrix/${fileName}`, fsDir: "/rast-matrix", fsPath: `/rast-matrix/${fileName}`, mkdir: true };
  }
  // Legacy: the app's HOME is its own profile; nothing it saves is expected in
  // File Station. It still saves to `~/` so the attempt is observable.
  return { appPath: `${homePath ?? "~"}/${fileName}`, fsDir: "/", fsPath: `/${fileName}`, mkdir: false };
}

export const LEGACY_SAVE_REASON =
  "legacy mode: apps keep their own profile (<data_dir>/linux-apps/profiles/<app>/<user>), so a file the app saves is not in the account's File Station home";

/** Reason a check is expected to fail in this mode, or null. */
export function expectedFailReason({ homeMode, check }) {
  if (homeMode === "legacy" && check === "saved") return LEGACY_SAVE_REASON;
  return null;
}

/**
 * Final status of a check: a failure with a reason is "expected-fail", a pass
 * where a failure was expected is "unexpected-pass" (reported, not an error).
 */
export function classify(ok, expectedReason) {
  if (ok) return expectedReason ? "unexpected-pass" : "pass";
  return expectedReason ? "expected-fail" : "fail";
}

// ---------------------------------------------------------------------------
// The app table
// ---------------------------------------------------------------------------
// Steps run against the focused window of the app:
//   {op:"click"}                      click the middle of the window body
//   {op:"key", keys:"Control+l"}      Playwright key chord
//   {op:"type", text, delay?}         typed key by key
//   {op:"lines", count, text}         `count` lines, "{i}" is the line number
//   {op:"wait", ms}
// Text fields may use {marker} {savePath} {scrollUrl} {saveUrl} {i}.
// The flows are best effort and unverified on a live session until the first
// native run: adjust waits and keys here, not in the scenario.
const SCROLL_URL =
  'data:text/html,<body style="height:30000px;background:repeating-linear-gradient(%23fff,%23fff 40px,%23c33 40px,%23c33 80px)"><h1>{marker}</h1></body>';

const BROWSER_SAVE = (saveUrl) => [
  { op: "key", keys: "Control+l" },
  { op: "type", text: saveUrl },
  { op: "key", keys: "Enter" },
  { op: "wait", ms: 1500 },
  { op: "key", keys: "Control+s" },
  { op: "wait", ms: 2500 },
  { op: "key", keys: "Control+a" },
  { op: "type", text: "{savePath}", delay: 15 },
  { op: "key", keys: "Enter" },
  { op: "wait", ms: 1500 },
];

const EDITOR_SAVE = [
  { op: "key", keys: "Control+s" },
  { op: "wait", ms: 2500 },
  { op: "key", keys: "Control+a" },
  { op: "type", text: "{savePath}", delay: 15 },
  { op: "key", keys: "Enter" },
  { op: "wait", ms: 2000 },
];

export const APPS = [
  {
    key: "chrome",
    label: "Google Chrome",
    match: /google[- ]?chrome|^chrome$|\bchrome\b/i,
    prefer: /google[- ]?chrome/i,
    kind: "browser",
    ext: "html",
    content: "text",
    scrollFirst: "down",
    prepare: [
      { op: "wait", ms: 1500 },
      { op: "click" },
      { op: "key", keys: "Control+l" },
      { op: "type", text: "{scrollUrl}", delay: 5 },
      { op: "key", keys: "Enter" },
      { op: "wait", ms: 2000 },
    ],
    typeFocus: [{ op: "key", keys: "Control+l" }],
    typeCleanup: [{ op: "key", keys: "Escape" }, { op: "key", keys: "Escape" }],
    saveUrl: "data:text/html,<p>{marker}</p>",
    save: BROWSER_SAVE("data:text/html,<p>{marker}</p>"),
  },
  {
    key: "firefox",
    label: "Firefox",
    match: /firefox/i,
    kind: "browser",
    ext: "txt",
    content: "text",
    scrollFirst: "down",
    prepare: [
      { op: "wait", ms: 2500 },
      { op: "click" },
      { op: "key", keys: "Control+l" },
      { op: "type", text: "{scrollUrl}", delay: 5 },
      { op: "key", keys: "Enter" },
      { op: "wait", ms: 2000 },
    ],
    typeFocus: [{ op: "key", keys: "Control+l" }],
    typeCleanup: [{ op: "key", keys: "Escape" }, { op: "key", keys: "Escape" }],
    saveUrl: "data:text/plain,{marker}",
    save: BROWSER_SAVE("data:text/plain,{marker}"),
  },
  {
    key: "vscode",
    label: "Visual Studio Code",
    match: /visual studio code|vs ?code|\bcode\b/i,
    prefer: /visual studio code/i,
    kind: "editor",
    ext: "txt",
    content: "text",
    scrollFirst: "up",
    // Numbers only: no word-completion popup can swallow an Enter.
    prepare: [
      { op: "wait", ms: 3000 },
      { op: "click" },
      { op: "key", keys: "Control+n" },
      { op: "wait", ms: 800 },
      { op: "lines", count: 150, text: "{i}" },
    ],
    typeFocus: [],
    typeCleanup: [],
    save: EDITOR_SAVE,
  },
  {
    key: "libreoffice",
    label: "LibreOffice Writer",
    match: /libre ?office|\bwriter\b|soffice/i,
    prefer: /writer/i,
    kind: "editor",
    ext: "odt",
    content: "odt",
    scrollFirst: "up",
    prepare: [
      { op: "wait", ms: 4000 },
      { op: "key", keys: "Escape" },
      { op: "click" },
      { op: "lines", count: 120, text: "{i}" },
    ],
    typeFocus: [],
    typeCleanup: [],
    save: EDITOR_SAVE,
  },
];

export const CHECK_IDS = ["launcher", "window", "typing", "saved", "closed"];

/** Pick the API row for an app: first match, `prefer` rows first. */
export function findAppRow(rows, app) {
  const candidates = (rows ?? []).filter((row) => app.match.test(String(row.name ?? "")) || app.match.test(String(row.id ?? "")));
  if (candidates.length === 0) return null;
  const preferred = app.prefer ? candidates.filter((row) => app.prefer.test(`${row.name} ${row.id}`)) : [];
  return (preferred[0] ?? candidates[0]) || null;
}

export function expandText(text, vars) {
  return String(text).replace(/\{(\w+)\}/g, (whole, name) => (name in vars ? String(vars[name]) : whole));
}

export function expandSteps(steps, vars) {
  return steps.map((step) => {
    const out = { ...step };
    if (typeof out.text === "string") out.text = expandText(out.text, vars);
    if (typeof out.keys === "string") out.keys = expandText(out.keys, vars);
    return out;
  });
}

/** The page browsers scroll: tall, striped, the marker as its heading. */
export function scrollUrl(marker) {
  return expandText(SCROLL_URL, { marker });
}

export function makeMarker(runId, appKey) {
  return `rast-matrix-${runId}-${appKey}`.toLowerCase();
}

// ---------------------------------------------------------------------------
// Native session helpers
// ---------------------------------------------------------------------------
/** Active runs of an app in `GET /api/linux-apps/session`. */
export function activeRunsOf(session, appId) {
  const runs = session?.apps ?? session?.runs ?? [];
  if (!Array.isArray(runs)) return [];
  return runs.filter((run) => run.appId === appId && ["launching", "running", "starting"].includes(String(run.status)));
}

// ---------------------------------------------------------------------------
// ZIP / ODT reading (no dependency): enough to read content.xml of an .odt
// ---------------------------------------------------------------------------
export function readZipEntry(buffer, name) {
  let eocd = -1;
  for (let i = buffer.length - 22; i >= Math.max(0, buffer.length - 22 - 65535); i -= 1) {
    if (buffer.readUInt32LE(i) === 0x06054b50) {
      eocd = i;
      break;
    }
  }
  if (eocd < 0) return null;
  const count = buffer.readUInt16LE(eocd + 10);
  let offset = buffer.readUInt32LE(eocd + 16);
  for (let n = 0; n < count; n += 1) {
    if (offset + 46 > buffer.length || buffer.readUInt32LE(offset) !== 0x02014b50) return null;
    const method = buffer.readUInt16LE(offset + 10);
    const compressedSize = buffer.readUInt32LE(offset + 20);
    const nameLength = buffer.readUInt16LE(offset + 28);
    const extraLength = buffer.readUInt16LE(offset + 30);
    const commentLength = buffer.readUInt16LE(offset + 32);
    const localOffset = buffer.readUInt32LE(offset + 42);
    const entryName = buffer.toString("utf8", offset + 46, offset + 46 + nameLength);
    if (entryName === name) {
      if (buffer.readUInt32LE(localOffset) !== 0x04034b50) return null;
      const dataStart = localOffset + 30 + buffer.readUInt16LE(localOffset + 26) + buffer.readUInt16LE(localOffset + 28);
      const data = buffer.subarray(dataStart, dataStart + compressedSize);
      if (method === 0) return Buffer.from(data);
      if (method === 8) return inflateRawSync(data);
      return null;
    }
    offset += 46 + nameLength + extraLength + commentLength;
  }
  return null;
}

export function odtText(buffer) {
  const xml = readZipEntry(buffer, "content.xml");
  if (!xml) return null;
  return xml.toString("utf8").replace(/<[^>]+>/g, "");
}

export function containsMarker(text, marker) {
  return typeof text === "string" && text.toLowerCase().includes(String(marker).toLowerCase());
}

// ---------------------------------------------------------------------------
// Results
// ---------------------------------------------------------------------------
export function emptyChecks(reason = null) {
  const checks = {};
  for (const id of CHECK_IDS) checks[id] = { status: "skip", reason };
  return checks;
}

export function summarize(appResults, budgetsMode = "enforce") {
  const checks = { pass: 0, fail: 0, "expected-fail": 0, "unexpected-pass": 0, skip: 0 };
  const budgets = { pass: 0, fail: 0, unmeasured: 0 };
  for (const app of appResults) {
    for (const check of Object.values(app.checks ?? {})) checks[check.status] = (checks[check.status] ?? 0) + 1;
    for (const row of app.responsiveness?.budgets ?? []) budgets[row.status] += 1;
  }
  const ok = checks.fail === 0 && (budgetsMode === "report" || budgets.fail === 0);
  return { checks, budgets, budgetsMode, ok };
}

export function buildResult({ meta, environment, apps, budgetsMode = "enforce" }) {
  return {
    schema: SCHEMA,
    ...meta,
    environment,
    budgets: BUDGETS.map(({ id, label, unit, op, limit }) => ({ id, label, unit, op, limit })),
    apps,
    summary: summarize(apps, budgetsMode),
  };
}

const STATUS_LABEL = {
  pass: "PASS",
  fail: "FAIL",
  "expected-fail": "XFAIL",
  "unexpected-pass": "XPASS",
  skip: "SKIP",
};

function cell(value, unit = "") {
  return value === null || value === undefined ? "-" : `${value}${unit}`;
}

function esc(text) {
  return String(text ?? "").replace(/\|/g, "\\|").replace(/\n/g, " ");
}

export function renderMatrixMarkdown(result) {
  const lines = [
    `# RAST app matrix: ${result.mode} backend, ${result.homeMode} home`,
    "",
    `Generated ${result.finishedAt} against ${result.baseUrl} as \`${result.user}\`` +
      `${result.production ? " (production, test account)" : ""}. RAST ${result.environment?.rastVersion ?? "?"}.`,
    "",
    `Browser: devicePixelRatio ${cell(result.environment?.devicePixelRatio)}, viewport ${cell(result.environment?.viewport)}.`,
    "",
    "## Checks",
    "",
    "| App | " + ["Launcher", "RAST window", "Typing", "Saved file in File Station", "Closes"].join(" | ") + " |",
    "|---|---|---|---|---|---|",
  ];
  for (const app of result.apps) {
    const row = CHECK_IDS.map((id) => {
      const check = app.checks[id];
      return `**${STATUS_LABEL[check.status] ?? check.status}**`;
    });
    lines.push(`| ${esc(app.label)} | ${row.join(" | ")} |`);
  }
  lines.push("", "XFAIL = expected to fail in this mode (reason below); XPASS = passed although expected to fail.", "");
  const notes = [];
  for (const app of result.apps) {
    for (const id of CHECK_IDS) {
      const check = app.checks[id];
      if (check.reason || check.detail) notes.push(`- ${app.label}, ${id}: ${esc(check.reason ?? "")} ${esc(check.detail ?? "")}`.trimEnd());
    }
  }
  if (notes.length > 0) lines.push("### Notes", "", ...notes, "");

  lines.push(
    "## Responsiveness",
    "",
    "| App | Method | Typing p50 / p95 / max (ms) | Scroll p50 / p95 / max (ms) | Scroll fps | Drag fps | Repaint after drop (ms) | Window | Xpra |",
    "|---|---|---|---|---|---|---|---|---|",
  );
  for (const app of result.apps) {
    const r = app.responsiveness;
    if (!r) {
      lines.push(`| ${esc(app.label)} | not measured | | | | | | | |`);
      continue;
    }
    const typing = r.typing?.stats;
    const scroll = r.scroll?.latency;
    const xpra = r.environment?.xpra;
    lines.push(
      `| ${esc(app.label)} | ${esc(r.method)} | ${cell(typing?.p50)} / ${cell(typing?.p95)} / ${cell(typing?.max)} | ` +
        `${cell(scroll?.p50)} / ${cell(scroll?.p95)} / ${cell(scroll?.max)} | ${cell(r.scroll?.fps)} | ${cell(r.drag?.fps)} | ` +
        `${r.drag?.repaint === "none-needed" ? "none needed" : cell(r.drag?.repaintMs)} | ` +
        `${esc(r.environment?.windowSize ?? "-")} | ${esc(xpra ? `${xpra.encoding ?? "?"}, dpi ${xpra.dpi ?? "?"}` : "-")} |`,
    );
  }
  lines.push("", "## Budgets", "", "| App | Budget | Limit | Measured | Result |", "|---|---|---|---|---|");
  for (const app of result.apps) {
    for (const row of app.responsiveness?.budgets ?? []) {
      lines.push(
        `| ${esc(app.label)} | ${esc(row.label)} | ${row.op} ${row.limit} ${row.unit} | ${cell(row.value, ` ${row.unit}`)} | ${row.status.toUpperCase()} |`,
      );
    }
  }
  const s = result.summary;
  lines.push(
    "",
    `Totals: checks ${s.checks.pass} pass, ${s.checks.fail} fail, ${s.checks["expected-fail"]} expected fail, ` +
      `${s.checks["unexpected-pass"]} unexpected pass, ${s.checks.skip} skip; budgets ${s.budgets.pass} pass, ` +
      `${s.budgets.fail} fail, ${s.budgets.unmeasured} unmeasured (${s.budgetsMode}). Overall: ${s.ok ? "OK" : "FAILED"}.`,
    "",
  );
  return lines.join("\n");
}
