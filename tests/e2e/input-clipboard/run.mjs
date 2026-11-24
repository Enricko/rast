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

import { execFile } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { promisify } from "node:util";
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
    diagnose: false,
    chromium: process.env.RAST_LIVE_CHROMIUM || null,
  };
  for (const item of argv) {
    if (item === "--headless=false") args.headless = false;
    else if (item === "--emulate-mac") args.emulateMac = true;
    else if (item === "--strict") args.strict = true;
    else if (item === "--keep-open") args.keepOpen = true;
    else if (item === "--diagnose") args.diagnose = true;
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

const execFileAsync = promisify(execFile);
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
  const allConsole = [];
  page.on("console", (message) => {
    const entry = { type: message.type(), text: message.text().slice(0, 500) };
    allConsole.push(entry);
    if (message.type() === "error" || message.type() === "warning") consoleLog.push(entry);
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
  return { context, page, consoleLog, allConsole };
}

async function signIn(page, credentials) {
  // The production UI can take a moment to boot after a service restart, and a
  // rate-limited response may not render the SPA at all: retry the navigation.
  let booted = false;
  for (let attempt = 1; attempt <= 3 && !booted; attempt += 1) {
    await page.goto(`${args.url}/`, { waitUntil: "domcontentloaded" }).catch(() => undefined);
    booted = await page
      .waitForSelector(".auth-form, .taskbar", { timeout: 45_000 })
      .then(() => true)
      .catch(() => false);
    if (!booted) {
      const text = await page.evaluate(() => (document.body?.innerText || "").slice(0, 300)).catch(() => "");
      note(`RAST UI did not boot (attempt ${attempt}); page text: ${text.replace(/\s+/g, " ").slice(0, 200)}`);
      await sleep(2_000);
    }
  }
  if (!booted) {
    await screenshot(page, "login-ui-missing");
    throw new Error("the RAST UI did not render the login form or the desktop");
  }
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

async function openStartMenu(page) {
  for (let attempt = 1; attempt <= 4; attempt += 1) {
    await page.keyboard.press("Escape").catch(() => undefined);
    await page.locator(".taskbar-start").click({ force: attempt > 1 }).catch(() => undefined);
    const opened = await page
      .waitForSelector(".start-menu", { timeout: 8_000, state: "visible" })
      .then(() => true)
      .catch(() => false);
    if (opened) return page.locator(".start-menu");
    await page.waitForTimeout(800);
  }
  return null;
}

/**
 * Launch an app from the start menu. The menu is rebuilt from the shell's app
 * store, which can still be loading right after sign-in on a busy host: retry
 * by closing/reopening the menu (and the whole search) a few times.
 */
async function launchFromApplications(page, name) {
  for (let attempt = 1; attempt <= 3; attempt += 1) {
    const menu = await openStartMenu(page);
    if (!menu) continue;
    const search = menu.locator(".menu-search");
    if (await search.count()) {
      await search.fill(name).catch(() => undefined);
      await page.waitForTimeout(500);
    }
    const tile = menu.locator(".menu-app", { hasText: name }).first();
    const found = await tile
      .waitFor({ state: "visible", timeout: 12_000 })
      .then(() => true)
      .catch(() => false);
    if (found) {
      await tile.click();
      await page.waitForSelector(".start-menu", { state: "detached", timeout: 10_000 }).catch(() => undefined);
      return;
    }
    const text = await menu.innerText().catch(() => "");
    note(`start menu attempt ${attempt}: could not find "${name}" (menu text: ${text.replace(/\s+/g, " ").slice(0, 220)})`);
    await page.keyboard.press("Escape").catch(() => undefined);
    await page.waitForTimeout(1_000);
  }
  await screenshot(page, "start-menu-failed");
  throw new Error(`could not find "${name}" in the desktop start menu`);
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

/**
 * Wait until at least one adopted canvas has visible pixels. A freshly adopted
 * Xpra window can be all-black until the first damage frame arrives; a
 * screenshot taken then is useless as evidence.
 */
async function waitForPaintedSurface(page, { timeoutMs = 20_000 } = {}) {
  const started = Date.now();
  for (;;) {
    const state = await page
      .evaluate(() => {
        const canvases = [...document.querySelectorAll(".linux-native-window canvas")];
        if (canvases.length === 0) return "none";
        let readable = false;
        for (const canvas of canvases) {
          if (canvas.width < 2 || canvas.height < 2) continue;
          try {
            const context = canvas.getContext("2d");
            if (!context) continue;
            readable = true;
            for (let ix = 1; ix <= 4; ix += 1) {
              for (let iy = 1; iy <= 4; iy += 1) {
                const pixel = context.getImageData(
                  Math.floor((canvas.width * ix) / 5),
                  Math.floor((canvas.height * iy) / 5),
                  1,
                  1,
                ).data;
                if (pixel[0] + pixel[1] + pixel[2] > 12 || pixel[3] > 0) return "painted";
              }
            }
          } catch {
            /* an offscreen/worker-owned canvas cannot be sampled here */
          }
        }
        // The Xpra client may transfer its canvas to an OffscreenCanvas decode
        // worker: the visible pixels are real but not readable from this realm.
        return readable ? "black" : "unknown";
      })
      .catch(() => "unknown");
    if (state === "painted") return true;
    if (state === "unknown") {
      // Cannot sample: give the first damage frames a moment, then accept.
      await sleep(2_500);
      return true;
    }
    if (state === "black" && Date.now() - started < timeoutMs) {
      await sleep(400);
      continue;
    }
    return state === "black" ? false : (await sleep(500), true);
  }
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

async function readWindowTitle(page) {
  return page
    .locator(`${nativeWindowSelector} .win-title`)
    .first()
    .textContent()
    .then((text) => (text || "").trim())
    .catch(() => "");
}

async function waitForWindowTitle(page, pattern, timeoutMs = 15_000) {
  const started = Date.now();
  let last = "";
  for (;;) {
    last = await readWindowTitle(page);
    if (pattern.test(last)) return { ok: true, title: last };
    if (Date.now() - started > timeoutMs) return { ok: false, title: last };
    await sleep(250);
  }
}

/** Click the remote application's address-bar row at one of several offsets. */
async function clickRemoteToolbar(page, windowLocator, keys, { navigate } = {}) {
  const candidates = [
    { x: 0.45, y: 100 },
    { x: 0.4, y: 104 },
    { x: 0.5, y: 96 },
    { x: 0.35, y: 108 },
    { x: 0.45, y: 112 },
  ];
  const box = await windowLocator.boundingBox();
  if (!box) return { ok: false, at: null, title: "" };
  for (const candidate of candidates) {
    await page.keyboard.press("Escape").catch(() => undefined);
    await windowLocator
      .click({ position: { x: Math.round(box.width * candidate.x), y: candidate.y } })
      .catch(() => undefined);
    await page.waitForTimeout(250);
    if (!navigate) return { ok: true, at: candidate, title: "" };
    await page.keyboard.press(keys.selectAll);
    await page.keyboard.type(navigate.text, { delay: 10 });
    await page.keyboard.press("Enter");
    const result = await waitForWindowTitle(page, navigate.pattern, 6_000);
    if (result.ok) return { ok: true, at: candidate, title: result.title };
  }
  return { ok: false, at: null, title: await readWindowTitle(page) };
}

/**
 * Diagnostics for a failed clipboard/focus assertion: toasts and the shim
 * snapshot tell whether the failure was surfaced to the person or silent.
 */
async function clipboardDiagnostics(page, allConsole) {
  const toasts = await page.locator(".toast-message").allTextContents().catch(() => []);
  const snapshot = await page
    .evaluate(() => {
      const iframe = document.querySelector(".linux-native-session-frame");
      try {
        const state = iframe?.contentWindow?.RASTNative?.snapshot?.() ?? null;
        if (!state) return null;
        return {
          version: state.version,
          clipboardEnabled: state.clipboardEnabled,
          clipboardDirection: state.clipboardDirection,
          clipboardPending: state.clipboardPending,
          clipboardLastEventAt: state.clipboardLastEventAt,
          keyboardSwap: state.keyboardSwap,
          captureKeyboard: state.captureKeyboard,
          connected: state.connected,
        };
      } catch (error) {
        return { error: String(error) };
      }
    })
    .catch(() => null);
  const consoleTail = allConsole
    .filter((entry) => /clipboard|shim|keyboard|fail|error/i.test(entry.text))
    .slice(-30);
  return { toasts, snapshot, consoleTail };
}


/* ------------------------------------------------ remote X11 introspection */
// The remote application is an X11 client on the person's private Xpra display.
// Reading its window title and clipboard from the host is read-only evidence of
// what the remote app actually received; it never sends input or changes state.
const remote = { userId: null, display: null, username: null, available: false };

async function sudoExec(argv, timeoutMs = 15_000) {
  const result = await execFileAsync("sudo", ["-n", ...argv], { timeout: timeoutMs, maxBuffer: 4 * 1024 * 1024 }).then(
    (value) => ({ ok: true, stdout: value.stdout, stderr: value.stderr }),
    (error) => ({ ok: false, stdout: error.stdout ?? "", stderr: error.stderr ?? String(error) }),
  );
  return result;
}

async function remoteCommand(argv, app) {
  if (!remote.available) return { ok: false, stdout: "", stderr: "remote introspection unavailable" };
  const env = ["env", `DISPLAY=${remote.display}`, `XAUTHORITY=/var/lib/rast/linux-users/${remote.userId}/.Xauthority`, ...argv];
  const result = await sudoExec(["-u", remote.username, ...env]);
  if (!result.ok && app) return result;
  return result;
}

/** Discover the test account's remote Xpra display (read-only). */
async function discoverRemoteDisplay(page, username) {
  try {
    const me = await page.evaluate(async () => {
      const response = await fetch("/api/auth/me", { credentials: "same-origin" });
      return response.ok ? await response.json() : null;
    });
    const userId = me?.user?.id ?? me?.id ?? null;
    if (userId == null) {
      note("remote X11 evidence unavailable: /api/auth/me did not expose a user id");
      return false;
    }
    remote.userId = userId;
    remote.username = `rast-u${userId}`;
    const identity = await sudoExec(["getent", "passwd", remote.username]);
    if (!identity.ok) {
      note(`remote X11 evidence unavailable: no local identity ${remote.username}`);
      return false;
    }
    const processes = await sudoExec(["pgrep", "-a", "-f", "-u", remote.username, "Xvfb-for-Xpra"]);
    if (!processes.ok) {
      note(`remote X11 evidence unavailable: could not find the Xpra display for ${remote.username}`);
      return false;
    }
    const display = /:(\d+)/.exec(processes.stdout);
    if (!display) {
      note(`remote X11 evidence unavailable: no display in ${JSON.stringify(processes.stdout.slice(0, 160))}`);
      return false;
    }
    remote.display = `:${display[1]}`;
    const probe = await remoteCommand(["xdotool", "getdisplaygeometry"]);
    remote.available = probe.ok;
    note(`remote X11 evidence: account=${remote.username} display=${remote.display} xdotool=${remote.available}`);
    return remote.available;
  } catch (error) {
    note(`remote X11 evidence unavailable: ${String(error?.message ?? error)}`);
    return false;
  }
}

/** Titles of the visible remote Chrome windows (what the X server has). */
async function remoteWindowTitles() {
  if (!remote.available) return [];
  const search = await remoteCommand(["xdotool", "search", "--class", "chrome"]);
  if (!search.ok) return [];
  const ids = String(search.stdout)
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean);
  const titles = [];
  for (const id of ids) {
    const name = await remoteCommand(["xdotool", "getwindowname", id]);
    if (name.ok) titles.push(String(name.stdout).trim());
  }
  return titles;
}

async function waitForRemoteTitle(pattern, timeoutMs = 15_000) {
  const started = Date.now();
  let titles = [];
  for (;;) {
    titles = (await remoteWindowTitles()).filter((title) => title.length > 0);
    if (titles.some((title) => pattern.test(title))) return { ok: true, titles };
    if (Date.now() - started > timeoutMs) return { ok: false, titles };
    await sleep(300);
  }
}

/** Current text of the remote X11 CLIPBOARD selection (empty when unowned). */
async function remoteClipboardText() {
  if (!remote.available) return null;
  for (const target of ["UTF8_STRING", "STRING"]) {
    const result = await remoteCommand(["xclip", "-selection", "clipboard", "-o", "-t", target]);
    if (result.ok) return String(result.stdout);
  }
  return null;
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
    { x: 0.45, y: 100 },
    { x: 0.4, y: 104 },
    { x: 0.5, y: 108 },
    { x: 0.35, y: 96 },
    { x: 0.45, y: 116 },
  ];
  const nativeWindow = page.locator(nativeWindowSelector).first();
  for (const candidate of candidates) {
    const box = await nativeWindow.boundingBox();
    if (!box) continue;
    await page.keyboard.press("Escape").catch(() => undefined);
    await nativeWindow.click({ position: { x: Math.round(box.width * candidate.x), y: candidate.y } }).catch(() => undefined);
    await page.waitForTimeout(250);
    await page.keyboard.type(marker, { delay: 12 });
    await page.keyboard.press(keys.selectAll);
    await page.keyboard.press(keys.copy);
    const copied = await waitForLocalClipboard(page, marker, 4_000);
    if (copied.ok) return { ok: true, at: candidate };
    await page.keyboard.press("Escape").catch(() => undefined);
  }
  return { ok: false, at: null };
}

/* ------------------------------------------------------------ diagnostics */

/** One-shot debug probe: click the app, type, select all, copy, dump state. */
async function diagnoseRemoteCopy(page, keys, allConsole) {
  const windowLocator = page.locator(nativeWindowSelector).first();
  const box = await windowLocator.boundingBox();
  const diagnostics = { marker: null, localClipboard: null, toasts: [], snapshot: null, activeElement: null, instrumentation: null, console: [] };
  if (!box) {
    console.log("DIAGNOSTICS " + JSON.stringify({ error: "no window box" }));
    return;
  }
  // Instrument the desktop page: does the parent see real key events, and is
  // the shim's sendKey called for them? (read-only probe, no production code)
  await page.evaluate(() => {
    const state = { realKeydowns: [], sendKeyCalls: [] };
    window.__RAST_INPUT_PROBE = state;
    document.addEventListener(
      "keydown",
      (event) => {
        state.realKeydowns.push({
          key: event.key,
          code: event.code,
          ctrlKey: event.ctrlKey,
          metaKey: event.metaKey,
          target: event.target?.tagName + "." + (typeof event.target?.className === "string" ? event.target.className.split(" ")[0] : ""),
        });
      },
      true,
    );
    const iframe = document.querySelector(".linux-native-session-frame");
    const shim = iframe?.contentWindow?.RASTNative;
    if (shim && typeof shim.sendKey === "function") {
      const original = shim.sendKey;
      shim.sendKey = function (pressed, init) {
        try {
          state.sendKeyCalls.push({ pressed, key: init?.key, code: init?.code, ctrlKey: !!init?.ctrlKey, metaKey: !!init?.metaKey });
        } catch {
          /* ignore */
        }
        return original.call(shim, pressed, init);
      };
    }
    state.shimAvailable = Boolean(shim);
  });
  await windowLocator.click({ position: { x: Math.round(box.width * 0.45), y: 100 } });
  await page.waitForTimeout(300);
  await page.keyboard.type("abc", { delay: 20 });
  await page.waitForTimeout(400);
  const afterTyping = await page.evaluate(() => ({ ...window.__RAST_INPUT_PROBE }));
  await page.keyboard.press(keys.selectAll);
  await page.keyboard.press(keys.copy);
  await page.waitForTimeout(1_200);
  const afterCopy = await page.evaluate(() => ({ ...window.__RAST_INPUT_PROBE }));
  diagnostics.instrumentation = { afterTyping, afterCopy };
  diagnostics.localClipboard = await readLocalClipboard(page);
  diagnostics.toasts = await page.locator(".toast-message").allTextContents().catch(() => []);
  diagnostics.snapshot = await page
    .evaluate(() => {
      const iframe = document.querySelector(".linux-native-session-frame");
      try {
        const state = iframe?.contentWindow?.RASTNative?.snapshot?.() ?? null;
        if (!state) return null;
        return {
          version: state.version,
          connected: state.connected,
          clipboardEnabled: state.clipboardEnabled,
          clipboardDirection: state.clipboardDirection,
          clipboardPending: state.clipboardPending,
          clipboardLastEventAt: state.clipboardLastEventAt,
          keyboardSwap: state.keyboardSwap,
          captureKeyboard: state.captureKeyboard,
        };
      } catch (error) {
        return { error: String(error) };
      }
    })
    .catch(() => null);
  diagnostics.activeElement = await page
    .evaluate(() => {
      const iframe = document.querySelector(".linux-native-session-frame");
      const describe = (element) => (element ? { tag: element.tagName, id: element.id || "", className: typeof element.className === "string" ? element.className : "" } : null);
      return {
        parent: describe(document.activeElement),
        viewer: describe(iframe?.contentDocument?.activeElement ?? null),
      };
    })
    .catch(() => null);
  diagnostics.console = allConsole.filter((entry) => /clipboard|shim|keyboard|toast/i.test(entry.text)).slice(-20);
  await screenshot(page, "diagnose-input");
  console.log("DIAGNOSTICS " + JSON.stringify(diagnostics, null, 2));
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
  const { context, page, consoleLog, allConsole } = session;
  let openedWindow = false;
  let openedIds = [];

  try {
    console.log(`RAST input/clipboard verification — ${args.mode} mode${args.emulateMac ? " (emulated macOS)" : ""}`);
    console.log(`  target ${args.url} as ${args.user}`);

    await signIn(page, { username: args.user, password: credentials });
    await page.waitForTimeout(1_200);
    pass("00.login", "signed in through the real form");

    const beforeLaunch = await nativeWindows(page);
    const beforeIds = new Set(beforeLaunch.map((window) => window.id));
    await launchFromApplications(page, "Google Chrome");
    const launched = await waitFor(
      async () => {
        const fresh = (await readyNativeWindows(page)).filter((window) => !beforeIds.has(window.id));
        return fresh.length > 0 ? fresh : null;
      },
      { timeoutMs: beforeIds.size > 0 ? 12_000 : 90_000, intervalMs: 250, what: "a newly adopted Google Chrome window" },
    ).catch(() => null);
    let windows = launched;
    let reusedExisting = false;
    if (!windows) {
      // Another workstream may already have a Chrome window for this account:
      // use it for the probes but never close it.
      windows = (await readyNativeWindows(page)).filter((window) => window.hasCanvas);
      reusedExisting = windows.length > 0;
    }
    if (windows.length === 0) throw new Error("Google Chrome did not open an adopted native window");
    const targetWindow = windows[windows.length - 1];
    if (!reusedExisting) openedIds = windows.map((window) => window.id);
    openedWindow = true;
    if (reusedExisting) note(`re-used ${windows.length} pre-existing Chrome window(s) for this probe; they will not be closed`);
    const painted = await waitForPaintedSurface(page);
    pass("01.window", "Google Chrome became an adopted native window", `${targetWindow.id} (painted=${painted}, pre-existing=${beforeLaunch.length})`);
    if (!painted) note("the adopted surface did not paint within 45 s; screenshots may be black");
    await screenshot(page, "01-desktop-native-window");
    const windowLocator = page.locator(`.window[data-window-id="${targetWindow.id}"]`);
    await elementScreenshot(windowLocator.locator(".win-body"), "01-window-body");

    // --- focus ownership after a real click -------------------------------
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

    const keys = platformKeys(args.emulateMac);
    if (args.diagnose) {
      await diagnoseRemoteCopy(page, keys, allConsole);
      return;
    }
    await discoverRemoteDisplay(page, args.user);

    /** Does the remote application show `pattern` in its window title? */
    async function remoteTitleMatches(pattern, timeoutMs = 15_000) {
      if (remote.available) return waitForRemoteTitle(pattern, timeoutMs);
      // Fallback: the RAST window title mirrors the remote WM_NAME.
      const dom = await waitForWindowTitle(page, pattern, timeoutMs);
      return { ok: dom.ok, titles: [dom.title] };
    }

    // --- keyboard: type a URL and observe the remote application title ----
    // `about:blank` is typed through the real key path; the X11 window title of
    // the remote Chrome window is objective evidence that the keys arrived in
    // order, exactly once (a duplicated/dropped character would not navigate).
    const typingUrl = "about:blank";
    let typed = { ok: false, titles: [], at: null };
    const candidates = [
      { x: 0.45, y: 100 },
      { x: 0.4, y: 104 },
      { x: 0.5, y: 96 },
      { x: 0.35, y: 108 },
    ];
    for (const candidate of candidates) {
      await page.keyboard.press("Escape").catch(() => undefined);
      await windowLocator
        .click({ position: { x: Math.round(box.width * candidate.x), y: candidate.y } })
        .catch(() => undefined);
      await page.waitForTimeout(250);
      await page.keyboard.press(keys.selectAll);
      await page.keyboard.type(typingUrl, { delay: 12 });
      await page.keyboard.press("Enter");
      const result = await remoteTitleMatches(/about:blank/i, 6_000);
      if (result.ok) {
        typed = { ok: true, titles: result.titles, at: candidate };
        break;
      }
    }
    for (const screenshotName of ["03-typed-navigation"]) await screenshot(page, screenshotName);
    await elementScreenshot(windowLocator.locator(".win-body"), "03-window-after-typing");
    check(
      "03.typing",
      "keystrokes reach the remote application exactly once (typed URL navigated, no duplicated characters)",
      typed.ok,
      typed.ok ? `remote title: ${JSON.stringify(typed.titles)}` : `remote titles: ${JSON.stringify(typed.titles)}`,
    );

    if (!typed.ok) {
      skip("04.remote-to-browser-clipboard", "remote copy probe", "typing probe could not focus the remote address bar");
      skip("05.browser-to-remote-clipboard", "browser paste probe", "typing probe could not focus the remote address bar");
    } else {
      // --- browser -> remote clipboard, then remote -> browser ------------
      // One unicode line is written to the BROWSER clipboard, pasted into the
      // remote address bar (Ctrl/Cmd+V), selected and copied back out inside
      // the application (Ctrl/Cmd+A, Ctrl/Cmd+C). This proves:
      //   * the browser clipboard reached the X server (X11 clipboard = line),
      //   * the remote copy reached the browser clipboard through the shim.
      const unicode = `RAST ⇄ Linux ✓ — ünïcødé テスト ${STAMP.slice(-6)}`;
      const wrote = await writeLocalClipboard(page, unicode);
      let pasteOk = false;
      let remoteClipboard = null;
      let localAfterCopy = null;
      if (!wrote) {
        skip("05.browser-to-remote-clipboard", "write the browser clipboard", "harness could not write the browser clipboard");
      } else {
        await windowLocator
          .click({ position: { x: Math.round(box.width * 0.45), y: 100 } })
          .catch(() => undefined);
        await page.waitForTimeout(250);
        await page.keyboard.press(keys.selectAll);
        await page.keyboard.press(keys.paste);
        await page.waitForTimeout(900);
        // Select what the application received, then copy it out remotely.
        await page.keyboard.press(keys.selectAll);
        await writeLocalClipboard(page, "RAST-SENTINEL-" + STAMP.slice(-6));
        await page.keyboard.press(keys.copy);
        const copied = await waitForLocalClipboard(page, unicode, 12_000);
        remoteClipboard = await remoteClipboardText();
        localAfterCopy = copied.actual;
        report.pasteRoundTrip = {
          expected: unicode,
          remoteClipboard,
          localClipboard: localAfterCopy,
          ok: copied.ok,
        };
        pasteOk = remoteClipboard === unicode;
        check(
          "05.browser-to-remote-clipboard",
          "browser clipboard text (unicode) pastes into the remote application unchanged",
          pasteOk,
          `remote X11 clipboard=${JSON.stringify(remoteClipboard)} expected=${JSON.stringify(unicode)}`,
        );
        check(
          "04.remote-to-browser-clipboard",
          "a copy inside the remote application lands on the browser clipboard (shim text token path)",
          copied.ok,
          copied.ok ? `browser clipboard=${JSON.stringify(localAfterCopy)}` : `browser clipboard=${JSON.stringify(localAfterCopy)} token=${JSON.stringify(await clipboardDiagnostics(page, allConsole))}`,
        );
        if (!copied.ok) report.remoteCopyDiagnostics = await clipboardDiagnostics(page, allConsole);
        await screenshot(page, "05-paste-copy-roundtrip");
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
      await closeOpenedNativeWindows(page, openedIds).catch(() => undefined);
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
async function closeOpenedNativeWindows(page, ids) {
  const pending = new Set(ids.filter(Boolean));
  for (let round = 0; round < 20 && pending.size > 0; round += 1) {
    const windows = await nativeWindows(page).catch(() => []);
    for (const window of windows) {
      if (!pending.has(window.id)) continue;
      if (window.starting) continue;
      const close = page.locator(`.window[data-window-id="${window.id}"] .win-control-close`).first();
      if ((await close.count()) === 0) {
        pending.delete(window.id);
        continue;
      }
      await close.click({ force: true }).catch(() => undefined);
      pending.delete(window.id);
      await page.waitForTimeout(700);
    }
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
