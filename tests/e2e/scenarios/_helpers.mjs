// Shared helpers for the E2E scenarios.
//
// Scenarios must work on both backends:
//   docker — one container per app copy; per-window streaming; sessions are
//            in-memory and reported by GET /api/linux-apps {sessions};
//   native — one shared Xpra session per user, one run per launched app;
//            session state from GET /api/linux-apps/session.
// Helpers only assert on the public API/DOM contract and fall back to
// backend-specific host introspection for evidence.

import { existsSync } from "node:fs";

import { config } from "../lib/config.mjs";
import * as browser from "../lib/browser.mjs";
import { sleep } from "../lib/rast-api.mjs";

export function assert(condition, message) {
  if (!condition) throw new Error(`assertion failed: ${message}`);
}

export function assertEqual(actual, expected, what) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) throw new Error(`assertion failed: ${what}: expected ${e}, got ${a}`);
}

export function assertMatch(value, regex, what) {
  if (!regex.test(String(value))) {
    throw new Error(`assertion failed: ${what}: ${JSON.stringify(value)} does not match ${regex}`);
  }
}

/** Native mode: make the helper-created profile ancestors traversable (0711). */
export async function fixNativePermissions(ctx) {
  if (ctx.mode !== "native") return;
  const root = ctx.config.instanceRoot;
  const script = "/home/lufria/RAST/scripts/e2e/dev-instance.sh";
  const result = await ctx.host.run("bash", [script, "fix-perms"], {
    env: { RAST_E2E_ROOT: root },
    timeoutMs: 60_000,
  });
  if (!result.ok) ctx.record(`fix-perms failed: ${result.stderr || result.stdout}`);
  ctx.record("native profile ancestor permissions fixed (0711)");
}

/**
 * Native only: start the shared session/app ahead of the UI and retry once
 * after repairing profile-ancestor permissions. On this host the root helper's
 * bootstrap can tighten <data_dir> to 0750, which makes the identity unable to
 * reach its profile; `dev-instance.sh fix-perms` repairs it (0711). The UI
 * launch that follows is then a warm launch on the existing session.
 */
export async function ensureNativeSessionReady(ctx, client, app, { attempts = 3 } = {}) {
  if (ctx.mode !== "native") return null;
  let lastError = null;
  for (let attempt = 1; attempt <= attempts; attempt += 1) {
    try {
      const launched = await client.launch(app.id);
      const waited = await waitNativeSession(ctx, client, launched, { timeoutMs: 240_000 });
      ctx.record(
        `native launch ready on attempt ${attempt}: run ${launched?.run?.id ?? "?"}, display ${waited.session?.display ?? "?"}`,
      );
      return { launched, session: waited.session };
    } catch (error) {
      lastError = error;
      const message = String(error?.message ?? error);
      const permissionIssue = /not traversable|CHDIR|profile directory|not prepared|Permission denied/i.test(message);
      ctx.record(`native launch attempt ${attempt} failed: ${message.slice(0, 300)}`);
      if (!permissionIssue || attempt === attempts) break;
      await fixNativePermissions(ctx);
      await sleep(1500);
    }
  }
  throw lastError;
}

// ---------------------------------------------------------------------------
// Installing apps
// ---------------------------------------------------------------------------
/**
 * Install a file through the real API (File Station upload → App Center
 * install job) unless an app for that file already exists.
 * Returns { app, installedNow }.
 */
export async function installFromFile(ctx, client, { localPath, fileName, timeoutMs = 20 * 60_000, label = fileName }) {
  if (!existsSync(localPath)) ctx.skip(`fixture missing: ${localPath}`);
  const name = fileName || localPath.split("/").pop();
  const log = (line) => ctx.log(`[install ${label}] ${line}`);

  const existing = await client.findApp((candidate) => candidate.fileName === name);
  if (existing && existing.status === "ready") {
    log(`already installed: ${existing.id}`);
    return { app: existing, installedNow: false };
  }
  // Remember the ids that already exist so a fresh install is not confused
  // with an older failed row for the same file name.
  const beforeIds = new Set((await client.listApps()).apps.map((candidate) => candidate.id));
  log(`uploading ${localPath}`);
  await client.uploadFile(localPath, { name });
  const started = await client.installFromPath(`/${name}`);
  log(`install job ${started.jobId}`);
  const job = await client.waitJob(started.jobId, { timeoutMs, intervalMs: 1500 });
  log(`job ${started.jobId}: ${job.status} ${job.message ?? ""}`);
  let app;
  if (started.app?.id || typeof started.app === "string") {
    app = await client.waitAppReady(started.app.id ?? started.app, { timeoutMs: 60_000 });
  } else {
    // Native installs answer {"jobId","installing"} (or a stale failed row for
    // the same file name may exist): wait for the new app row to appear ready.
    const startedAt = Date.now();
    for (;;) {
      const listing = await client.listApps();
      app = listing.apps.find(
        (candidate) =>
          candidate.fileName === name && !beforeIds.has(candidate.id) && candidate.status === "ready",
      );
      if (app) break;
      if (Date.now() - startedAt > 60_000) {
        const all = listing.apps
          .filter((candidate) => candidate.fileName === name)
          .map((candidate) => `${candidate.id}:${candidate.status}:${candidate.error ?? ""}`);
        throw new Error(`install finished without a ready app row for ${name} (rows: ${all.join(" | ")})`);
      }
      await sleep(500);
    }
  }
  return { app, installedNow: true };
}

