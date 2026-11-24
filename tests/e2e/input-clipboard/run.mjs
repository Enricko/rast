#!/usr/bin/env node
// Real-browser verification for RAST native-app keyboard, pointer and
// clipboard input (task-6: universal input + two-way clipboard).
//
// It drives the *public production UI* exactly like a person: sign in, open
// Google Chrome from Applications, use the remote application's own address
// bar through the RAST desktop, and copy/paste through the clipboard. Nothing
// is restarted, stopped or reconfigured; only windows opened by this script are
// closed (through the application's own close path) and sessions are left
// running for the account owner / Lead.
//
// Modes
//   --mode=baseline   run against the current (old) production build: every
//                     probe is recorded as an observation, exit code 0 unless
//                     the run itself is broken. Use it for BEFORE evidence.
//   --mode=after      run against the new build: the focused checks must pass,
//                     otherwise the process exits 1. Use it for AFTER evidence.
//
// Options
//   --url=https://desktop.lufria.tech   target RAST instance
//   --creds=/home/lufria/.config/rast-test/creds   username=password lines
//   --user=t-alice                      account to sign in as (t-bob if t-alice is busy)
//   --headless=false                    watch the browser
//   --emulate-mac                       override navigator.platform/userAgent to
//                                       macOS (tests the Command -> Control path;
//                                       the CI host is Linux, so a real macOS
//                                       browser is not available)
//   --strict                            baseline mode exits non-zero on failures
//   --keep-open                         leave the Chrome window open at the end
//
// Credentials are read but never printed, logged or written to the report.
//
// Evidence: tests/e2e/input-clipboard/results/<mode>-<stamp>/report.json and
// screenshots under tests/e2e/input-clipboard/screenshots/<mode>-<stamp>/.
// Both directories are git-ignored.

import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { chromium } from "playwright";

const HERE = dirname(fileURLToPath(import.meta.url));
const STAMP = new Date().toISOString().replace(/[:.]/g, "-").replace("Z", "");

const VIEWPORT = { width: 1480, height: 920 };
const WINDOW_TIMEOUT_MS = 240_000;
const CLIPBOARD_WAIT_MS = 12_000;
const HEADLESS_DEFAULT = true;
const CACHED_CHROMIUM = "/home/lufria/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome";

function parseArgs(argv) {
  const args = {
    url: process.env.RAST_LIVE_URL || "https://desktop.lufria.tech",
    creds: process.env.RAST_LIVE_CREDS || "/home/lufria/.config/rast-test/creds",
    user: "t-alice",
    mode: "baseline",
    headless: HEADLESS_DEFAULT,
    emulateMac: false,
    strict: false,
    keepOpen: false,
    chromium: process.env.RAST_LIVE_CHROMIUM || null,
  };
  for (const item of argv) {
    if (item === "--headless=false") args.headless = false;
    else if (item === "--emulate-mac") args.emulateMac = true;
    else if (item === "--strict") args.strict = true;
    else if (item === "--keep-open") args.keepOpen = true;
    else if (item.startsWith("--url=")) args.url = item.slice("--url=".length);
    else if (item.startsWith("--creds=")) args.creds = item.slice("--creds=".length);
    else if (item.startsWith("--user=")) args.user = item.slice("--user=".length);
    else if (item.startsWith("--mode=")) args.mode = item.slice("--mode=".length);
    else if (item.startsWith("--chromium=")) args.chromium = item.slice("--chromium=".length);
    else {
      console.error(`unknown argument: ${item}`);
      process.exit(2);
    }
  }
  if (!["baseline", "after"].includes(args.mode)) {
    console.error("--mode must be baseline or after");
    process.exit(2);
  }
  args.url = args.url.replace(/\/$/, "");
  args.after = args.mode === "after";
  return args;
}

const args = parseArgs(process.argv.slice(2));
const RUN_ID = `${args.mode}-${STAMP}${args.emulateMac ? "-mac" : ""}`;
const RESULTS_DIR = join(HERE, "results", RUN_ID);
const SHOTS_DIR = join(HERE, "screenshots", RUN_ID);
mkdirSync(RESULTS_DIR, { recursive: true });
mkdirSync(SHOTS_DIR, { recursive: true });

