// Unit tests for the pure parts of the app matrix. No browser, no network:
//   node --test tests/e2e/lib/

import assert from "node:assert/strict";
import { spawnSync } from "node:child_process";
import { deflateRawSync } from "node:zlib";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import test from "node:test";

import {
  APPS,
  BUDGETS,
  CHECK_IDS,
  LEGACY_SAVE_REASON,
  MATRIX_OPTION_NAMES,
  SCHEMA,
  activeRunsOf,
  assertAllowedApiCall,
  assertTargetGuard,
  buildResult,
  classify,
  containsMarker,
  detectHomeMode,
  emptyChecks,
  evaluateBudgets,
  expandSteps,
  expectedFailReason,
  findAppRow,
  isAllowedApiCall,
  isProductionTarget,
  latenciesFromEvents,
  makeMarker,
  movesPerSecond,
  odtText,
  parseCredsFile,
  parseMatrixOptions,
  percentile,
  planSavePaths,
  rateInWindow,
  readZipEntry,
  renderMatrixMarkdown,
  resolveCredentials,
  scrollUrl,
  stats,
  summarize,
} from "./app-matrix-core.mjs";

const E2E_DIR = join(dirname(fileURLToPath(import.meta.url)), "..");

// ---------------------------------------------------------------- statistics
test("percentile uses the nearest rank and ignores non-finite values", () => {
  const values = Array.from({ length: 20 }, (_, i) => i + 1); // 1..20
  assert.equal(percentile(values, 50), 10);
  assert.equal(percentile(values, 95), 19);
  assert.equal(percentile(values, 100), 20);
  assert.equal(percentile([5], 95), 5);
  assert.equal(percentile([], 50), null);
  assert.equal(percentile([3, NaN, 1], 100), 3);
});

test("stats summarises samples and handles an empty list", () => {
  assert.deepEqual(stats([10, 20, 30, 40]), { n: 4, min: 10, p50: 20, p95: 40, max: 40, mean: 25 });
  assert.deepEqual(stats([]), { n: 0, min: null, p50: null, p95: null, max: null, mean: null });
});

test("latenciesFromEvents pairs each trigger with the first change before the next one", () => {
  const result = latenciesFromEvents([0, 300, 600], [40, 330, 910], 1000);
  assert.deepEqual(result.samples, [40, 30, 310]); // the last trigger is bounded by the 1000 ms cap only
  assert.equal(result.missed, 0);
});

test("latenciesFromEvents counts a trigger without a change as missed, at its window length", () => {
  const result = latenciesFromEvents([0, 300], [20], 1000);
  assert.equal(result.missed, 1);
  assert.deepEqual(result.samples, [20, 1000]);
  // A change after the next trigger does not belong to the earlier one.
  const late = latenciesFromEvents([0, 300], [350], 1000);
  assert.equal(late.missed, 1);
  assert.deepEqual(late.samples, [300, 50]);
});

test("rateInWindow and movesPerSecond", () => {
  assert.equal(rateInWindow([0, 100, 200, 300, 400], 0, 400), 12.5);
  assert.equal(rateInWindow([], 0, 400), 0);
  assert.equal(rateInWindow([1], 5, 5), 0);
  const rects = [];
  for (let i = 0; i < 61; i += 1) rects.push({ t: i * 16, x: i, y: 0 }); // moves every frame
  rects.push({ t: 61 * 16, x: 60, y: 0 }); // standing still afterwards
  const moves = movesPerSecond(rects);
  assert.equal(moves.frames, 60);
  assert.ok(moves.fps > 60 && moves.fps < 64, `fps ${moves.fps}`);
  assert.deepEqual(movesPerSecond([{ t: 0, x: 0, y: 0 }]), { fps: 0, frames: 0, durationMs: 0 });
});

// ------------------------------------------------------------------- budgets
const GOOD = {
  typing: { stats: { p95: 120 } },
  scroll: { latency: { p95: 180 }, fps: 20 },
  drag: { fps: 45, repaint: "repainted", repaintMs: 200 },
};

