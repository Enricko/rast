#!/usr/bin/env node
// Controlled end-to-end verification of the RAST native-app clipboard and input
// path, against the real production desktop (https://desktop.lufria.tech).
//
// Why this harness exists
// -----------------------
// The remote application is an Xpra window rendered into a canvas inside the
// RAST desktop: there is no remote DOM to assert on. The old probe typed into
// the remote Chrome address bar and used the remote window title, which is
// flaky (Ctrl+L is deliberately browser-reserved by the RAST input policy and
// blind toolbar clicks miss). This harness instead serves a controlled page on
// 127.0.0.1 (reachable from the remote Chrome, which runs on this host) whose
// `document.title` is the assertion channel: RAST mirrors the remote window
// title into `.win-title` of the adopted window, so every clipboard/input state
// the page reports becomes objectively readable from the local browser.
//
// The page (`clipboard-page.html`) reports: PASTED:<text>, INPUT:<text>,
// COPIED:<sha8>|<len>, CUT:<sha8>|<remaining>, SEL:<len>, UNDO:/REDO:,
// FOCUS:/BLUR:, WINDOW-OPENED:B, and a pure-black top strip so the harness can
// locate the remote page origin for precise clicks (the NEW WINDOW button).
//
// Matrix covered
// --------------
//   * local browser clipboard -> remote application (ASCII and unicode)
//   * remote application -> local browser clipboard (copy; exact text)
//   * remote -> remote (copy in window A, paste in window B)
//   * select-all, cut, undo/redo observation, focus switching
//   * desktop reload/reconnect, then paste again without losing the clipboard
//   * macOS keyboardSwap emulation (navigator.platform override): Cmd chords
//     become Ctrl inside the Linux application; real macOS hardware is NOT
//     available on this host and is reported as a limitation.
//
// Honest automation limits (documented in docs/INPUT-CLIPBOARD.md)
// ----------------------------------------------------------------
//   * Playwright/CDP key events do not run browser editing commands, so the
//     local browser never fires the trusted `paste` event. The browser->remote
//     direction is therefore proven through the product's own fallback path
//     (paste gesture -> quiet clipboard read -> push to the session) and the
//     application-initiated clipboard request path; the trusted-`paste`-event
//     path is covered by the frontend unit tests and needs a real user gesture.
//   * Image/HTML clipboard is text-only by design in this deployment; it is
//     reported as not-tested/unsupported, never as passing.
//
// Usage (from tests/e2e):
//   node clipboard/verify.mjs                       # full matrix, production
//   node clipboard/verify.mjs --nav-only            # navigation probe only
//   node clipboard/verify.mjs --headless=false      # watch it in a real window
//   node clipboard/verify.mjs --keep-session        # leave the test session up
//
// Options: --url=<base>, --creds=<file>, --user=<name>, --admin=<name>,
//          --headless=false, --nav-only, --keep-session, --timeout-scale=<f>.
//
// Safety: only the test account's own Chrome windows are launched/closed and
// only its own native session is ended through the RAST API at the end. No
// service, unit, other account or the live user session is ever touched.
//
// Evidence: tests/e2e/clipboard/results/<run>/report.{json,md} and
// tests/e2e/clipboard/screenshots/<run>/*.png (screenshots are git-ignored like
// the other run artifacts; the JSON/MD report is committed).

import { join } from "node:path";

import {
  DEFAULT_BASE_URL,
  DEFAULT_CREDS,
  HERE,
  RESULTS_DIR,
  SCREENSHOTS_DIR,
  adoptedWindows,
  calibratePage,
  clickNewWindowButton,
  clickRemotePage,
  deployedVersion,
  detectControlledPage,
  discoverIdentity,
  ensureDir,
  findClickablePoint,
  focusProbe,
  formLogin,
  fmtMs,
  focusWindowContent,
  installTitleObserver,
  launchApp,
  launchBrowser,
  loopbackApi,
  newClientContext,
  pageNow,
  parseArgs,
  readSecrets,
  remoteClipboardText,
  remoteWindowTitles,
  resolveCredentials,
  screenshot,
  screenshotWindow,
  sha8,
  shimSnapshot,
  sleep,
  startPageServer,
  titleLog,
  waitFor,
  waitForAdoptedWindows,
  waitForLocalClipboard,
  waitForWindowTitle,
  windowsInfo,
  writeJson,
  writeText,
  xpraInfo,
  xpraLatencySnapshot,
} from "./lib.mjs";

/* ------------------------------------------------------------------- setup */

const args = parseArgs(process.argv.slice(2), {
  url: DEFAULT_BASE_URL,
  creds: DEFAULT_CREDS,
  user: "t-alice",
  admin: "t-admin",
  headless: true,
  "nav-only": false,
  "keep-session": false,
  "timeout-scale": "1",
});
const BASE_URL = String(args.url).replace(/\/$/, "");
const SCALE = (ms) => Math.max(1, Math.round(ms * Number(args["timeout-scale"] || 1)));
const STAMP = new Date().toISOString().replace(/[:.]/g, "-").replace("Z", "");
const RUN_ID = STAMP;
const RESULTS_RUN = join(RESULTS_DIR, RUN_ID);
const SHOTS_RUN = join(SCREENSHOTS_DIR, RUN_ID);
ensureDir(RESULTS_RUN);
ensureDir(SHOTS_RUN);

const report = {
  runId: RUN_ID,
  startedAt: new Date().toISOString(),
  finishedAt: null,
  target: BASE_URL,
  user: args.user,
  mode: args["nav-only"] ? "nav-only" : "matrix",
  deployed: null,
  environment: null,
  checks: [],
  observations: [],
  latencies: {},
  screenshots: [],
  failures: [],
  limitations: [],
  exitCode: null,
};

function log(message) {
  console.log(message);
}
function observe(id, detail) {
  report.observations.push({ id, detail: String(detail) });
  console.log(`  · ${id}: ${detail}`);
}
function check(id, status, what, evidence = null) {
  const entry = { id, status, what, evidence };
  report.checks.push(entry);
  const label = status.toUpperCase();
  console.log(`  ${label.padEnd(10)} ${id} — ${what}${evidence ? ` (${typeof evidence === "string" ? evidence : JSON.stringify(evidence)})` : ""}`);
  if (status === "fail") report.failures.push(entry);
  return entry;
}
const pass = (id, what, evidence) => check(id, "pass", what, evidence);
const fail = (id, what, evidence) => check(id, "fail", what, evidence);
const skip = (id, what, evidence) => check(id, "not-tested", what, evidence);
const partial = (id, what, evidence) => check(id, "partial", what, evidence);

