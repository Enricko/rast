// app-matrix (task 0.6): the five checks of CLAUDE.md per app, plus the
// responsiveness measurements, for Chrome, Firefox, VS Code and LibreOffice.
//
//   1. appears in the launcher (start menu tile, not only the API);
//   2. opens as a RAST window (native Xpra window adopted, painted);
//   3. accepts typing (pixels change while a marker string is typed);
//   4. a file the app saves is visible in File Station within 5 s, with the
//      typed marker inside;
//   5. closes normally (window gone, app run ended, no error toast).
//
// Standalone: it needs no administrator. It runs against the dev instance and
// against production with a test account (t-*), and only uses apps that are
// already installed and visible to that account. See tests/e2e/results/README.md.
//
//   node tests/e2e/run.mjs --scenario=app-matrix --plan --user=t-alice --base-url=http://127.0.0.1:8720
//   node tests/e2e/run.mjs --scenario=app-matrix --mode=native \
//     --base-url=http://127.0.0.1:8720 --user=t-alice --password-file=$HOME/.config/rast-test/creds

import { randomBytes } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";

import {
  APPS,
  BUDGETS,
  CHECK_IDS,
  LEGACY_SAVE_REASON,
  activeRunsOf,
  assertTargetGuard,
  buildResult,
  classify,
  containsMarker,
  detectHomeMode,
  emptyChecks,
  evaluateBudgets,
  expectedFailReason,
  findAppRow,
  makeMarker,
  odtText,
  parseMatrixOptions,
  planSavePaths,
  redactUrl,
  scrollUrl,
  renderMatrixMarkdown,
  resolveCredentials,
} from "../lib/app-matrix-core.mjs";
import {
  SafeApi,
  bodyBox,
  bodyBuffer,
  clickBody,
  closeStartMenuQuietly,
  closeWindowByControl,
  findLauncherTile,
  measureDrag,
  measureScroll,
  measureTyping,
  openMatrixDesktop,
  readEnvironment,
  readXpra,
  runSteps,
  sleep,
  waitBodyChange,
  waitWindowDetached,
} from "../lib/app-matrix-driver.mjs";
import * as browser from "../lib/browser.mjs";
import { RastApi } from "../lib/rast-api.mjs";
import { appWindowMatches, isRealAppWindow } from "./_helpers.mjs";

const BLANK_PNG_BYTES = 3000; // a uniformly coloured body compresses below this

function selectedApps(options) {
  return APPS.filter((app) => !options.apps || options.apps.includes(app.key));
}

function describeFlow(app) {
  const names = (steps) => steps.map((s) => (s.op === "key" ? `key ${s.keys}` : s.op === "type" ? `type "${s.text}"` : s.op === "lines" ? `${s.count} lines` : s.op === "wait" ? `wait ${s.ms}` : s.op));
  return [
    `    prepare: ${names(app.prepare).join(", ")}`,
    `    typing:  ${[...names(app.typeFocus), 'type "{marker}"', ...names(app.typeCleanup)].join(", ")}`,
    `    save:    ${names(app.save).join(", ")}`,
  ];
}

/** `--plan`: what the run would do. No network, no browser. */
function plan(ctx) {
  const options = parseMatrixOptions(ctx.options ?? []);
  const lines = [];
  const user = options.user ?? ctx.config.users?.[0]?.username ?? "(none)";
  lines.push(`target:   ${redactUrl(ctx.config.baseUrl)}`);
  lines.push(`account:  ${user}`);
  try {
    const guard = assertTargetGuard(ctx.config.baseUrl, user);
    lines.push(`guard:    ok (${guard.production ? "production target, test account" : "not production"})`);
  } catch (error) {
    lines.push(`guard:    REFUSED: ${error.message}`);
  }
  lines.push(`home:     ${options.homeMode}${options.homeMode === "auto" ? " (detected from GET /api/files/usage)" : ""}`);
  lines.push(`budgets:  ${options.budgets}; keys ${options.keys}; responsiveness ${options.responsiveness ? "on" : "off"}`);
  lines.push("API calls: GET healthz/bootstrap/auth/me/linux-apps/linux-apps/session/files/*, POST auth/login, POST files/mkdir" +
    `${options.cleanup ? ", POST files/delete (--cleanup)" : ""}, End application of a leftover run. Never: install, update, uninstall, admin endpoints.`);
  lines.push("per app:");
  for (const app of selectedApps(options)) {
    lines.push(`  ${app.key} (${app.label})`);
    lines.push("    checks:  launcher tile, RAST window painted, typing changes pixels, saved file in File Station (<= 5 s, marker inside), window closes + run ends + no error toast");
    if (options.responsiveness) lines.push("    measure: typing (keydown to first changed pixel), scroll (wheel to first changed frame, fps), window drag (fps, repaint after drop)");
    lines.push(...describeFlow(app));
  }
  lines.push("budgets:");
  for (const budget of BUDGETS) lines.push(`  ${budget.id}: ${budget.op} ${budget.limit} ${budget.unit}  (${budget.label})`);
  lines.push(`expected fail: legacy home, saved file: ${LEGACY_SAVE_REASON}`);
  lines.push("results:  tests/e2e/results/app-matrix-<home-mode>-<timestamp>/{results.json,matrix.md}, app-matrix-latest.{json,md}");
  lines.push("screens:  tests/e2e/screenshots/app-matrix/<timestamp>/");
  return lines;
}