export async function ensureChrome(ctx) {
  const client = ctx.admin;
  if (!existsSync(config.chromeDeb)) {
    ctx.skip(`Chrome fixture missing (${config.chromeDeb}); see tests/e2e/fixtures/build-fixtures.sh`);
  }
  const { app, installedNow } = await installFromFile(ctx, client, {
    localPath: config.chromeDeb,
    fileName: "chrome.deb",
    label: "google-chrome-stable",
  });
  await fixNativePermissions(ctx);
  ctx.shared.chrome = app;
  ctx.record(`Chrome app ${app.id} (${app.status}, backend ${app.backend ?? "docker"}, build ${app.build})`);
  return { app, installedNow };
}

export async function ensurePortableApp(ctx) {
  const client = ctx.admin;
  const { app, installedNow } = await installFromFile(ctx, client, {
    localPath: config.fixtures.archive,
    fileName: "rast-test-app-1.0.tar.gz",
    label: "RAST Test App (portable)",
  });
  await fixNativePermissions(ctx);
  ctx.shared.portable = app;
  ctx.record(`Portable app ${app.id} (${app.status}, backend ${app.backend ?? "docker"}, build ${app.build})`);
  return { app, installedNow };
}

// ---------------------------------------------------------------------------
// Sessions
// ---------------------------------------------------------------------------
export function isDockerSession(session) {
  return Boolean(session && typeof session.url === "string" && session.id && !("display" in session));
}

export function isNativeSession(session) {
  return Boolean(session && (typeof session.display === "number" || "key" in session));
}

/** Open an app for a user through the API; returns the backend session object. */
export async function openAppForUser(ctx, client, app) {
  if (ctx.mode === "native") {
    // Native launch is idempotent: the same run/session comes back.
    const launched = await client.launch(app.id);
    const waited = await waitNativeSession(ctx, client, launched, { timeoutMs: 240_000 });
    return { ...waited.session, _launched: launched };
  }
  const opened = await client.openApp(app.id);
  // Docker: `open` returns the copy's session; it may still be starting.
  const sessionId = opened?.id;
  assert(sessionId, `open ${app.id} returned no session id`);
  return waitDockerSession(ctx, client, sessionId);
}

export async function waitDockerSession(ctx, client, sessionId, { timeoutMs = 120_000 } = {}) {
  const started = Date.now();
  for (;;) {
    const session = await client.session(sessionId);
    if (session.status === "ready") return session;
    if (session.status === "ended") {
      throw new Error(`session ${sessionId} ended: ${session.reason ?? ""} ${session.error ?? ""}`);
    }
    if (Date.now() - started > timeoutMs) {
      throw new Error(`session ${sessionId} not ready after ${timeoutMs} ms (last ${session.status})`);
    }
    await sleep(500);
  }
}

/** GET /api/linux-apps/session (native) and wait for a ready session. */
export async function waitNativeSession(ctx, client, launched = null, { timeoutMs = 240_000 } = {}) {
  const started = Date.now();
  for (;;) {
    let state;
    try {
      state = await client.nativeSessionState();
    } catch (error) {
      if (error.status === 404) {
        // Older snapshots do not have the native endpoints at all.
        throw new Error(`native session endpoint unavailable: ${error.message}`);
      }
      throw error;
    }
    const session = state?.session ?? null;
    if (session && (session.status === undefined || session.status === "ready")) {
      return { session, launched, limits: state.limits, raw: state };
    }
    if (session && session.status === "stopping") {
      throw new Error(`native session is stopping: ${JSON.stringify(session).slice(0, 300)}`);
    }
    if (Date.now() - started > timeoutMs) {
      throw new Error(`native session not ready after ${timeoutMs} ms (last ${JSON.stringify(session)})`);
    }
    await sleep(500);
  }
}

/** All non-ended running copies/sessions of the caller. */
export async function runningSessions(ctx, client) {
  if (ctx.mode === "native") {
    try {
      const state = await client.nativeSessionState();
      return state?.session ? [state.session] : [];
    } catch {
      return [];
    }
  }
  const listing = await client.listApps();
  return (listing.sessions ?? []).filter((session) => session.status !== "ended");
}