/* ------------------------------------------------------------------ report */

const report = {
  runId: RUN_ID,
  mode: args.mode,
  url: args.url,
  user: args.user,
  emulatedMac: args.emulateMac,
  startedAt: new Date().toISOString(),
  finishedAt: null,
  checks: [],
  observations: [],
  screenshots: [],
  failures: [],
  exitCode: null,
};

function note(text) {
  report.observations.push(String(text));
  console.log(`  · ${text}`);
}

function pass(id, what, detail) {
  report.checks.push({ id, what, status: "pass", detail: detail ?? null });
  console.log(`  PASS ${id} — ${what}${detail ? ` (${detail})` : ""}`);
}

function fail(id, what, detail) {
  report.checks.push({ id, what, status: "fail", detail: detail ?? null });
  report.failures.push({ id, what, detail: detail ?? null });
  console.log(`  FAIL ${id} — ${what}${detail ? ` (${detail})` : ""}`);
}

function skip(id, what, detail) {
  report.checks.push({ id, what, status: "skipped", detail: detail ?? null });
  console.log(`  SKIP ${id} — ${what}${detail ? ` (${detail})` : ""}`);
}

/** In baseline mode failures are observations of the old build. */
function check(id, what, condition, detail) {
  if (condition) {
    pass(id, what, detail);
    return true;
  }
  if (args.after || args.strict) fail(id, what, detail);
  else {
    report.checks.push({ id, what, status: "observed-failing", detail: detail ?? null });
    console.log(`  OBSERVED-FAILING ${id} — ${what}${detail ? ` (${detail})` : ""}`);
  }
  return false;
}

async function screenshot(page, name) {
  const path = join(SHOTS_DIR, `${name}.png`);
  await page.screenshot({ path, animations: "disabled" }).catch(() => undefined);
  report.screenshots.push(path);
  return path;
}

async function elementScreenshot(locator, name) {
  const path = join(SHOTS_DIR, `${name}.png`);
  await locator.screenshot({ path, animations: "disabled" }).catch(() => undefined);
  report.screenshots.push(path);
  return path;
}

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

async function waitFor(fn, { timeoutMs = 60_000, intervalMs = 250, what = "condition" } = {}) {
  const started = Date.now();
  let lastError = null;
  for (;;) {
    try {
      const value = await fn();
      if (value) return value;
      lastError = null;
    } catch (error) {
      lastError = error;
    }
    if (Date.now() - started > timeoutMs) {
      throw new Error(`timed out after ${timeoutMs} ms waiting for ${what}${lastError ? ` (${lastError.message})` : ""}`);
    }
    await sleep(intervalMs);
  }
}

/* ------------------------------------------------------------- credentials */

function loadCredentials(path) {
  const text = readFileSync(path, "utf8");
  const users = new Map();
  for (const rawLine of text.split("\n")) {
    const line = rawLine.trim();
    if (!line || line.startsWith("#")) continue;
    const index = line.indexOf("=");
    if (index <= 0) continue;
    users.set(line.slice(0, index).trim(), line.slice(index + 1).trim());
  }
  return users;
}

/* --------------------------------------------------------------- browser */

async function newSession(browser) {
  const context = await browser.newContext({ viewport: VIEWPORT });
  // The probes read and write the real browser clipboard; grant the Clipboard
  // API permissions for the RAST origin so the harness itself is not the
  // limiting factor (the product still has to surface any refusal to the
  // person, which is covered by the frontend unit tests).
  await context.grantPermissions(["clipboard-read", "clipboard-write"], { origin: args.url }).catch(() => undefined);
  const page = await context.newPage();
  const consoleLog = [];
  page.on("console", (message) => {
    if (message.type() === "error" || message.type() === "warning") {
      consoleLog.push({ type: message.type(), text: message.text().slice(0, 500) });
    }
  });
  page.on("pageerror", (error) => consoleLog.push({ type: "pageerror", text: String(error).slice(0, 500) }));

  if (args.emulateMac) {
    const cdp = await context.newCDPSession(page);
    await cdp.send("Emulation.setUserAgentOverride", {
      userAgent:
        "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36",
      platform: "MacIntel",
      acceptLanguage: "en-US,en",
    });
    await cdp.send("Emulation.setNavigatorOverrides", { platform: "MacIntel" }).catch(() => undefined);
  }
  return { context, page, consoleLog };
}