test("the budget table keeps the agreed limits in one place", () => {
  const byId = Object.fromEntries(BUDGETS.map((b) => [b.id, `${b.op} ${b.limit}`]));
  assert.deepEqual(byId, {
    "typing.p95": "<= 150",
    "scroll.p95": "<= 200",
    "scroll.fps": ">= 15",
    "drag.fps": ">= 30",
    "drag.repaint": "<= 250",
  });
});

test("evaluateBudgets passes inside the limits, fails outside, boundaries are inclusive", () => {
  assert.ok(evaluateBudgets(GOOD).every((row) => row.status === "pass"));
  const edge = evaluateBudgets({
    typing: { stats: { p95: 150 } },
    scroll: { latency: { p95: 200 }, fps: 15 },
    drag: { fps: 30, repaint: "repainted", repaintMs: 250 },
  });
  assert.ok(edge.every((row) => row.status === "pass"));
  const bad = evaluateBudgets({
    typing: { stats: { p95: 151 } },
    scroll: { latency: { p95: 201 }, fps: 14.9 },
    drag: { fps: 29.9, repaint: "repainted", repaintMs: 251 },
  });
  assert.ok(bad.every((row) => row.status === "fail"));
});

test("evaluateBudgets marks missing data as unmeasured and no repaint as 0 ms", () => {
  assert.ok(evaluateBudgets(null).every((row) => row.status === "unmeasured"));
  const partial = evaluateBudgets({ typing: { stats: { p95: 100 } }, scroll: { latency: { p95: 50 }, fps: null }, drag: { fps: 60, repaint: "none-needed", repaintMs: null } });
  const byId = Object.fromEntries(partial.map((row) => [row.id, row]));
  assert.equal(byId["scroll.fps"].status, "unmeasured");
  assert.equal(byId["drag.repaint"].status, "pass");
  assert.equal(byId["drag.repaint"].value, 0);
  assert.equal(byId["typing.p95"].status, "pass");
});

// ---------------------------------------------------------------- the guard
test("production targets: ports 8720/8721, desktop.lufria.tech, any non-loopback host", () => {
  assert.equal(isProductionTarget("http://127.0.0.1:8720"), true);
  assert.equal(isProductionTarget("http://127.0.0.1:8721/"), true);
  assert.equal(isProductionTarget("https://desktop.lufria.tech"), true);
  assert.equal(isProductionTarget("http://localhost:8720"), true);
  assert.equal(isProductionTarget("http://192.0.2.10:8731"), true);
  assert.equal(isProductionTarget("not a url"), true);
  assert.equal(isProductionTarget("http://127.0.0.1:8731"), false);
  assert.equal(isProductionTarget("http://127.0.0.1:8732"), false);
  assert.equal(isProductionTarget("http://localhost:8731"), false);
});

test("the guard accepts t-alice on production and refuses everything else there", () => {
  assert.deepEqual(assertTargetGuard("http://127.0.0.1:8720", "t-alice"), { production: true });
  assert.throws(() => assertTargetGuard("http://127.0.0.1:8720", "alice"), /test accounts/);
  assert.throws(() => assertTargetGuard("http://127.0.0.1:8720", "admin"), /test accounts/);
  assert.throws(() => assertTargetGuard("https://desktop.lufria.tech", "enricko"), /test accounts/);
  assert.throws(() => assertTargetGuard("http://127.0.0.1:8720", "t-admin"), /t-admin/);
  assert.throws(() => assertTargetGuard("http://127.0.0.1:8720", undefined), /test accounts/);
  // On a dev instance any account is fine.
  assert.deepEqual(assertTargetGuard("http://127.0.0.1:8731", "alice"), { production: false });
});

test("an error message never contains credentials or a path", () => {
  try {
    assertTargetGuard("http://user:secret@127.0.0.1:8720/x?token=abc", "alice");
    assert.fail("should throw");
  } catch (error) {
    assert.doesNotMatch(error.message, /secret|token|abc/);
  }
});