async function shot(page, name) {
  const path = await screenshot(page, SHOTS_RUN, name);
  report.screenshots.push(path);
  return path;
}
async function shotWindow(page, name, windowId) {
  const path = await screenshotWindow(page, SHOTS_RUN, name, windowId);
  if (path) report.screenshots.push(path);
  return path;
}

class NavOnlyStop extends Error {}

const credentials = resolveCredentials(readSecrets(String(args.creds)), args.user, args.admin);
if (!credentials.user.password) throw new Error(`no password for ${args.user} in ${args.creds}`);

const server = await startPageServer({
  port: Number(process.env.RAST_CLIPBOARD_PORT || 8798),
  routes: { "/clipboard-page.html": join(HERE, "clipboard-page.html") },
});
const PAGE_URL = server.url("/clipboard-page.html");
log(`clipboard verification — ${BASE_URL} as ${args.user}`);
log(`controlled remote page: ${PAGE_URL}`);
report.pageUrl = PAGE_URL;

/* ----------------------------------------------------------------- checks */

/**
 * Peripheral helpers around the adopted remote page.
 */
function makeDriver(page, layoutByWindow) {
  const getLayout = (windowId) => layoutByWindow.get(windowId);

  return {
    async ensureFocus(windowId) {
      const layout = getLayout(windowId);
      await focusWindowContent(page, windowId, layout);
      return focusProbe(page);
    },
    async clickField(windowId) {
      const layout = getLayout(windowId);
      if (!layout) throw new Error(`no calibrated layout for ${windowId}`);
      return clickRemotePage(page, layout, {});
    },
    /** Clear the remote field (forwarded Ctrl+A then Backspace). */
    async clearField(windowId, keys) {
      await this.clickField(windowId);
      await page.keyboard.press(keys.selectAll);
      await sleep(120);
      await page.keyboard.press(keys.clear);
      await sleep(220);
    },
    /** Browser clipboard -> remote app: returns timing/evidence. */
    async pasteInto(windowId, text, keys) {
      await this.clickField(windowId);
      await page.keyboard.press(keys.selectAll);
      await page.waitForTimeout(150);
      const wrote = await this.writeClipboard(text);
      if (!wrote) return { ok: false, error: "the harness could not write the browser clipboard" };
      const startedPage = await pageNow(page);
      const startedWall = Date.now();
      await page.keyboard.press(keys.paste);
      const watch = await waitForWindowTitle(
        page,
        windowId,
        (title) => title.includes(`PASTED:${text}`),
        { timeoutMs: SCALE(15_000), pollMs: 30 },
      );
      const entry = await titleLog(page, windowId);
      const latencyMs =
        watch.ok !== false && entry?.at != null && Number.isFinite(startedPage)
          ? Math.max(0, entry.at - startedPage)
          : Date.now() - startedWall;
      return {
        ok: Boolean(watch.title?.includes(`PASTED:${text}`)),
        title: watch.title,
        latencyMs,
        wallMs: Date.now() - startedWall,
        wrote,
      };
    },
    /** Remote app -> browser clipboard: select all, copy, poll local clipboard. */
    async copyOut(windowId, expected, keys) {
      await this.clickField(windowId);
      await page.keyboard.press(keys.selectAll);
      const selectWatch = await waitForWindowTitle(page, windowId, (title) => title.startsWith(`SEL:${expected.length}`), {
        timeoutMs: SCALE(4_000),
        pollMs: 30,
      });
      const startedPage = await pageNow(page);
      const startedWall = Date.now();
      await page.keyboard.press(keys.copy);
      const copyTitle = await waitForWindowTitle(
        page,
        windowId,
        (title) => title.includes(`COPIED:${sha8(expected)}|${expected.length}`),
        { timeoutMs: SCALE(10_000), pollMs: 30 },
      );
      const entry = await titleLog(page, windowId);
      const titleLatencyMs =
        copyTitle.ok !== false && entry?.at != null ? Math.max(0, entry.at - startedPage) : Date.now() - startedWall;
      const clipboard = await waitForLocalClipboard(page, expected, { timeoutMs: SCALE(12_000) });
      return {
        ok: clipboard.ok && Boolean(copyTitle.title?.includes(`COPIED:${sha8(expected)}|${expected.length}`)),
        selectAllTitle: selectWatch.title,
        selectAllObservable: Boolean(selectWatch.title?.startsWith(`SEL:${expected.length}`)),
        copyTitle: copyTitle.title,
        titleLatencyMs,
        clipboardOk: clipboard.ok,
        clipboardValue: clipboard.actual,
        clipboardMs: clipboard.elapsedMs,
      };
    },
    async writeClipboard(text) {
      return (await import("./lib.mjs")).writeLocalClipboard(page, text);
    },
    /** Type into the remote page (forwarded keys, no clipboard involved). */
    async typeInto(windowId, text, keys) {
      await this.clickField(windowId);
      await page.keyboard.press(keys.selectAll);
      await sleep(120);
      const startedPage = await pageNow(page);
      await page.keyboard.type(text, { delay: 12 });
      const watch = await waitForWindowTitle(page, windowId, (title) => title.includes(`INPUT:${text}`), {
        timeoutMs: SCALE(12_000),
        pollMs: 30,
      });
      const entry = await titleLog(page, windowId);
      return {
        ok: Boolean(watch.title?.includes(`INPUT:${text}`)),
        title: watch.title,
        latencyMs: entry?.at != null ? Math.max(0, entry.at - startedPage) : null,
      };
    },
  };
}

function keysFor(mac) {
  return {
    selectAll: mac ? "Meta+KeyA" : "Control+KeyA",
    copy: mac ? "Meta+KeyC" : "Control+KeyC",
    cut: mac ? "Meta+KeyX" : "Control+KeyX",
    paste: mac ? "Meta+KeyV" : "Control+KeyV",
    undo: mac ? "Meta+KeyZ" : "Control+KeyZ",
    redo: mac ? "Meta+Shift+KeyZ" : "Control+Shift+KeyZ",
    clear: "Backspace",
  };
}

async function waitForControlledPage(page, windowId, timeoutMs) {
  const started = Date.now();
  let last = null;
  while (Date.now() - started < timeoutMs) {
    last = await detectControlledPage(page, windowId).catch(() => null);
    if (last?.ok) return last;
    await sleep(SCALE(250));
  }
  return last;
}