// ---------------------------------------------------------------------------
async function run(ctx) {
  const options = parseMatrixOptions(ctx.options ?? []);
  const credentials = resolveCredentials(options, ctx.config, (path) => readFileSync(path, "utf8"));
  const baseUrl = ctx.config.baseUrl;
  const runId = randomBytes(3).toString("hex");
  const stamp = new Date().toISOString().replace(/[:.]/g, "-").replace("Z", "");
  const startedAt = new Date().toISOString();

  const inner = new RastApi(baseUrl);
  await inner.login(credentials.username, credentials.password);
  const api = new SafeApi(inner, { cleanup: options.cleanup });

  const bootstrap = await fetch(`${baseUrl}/api/bootstrap`).then((r) => r.json()).catch(() => ({}));
  let homeMode = options.homeMode;
  if (homeMode === "auto") homeMode = detectHomeMode(await api.get("/api/files/usage"));
  ctx.record(`home mode: ${homeMode} (${options.homeMode === "auto" ? "detected" : "given"})`);

  const listing = await api.get("/api/linux-apps");
  const rows = listing.apps ?? [];
  const shotDir = join(ctx.config.screenshotsDir, "app-matrix", stamp);
  mkdirSync(shotDir, { recursive: true });

  const desktop = await openMatrixDesktop(ctx.browser, inner, credentials);
  const page = desktop.page;
  const environment = { ...(await readEnvironment(page)), rastVersion: bootstrap.version ?? null };
  const S = { ctx, options, api, page, rows, homeMode, runId, credentials, shotDir, mkdirDone: false };

  const apps = [];
  try {
    for (const app of selectedApps(options)) {
      process.stdout.write(`\n    [${app.key}] `);
      const result = await runApp(S, app);
      apps.push(result);
      await sleep(1500);
    }
  } finally {
    if (options.cleanup) {
      // Our own test files only (unique rast-matrix-<run>-<app> names): to the recycle bin.
      const mine = apps.map((entry) => entry.savedFile).filter((file) => file?.visible).map((file) => file.fsPath);
      if (mine.length > 0) {
        await api.post("/api/files/delete", { paths: mine }).then(
          () => ctx.record(`--cleanup: moved ${mine.length} test file(s) to the recycle bin`),
          (error) => ctx.record(`--cleanup failed: ${message(error)}`),
        );
      }
    }
    await desktop.context.close().catch(() => {});
    const finishedAt = new Date().toISOString();
    const output = buildResult({
      meta: {
        scenario: "app-matrix",
        mode: ctx.mode,
        homeMode,
        baseUrl: redactUrl(baseUrl),
        user: credentials.username,
        production: credentials.production,
        runId,
        startedAt,
        finishedAt,
        options: { apps: options.apps, keys: options.keys, budgets: options.budgets, responsiveness: options.responsiveness },
      },
      environment,
      apps,
      budgetsMode: options.budgets,
    });
    const dir = join(ctx.config.resultsDir, `app-matrix-${homeMode}-${stamp}`);
    mkdirSync(dir, { recursive: true });
    const markdown = renderMatrixMarkdown(output);
    writeFileSync(join(dir, "results.json"), JSON.stringify(output, null, 2));
    writeFileSync(join(dir, "matrix.md"), markdown + "\n");
    writeFileSync(join(ctx.config.resultsDir, "app-matrix-latest.json"), JSON.stringify(output, null, 2));
    writeFileSync(join(ctx.config.resultsDir, "app-matrix-latest.md"), markdown + "\n");
    ctx.record(`results: ${dir}`);
    ctx.metric("checksPass", output.summary.checks.pass);
    ctx.metric("checksFail", output.summary.checks.fail);
    ctx.metric("budgetsFail", output.summary.budgets.fail);
    console.log(`\n${markdown}\nresults written to ${dir}`);
    S.output = output;
  }
  if (!S.output.summary.ok) {
    throw new Error(
      `app matrix: ${S.output.summary.checks.fail} check(s) failed, ${S.output.summary.budgets.fail} budget(s) failed (${options.budgets})`,
    );
  }
}