test("the API allow-list has reads, mkdir and the End application of a leftover run only", () => {
  for (const [method, path] of [
    ["GET", "/api/linux-apps"],
    ["GET", "/api/linux-apps/session"],
    ["GET", "/api/linux-apps/chrome"],
    ["GET", "/api/files?path=%2Frast-matrix&perPage=500"],
    ["GET", "/api/files/text?path=%2Fa.txt"],
    ["GET", "/api/files/download?path=%2Fa.odt"],
    ["GET", "/api/files/usage"],
    ["POST", "/api/files/mkdir"],
    ["POST", "/api/auth/login"],
    ["POST", "/api/linux-apps/session/runs/run-1/close"],
  ]) {
    assert.equal(isAllowedApiCall(method, path), true, `${method} ${path}`);
  }
  for (const [method, path] of [
    ["POST", "/api/linux-apps/install"],
    ["POST", "/api/linux-apps/preview"],
    ["GET", "/api/linux-apps/install"],
    ["DELETE", "/api/linux-apps/chrome"],
    ["POST", "/api/linux-apps/chrome/update"],
    ["POST", "/api/linux-apps/chrome/visibility"],
    ["PATCH", "/api/linux-apps/chrome"],
    ["GET", "/api/users"],
    ["POST", "/api/users"],
    ["GET", "/api/admin/anything"],
    ["POST", "/api/files/delete"],
    ["POST", "/api/files/upload"],
    ["POST", "/api/linux-apps/session/close"],
  ]) {
    assert.equal(isAllowedApiCall(method, path), false, `${method} ${path}`);
  }
  assert.equal(isAllowedApiCall("POST", "/api/files/delete", { cleanup: true }), true);
  assert.throws(() => assertAllowedApiCall("POST", "/api/linux-apps/install?x=1"), /not allowed: POST \/api\/linux-apps\/install$/);
});

test("credentials come from a KEY=VALUE file by username; the guard runs before the file is read", () => {
  const creds = parseCredsFile("# test accounts\nt-admin=aaa\nt-alice=bbb\n\nt-bob = ccc\nbad line\n");
  assert.deepEqual(creds, { "t-admin": "aaa", "t-alice": "bbb", "t-bob": "ccc" });
  let reads = 0;
  const reader = () => {
    reads += 1;
    return "t-alice=bbb\nalice=zzz\n";
  };
  const config = { baseUrl: "http://127.0.0.1:8720", users: [] };
  assert.deepEqual(resolveCredentials({ user: "t-alice", passwordFile: "/x" }, config, reader), {
    username: "t-alice",
    password: "bbb",
    production: true,
  });
  reads = 0;
  assert.throws(() => resolveCredentials({ user: "alice", passwordFile: "/x" }, config, reader), /test accounts/);
  assert.equal(reads, 0, "the password file must not be read for a refused account");
  assert.throws(() => resolveCredentials({ user: "t-bob", passwordFile: "/x" }, config, reader), /no entry for "t-bob"/);
  const dev = { baseUrl: "http://127.0.0.1:8731", users: [{ username: "alice", password: "pw" }] };
  assert.equal(resolveCredentials({}, dev, reader).username, "alice");
  assert.throws(() => resolveCredentials({ user: "carol" }, dev, reader), /no password for "carol"/);
});