async function signIn(page, credentials) {
  await page.goto(`${args.url}/`, { waitUntil: "domcontentloaded" });
  await page.waitForSelector(".auth-form, .taskbar", { timeout: 90_000 });
  if (await page.locator(".taskbar").count()) return;
  for (let attempt = 1; attempt <= 3; attempt += 1) {
    await page.fill('input[name="username"]', credentials.username);
    await page.fill('input[type="password"]', credentials.password);
    const [response] = await Promise.all([
      page
        .waitForResponse((candidate) => candidate.url().includes("/api/auth/login") && candidate.request().method() === "POST", {
          timeout: 60_000,
        })
        .catch(() => null),
      page.locator('.auth-form button[type="submit"], button:has-text("Sign in")').first().click(),
    ]);
    const status = response?.status() ?? 0;
    if (status === 401) throw new Error(`sign-in rejected for ${credentials.username} (HTTP 401)`);
    if (status === 429) {
      const waitSecs = Math.min(Number(response?.headers()?.["retry-after"] ?? 60) || 60, 120);
      note(`sign-in throttled (HTTP 429); waiting ${waitSecs}s before retry ${attempt}/3`);
      await sleep(waitSecs * 1000);
      continue;
    }
    try {
      await page.waitForSelector(".taskbar", { timeout: 30_000 });
      return;
    } catch {
      await note(`sign-in attempt ${attempt} did not reach the desktop (HTTP ${status})`);
    }
  }
  throw new Error(`could not sign in as ${credentials.username} (production login rate limit?)`);
}

/* ------------------------------------------------------------- desktop */

async function launchFromApplications(page, name) {
  await page.locator(".taskbar-start").click();
  await page.waitForSelector(".start-menu", { timeout: 15_000 });
  const menu = page.locator(".start-menu");
  const search = menu.locator(".menu-search");
  if (await search.count()) {
    await search.fill(name);
    await page.waitForTimeout(400);
  }
  const tile = menu.locator(".menu-app", { hasText: name }).first();
  await tile.waitFor({ timeout: 20_000 });
  await tile.click();
  await page.waitForSelector(".start-menu", { state: "detached", timeout: 10_000 }).catch(() => undefined);
}

const nativeWindowSelector = ".window:has(.linux-native-window)";

async function nativeWindows(page) {
  return page.evaluate(() =>
    [...document.querySelectorAll(".window")]
      .filter((element) => element.querySelector(".linux-native-window"))
      .map((element) => ({
        id: element.dataset.windowId ?? "",
        title: element.querySelector(".win-title")?.textContent?.trim() ?? "",
        hasCanvas: Boolean(element.querySelector(".linux-native-window canvas")),
        hasProblem: Boolean(element.querySelector(".linux-native-problem")),
        starting: /^Starting\b/.test(element.querySelector(".win-title")?.textContent?.trim() ?? ""),
      })),
  );
}

function isReadyNativeWindow(window) {
  return window.hasCanvas && !window.hasProblem && !window.starting;
}

async function readyNativeWindows(page) {
  return (await nativeWindows(page)).filter(isReadyNativeWindow);
}

async function waitForReadyNativeWindows(page, { min = 1, max = Number.POSITIVE_INFINITY, timeoutMs = WINDOW_TIMEOUT_MS } = {}) {
  return waitFor(
    async () => {
      const windows = await readyNativeWindows(page);
      if (windows.length >= min && windows.length <= max) return windows;
      return null;
    },
    { timeoutMs, intervalMs: 250, what: `${min === max ? min : `≥${min}`} adopted native window(s)` },
  );
}