// ---------------------------------------------------------------------------
function message(error) {
  return String(error?.message ?? error).replace(/\s+/g, " ").slice(0, 300);
}

function setCheck(S, result, id, ok, { detail = null, ms = null, skipReason = null } = {}) {
  if (skipReason) {
    result.checks[id] = { status: "skip", reason: skipReason };
    process.stdout.write(`${id}=SKIP `);
    return;
  }
  const expected = expectedFailReason({ homeMode: S.homeMode, check: id });
  const status = classify(ok, expected);
  const check = { status };
  if (ms !== null) check.ms = ms;
  if (detail) check.detail = detail;
  if (expected && status !== "pass") check.reason = expected;
  result.checks[id] = check;
  process.stdout.write(`${id}=${status.toUpperCase()} `);
}

async function shot(S, app, name) {
  try {
    return await browser.screenshot(S.page, `${app.key}-${name}`, { dir: S.shotDir });
  } catch {
    return null;
  }
}

function appWindows(windows, row) {
  return windows.filter((window) => appWindowMatches(window, row) && isRealAppWindow(window));
}

function largest(windows) {
  return [...windows].sort((a, b) => b.rect.width * b.rect.height - a.rect.width * a.rect.height)[0] ?? null;
}

/** Close every RAST window of an app, then end its run if it still lives. */
async function forceCleanup(S, row) {
  for (let round = 0; round < 6; round += 1) {
    const windows = appWindows(await browser.windowsInfo(S.page), row);
    if (windows.length === 0) break;
    await closeWindowByControl(S.page, windows[windows.length - 1].id).catch(() => {});
    await sleep(800);
  }
  if (S.ctx.mode !== "native") return;
  try {
    const state = await S.api.get("/api/linux-apps/session");
    for (const run of activeRunsOf(state?.session, row.id)) {
      await S.api.post(`/api/linux-apps/session/runs/${encodeURIComponent(run.id)}/close`).catch(() => {});
    }
  } catch {
    /* nothing left to clean */
  }
}