// ------------------------------------------------------------------- options
test("parseMatrixOptions: defaults, values and errors", () => {
  const defaults = parseMatrixOptions([]);
  assert.equal(defaults.apps, null);
  assert.equal(defaults.homeMode, "auto");
  assert.equal(defaults.budgets, "enforce");
  assert.equal(defaults.keys, 30);
  assert.equal(defaults.responsiveness, true);
  const set = parseMatrixOptions([
    "--apps=chrome,vscode",
    "--user=t-alice",
    "--password-file=/home/x/creds",
    "--home-mode=one-home",
    "--home-path=/srv/rast/homes/t-alice/",
    "--budgets=report",
    "--keys=40",
    "--launch-timeout=90",
    "--cleanup",
    "--no-responsiveness",
    "--plan",
  ]);
  assert.deepEqual(set.apps, ["chrome", "vscode"]);
  assert.equal(set.user, "t-alice");
  assert.equal(set.passwordFile, "/home/x/creds");
  assert.equal(set.homeMode, "one-home");
  assert.equal(set.homePath, "/srv/rast/homes/t-alice");
  assert.equal(set.budgets, "report");
  assert.equal(set.keys, 40);
  assert.equal(set.launchTimeoutMs, 90_000);
  assert.equal(set.cleanup, true);
  assert.equal(set.responsiveness, false);
  assert.equal(set.plan, true);
  assert.throws(() => parseMatrixOptions(["--apps=chrome,gimp"]), /unknown app\(s\) gimp/);
  assert.throws(() => parseMatrixOptions(["--apps="]), /--apps/);
  assert.throws(() => parseMatrixOptions(["--home-mode=both"]), /--home-mode/);
  assert.throws(() => parseMatrixOptions(["--home-path=relative"]), /absolute/);
  assert.throws(() => parseMatrixOptions(["--keys=2"]), /--keys/);
  assert.throws(() => parseMatrixOptions(["--bogus"]), /unknown app matrix option/);
  // Every name the runner passes through is understood by the parser.
  for (const name of MATRIX_OPTION_NAMES) {
    const needsValue = ["--apps", "--user", "--password-file", "--home-mode", "--home-path", "--budgets", "--keys", "--launch-timeout"].includes(name);
    const sample = { "--apps": "chrome", "--user": "u", "--password-file": "/p", "--home-mode": "legacy", "--home-path": "/h", "--budgets": "report", "--keys": "10", "--launch-timeout": "60" }[name];
    assert.doesNotThrow(() => parseMatrixOptions([needsValue ? `${name}=${sample}` : name]), name);
  }
});

// ---------------------------------------------------- modes and expected fail
test("home mode is detected from the usage breakdown", () => {
  assert.equal(detectHomeMode({ byLocation: { files: 1, trash: 0 } }), "legacy");
  assert.equal(detectHomeMode({ byLocation: { files: 1, apps: 2, caches: 0, trash: 0 } }), "one-home");
  assert.equal(detectHomeMode({ byLocation: { files: 1, shared: 5 } }), "one-home");
  assert.equal(detectHomeMode({}), "legacy");
  assert.equal(detectHomeMode(null), "legacy");
});

test("save paths: one-HOME saves in a folder of HOME, legacy only observes", () => {
  assert.deepEqual(planSavePaths({ homeMode: "one-home", homePath: null, username: "t-alice", fileName: "f.txt" }), {
    appPath: "/srv/rast/homes/t-alice/rast-matrix/f.txt",
    fsDir: "/rast-matrix",
    fsPath: "/rast-matrix/f.txt",
    mkdir: true,
  });
  assert.equal(planSavePaths({ homeMode: "one-home", homePath: "/data/h", username: "t-alice", fileName: "f.txt" }).appPath, "/data/h/rast-matrix/f.txt");
  assert.deepEqual(planSavePaths({ homeMode: "legacy", homePath: null, username: "t-alice", fileName: "f.txt" }), {
    appPath: "~/f.txt",
    fsDir: "/",
    fsPath: "/f.txt",
    mkdir: false,
  });
});

test("legacy accounts are expected to fail the saved-file check, nothing else", () => {
  assert.equal(expectedFailReason({ homeMode: "legacy", check: "saved" }), LEGACY_SAVE_REASON);
  for (const check of CHECK_IDS.filter((id) => id !== "saved")) {
    assert.equal(expectedFailReason({ homeMode: "legacy", check }), null, check);
  }
  assert.equal(expectedFailReason({ homeMode: "one-home", check: "saved" }), null);
  assert.equal(classify(true, null), "pass");
  assert.equal(classify(false, null), "fail");
  assert.equal(classify(false, "reason"), "expected-fail");
  assert.equal(classify(true, "reason"), "unexpected-pass");
});