/** Where does DOM focus live after the person clicks the application surface? */
async function focusProbe(page) {
  return page.evaluate(() => {
    const iframe = document.querySelector(".linux-native-session-frame");
    const describe = (element) =>
      element
        ? {
            tag: element.tagName,
            id: element.id || "",
            className: typeof element.className === "string" ? element.className : "",
          }
        : null;
    const viewerDocument = iframe && iframe.contentDocument;
    let snapshot = null;
    try {
      snapshot = iframe?.contentWindow?.RASTNative?.snapshot?.() ?? null;
    } catch (error) {
      snapshot = { error: String(error) };
    }
    return {
      activeElement: describe(document.activeElement),
      activeIsViewer: Boolean(iframe && document.activeElement === iframe),
      viewerActiveElement: describe(viewerDocument?.activeElement ?? null),
      viewer: snapshot
        ? {
            version: snapshot.version,
            connected: snapshot.connected,
            clipboardEnabled: snapshot.clipboardEnabled,
            clipboardDirection: snapshot.clipboardDirection,
            captureKeyboard: snapshot.captureKeyboard,
            keyboardSwap: snapshot.keyboardSwap,
            windows: Array.isArray(snapshot.windows) ? snapshot.windows.length : -1,
          }
        : null,
      nativeWindowCount: document.querySelectorAll(".linux-native-window").length,
      adoptedCanvases: document.querySelectorAll(".linux-native-window canvas").length,
    };
  });
}

/* ----------------------------------------------------------- clipboard */

async function writeLocalClipboard(page, text) {
  await page.bringToFront();
  return page.evaluate((value) => navigator.clipboard.writeText(value), text).then(
    () => true,
    () => false,
  );
}

async function readLocalClipboard(page) {
  return page.evaluate(() => navigator.clipboard.readText()).catch(() => null);
}

async function waitForLocalClipboard(page, expected, timeoutMs = CLIPBOARD_WAIT_MS) {
  const started = Date.now();
  let actual = null;
  for (;;) {
    actual = await readLocalClipboard(page);
    if (actual === expected) return { ok: true, actual };
    if (Date.now() - started > timeoutMs) return { ok: false, actual };
    await sleep(200);
  }
}

function platformKeys(mac) {
  return {
    selectAll: mac ? "Meta+A" : "Control+A",
    copy: mac ? "Meta+C" : "Control+C",
    paste: mac ? "Meta+V" : "Control+V",
    clear: "Backspace",
  };
}

/**
 * Focus the remote Chrome omnibox with a real pointer click (mouse input is
 * platform independent and never depends on browser-reserved shortcuts, which
 * the RAST policy deliberately leaves to the browser). Returns true once a
 * typed probe can be copied back out of the application.
 */
async function focusRemoteOmnibox(page, keys, marker) {
  const candidates = [
    { x: 0.45, y: 44 },
    { x: 0.5, y: 52 },
    { x: 0.4, y: 36 },
    { x: 0.5, y: 62 },
  ];
  const nativeWindow = page.locator(nativeWindowSelector).first();
  for (const candidate of candidates) {
    const box = await nativeWindow.boundingBox();
    if (!box) continue;
    await nativeWindow.click({ position: { x: Math.round(box.width * candidate.x), y: candidate.y } }).catch(() => undefined);
    await page.waitForTimeout(150);
    await page.keyboard.type(marker, { delay: 12 });
    await page.keyboard.press(keys.selectAll);
    await page.keyboard.press(keys.copy);
    const copied = await waitForLocalClipboard(page, marker, 4_000);
    if (copied.ok) return { ok: true, at: candidate };
    await page.keyboard.press("Escape").catch(() => undefined);
  }
  return { ok: false, at: null };
}

/* -------------------------------------------------------------- checks */