// ---------------------------------------------------------------------------
// UI
// ---------------------------------------------------------------------------
/** Launch an installed app from the desktop start menu and wait for its window. */
export async function openAppViaUi(
  ctx,
  page,
  app,
  { waitForFrame = ctx.mode === "docker", timeoutMs = 150_000, client = null } = {},
) {
  if (ctx.mode === "native" && client) {
    await ensureNativeSessionReady(ctx, client, app);
    await fixNativePermissions(ctx);
  }
  await browser.launchApp(page, app.name);
  const windows = await waitForAppWindows(ctx, page, app, { timeoutMs });
  if (waitForFrame) {
    try {
      await browser.waitForAppFrame(page, { timeoutMs: 60_000 });
    } catch (error) {
      ctx.log(`app window opened but no streamed frame yet: ${error.message}`);
    }
  }
  return windows;
}

export function appWindowMatches(window, app) {
  const appId = String(window.appId ?? "");
  if (appId === `linux:${app.id}` || appId.includes(app.id)) return true;
  // Native windows are mirrored by the frontend with their remote title and an
  // appId that may not contain the app id (e.g. linuxnative-window:<id>), so
  // match on the visible title as well.
  const title = String(window.title ?? "").toLowerCase();
  const name = String(app.name ?? "").toLowerCase();
  return name.length > 0 && title.includes(name);
}

/** Windows that are no longer a "Starting <app>…" placeholder. */
export function isRealAppWindow(window) {
  return window.visible !== false && !/^Starting\b/i.test(String(window.title ?? ""));
}

export function waitForAppWindows(ctx, page, app, { timeoutMs = 150_000, what } = {}) {
  return browser.waitForWindows(
    page,
    (windows) => windows.some((window) => appWindowMatches(window, app)),
    { timeoutMs, what: what || `a window for ${app.name}` },
  );
}

/** Close a RAST window through the taskbar (middle click = close). */
export async function closeWindowViaTaskbar(page, windowId) {
  const entry = page.locator(".taskbar-app").filter({ has: page.locator("xpath=ancestor-or-self::*") });
  // Give the window focus first so the matching taskbar entry is the focused one.
  await page.locator(`.window[data-window-id="${windowId}"]`).click({ position: { x: 20, y: 8 } }).catch(() => {});
  await page.waitForTimeout(300);
  const buttons = page.locator(".taskbar-app");
  const count = await buttons.count();
  for (let index = 0; index < count; index += 1) {
    const button = buttons.nth(index);
    const active = await button.evaluate((el) => el.classList.contains("is-active"));
    if (!active) continue;
    await button.click({ button: "middle" });
    return;
  }
  // Fallback: click the window's own close control when it has one.
  const control = page.locator(`.window[data-window-id="${windowId}"] .win-control-close`);
  if (await control.count()) await control.first().click();
  else throw new Error(`no way to close window ${windowId}`);
}

export async function waitWindowGone(page, windowId, { timeoutMs = 60_000 } = {}) {
  await page
    .locator(`.window[data-window-id="${windowId}"]`)
    .waitFor({ state: "detached", timeout: timeoutMs });
}

/** Type into the streamed app after clicking its centre (Xpra forwards input). */
export async function interactWithApp(page, { text = "RAST e2e input", x = 0.5, y = 0.55 } = {}) {
  await browser.clickInApp(page, { x, y });
  await page.waitForTimeout(300);
  const before = await browser.frameBuffer(page);
  await browser.typeInApp(page, text);
  return { before };
}

/** Resize a RAST window by dragging its bottom-right resize handle. */
export async function resizeWindow(page, windowId, { dx = 120, dy = 80 } = {}) {
  const window = page.locator(`.window[data-window-id="${windowId}"]`);
  const before = await window.boundingBox();
  const handle = window.locator(".win-resize-se");
  if (!(await handle.count())) throw new Error(`window ${windowId} has no resize handle`);
  const box = await handle.first().boundingBox();
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  await page.mouse.down();
  await page.mouse.move(box.x + box.width / 2 + dx, box.y + box.height / 2 + dy, { steps: 10 });
  await page.mouse.up();
  await page.waitForTimeout(500);
  const after = await window.boundingBox();
  return { before, after };
}

// ---------------------------------------------------------------------------
// Host-side evidence
// ---------------------------------------------------------------------------
export async function identityFor(ctx, client) {
  const identities = await ctx.host.linuxIdentities();
  return identities.find((identity) => identity.userId === client.user.id) ?? null;
}

/** Containers of one app copy (docker backend). */
export async function dockerCopiesOf(ctx, app, client) {
  const containers = await ctx.host.dockerPs(["--filter", "label=rast.managed=linux-app"]);
  return containers.filter((container) => {
    const labels = container.labels ?? {};
    return (
      labels["rast.linux.app"] === app.id &&
      (client ? String(labels["rast.linux.user"] ?? "") === String(client.user.id) : true)
    );
  });
}