// ------------------------------------------------------------------ app table
test("the app table covers the four matrix apps with valid flows", () => {
  assert.deepEqual(APPS.map((app) => app.key), ["chrome", "firefox", "vscode", "libreoffice"]);
  const ops = new Set(["click", "key", "type", "lines", "wait"]);
  for (const app of APPS) {
    assert.ok(app.label && app.ext && app.content && app.scrollFirst, app.key);
    for (const steps of [app.prepare, app.typeFocus, app.typeCleanup, app.save]) {
      assert.ok(Array.isArray(steps), app.key);
      for (const step of steps) assert.ok(ops.has(step.op), `${app.key}: ${step.op}`);
    }
    assert.ok(app.save.some((step) => step.text === "{savePath}"), `${app.key} types the save path`);
  }
});

test("findAppRow matches by name or id and prefers the exact product", () => {
  const rows = [
    { id: "libreoffice-calc", name: "LibreOffice Calc" },
    { id: "libreoffice-writer", name: "LibreOffice Writer" },
    { id: "google-chrome", name: "Google Chrome" },
    { id: "chromium", name: "Chromium" },
    { id: "code", name: "Visual Studio Code" },
    { id: "firefox", name: "Firefox" },
  ];
  const byKey = Object.fromEntries(APPS.map((app) => [app.key, app]));
  assert.equal(findAppRow(rows, byKey.libreoffice).id, "libreoffice-writer");
  assert.equal(findAppRow(rows, byKey.chrome).id, "google-chrome");
  assert.equal(findAppRow(rows, byKey.vscode).id, "code");
  assert.equal(findAppRow(rows, byKey.firefox).id, "firefox");
  assert.equal(findAppRow([{ id: "gimp", name: "GIMP" }], byKey.chrome), null);
  assert.equal(findAppRow([], byKey.chrome), null);
});