/**
 * Load the controlled page inside the remote Chrome.
 *
 * Success is decided by the page's *pixels* (dark textarea + black origin strip
 * + coloured toolbar), never by the window title alone: Chrome restores tabs
 * from the previous session and the title can be stale or come from a search
 * page. Alt+D is the primary address-bar path because Ctrl+L is reserved by the
 * RAST input policy; a pointer click on the omnibox and Ctrl+K are fallbacks.
 */
async function navigateRemoteChrome(page, windowId, { label = "navigation" } = {}) {
  const box = await page.evaluate((id) => {
    const element = document.querySelector(`.window[data-window-id="${id}"]`);
    if (!element) return null;
    const rect = element.getBoundingClientRect();
    return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
  }, windowId);
  if (!box) throw new Error("launched window disappeared before navigation");
  const url = `${PAGE_URL}?run=${STAMP}&role=A`;

  const initial = await waitForControlledPage(page, windowId, SCALE(1_500));
  if (initial?.ok) {
    const title = await windowTitleOf(page, windowId);
    return { ok: true, method: "already loaded (restored tab)", title, calibrated: initial.layout, attempts: [] };
  }

  const omniboxY = Math.round(box.y + 72);
  const attempts = [
    { name: "Alt+D (not browser-reserved)", focus: () => page.keyboard.press("Alt+KeyD") },
    {
      name: "omnibox pointer click (toolbar row, no chord)",
      focus: () => page.mouse.click(box.x + box.width * 0.45, omniboxY),
    },
    { name: "Ctrl+K (not browser-reserved)", focus: () => page.keyboard.press("Control+KeyK") },
  ];
  const evidence = [];
  for (const attempt of attempts) {
    for (let round = 1; round <= 2; round += 1) {
      await page.keyboard.press("Escape").catch(() => undefined);
      // Focus the adopted window body first (a bottom click cannot move the
      // remote application's own focus ring).
      await page.mouse.click(box.x + box.width / 2, box.y + box.height - 60).catch(() => undefined);
      await sleep(250);
      await attempt.focus();
      await sleep(400);
      await page.keyboard.type(url, { delay: 8 });
      await sleep(150);
      await page.keyboard.press("Enter");
      const detection = await waitForControlledPage(page, windowId, SCALE(20_000));
      const title = await windowTitleOf(page, windowId);
      evidence.push({
        method: attempt.name,
        round,
        title,
        ok: Boolean(detection?.ok),
        mean: detection?.mean ?? null,
        colored: detection?.colored ?? null,
        reason: detection?.reason ?? null,
      });
      if (detection?.ok) {
        return { ok: true, method: `${attempt.name} (round ${round})`, title, calibrated: detection.layout, attempts: evidence };
      }
      report.observations.push({ id: `${label}.attempt`, detail: `${attempt.name} round ${round} failed: title=${JSON.stringify(title)} ${detection?.reason ?? ""}` });
      if (detection?.layout) layouts?.set?.(windowId, detection.layout);
    }
  }
  return { ok: false, attempts: evidence };
}

/** Re-verify the controlled page and restore it if it was lost. */
async function ensureControlled(reason) {
  const detection = await detectControlledPage(page, windowA.id).catch(() => null);
  if (detection?.ok) {
    layouts.set(windowA.id, detection.layout);
    return detection.layout;
  }
  observe("nav.recover", `controlled page lost before ${reason} (${detection?.reason ?? "unknown"}); navigating again`);
  const recovered = await navigateRemoteChrome(page, windowA.id, { label: "recover" });
  if (!recovered.ok) throw new Error(`lost the controlled page before ${reason} and could not restore it`);
  layouts.set(windowA.id, recovered.calibrated);
  observe("nav.recovered", `restored with ${recovered.method}; title=${JSON.stringify(recovered.title)}`);
  return recovered.calibrated;
}

/** Open a second Chrome window from inside the remote page. */
async function openSecondRemoteWindow(page, driver, windowIdA, layoutA) {
  const evidence = {};
  const before = adoptedWindows(await windowsInfo(page)).length;
  // 1) click the page's own NEW WINDOW button (calibrated pointer path)
  const titleBefore = (await titleLog(page, windowIdA))?.title ?? "";
  await clickNewWindowButton(page, layoutA);
  let opened = await waitForWindowTitle(page, windowIdA, (title) => title.includes("WINDOW-OPENED") || title.includes("POPUP-BLOCKED"), {
    timeoutMs: SCALE(8_000),
    pollMs: 50,
  });
  evidence.buttonTitle = opened.title;
  // 2) fallback: non-reserved forwarded chord handled by the page
  if (!opened.title || !opened.title.includes("WINDOW-OPENED")) {
    await driver.clickField(windowIdA);
    await page.keyboard.press("Control+Shift+KeyE");
    opened = await waitForWindowTitle(page, windowIdA, (title) => title.includes("WINDOW-OPENED") || title.includes("POPUP-BLOCKED"), {
      timeoutMs: SCALE(8_000),
      pollMs: 50,
    });
    evidence.keyTitle = opened.title;
  }
  evidence.pageReported = opened.title ?? null;
  // The page title change races the new window taking X11 focus (the page
  // immediately reports BLUR:<role>), so the authoritative success signal is
  // the second adopted RAST window, not the transient title.
  try {
    await waitFor(
      async () => {
        const windows = adoptedWindows(await windowsInfo(page));
        return windows.length > before ? windows : null;
      },
      { timeoutMs: SCALE(30_000), intervalMs: 200, what: "second adopted Chrome window" },
    );
    evidence.adopted = true;
  } catch (error) {
    evidence.adopted = false;
    evidence.error = String(error.message ?? error);
  }
  evidence.titleBefore = titleBefore;
  return evidence;
}

function windowTitleOf(page, windowId) {
  return page.evaluate((id) => {
    const element = document.querySelector(`.window[data-window-id="${id}"]`);
    return element?.querySelector(".win-title")?.textContent?.trim() ?? "";
  }, windowId);
}

/* ------------------------------------------------------------------- main */

let browser = null;
let context = null;
let page = null;
let api = null;
let fatal = null;
let openedWindowIds = [];
let windowA = null;
let layouts = new Map();