async function runApp(S, app) {
  const { page, options } = S;
  const result = {
    key: app.key,
    label: app.label,
    appId: null,
    appName: null,
    installed: false,
    checks: emptyChecks(),
    window: null,
    savedFile: null,
    responsiveness: null,
  };
  const row = findAppRow(S.rows, app);
  const marker = makeMarker(S.runId, app.key);
  const fileName = `${marker}.${app.ext}`;
  const paths = planSavePaths({ homeMode: S.homeMode, homePath: options.homePath, username: S.credentials.username, fileName });
  const vars = {
    marker,
    savePath: paths.appPath,
    scrollUrl: scrollUrl(marker),
  };

  // 1. launcher
  let tile = null;
  if (!row) {
    setCheck(S, result, "launcher", false, {
      detail: `no installed app matching ${app.label} is visible to ${S.credentials.username}; the matrix never installs anything`,
    });
    for (const id of CHECK_IDS.slice(1)) setCheck(S, result, id, false, { skipReason: "app is not available to this account" });
    return result;
  }
  result.appId = row.id;
  result.appName = row.name;
  result.installed = row.status ? row.status === "ready" : true;
  try {
    const leftovers = appWindows(await browser.windowsInfo(page), row);
    if (leftovers.length > 0) {
      result.note = `${leftovers.length} window(s) of this app were open before the run and were closed first`;
      await forceCleanup(S, row);
    }
    tile = await findLauncherTile(page, row.name);
    setCheck(S, result, "launcher", Boolean(tile), tile ? {} : { detail: `no tile "${row.name}" in the start menu although the API lists the app` });
  } catch (error) {
    setCheck(S, result, "launcher", false, { detail: message(error) });
  }
  if (!tile) {
    await closeStartMenuQuietly(page);
    for (const id of CHECK_IDS.slice(1)) setCheck(S, result, id, false, { skipReason: "not in the launcher" });
    return result;
  }

  // 2. window
  let target = null;
  try {
    const t0 = Date.now();
    await tile.click();
    await page.waitForSelector(".start-menu", { state: "detached", timeout: 10_000 }).catch(() => {});
    const windows = await browser.waitForWindows(page, (list) => appWindows(list, row).length > 0, {
      timeoutMs: options.launchTimeoutMs,
      what: `a window for ${row.name}`,
    });
    target = largest(appWindows(windows, row));
    const windowMs = Date.now() - t0;
    let bytes = 0;
    const paintDeadline = Date.now() + 60_000;
    while (Date.now() < paintDeadline) {
      bytes = (await bodyBuffer(page, target.id)).length;
      if (bytes > BLANK_PNG_BYTES) break;
      await sleep(500);
    }
    const info = (await browser.windowsInfo(page)).find((w) => w.id === target.id) ?? target;
    result.window = { id: target.id, appId: target.appId, title: info.title, rect: info.rect, firstPaintMs: Date.now() - t0, paintedBytes: bytes };
    await shot(S, app, "window");
    setCheck(S, result, "window", bytes > BLANK_PNG_BYTES, {
      ms: windowMs,
      detail: bytes > BLANK_PNG_BYTES ? null : `the window opened but its content stayed blank (${bytes} bytes of PNG)`,
    });
  } catch (error) {
    await shot(S, app, "window-failed");
    setCheck(S, result, "window", false, { detail: message(error) });
  }
  if (!target) {
    for (const id of CHECK_IDS.slice(2)) setCheck(S, result, id, false, { skipReason: "no RAST window" });
    await forceCleanup(S, row);
    return result;
  }
  const winId = target.id;

  // 3. typing
  let prepared = false;
  try {
    await runSteps(page, winId, app.prepare, vars);
    prepared = true;
    await runSteps(page, winId, app.typeFocus, vars);
    const baseline = await bodyBuffer(page, winId);
    await page.keyboard.type(marker, { delay: 40 });
    const change = await waitBodyChange(page, winId, baseline, { timeoutMs: 5000 });
    await shot(S, app, "typed");
    await runSteps(page, winId, app.typeCleanup, vars);
    setCheck(S, result, "typing", change.changed, {
      ms: change.ms,
      detail: change.changed ? "pixels changed while typing; the typed text is verified in the saved file (check 4)" : "no pixel changed within 5 s of typing",
    });
  } catch (error) {
    await shot(S, app, "typing-failed");
    setCheck(S, result, "typing", false, { detail: `${prepared ? "typing" : "preparing the window"}: ${message(error)}` });
  }

  // Responsiveness (does not decide checks 1 to 5).
  if (options.responsiveness) result.responsiveness = await measureApp(S, app, winId);

  // 4. saved file
  try {
    await ensureMatrixFolder(S, paths);
    await clickBody(page, winId);
    await runSteps(page, winId, app.save, vars);
    const t0 = Date.now();
    const found = await waitForListed(S, paths, fileName, 5000);
    await shot(S, app, "saved");
    if (!found) {
      result.savedFile = { name: fileName, fsPath: paths.fsPath, appPath: paths.appPath, visible: false };
      setCheck(S, result, "saved", false, {
        detail: `${paths.fsPath} did not appear in File Station within 5 s (the app was told to save ${paths.appPath})`,
      });
    } else {
      const visibleMs = Date.now() - t0;
      const text = await readSaved(S, app, paths);
      const hasMarker = containsMarker(text, marker);
      result.savedFile = { name: fileName, fsPath: paths.fsPath, appPath: paths.appPath, visible: true, visibleMs, containsMarker: hasMarker, sizeBytes: found.size ?? null };
      setCheck(S, result, "saved", hasMarker, {
        ms: visibleMs,
        detail: hasMarker ? `visible after ${visibleMs} ms, typed text inside` : `visible after ${visibleMs} ms but the typed text is not in the file`,
      });
    }
  } catch (error) {
    await shot(S, app, "saved-failed");
    setCheck(S, result, "saved", false, { detail: message(error) });
  }

  // 5. closes normally
  try {
    const errorsBefore = (await page.evaluate(() => window.__rastProbe.toastErrors())).length;
    const windowsBefore = appWindows(await browser.windowsInfo(page), row);
    const t0 = Date.now();
    await page.locator(`.window[data-window-id="${winId}"]`).click({ position: { x: 30, y: 8 }, force: true }).catch(() => {});
    await closeWindowByControl(page, winId);
    const gone = await waitWindowDetached(page, winId, 20_000);
    await sleep(500);
    const remaining = appWindows(await browser.windowsInfo(page), row);
    const errors = (await page.evaluate(() => window.__rastProbe.toastErrors())).slice(errorsBefore);
    let ended = true;
    let endedNote = "";
    if (S.ctx.mode === "native") {
      const deadline = Date.now() + 30_000;
      for (;;) {
        const state = await S.api.get("/api/linux-apps/session").catch(() => null);
        if (activeRunsOf(state?.session, row.id).length === 0) break;
        if (Date.now() > deadline) {
          ended = false;
          break;
        }
        await sleep(1000);
      }
    } else {
      endedNote = "legacy copy lingers by design, process end not asserted";
    }
    await shot(S, app, "closed");
    const problems = [];
    if (!gone) problems.push("the RAST window did not close within 20 s");
    if (remaining.length > 0) problems.push(`${remaining.length} window(s) of the app remain (a prompt?): ${remaining.map((w) => w.title).join(", ")}`);
    if (!ended) problems.push("the app run was still active 30 s after the window closed");
    if (errors.length > 0) problems.push(`error toast: ${errors.join(" | ")}`);
    setCheck(S, result, "closed", problems.length === 0, {
      ms: Date.now() - t0,
      detail: problems.length > 0 ? problems.join("; ") : `closed${windowsBefore.length > 1 ? ` (${windowsBefore.length} windows)` : ""}${endedNote ? `; ${endedNote}` : ""}`,
    });
  } catch (error) {
    setCheck(S, result, "closed", false, { detail: message(error) });
  }
  await forceCleanup(S, row);
  return result;
}