test("expandSteps fills placeholders and leaves unknown ones alone", () => {
  const marker = makeMarker("ab12cd", "Chrome");
  assert.equal(marker, "rast-matrix-ab12cd-chrome");
  const steps = expandSteps(
    [{ op: "type", text: "{savePath}" }, { op: "type", text: "x{marker}{nope}" }, { op: "key", keys: "Enter" }],
    { marker, savePath: "/h/f.txt" },
  );
  assert.deepEqual(steps.map((s) => s.text ?? s.keys), ["/h/f.txt", `x${marker}{nope}`, "Enter"]);
  assert.match(scrollUrl(marker), /^data:text\/html,<body style="height:30000px.*%23fff.*<h1>rast-matrix-ab12cd-chrome<\/h1>/);
  assert.ok(!scrollUrl(marker).includes("#"), "# would start a URL fragment");
});

test("containsMarker is case-insensitive (autocapitalisation)", () => {
  assert.equal(containsMarker("Rast-matrix-ab12cd-vscode more", "rast-matrix-ab12cd-vscode"), true);
  assert.equal(containsMarker("other", "rast-matrix-ab12cd-vscode"), false);
  assert.equal(containsMarker(null, "x"), false);
});

// ------------------------------------------------------------------- sessions
test("activeRunsOf counts only the app's launching or running runs", () => {
  const session = {
    apps: [
      { id: "r1", appId: "code", status: "running" },
      { id: "r2", appId: "code", status: "exited" },
      { id: "r3", appId: "firefox", status: "running" },
      { id: "r4", appId: "code", status: "launching" },
    ],
  };
  assert.deepEqual(activeRunsOf(session, "code").map((r) => r.id), ["r1", "r4"]);
  assert.deepEqual(activeRunsOf(null, "code"), []);
  assert.deepEqual(activeRunsOf({ apps: "x" }, "code"), []);
});

// ------------------------------------------------------------------ ODT / zip
function zip(entries) {
  const locals = [];
  const centrals = [];
  let offset = 0;
  for (const [name, content, method] of entries) {
    const nameBuf = Buffer.from(name);
    const raw = Buffer.from(content);
    const data = method === 8 ? deflateRawSync(raw) : raw;
    const local = Buffer.alloc(30);
    local.writeUInt32LE(0x04034b50, 0);
    local.writeUInt16LE(method, 8);
    local.writeUInt32LE(data.length, 18);
    local.writeUInt32LE(raw.length, 22);
    local.writeUInt16LE(nameBuf.length, 26);
    locals.push(local, nameBuf, data);
    const central = Buffer.alloc(46);
    central.writeUInt32LE(0x02014b50, 0);
    central.writeUInt16LE(method, 10);
    central.writeUInt32LE(data.length, 20);
    central.writeUInt32LE(raw.length, 24);
    central.writeUInt16LE(nameBuf.length, 28);
    central.writeUInt32LE(offset, 42);
    centrals.push(central, nameBuf);
    offset += 30 + nameBuf.length + data.length;
  }
  const centralBytes = Buffer.concat(centrals);
  const end = Buffer.alloc(22);
  end.writeUInt32LE(0x06054b50, 0);
  end.writeUInt16LE(entries.length, 8);
  end.writeUInt16LE(entries.length, 10);
  end.writeUInt32LE(centralBytes.length, 12);
  end.writeUInt32LE(offset, 16);
  return Buffer.concat([...locals, centralBytes, end]);
}

test("readZipEntry reads stored and deflated entries; odtText strips the XML", () => {
  const xml = '<office:text><text:p>Rast-matrix-ab12cd-libreoffice</text:p><text:p>00001</text:p></office:text>';
  const archive = zip([
    ["mimetype", "application/vnd.oasis.opendocument.text", 0],
    ["content.xml", xml, 8],
  ]);
  assert.equal(readZipEntry(archive, "mimetype").toString(), "application/vnd.oasis.opendocument.text");
  assert.equal(readZipEntry(archive, "content.xml").toString(), xml);
  assert.equal(readZipEntry(archive, "missing.xml"), null);
  assert.equal(odtText(archive), "Rast-matrix-ab12cd-libreoffice00001");
  assert.equal(containsMarker(odtText(archive), "rast-matrix-ab12cd-libreoffice"), true);
  assert.equal(readZipEntry(Buffer.from("not a zip"), "content.xml"), null);
});

// -------------------------------------------------------------------- results
function sampleApp(overrides = {}) {
  return {
    key: "chrome",
    label: "Google Chrome",
    appId: "google-chrome",
    checks: {
      launcher: { status: "pass" },
      window: { status: "pass", ms: 4200 },
      typing: { status: "pass", ms: 90 },
      saved: { status: "expected-fail", reason: LEGACY_SAVE_REASON },
      closed: { status: "pass" },
    },
    responsiveness: {
      method: "canvas",
      typing: { stats: stats([80, 90, 100]) },
      scroll: { latency: stats([100, 120]), fps: 22 },
      drag: { fps: 58, repaint: "none-needed", repaintMs: null },
      environment: { windowSize: "900x600", devicePixelRatio: 1, xpra: { encoding: "auto", dpi: 96 } },
      budgets: [],
    },
    ...overrides,
  };
}

test("buildResult has the documented JSON shape and summarize decides ok", () => {
  const app = sampleApp();
  app.responsiveness.budgets = evaluateBudgets(app.responsiveness);
  const result = buildResult({
    meta: { scenario: "app-matrix", mode: "native", homeMode: "legacy", baseUrl: "http://127.0.0.1:8720", user: "t-alice", production: true, runId: "ab12cd", startedAt: "a", finishedAt: "b" },
    environment: { devicePixelRatio: 1, viewport: "1480x920", rastVersion: "1.0.1-alpha.1" },
    apps: [app],
  });
  assert.equal(result.schema, SCHEMA);
  assert.deepEqual(Object.keys(result).sort(), ["apps", "baseUrl", "budgets", "environment", "finishedAt", "homeMode", "mode", "production", "runId", "scenario", "schema", "startedAt", "summary", "user"]);
  assert.deepEqual(result.budgets.map((b) => b.id), BUDGETS.map((b) => b.id));
  assert.equal(result.summary.checks["expected-fail"], 1);
  assert.equal(result.summary.checks.pass, 4);
  assert.equal(result.summary.ok, true);
  // JSON round trip keeps the shape.
  assert.deepEqual(JSON.parse(JSON.stringify(result)).summary, result.summary);
});

test("summarize: a failed check or (when enforced) a failed budget fails the run", () => {
  const failing = sampleApp({ checks: { ...sampleApp().checks, typing: { status: "fail", detail: "x" } } });
  assert.equal(summarize([failing]).ok, false);
  const slow = sampleApp();
  slow.responsiveness.typing = { stats: stats([400]) };
  slow.responsiveness.budgets = evaluateBudgets(slow.responsiveness);
  assert.equal(summarize([slow], "enforce").ok, false);
  assert.equal(summarize([slow], "report").ok, true);
  assert.equal(summarize([sampleApp({ checks: { ...sampleApp().checks, saved: { status: "unexpected-pass" } } })]).ok, true);
  assert.equal(emptyChecks("why").launcher.reason, "why");
  assert.deepEqual(Object.keys(emptyChecks()), CHECK_IDS);
});

test("renderMatrixMarkdown lists checks, responsiveness and budgets", () => {
  const app = sampleApp();
  app.responsiveness.budgets = evaluateBudgets(app.responsiveness);
  const result = buildResult({
    meta: { scenario: "app-matrix", mode: "native", homeMode: "legacy", baseUrl: "http://127.0.0.1:8720", user: "t-alice", production: true, runId: "ab12cd", startedAt: "a", finishedAt: "2026-11-17T10:00:00Z" },
    environment: { devicePixelRatio: 1, viewport: "1480x920", rastVersion: "1.0.1-alpha.1" },
    apps: [app],
  });
  const markdown = renderMatrixMarkdown(result);
  assert.match(markdown, /# RAST app matrix: native backend, legacy home/);
  assert.match(markdown, /\| Google Chrome \| \*\*PASS\*\* \| \*\*PASS\*\* \| \*\*PASS\*\* \| \*\*XFAIL\*\* \| \*\*PASS\*\* \|/);
  assert.match(markdown, /Scroll p50/);
  assert.match(markdown, /none needed/);
  assert.match(markdown, /typing p95, keydown to first changed pixel \| <= 150 ms/);
  assert.match(markdown, /Overall: OK/);
  assert.doesNotMatch(markdown, /password/i);
});

// ------------------------------------------------------------------ the runner
function runRunner(args) {
  return spawnSync(process.execPath, [join(E2E_DIR, "run.mjs"), ...args], { encoding: "utf8", timeout: 30_000 });
}

test("run.mjs --list shows app-matrix, and the default selection leaves it out", () => {
  const listed = runRunner(["--list"]);
  assert.equal(listed.status, 0);
  assert.match(listed.stdout, /^app-matrix\t/m);
});

test("run.mjs --plan prints the plan and the guard verdict without contacting anything", () => {
  const ok = runRunner(["--scenario=app-matrix", "--plan", "--base-url=http://127.0.0.1:8720", "--user=t-alice", "--apps=chrome"]);
  assert.equal(ok.status, 0, ok.stderr);
  assert.match(ok.stdout, /guard:\s+ok \(production target, test account\)/);
  assert.match(ok.stdout, /chrome \(Google Chrome\)/);
  assert.doesNotMatch(ok.stdout, /firefox/);
  const refused = runRunner(["--scenario=app-matrix", "--plan", "--base-url=http://127.0.0.1:8720", "--user=alice"]);
  assert.match(refused.stdout, /guard:\s+REFUSED/);
});

test("run.mjs refuses scenarios that are not productionSafe on the production URL", () => {
  const refused = runRunner(["--scenario=d", "--base-url=http://127.0.0.1:8720"]);
  assert.equal(refused.status, 2);
  assert.match(refused.stderr, /refusing to run d/);
});