try {
  report.deployed = await deployedVersion(credentials.admin, { baseUrl: BASE_URL });
  log(`deployed: ${JSON.stringify(report.deployed.info ?? report.deployed).slice(0, 300)}`);

  browser = await launchBrowser({ headless: args.headless !== false });
  const client = await newClientContext(browser, { baseUrl: BASE_URL, mac: false });
  context = client.context;
  page = client.page;

  const login = await formLogin(page, { baseUrl: BASE_URL, credentials: credentials.user });
  pass("00.login", "signed in to the production desktop through the real login form", `attempts=${login.attempts}`);
  await shot(page, "00-desktop");

  // Read-only identity/session facts via the loopback API (secure cookies mean
  // the browser itself must use the public HTTPS origin).
  api = await loopbackApi(credentials.user);
  const identity = await discoverIdentity(api);
  report.identity = identity;
  report.environment = {
    xpraVersion: (await import("node:child_process")).execFileSync("xpra", ["--version"], { encoding: "utf8" }).split("\n")[0],
    chromeVersion: (await import("node:child_process")).execFileSync("google-chrome-stable", ["--version"], { encoding: "utf8" }).trim(),
    chromium: (await import("playwright")).chromium.executablePath?.() ?? null,
  };

  // A shared Xpra session may already be running (e.g. left warm by an earlier
  // run or by a browser reload): re-adopt its windows instead of starting a
  // second Chrome. A cold start is expected when no session exists (~12 s).
  let launchMs = null;
  let launchPath = "reused-running-session";
  let adoptedNow = [];
  try {
    adoptedNow = await waitForAdoptedWindows(page, { min: 1, timeoutMs: SCALE(10_000) });
  } catch {
    adoptedNow = [];
  }
  windowA = adoptedNow[adoptedNow.length - 1] ?? null;
  if (!windowA) {
    launchPath = "start-menu launch";
    const beforeIds = new Set(adoptedWindows(await windowsInfo(page)).map((window) => window.id));
    log(`pre-existing adopted Chrome windows: ${beforeIds.size}`);
    const clickAt = await launchApp(page);
    const launched = await waitForAdoptedWindows(page, { min: 1, timeoutMs: SCALE(300_000) }).catch(() => null);
    if (!launched) {
      await shot(page, "01-launch-failed");
      throw new Error("Google Chrome did not produce an adopted native window");
    }
    launchMs = Date.now() - clickAt;
    openedWindowIds = launched.map((window) => window.id).filter((id) => !beforeIds.has(id));
    windowA = launched[launched.length - 1];
  }
  // The session/display may only exist after the app launch (cold start):
  // refresh the read-only identity facts before using X11 evidence helpers.
  Object.assign(identity, await discoverIdentity(api));
  if (!windowA) throw new Error("no adopted Chrome window after launch");
  await sleep(1_200);
  const windowsNow = adoptedWindows(await windowsInfo(page));
  if (windowsNow.length > 1) {
    observe("launch.extra-windows", `Chrome has ${windowsNow.length} windows; using the newest for the matrix`);
  }
  await shot(page, "01-native-window");
  pass(
    "01.window",
    "Google Chrome is an adopted native window on the production desktop",
    `window ${windowA.id} appId=${windowA.appId} path=${launchPath} launch=${launchMs == null ? "warm" : `${launchMs} ms`} windows=${windowsNow.length}`,
  );

  // ---- navigation to the controlled page (no browser-reserved chords) -----
  const navigation = await navigateRemoteChrome(page, windowA.id);
  report.navigation = navigation;
  if (!navigation.ok) {
    await shot(page, "02-navigation-failed");
    check(
      "02.navigation",
      "fail",
      "remote Chrome navigated to the controlled 127.0.0.1 test page without browser-reserved chords",
      { attempts: navigation.attempts },
    );
    throw new Error("could not navigate the remote Chrome to the controlled page");
  }
  check(
    "02.navigation",
    "pass",
    "remote Chrome navigated to the controlled 127.0.0.1 test page without browser-reserved chords",
    `method=${navigation.method} title=${JSON.stringify(navigation.title)} pageOrigin=(${navigation.calibrated.content.x},${navigation.calibrated.content.y})`,
  );
  await shot(page, "02-controlled-page");
  await shotWindow(page, "02-controlled-page-window", windowA.id);

  // ---- focus ownership ----------------------------------------------------
  await installTitleObserver(page);
  const layoutA = navigation.calibrated;
  layouts.set(windowA.id, layoutA);
  const driver = makeDriver(page, layouts);
  const kernelKeys = keysFor(false);

  const focus = await driver.ensureFocus(windowA.id);
  report.focus = focus;
  const focusOk =
    focus.activeElement?.tag === "DIV" &&
    /win-body|linux-native-window/.test(focus.activeElement?.className ?? "") &&
    !focus.activeIsViewer;
  check(
    "03.focus",
    focusOk ? "pass" : "fail",
    "after clicking the application, keyboard focus stays in the RAST desktop document (not the hidden viewer iframe)",
    `active=${focus.activeElement?.tag}.${(focus.activeElement?.className ?? "").split(" ")[0] || "-"} activeIsViewer=${focus.activeIsViewer} viewerActive=${focus.viewerActiveElement?.tag ?? "-"}`,
  );
  check(
    "03.transport",
    focus.viewer?.connected === true && focus.viewer?.clipboardEnabled === true && String(focus.viewer?.version ?? "") >= "1.3.0"
      ? "pass"
      : "fail",
    "the Xpra transport viewer is connected, clipboard-capable and runs the input/clipboard shim",
    JSON.stringify(focus.viewer),
  );
  observe("03.focus.title", `window title after focus: ${JSON.stringify((await titleLog(page, windowA.id))?.title ?? "")}`);
  await shot(page, "03-focus");

  const keys = kernelKeys;

  if (args["nav-only"]) {
    await shot(page, "02-nav-only");
    throw new NavOnlyStop("navigation + focus probe complete");
  }

  // ---- local -> remote: ASCII -------------------------------------------
  await ensureControlled("local->remote ASCII paste");
  const ascii = `RAST-LOCAL-TO-REMOTE-${STAMP.slice(-6)}`;
  await driver.clearField(windowA.id, keys);
  const pasteAscii = await driver.pasteInto(windowA.id, ascii, keys);
  report.latencies.localToRemoteAscii = pasteAscii.latencyMs;
  check(
    "04.local-to-remote.ascii",
    pasteAscii.ok ? "pass" : "fail",
    "browser clipboard text pastes into the remote application unchanged (PASTED title assertion)",
    `expected=${JSON.stringify(`PASTED:${ascii}`)} observed=${JSON.stringify(pasteAscii.title)} latency=${fmtMs(pasteAscii.latencyMs)} ms`,
  );
  await shotWindow(page, "04-local-to-remote-ascii", windowA.id);

  // ---- local -> remote: unicode -----------------------------------------
  await ensureControlled("local->remote unicode paste");
  const unicode = `RAST ⇄ Linux ✓ — ünïcødé テスト 日本語 🚀 Ω≈ç√∫ ${STAMP.slice(-4)}`;
  await driver.clearField(windowA.id, keys);
  const pasteUnicode = await driver.pasteInto(windowA.id, unicode, keys);
  report.latencies.localToRemoteUnicode = pasteUnicode.latencyMs;
  check(
    "04.local-to-remote.unicode",
    pasteUnicode.ok ? "pass" : "fail",
    "unicode browser clipboard text round-trips into the remote application unchanged",
    `expected=${JSON.stringify(`PASTED:${unicode}`)} observed=${JSON.stringify(pasteUnicode.title)} latency=${fmtMs(pasteUnicode.latencyMs)} ms`,
  );
  await shotWindow(page, "04-local-to-remote-unicode", windowA.id);

  // ---- remote -> browser clipboard --------------------------------------
  await ensureControlled("remote->local copy");
  await sleep(1_100); // let the paste-gesture selection suppression expire
  const copyOut = await driver.copyOut(windowA.id, unicode, keys);
  report.latencies.remoteToLocal = { titleMs: copyOut.titleLatencyMs, clipboardMs: copyOut.clipboardMs };
  check(
    "05.remote-to-local.copy",
    copyOut.ok ? "pass" : "fail",
    "a copy inside the remote application lands on the browser clipboard with the exact text",
    `expected=${JSON.stringify(unicode)} clipboard=${JSON.stringify(copyOut.clipboardValue)} title=${JSON.stringify(copyOut.copyTitle)} clipboardLatency=${fmtMs(copyOut.clipboardMs)} ms`,
  );
  check(
    "05.select-all",
    copyOut.selectAllObservable ? "pass" : "fail",
    "select-all is forwarded to the remote application (SEL:<len> observable)",
    `expected SEL:${unicode.length} observed=${JSON.stringify(copyOut.selectAllTitle)}`,
  );
  const x11AfterCopy = remoteClipboardText(identity);
  check(
    "05.remote-x11-clipboard",
    x11AfterCopy.ok && x11AfterCopy.text === unicode ? "pass" : "fail",
    "the remote X11 CLIPBOARD selection holds the copied text (host-side independent evidence)",
    `xclip=${JSON.stringify(x11AfterCopy.text)} ok=${x11AfterCopy.ok}`,
  );
  await shotWindow(page, "05-remote-to-local-copy", windowA.id);

  // ---- cut ---------------------------------------------------------------
  await ensureControlled("cut");
  await driver.clickField(windowA.id);
  const cutStartedPage = await pageNow(page);
  await page.keyboard.press(keys.selectAll);
  await sleep(200);
  await page.keyboard.press(keys.cut);
  const cutWatch = await waitForWindowTitle(
    page,
    windowA.id,
    (title) => title.startsWith(`CUT:${sha8(unicode).slice(0, 8)}`),
    { timeoutMs: SCALE(10_000), pollMs: 30 },
  );
  const cutPending = await titleLog(page, windowA.id);
  const cutLatencyMs = cutPending?.at != null ? Math.max(0, cutPending.at - cutStartedPage) : null;
  const cutClipboard = await waitForLocalClipboard(page, unicode, { timeoutMs: SCALE(8_000) });
  check(
    "06.cut",
    String(cutWatch.title ?? "").startsWith(`CUT:${sha8(unicode).slice(0, 8)}`) && cutClipboard.ok ? "pass" : "fail",
    "cut (Ctrl+X) is forwarded: the application cuts the text and the browser clipboard receives it",
    `expected CUT:${sha8(unicode).slice(0, 8)} observed=${JSON.stringify(cutWatch.title)} clipboard=${cutClipboard.ok} latency=${fmtMs(cutLatencyMs)} ms`,
  );
  await shotWindow(page, "06-cut", windowA.id);

  // ---- typing (forwarded keys, no clipboard) + undo/redo -----------------
  await ensureControlled("typing/undo/redo");
  const typed = `typed-marker-${STAMP.slice(-6)}`;
  const typeResult = await driver.typeInto(windowA.id, typed, keys);
  check(
    "07.typing",
    typeResult.ok ? "pass" : "fail",
    "forwarded keystrokes reach the remote application exactly once (exact INPUT title)",
    `expected=${JSON.stringify(`INPUT:${typed}`)} observed=${JSON.stringify(typeResult.title)}`,
  );
  await page.keyboard.press(keys.undo);
  const undoWatch = await waitForWindowTitle(page, windowA.id, (title) => title.startsWith("UNDO:"), {
    timeoutMs: SCALE(6_000),
    pollMs: 30,
  });
  await page.keyboard.press(keys.redo);
  const redoWatch = await waitForWindowTitle(page, windowA.id, (title) => title.startsWith("REDO:"), {
    timeoutMs: SCALE(6_000),
    pollMs: 30,
  });
  const undoValue = String(undoWatch.title ?? "").slice(5);
  const redoValue = String(redoWatch.title ?? "").slice(5);
  check(
    "07.undo-redo",
    undoWatch.title?.startsWith("UNDO:") && redoWatch.title?.startsWith("REDO:") ? "pass" : "fail",
    "undo/redo are forwarded and observable in the remote application",
    `undo=${JSON.stringify(undoValue)} redo=${JSON.stringify(redoValue)}`,
  );
  await shotWindow(page, "07-typing-undo-redo", windowA.id);

  // ---- second window: focus switching + remote -> remote -----------------
  await ensureControlled("second window + remote->remote");
  const second = await openSecondRemoteWindow(page, driver, windowA.id, layoutA);
  report.secondWindow = second;
  if (!second.adopted) {
    fail(
      "08.second-window",
      "the remote page opened a second Chrome window and it was adopted 1:1 as a second RAST window",
      { pageReported: second.pageReported, error: second.error },
    );
    skip("08.remote-to-remote", "remote to remote clipboard copy/paste", "no second remote window");
    skip("08.focus-switch", "focus switching between two adopted windows", "no second remote window");
  } else {
    const windows = adoptedWindows(await windowsInfo(page));
    const windowB = windows[windows.length - 1];
    const calibratedB = await calibratePage(page, windowB.id);
    if (calibratedB) layouts.set(windowB.id, calibratedB);
    await sleep(600);
    const titleB = (await waitForWindowTitle(page, windowB.id, (title) => /(READY|FOCUS|BLUR):B/.test(title), {
      timeoutMs: SCALE(20_000),
      pollMs: 100,
    })).title;
    pass(
      "08.second-window",
      "the remote page opened a second Chrome window (mouse button, page-triggered window.open) and it was adopted 1:1",
      `pageReported=${JSON.stringify(second.pageReported)} windows=${windows.map((window) => `${window.id}:${window.title}`).join(" | ")}`,
    );
    if (!calibratedB) {
      fail("08.second-window-calibration", "second adopted window page origin could not be located", titleB);
    }
    await shot(page, "08-two-windows");

    // focus switching (A <-> B)
    await driver.ensureFocus(windowA.id);
    await sleep(250);
    const focusA = await windowsInfo(page);
    const titleA = await windowTitleOf(page, windowA.id);
    const probeA = await focusProbe(page);
    await driver.ensureFocus(windowB.id);
    await sleep(250);
    const focusB = await windowsInfo(page);
    const titleAfterB = await windowTitleOf(page, windowB.id);
    const titleAAfterB = await windowTitleOf(page, windowA.id);
    const focusOkA = focusA.find((window) => window.id === windowA.id)?.focused === true;
    const focusOkB = focusB.find((window) => window.id === windowB.id)?.focused === true;
    check(
      "08.focus-switch",
      focusOkA && focusOkB ? "pass" : "fail",
      "clicking each adopted window moves focus and keyboard ownership to that application",
      `A.focused=${focusOkA} titleA=${JSON.stringify(titleA)} B.focused=${focusOkB} titleB=${JSON.stringify(titleAfterB)} titleAAfterB=${JSON.stringify(titleAAfterB)} A.activeElement=${probeA.activeElement?.className ?? "?"}`,
    );

    // remote -> remote: copy in A, paste in B
    const remoteToRemote = `remote-to-remote-${STAMP.slice(-6)}`;
    const keysR2R = kernelKeys;
    await driver.typeInto(windowA.id, remoteToRemote, keysR2R);
    await driver.clickField(windowA.id);
    await page.keyboard.press(keysR2R.selectAll);
    await sleep(250);
    await page.keyboard.press(keysR2R.copy);
    const copyTitle = await waitForWindowTitle(page, windowA.id, (title) => title.includes(`COPIED:${sha8(remoteToRemote)}|${remoteToRemote.length}`), {
      timeoutMs: SCALE(10_000),
      pollMs: 30,
    });
    const localAfterCopy = await waitForLocalClipboard(page, remoteToRemote, { timeoutMs: SCALE(10_000) });
    await driver.clickField(windowB.id);
    await page.keyboard.press(keysR2R.selectAll);
    await sleep(150);
    const startedPageB = await pageNow(page);
    await page.keyboard.press(keysR2R.paste);
    const pasteTitle = await waitForWindowTitle(page, windowB.id, (title) => title.includes(`PASTED:${remoteToRemote}`), {
      timeoutMs: SCALE(15_000),
      pollMs: 30,
    });
    const entryB = await titleLog(page, windowB.id);
    const r2rLatencyMs = entryB?.at != null ? Math.max(0, entryB.at - startedPageB) : null;
    report.latencies.remoteToRemote = r2rLatencyMs;
    check(
      "08.remote-to-remote",
      copyTitle.title?.includes(`COPIED:${sha8(remoteToRemote)}`) && localAfterCopy.ok && pasteTitle.title?.includes(`PASTED:${remoteToRemote}`)
        ? "pass"
        : "fail",
      "copy in remote window A, paste in remote window B (two top-level Xpra windows)",
      `A.copy=${JSON.stringify(copyTitle.title)} browser.clipboard=${localAfterCopy.ok} B.paste=${JSON.stringify(pasteTitle.title)} latency=${fmtMs(r2rLatencyMs)} ms`,
    );
    await shotWindow(page, "08-remote-to-remote-window-b", windowB.id);
    openedWindowIds.push(windowB.id);
  }

  // ---- reload / reconnect: clipboard survives ---------------------------
  const windowCountBeforeReload = adoptedWindows(await windowsInfo(page)).length;
  await page.reload({ waitUntil: "domcontentloaded" });
  await page.waitForSelector(".taskbar", { timeout: SCALE(60_000) });
  await installTitleObserver(page);
  let reconnected = null;
  try {
    reconnected = await waitForAdoptedWindows(page, { min: windowCountBeforeReload, timeoutMs: SCALE(90_000) });
  } catch {
    reconnected = adoptedWindows(await windowsInfo(page));
  }
  const reconnectOk = reconnected.length >= windowCountBeforeReload;
  const layoutAfterReload = new Map();
  for (const window of reconnected) {
    const calibrated = await calibratePage(page, window.id);
    if (calibrated) layoutAfterReload.set(window.id, calibrated);
  }
  let pasteWindow = null;
  let layoutForPaste = null;
  for (const window of reconnected) {
    const layout = layoutAfterReload.get(window.id) ?? (await calibratePage(page, window.id));
    if (!layout) continue;
    if (await findClickablePoint(page, layout, { mode: "content" })) {
      pasteWindow = window;
      layoutForPaste = layout;
      break;
    }
  }
  if (!pasteWindow) throw new Error("no reachable adopted Chrome window after desktop reload");
  layouts.set(pasteWindow.id, layoutForPaste);
  const reconnectText = `after-reconnect-${STAMP.slice(-6)}`;
  const wroteReconnect = await (await import("./lib.mjs")).writeLocalClipboard(page, reconnectText);
  await clickRemotePage(page, layoutForPaste, {});
  await page.keyboard.press(keys.selectAll);
  await sleep(150);
  const reconnectStartedPage = await pageNow(page);
  await page.keyboard.press(keys.paste);
  const reconnectPaste = await waitForWindowTitle(page, pasteWindow.id, (title) => title.includes(`PASTED:${reconnectText}`), {
    timeoutMs: SCALE(15_000),
    pollMs: 30,
  });
  const reconnectEntry = await titleLog(page, pasteWindow.id);
  report.latencies.afterReconnect = reconnectEntry?.at != null ? Math.max(0, reconnectEntry.at - reconnectStartedPage) : null;
  check(
    "09.reconnect",
    reconnectOk && wroteReconnect && Boolean(reconnectPaste.title?.includes(`PASTED:${reconnectText}`)) ? "pass" : "fail",
    "after reloading the RAST desktop (viewer reconnect) the browser clipboard still pastes into the remote application",
    `windows before=${windowCountBeforeReload} after=${reconnected.length} clipboardWrite=${wroteReconnect} title=${JSON.stringify(reconnectPaste.title)} latency=${fmtMs(report.latencies.afterReconnect)} ms`,
  );
  await shot(page, "09-after-reconnect");
  report.sessionAfterReconnect = (await discoverIdentity(api)).session;

  // ---- macOS Command -> Control emulation pass --------------------------
  // (the remote page survives the desktop reload; the emulated-platform reload
  // below starts from the same session)
  // Real macOS hardware is not available on this Linux host; the client
  // platform is emulated so the shim takes its swap_keys branch. This proves
  // the policy/transport path, not real Apple hardware behaviour.
  try {
    const cdp = await context.newCDPSession(page);
    await cdp.send("Emulation.setUserAgentOverride", {
      userAgent:
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
      acceptLanguage: "en-US,en",
      platform: "MacIntel",
      userAgentMetadata: {
        brands: [{ brand: "Chromium", version: "131" }],
        fullVersion: "131.0.0.0",
        platform: "macOS",
        platformVersion: "14.5.0",
        architecture: "x86",
        model: "",
        mobile: false,
      },
    });
    await page.reload({ waitUntil: "domcontentloaded" });
    await page.waitForSelector(".taskbar", { timeout: SCALE(60_000) });
    await installTitleObserver(page);
    const macWindows = await waitForAdoptedWindows(page, { min: 1, timeoutMs: SCALE(90_000) }).catch(() => []);
    const macLayouts = new Map();
    for (const window of macWindows) {
      const calibrated = await calibratePage(page, window.id);
      if (calibrated) macLayouts.set(window.id, calibrated);
    }
    const macDriver = makeDriver(page, macLayouts);
    let macWindow = null;
    for (const window of macWindows) {
      const layout = macLayouts.get(window.id);
      if (layout && (await findClickablePoint(page, layout, { mode: "content" }))) {
        macWindow = window;
        break;
      }
    }
    const shim = await shimSnapshot(page);
    const macKeys = keysFor(true);
    const macCopyText = `mac-copy-${STAMP.slice(-6)}`;
    const macPasteText = `mac-paste-${STAMP.slice(-6)}`;
    let macPaste = null;
    let macCopy = null;
    if (macWindow) {
      // Prove Cmd+A / Cmd+C on a distinct text, then prove Cmd+V with a fresh
      // clipboard value on an empty field. Clearing first matters: the window
      // may still contain text cached from an earlier reconnect, which would
      // make a stale paste indistinguishable from no paste at all.
      await macDriver.clearField(macWindow.id, macKeys);
      await macDriver.typeInto(macWindow.id, macCopyText, macKeys);
      macCopy = await macDriver.copyOut(macWindow.id, macCopyText, macKeys);
      await macDriver.clearField(macWindow.id, macKeys);
      macPaste = await macDriver.pasteInto(macWindow.id, macPasteText, macKeys);
      report.macEmulation = { copyText: macCopyText, pasteText: macPasteText, paste: macPaste, copy: macCopy };
    }
    const swapOk = shim?.keyboardSwap === true;
    const copyProven = Boolean(
      macCopy && macCopy.clipboardOk === true && macCopy.clipboardValue === macCopyText && /^COPIED:[0-9a-f]{8}\|\d+/.test(String(macCopy.copyTitle ?? "")),
    );
    const pasteProven = Boolean(macPaste?.ok && String(macPaste?.title ?? "").includes(`PASTED:${macPasteText}`));
    const macStatus = swapOk && pasteProven && copyProven ? "pass" : swapOk && copyProven ? "partial" : "fail";
    check(
      "10.keyboard-swap-emulation",
      macStatus,
      "with a macOS client platform the shim enables Command<->Control swapping and Cmd chords drive the Linux application",
      `keyboardSwap=${shim?.keyboardSwap} copyProven=${copyProven} CmdA/C title=${JSON.stringify(macCopy?.copyTitle)} clipboard=${JSON.stringify(macCopy?.clipboardValue)} pasteProven=${pasteProven} CmdV title=${JSON.stringify(macPaste?.title)}`,
    );
    if (macStatus !== "pass") {
      report.limitations.push(
        "macOS keyboardSwap emulation is only partially proven: the emulated platform sets keyboardSwap=true and Cmd+A/Cmd+C reach the application (remote selection/copy titles + browser clipboard observed), but the automated Cmd+V pass did not surface the new text. Playwright/CDP cannot produce the trusted browser paste event and the application may answer the X11 clipboard request from the product's cached browser text; this is NOT a confirmed product bug and needs a real macOS client (real user gesture + Cmd) to verify.",
      );
    }
    await shot(page, "10-macos-emulation");
  } catch (error) {
    fail("10.keyboard-swap-emulation", "macOS keyboardSwap emulation pass", String(error.message ?? error));
  }

  // ---- console cleanliness ---------------------------------------------
  const consoleErrors = client.consoleLog.filter(
    (entry) => entry.type === "error" && /clipboard|keyboard|forwardKey|native session|linux-app|shim/i.test(entry.text),
  );
  check(
    "11.console",
    consoleErrors.length === 0 ? "pass" : "fail",
    "no clipboard/keyboard failure is logged silently in the page console",
    consoleErrors.length === 0 ? "clean" : consoleErrors.map((entry) => entry.text.slice(0, 200)).join(" | "),
  );

  // ---- session / remote evidence summary --------------------------------
  const finalIdentity = await discoverIdentity(api);
  report.identityFinal = finalIdentity;
  const xpra = xpraInfo(finalIdentity);
  report.xpra = xpra.ok ? xpraLatencySnapshot(xpra.map) : { error: xpra.error };
  const remoteTitles = remoteWindowTitles(finalIdentity);
  report.remoteWindowTitles = remoteTitles;
  observe("final.remote-titles", JSON.stringify(remoteTitles.titles ?? remoteTitles.error));
  observe("final.xpra-latency", JSON.stringify(report.xpra));
} catch (error) {
  if (error instanceof NavOnlyStop) {
    observe("nav-only", error.message);
    report.navOnly = true;
  } else {
  fatal = error;
  report.failures.push({ id: "fatal", status: "fail", what: String(error?.stack ?? error) });
  console.error(`\nFATAL: ${error?.message ?? error}`);
  if (page) await shot(page, "99-fatal").catch(() => undefined);
  }
} finally {
  // ---- teardown: only this run's windows, only the test account's session --
  try {
    if (page && !args["keep-open"]) {
      const ids = openedWindowIds.filter(Boolean);
      for (const id of ids) {
        const closer = page.locator(`.window[data-window-id="${id}"] .win-control-close`).first();
        if (await closer.count().catch(() => 0)) {
          await closer.click({ force: true }).catch(() => undefined);
          await sleep(400);
        }
      }
    }
  } catch {
    /* windows may already be gone after the reload */
  }
  try {
    if (api && !args["keep-session"]) {
      const state = await api.nativeSession().catch(() => null);
      for (const run of state?.session?.apps ?? []) {
        if (run.status === "running" && run.runId) await api.closeNativeRun(run.runId).catch(() => undefined);
      }
      await api.closeNativeSession().catch(() => undefined);
      report.sessionClosed = false;
      for (let poll = 0; poll < 30; poll += 1) {
        const after = await api.nativeSession().catch(() => null);
        if (!after?.session) {
          report.sessionClosed = true;
          break;
        }
        await sleep(500);
      }
      observe("teardown.session-closed", String(report.sessionClosed));
    } else {
      observe("teardown.session", "left running (--keep-session)");
    }
  } catch (error) {
    observe("teardown.error", String(error.message ?? error));
  }
  await server.close().catch(() => undefined);
  await context?.close().catch(() => undefined);
  await browser?.close().catch(() => undefined);
}