async function run() {
  const credentials = loadCredentials(args.creds).get(args.user);
  if (!credentials) throw new Error(`no credentials for ${args.user} in ${args.creds}`);

  const executablePath = args.chromium || (existsSync(CACHED_CHROMIUM) ? CACHED_CHROMIUM : null);
  const browser = await chromium.launch({
    headless: args.headless,
    args: ["--no-sandbox", "--disable-dev-shm-usage"],
    ...(executablePath ? { executablePath } : {}),
  });
  const session = await newSession(browser);
  const { context, page, consoleLog } = session;
  let openedWindow = false;

  try {
    console.log(`RAST input/clipboard verification — ${args.mode} mode${args.emulateMac ? " (emulated macOS)" : ""}`);
    console.log(`  target ${args.url} as ${args.user}`);

    await signIn(page, { username: args.user, password: credentials });
    pass("00.login", "signed in through the real form");

    await launchFromApplications(page, "Google Chrome");
    const windows = await waitForReadyNativeWindows(page, { min: 1, max: 2 });
    openedWindow = true;
    pass("01.window", "Google Chrome became an adopted native window", `windows=${windows.length}`);
    await screenshot(page, "01-desktop-native-window");
    await elementScreenshot(page.locator(nativeWindowSelector).first().locator(".win-body"), "01-window-body");

    // --- focus ownership after a real click -------------------------------
    const windowLocator = page.locator(nativeWindowSelector).first();
    const box = await windowLocator.boundingBox();
    if (!box) throw new Error("could not measure the native window");
    await windowLocator.click({ position: { x: Math.round(box.width / 2), y: Math.round(box.height * 0.7) } });
    await page.waitForTimeout(350);
    const focus = await focusProbe(page);
    report.focusProbe = focus;
    check(
      "02.focus.desktop",
      "keyboard focus stays in the RAST desktop document, not the hidden viewer iframe",
      focus.activeElement?.tag === "DIV" && /linux-native-window/.test(focus.activeElement?.className ?? "") && !focus.activeIsViewer,
      `active=${focus.activeElement?.tag}.${(focus.activeElement?.className ?? "").split(" ")[0] || "-"} activeIsViewer=${focus.activeIsViewer} viewerActive=${focus.viewerActiveElement?.id || focus.viewerActiveElement?.tag || "-"}`,
    );
    check(
      "02.viewer.connected",
      "the Xpra transport viewer is connected and clipboard-capable",
      focus.viewer?.connected === true && focus.viewer?.clipboardEnabled === true,
      JSON.stringify(focus.viewer),
    );
    if (args.after) {
      check(
        "02.viewer.version",
        "the deployed transport shim is the new input/clipboard shim",
        String(focus.viewer?.version ?? "") >= "1.3.0",
        `version=${focus.viewer?.version}`,
      );
      check(
        "02.viewer.keyboard",
        "keyboard capture is enforced and the macOS Command<->Control swap matches the platform",
        focus.viewer?.captureKeyboard === true && focus.viewer?.keyboardSwap === Boolean(args.emulateMac),
        `captureKeyboard=${focus.viewer?.captureKeyboard} keyboardSwap=${focus.viewer?.keyboardSwap} mac=${Boolean(args.emulateMac)}`,
      );
    } else {
      observe("02.viewer.version", `viewer shim version ${focus.viewer?.version}`);
    }

    // --- keyboard + remote -> browser clipboard ---------------------------
    const keys = platformKeys(args.emulateMac);
    const typedMarker = `RAST-input-${STAMP.slice(-8)}`;
    await writeLocalClipboard(page, "RAST-sentinel-before-typing");
    const focused = await focusRemoteOmnibox(page, keys, typedMarker);
    if (!focused.ok) {
      await screenshot(page, "03-omnibox-focus-failed");
      check("03.addressbar", "the pointer click focused the remote application's address bar", false, "could not focus the remote omnibox with a click");
    } else {
      pass("03.addressbar", "the pointer click focused the remote application's address bar", `at ${JSON.stringify(focused.at)}`);
      // Clear the probe text and type the marker as the real test.
      await page.keyboard.press(keys.selectAll);
      await page.keyboard.press(keys.clear);
      await page.keyboard.type(typedMarker, { delay: 12 });
      await page.waitForTimeout(400);
      await page.keyboard.press(keys.selectAll);
      await writeLocalClipboard(page, "RAST-sentinel-before-copy");
      await page.keyboard.press(keys.copy);
      const roundTrip = await waitForLocalClipboard(page, typedMarker);
      report.typingRoundTrip = { typed: typedMarker, actual: roundTrip.actual, ok: roundTrip.ok };
      check(
        "03.typing",
        "keystrokes reach the remote application exactly once (no duplicated or dropped characters)",
        roundTrip.ok && roundTrip.actual === typedMarker,
        `typed=${typedMarker} clipboard=${JSON.stringify(roundTrip.actual)}`,
      );
      check(
        "04.remote-to-browser-clipboard",
        "copy inside the remote application lands on the browser clipboard (same text, no loss)",
        roundTrip.ok,
        `clipboard=${JSON.stringify(roundTrip.actual)}`,
      );
      await screenshot(page, "03-typing-roundtrip");
      await elementScreenshot(page.locator(nativeWindowSelector).first().locator(".win-body"), "03-window-after-copy");

      // --- browser -> remote clipboard (unicode) ------------------------
      const unicode = "RAST ⇄ Linux ✓ — ünïcødé テスト 123";
      const wrote = await writeLocalClipboard(page, unicode);
      if (!wrote) {
        skip("05.browser-to-remote-clipboard", "wrote the browser clipboard", "harness could not write the browser clipboard");
      } else {
        await windowLocator.click({ position: { x: Math.round(box.width * 0.45), y: 44 } }).catch(() => undefined);
        await page.waitForTimeout(150);
        await page.keyboard.press(keys.selectAll);
        await page.keyboard.press(keys.paste);
        await page.waitForTimeout(600);
        await page.keyboard.press(keys.selectAll);
        await writeLocalClipboard(page, "RAST-sentinel-before-paste-check");
        await page.keyboard.press(keys.copy);
        const pastedBack = await waitForLocalClipboard(page, unicode);
        report.pasteRoundTrip = { expected: unicode, actual: pastedBack.actual, ok: pastedBack.ok };
        check(
          "05.browser-to-remote-clipboard",
          "browser clipboard text (unicode) pastes into the remote application and round-trips unchanged",
          pastedBack.ok,
          `expected=${JSON.stringify(unicode)} actual=${JSON.stringify(pastedBack.actual)}`,
        );
        await screenshot(page, "05-paste-roundtrip");
      }
    }

    // --- viewer console must not report input/clipboard errors ------------
    const inputConsoleErrors = consoleLog.filter((entry) =>
      /clipboard|keyboard|forwardKey|RAST native shim.*(fail|error)/i.test(`${entry.text}`),
    );
    report.consoleLog = consoleLog;
    check(
      "06.console",
      "no input/clipboard failure is logged silently in the page console",
      inputConsoleErrors.length === 0,
      inputConsoleErrors.map((entry) => entry.text.slice(0, 200)).join(" | ") || "clean",
    );
  } finally {
    if (openedWindow && !args.keepOpen) {
      await closeOpenedNativeWindows(page).catch(() => undefined);
    }
    await screenshot(page, "99-final").catch(() => undefined);
    await context.close().catch(() => undefined);
    await browser.close().catch(() => undefined);
  }
}