// ---------------------------------------------------------------------------
async function measureApp(S, app, winId) {
  const { page, options } = S;
  const errors = [];
  const out = { method: "unavailable", typing: null, scroll: null, drag: null, environment: null, budgets: [], errors };
  try {
    await runSteps(page, winId, app.typeFocus, {});
    out.typing = await measureTyping(page, winId, { keys: options.keys });
    out.method = out.typing.method;
    await runSteps(page, winId, app.typeCleanup, {});
  } catch (error) {
    errors.push(`typing: ${message(error)}`);
  }
  try {
    out.scroll = await measureScroll(page, winId, { first: app.scrollFirst });
  } catch (error) {
    errors.push(`scroll: ${message(error)}`);
  }
  try {
    out.drag = await measureDrag(page, winId);
  } catch (error) {
    errors.push(`drag: ${message(error)}`);
  }
  try {
    const info = (await browser.windowsInfo(page)).find((w) => w.id === winId);
    const box = await bodyBox(page, winId).catch(() => null);
    const env = await readEnvironment(page);
    out.environment = {
      devicePixelRatio: env.devicePixelRatio,
      windowSize: info ? `${info.rect.width}x${info.rect.height}` : null,
      bodySize: box ? `${Math.round(box.width)}x${Math.round(box.height)}` : null,
      xpra: await readXpra(page),
    };
  } catch (error) {
    errors.push(`environment: ${message(error)}`);
  }
  out.budgets = evaluateBudgets(out);
  await shot(S, app, "responsiveness");
  return out;
}

// ---------------------------------------------------------------------------
// File Station
// ---------------------------------------------------------------------------
async function ensureMatrixFolder(S, paths) {
  if (!paths.mkdir || S.mkdirDone) return;
  const exists = await S.api.get(`/api/files?path=${encodeURIComponent(paths.fsDir)}&perPage=1`).then(() => true, () => false);
  if (!exists) await S.api.post("/api/files/mkdir", { path: paths.fsDir });
  S.mkdirDone = true;
}

async function listDir(S, dir) {
  const listing = await S.api.get(`/api/files?path=${encodeURIComponent(dir)}&perPage=500&sort=name&order=desc`);
  return listing?.items ?? [];
}

/** Poll File Station for the file; at most `timeoutMs` (5 s by the matrix rules). */
async function waitForListed(S, paths, fileName, timeoutMs) {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const found = (await listDir(S, paths.fsDir).catch(() => [])).find((item) => item.name === fileName);
    if (found) return found;
    if (Date.now() > deadline) return null;
    await sleep(250);
  }
}

async function readSaved(S, app, paths) {
  if (app.content === "odt") return odtText(await S.api.download(`/api/files/download?path=${encodeURIComponent(paths.fsPath)}`));
  const reply = await S.api.get(`/api/files/text?path=${encodeURIComponent(paths.fsPath)}&maxBytes=262144`);
  return reply?.content ?? "";
}

export default {
  id: "app-matrix",
  title: "App matrix: launcher, window, typing, saved file, close, responsiveness",
  modes: ["native", "docker"],
  standalone: true, // no administrator login; the scenario logs in as its own test account
  optIn: true, // never part of the default selection
  productionSafe: true,
  timeoutMs: 60 * 60_000,
  plan,
  run,
};