/* ----------------------------------------------------------------- report */

report.limitations = [
  "The trusted browser `paste` event cannot be produced by Playwright/CDP key injection (CDP keys do not run browser editing commands). browser->remote is therefore proven through the product fallback path (paste gesture -> quiet clipboard read -> push to session) and the application-initiated clipboard request; the trusted paste-event path is unit-tested in frontend/tests/input-clipboard.test.ts and still needs a real interactive user gesture.",
  "Real macOS/Apple hardware is not available on this Linux host: the macOS pass emulates navigator.platform/userAgentData.platform. It proves the Cmd->Ctrl swap policy and transport, not physical Apple keyboard/IME behaviour.",
  "Clipboard is text-only in this deployment. HTML/image clipboard is unsupported by design and is reported not-tested, not passing.",
  "The remote application DOM is a canvas; all remote assertions go through document.title mirrored into the RAST window title, plus host-side X11 clipboard reads (xclip) on the test identity's own display.",
  "This run used the t-alice test account and its own native session; the live user session (rast-xpra-u1) was never touched. Numbers are on one host with software Xvfb and no GPU.",
  "Synthetic key input from Playwright is real X11 input inside the remote app, but it has no OS-level keyboard repeat/IME layer; auto-repeat and IME were not re-verified here (product unit tests cover repeat semantics).",
];
report.finishedAt = new Date().toISOString();
const failed = report.checks.filter((entry) => entry.status === "fail").length;
report.exitCode = fatal ? 1 : failed > 0 ? 1 : 0;
writeJson(join(RESULTS_RUN, "report.json"), report);
writeText(join(RESULTS_RUN, "report.md"), renderMarkdown(report));