function observe(id, detail) {
  report.observations.push(`${id}: ${detail}`);
  console.log(`  OBS ${id} — ${detail}`);
}

/** Close only the window(s) this script opened, through the app's close path. */
async function closeOpenedNativeWindows(page) {
  for (let round = 0; round < 20; round += 1) {
    const windows = await nativeWindows(page).catch(() => []);
    const remaining = windows.filter((window) => !window.starting);
    if (remaining.length === 0) return;
    const target = page.locator(nativeWindowSelector).first();
    const close = target.locator(".win-control-close").first();
    if ((await close.count()) === 0) return;
    await close.click({ force: true }).catch(() => undefined);
    await page.waitForTimeout(700);
  }
}


/* ------------------------------------------------------------------- main */

let fatal = null;
try {
  await run();
} catch (error) {
  fatal = error;
  report.failures.push({ id: "fatal", what: String(error?.stack ?? error) });
  console.error(`\nFATAL: ${error?.message ?? error}`);
}
report.finishedAt = new Date().toISOString();

const failed = report.checks.filter((entry) => entry.status === "fail").length;
report.exitCode = fatal ? 1 : args.after || args.strict ? (failed > 0 ? 1 : 0) : 0;
writeFileSync(join(RESULTS_DIR, "report.json"), `${JSON.stringify(report, null, 2)}\n`);

console.log(`\n=== ${args.mode.toUpperCase()} ${report.exitCode === 0 ? "PASS" : "FAIL"} ===`);
console.log(`checks: ${report.checks.length} (${failed} failed)`);
console.log(`report:  ${join(RESULTS_DIR, "report.json")}`);
console.log(`shots:   ${SHOTS_DIR}`);
for (const failure of report.failures) console.log(`  failure ${failure.id}: ${failure.detail ?? failure.what ?? ""}`);
process.exit(report.exitCode);