/** Native runs for one user (from the API when available, else systemd units). */
export async function nativeRunsOf(ctx, client) {
  const units = await ctx.host.appUnits();
  const identity = await identityFor(ctx, client);
  if (!identity) return { units: [], identity: null };
  // App unit names are rast-app-<hash>; ownership is visible in the unit's
  // Uid= property, so filter by the identity uid.
  const owned = [];
  for (const unit of units) {
    const status = await ctx.host.unitStatus(unit.unit);
    if (String(status.Uid ?? "") === String(identity.uid)) owned.push({ ...unit, status });
  }
  return { units: owned, identity };
}

export async function sessionUnitFor(ctx, client) {
  const identity = await identityFor(ctx, client);
  if (!identity) return null;
  const unit = `rast-xpra-u${client.user.id}`;
  const status = await ctx.host.unitStatus(unit);
  return { unit, identity, status };
}

export async function assertAppStillRunning(ctx, client, app) {
  if (ctx.mode === "native") {
    const state = await waitNativeSession(ctx, client, null, { timeoutMs: 10_000 }).catch(() => null);
    assert(state?.session, `${app.name}: native session disappeared`);
    return state.session;
  }
  const copies = await dockerCopiesOf(ctx, app, client);
  assert(copies.length > 0, `${app.name}: no Docker copy is running`);
  return copies;
}

export function summaryOfApp(app) {
  return {
    id: app.id,
    name: app.name,
    status: app.status,
    backend: app.backend,
    build: app.build,
    visibility: app.visibility,
    migrationState: app.migrationState,
    version: app.version,
  };
}

// ---------------------------------------------------------------------------
// Multi-window helpers
// ---------------------------------------------------------------------------
export function appWindowsFromList(windows, app) {
  return windows.filter((window) => appWindowMatches(window, app));
}

export async function allAppWindows(page, app) {
  return appWindowsFromList(await browser.windowsInfo(page), app);
}

/** Close every RAST window of an app (taskbar middle-click / close control). */
export async function closeAppWindows(page, app, { timeoutMs = 60_000, rounds = 12 } = {}) {
  for (let round = 0; round < rounds; round += 1) {
    const windows = await allAppWindows(page, app);
    if (windows.length === 0) return true;
    const target = windows[windows.length - 1];
    await closeWindowViaTaskbar(page, target.id).catch((error) => {
      page.waitForTimeout(300);
      return error;
    });
    await browser
      .waitForWindows(page, (list) => appWindowsFromList(list, app).length < windows.length, {
        timeoutMs: 15_000,
        what: `${app.name} window closed`,
      })
      .catch(() => {});
  }
  const left = await allAppWindows(page, app);
  if (left.length > 0) throw new Error(`could not close ${left.length} ${app.name} window(s)`);
  return true;
}

/** Wait until the backend reports no running copy/session for an app. */
export async function waitSessionGone(ctx, client, app, { timeoutMs = 180_000 } = {}) {
  const started = Date.now();
  let last = null;
  for (;;) {
    if (ctx.mode === "native") {
      const state = await client.nativeSession().catch(() => null);
      const session = state?.session ?? null;
      last = session;
      const runs = session?.apps ?? session?.runs ?? [];
      // The session keeps finished runs as history; only ACTIVE runs count.
      const appRuns = Array.isArray(runs)
        ? runs.filter(
            (run) =>
              run.appId === app.id && ["launching", "running", "starting"].includes(String(run.status)),
          )
        : [];
      if (!session || appRuns.length === 0) {
        // Also allow the session to stay ready with no runs when other apps live in it.
        if (!session) return { gone: true, session: null };
        return { gone: true, session, appRuns };
      }
    } else {
      const listing = await client.listApps();
      const mine = (listing.sessions ?? []).filter(
        (session) => session.appId === app.id && session.status !== "ended",
      );
      last = mine;
      if (mine.length === 0) return { gone: true };
    }
    if (Date.now() - started > timeoutMs) {
      throw new Error(`${app.name} session did not end within ${timeoutMs} ms (last: ${JSON.stringify(last)})`);
    }
    await sleep(1000);
  }
}

/** Session identity stable across operations (id for docker, display for native). */
export function sessionIdentity(ctx, session) {
  if (!session) return null;
  if (ctx.mode === "native") {
    return session.display !== undefined ? `display:${session.display}` : `key:${session.key}`;
  }
  return `id:${session.id}`;
}

export function appRunsOf(session, app) {
  const runs = session?.apps ?? session?.runs ?? [];
  if (!Array.isArray(runs)) return [];
  return runs.filter((run) => run.appId === app.id);
}