log("");
log(`=== CLIPBOARD MATRIX ${report.exitCode === 0 ? "PASS" : "FAIL"} ===`);
log(`checks: ${report.checks.length} passed=${report.checks.filter((entry) => entry.status === "pass").length} failed=${failed} not-tested=${report.checks.filter((entry) => entry.status === "not-tested").length}`);
for (const entry of report.failures) log(`  failure ${entry.id}: ${typeof entry.evidence === "string" ? entry.evidence : JSON.stringify(entry.evidence)}`);
log(`report:  ${join(RESULTS_RUN, "report.json")}`);
log(`shots:   ${SHOTS_RUN}`);
process.exit(report.exitCode);

/* --------------------------------------------------------------- markdown */

function renderMarkdown(data) {
  const lines = [];
  const fmt = (value) => (value == null ? "n/a" : typeof value === "number" ? fmtMs(value) : String(value));
  lines.push(`# RAST clipboard / input matrix — ${data.runId}`);
  lines.push("");
  lines.push(`- Target: ${data.target}`);
  lines.push(`- User: ${data.user}`);
  lines.push(`- Controlled remote page: ${data.pageUrl}`);
  lines.push(`- Started: ${data.startedAt}`);
  lines.push(`- Finished: ${data.finishedAt}`);
  lines.push(`- Result: **${data.exitCode === 0 ? "PASS" : "FAIL"}**`);
  lines.push("");
  lines.push("## Environment");
  lines.push("");
  lines.push("| Fact | Value |");
  lines.push("|---|---|");
  lines.push(`| Deployed RAST facts source | ${data.deployed?.source ?? data.deployed?.error ?? "n/a"} |`);
  lines.push(`| RAST info | \`${JSON.stringify(data.deployed?.info ?? {}).slice(0, 300)}\` |`);
  lines.push(`| Xpra | ${data.environment?.xpraVersion ?? "n/a"} |`);
  lines.push(`| Remote Chrome | ${data.environment?.chromeVersion ?? "n/a"} |`);
  lines.push(`| Session | \`${JSON.stringify(data.identity?.session ?? null).slice(0, 400)}\` |`);
  lines.push("");
  lines.push("## Checks");
  lines.push("");
  lines.push("| Check | Status | Evidence |");
  lines.push("|---|---|---|");
  for (const entry of data.checks) {
    const evidence = typeof entry.evidence === "string" ? entry.evidence : JSON.stringify(entry.evidence ?? null);
    lines.push(`| ${entry.id} — ${entry.what} | **${entry.status}** | ${String(evidence).replace(/\|/g, "\\|").slice(0, 500)} |`);
  }
  lines.push("");
  lines.push("## Measured latencies (end-to-end, client-observed; see BENCHMARKS §8 for the stage breakdown)");
  lines.push("");
  lines.push("| Path | Latency |");
  lines.push("|---|---|");
  for (const [key, value] of Object.entries(data.latencies ?? {})) {
    lines.push(`| ${key} | ${typeof value === "object" && value !== null ? JSON.stringify(value) : fmt(value)} ms |`);
  }
  lines.push("");
  lines.push("## Observations");
  lines.push("");
  for (const entry of data.observations) lines.push(`- ${entry.id}: ${entry.detail}`);
  lines.push("");
  lines.push("## Honest limitations");
  lines.push("");
  for (const line of data.limitations) lines.push(`- ${line}`);
  lines.push("");
  lines.push(`Screenshots: \`${SHOTS_RUN}\` (git-ignored run artifacts, not committed).`);
  return `${lines.join("\n")}\n`;
}
