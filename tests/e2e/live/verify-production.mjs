#!/usr/bin/env node
// Live production verification for the native Linux app integration.
//
// Verifies https://desktop.lufria.tech (the real, Cloudflare-tunnelled
// production instance) through the public UI, exactly as a person uses it:
//
//   1. Sign in as t-alice and open Google Chrome from Applications; the app
//      must become ONE RAST window with a `.linux-native-window` body that
//      hosts an adopted remote Xpra surface (canvas), with NO iframe and no
//      Xpra toolbar / remote desk top inside the window.
//   2. The single shared viewer iframe (`.linux-native-session-frame`) is the
//      hidden transport only: present in the DOM, off-screen, outside every
//      window.
//   3. From App Center: open the app tile, then close App Center; the Chrome
//      window must stay.
//   4. Ctrl+N inside Chrome -> a second independent RAST window + taskbar
//      entry; drag, resize, minimize/restore, maximize; closing one window
//      leaves the other alive.
//   5. Reload the desktop: the same Linux processes/units keep running and
//      the windows are re-adopted without duplication (host evidence via
//      read-only systemctl/pgrep).
//   6. Cross-user isolation: t-bob (and an anonymous browser) hitting
//      t-alice's session URL must get 403/401.
//   7. Metrics + JSON/markdown report (click-to-first-window latency, adopted
//      window count, session unit memory/process counts).
//
// Usage (from tests/e2e, where Playwright is installed):
//   node live/verify-production.mjs
//   node live/verify-production.mjs --headless=false   # watch it
//   node live/verify-production.mjs --skip-host        # UI-only (no sudo)
//   node live/verify-production.mjs --preflight-only   # deployment readiness only
//   node live/verify-production.mjs --url=https://desktop.lufria.tech
//
// Credentials: /home/lufria/.config/rast-test/creds (mode 0600), one
// `username=password` per line for the t-alice/t-bob test accounts. Secrets
// are never printed, logged or written into the report.
//
// The script is read-only towards the production installation: it signs in,
// opens/closes app windows through the public UI and reads host state with
// systemctl/pgrep. It never edits /etc/rast, never restarts services and never
// touches nginx/cloudflared.
//
// Exit code 0 only when every check passed (or was explicitly skipped).

import { execFile } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";

import { chromium } from "playwright";

const execFileAsync = promisify(execFile);

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------
const LIVE_DIR = dirname(fileURLToPath(import.meta.url));
const STAMP = new Date().toISOString().replace(/[:.]/g, "-").replace("Z", "");
const RUN_DIR = join(LIVE_DIR, "results", `live-${STAMP}`);
const SHOTS_DIR = join(LIVE_DIR, "screenshots", `live-${STAMP}`);

function parseArgs(argv) {
  const args = {
    url: process.env.RAST_LIVE_URL || "https://desktop.lufria.tech",
    creds: process.env.RAST_LIVE_CREDS || "/home/lufria/.config/rast-test/creds",
    headless: true,
    skipHost: false,
    keepWindows: false,
    preflightOnly: false,
    chromium: process.env.RAST_LIVE_CHROMIUM || null,
  };
  for (const item of argv) {
    if (item === "--headless=false") args.headless = false;
    else if (item.startsWith("--url=")) args.url = item.slice("--url=".length);
    else if (item.startsWith("--creds=")) args.creds = item.slice("--creds=".length);
    else if (item === "--skip-host") args.skipHost = true;
    else if (item === "--keep-windows") args.keepWindows = true;
    else if (item === "--preflight-only") args.preflightOnly = true;
    else if (item.startsWith("--chromium=")) args.chromium = item.slice("--chromium=".length);
    else if (item === "-h" || item === "--help") args.help = true;
    else {
      console.error(`unknown argument: ${item}`);
      process.exit(2);
    }
  }
  args.url = args.url.replace(/\/$/, "");
  return args;
}

const args = parseArgs(process.argv.slice(2));
if (args.help) {
  console.log(readFileSync(fileURLToPath(import.meta.url), "utf8").split("\n").slice(1, 40).join("\n"));
  process.exit(0);
}

const VIEWPORT = { width: 1480, height: 920 };
const WINDOW_TIMEOUT_MS = 300_000;
const CACHED_CHROMIUM = "/home/lufria/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome";

// ---------------------------------------------------------------------------
// Recorder / report model
// ---------------------------------------------------------------------------
class PhaseAbort extends Error {}

const rec = {
  checks: [],
  phases: [],
  metrics: {},
  details: [],
  screenshots: [],
  failures: [],
  note(text) {
    const line = String(text);
    rec.details.push(line);
    console.log(`  · ${line}`);
  },
  pass(id, what, detail) {
    rec.checks.push({ id, what, status: "pass", detail: detail ?? null });
    console.log(`  PASS ${id} — ${what}`);
  },
  fail(id, what, error) {
    const item = { id, what, status: "fail", error: String(error?.message ?? error) };
    rec.checks.push(item);
    rec.failures.push(item);
    console.log(`  FAIL ${id} — ${what}: ${item.error}`);
  },
  skip(id, what, reason) {
    rec.checks.push({ id, what, status: "skip", detail: reason });
    console.log(`  SKIP ${id} — ${what}: ${reason}`);
  },
  metric(name, value, unit = "") {
    rec.metrics[name] = { value, unit };
    return value;
  },
};

async function step(id, what, fn) {
  try {
    const detail = await fn();
    rec.pass(id, what, detail);
    return { ok: true, value: detail };
  } catch (error) {
    rec.fail(id, what, error);
    return { ok: false, error };
  }
}

async function required(id, what, fn) {
  const result = await step(id, what, fn);
  if (!result.ok) throw new PhaseAbort(`${id}: ${result.error.message}`);
  return result.value;
}

async function phase(id, title, fn) {
  const started = Date.now();
  const record = { id, title, status: "pass", startedAt: new Date().toISOString(), durationMs: 0, error: null };
  console.log(`\n[${id}] ${title}`);
  try {
    await fn();
  } catch (error) {
    record.status = error instanceof PhaseAbort ? "aborted" : "fail";
    record.error = String(error?.stack ?? error);
    rec.failures.push({ id, what: title, status: "fail", error: String(error?.message ?? error) });
    console.log(`  PHASE ${record.status.toUpperCase()}: ${error?.message ?? error}`);
  }
  record.durationMs = Date.now() - started;
  rec.phases.push(record);
  return record;
}

// ---------------------------------------------------------------------------
// Credentials (never printed) and generic helpers
// ---------------------------------------------------------------------------
function loadCredentials(path) {
  const text = readFileSync(path, "utf8");
  const users = new Map();
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const index = trimmed.indexOf("=");
    if (index <= 0) continue;
    users.set(trimmed.slice(0, index).trim(), trimmed.slice(index + 1).trim());
  }
  const alice = users.get("t-alice");
  const bob = users.get("t-bob");
  if (!alice || !bob) throw new Error(`${path} must define t-alice and t-bob (username=password per line)`);
  return { alice: { username: "t-alice", password: alice }, bob: { username: "t-bob", password: bob } };
}

/** Session keys are credentials: never let one reach the console or report. */
function redactSessionUrl(value) {
  return String(value ?? "").replace(/(\/linux-apps\/session\/)[^/?#]+/g, "$1<redacted>");
}

function redactDeep(value, depth = 0) {
  if (typeof value === "string") return redactSessionUrl(value);
  if (value === null || value === undefined) return value;
  // Guard against cyclic/deeply nested values: the report must always be
  // writable, even when a failure stores an exotic object.
  if (depth > 24) return "[truncated]";
  if (Array.isArray(value)) return value.map((item) => redactDeep(item, depth + 1));
  if (typeof value === "object") {
    const proto = Object.getPrototypeOf(value);
    if (proto !== Object.prototype && proto !== null) return String(value);
    return Object.fromEntries(
      Object.entries(value).map(([key, item]) => [
        key,
        /^(csrf|csrf_?token|password|secret|cookie|set-cookie|authorization)$/i.test(key)
          ? "<redacted>"
          : redactDeep(item, depth + 1),
      ]),
    );
  }
  return value;
}

const sleep = (ms) => new Promise((resolvePromise) => setTimeout(resolvePromise, ms));

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
      const suffix = lastError ? ` (last error: ${lastError.message})` : "";
      throw new Error(`timed out after ${timeoutMs} ms waiting for ${what}${suffix}`);
    }
    await sleep(intervalMs);
  }
}

async function screenshot(page, name, { clip = null } = {}) {
  mkdirSync(SHOTS_DIR, { recursive: true });
  const path = join(SHOTS_DIR, `${name}.png`);
  if (clip) await page.screenshot({ path, clip, animations: "disabled" });
  else await page.screenshot({ path, animations: "disabled" });
  rec.screenshots.push(path);
  return path;
}

async function elementScreenshot(locator, name) {
  mkdirSync(SHOTS_DIR, { recursive: true });
  const path = join(SHOTS_DIR, `${name}.png`);
  await locator.screenshot({ path, animations: "disabled" });
  rec.screenshots.push(path);
  return path;
}

// ---------------------------------------------------------------------------
// Host introspection (read-only)
// ---------------------------------------------------------------------------
async function hostRun(command, argv, { timeoutMs = 60_000 } = {}) {
  try {
    const { stdout } = await execFileAsync(command, argv, {
      timeout: timeoutMs,
      maxBuffer: 16 * 1024 * 1024,
      env: process.env,
    });
    return { ok: true, stdout, stderr: "" };
  } catch (error) {
    return { ok: false, stdout: error.stdout ?? "", stderr: error.stderr ?? String(error) };
  }
}

/** Read-only privileged command (passwordless sudo, as used by the E2E harness). */
async function hostSudo(command, argv) {
  const viaSudo = await hostRun("sudo", ["-n", command, ...argv]);
  if (viaSudo.ok) return viaSudo;
  const direct = await hostRun(command, argv);
  if (direct.ok) return direct;
  return { ok: false, stdout: "", stderr: `sudo and direct both failed: ${viaSudo.stderr || direct.stderr}` };
}

async function systemctlShow(unit) {
  const result = await hostSudo("systemctl", [
    "show",
    unit,
    "--property=ActiveState,SubState,MainPID,MemoryCurrent,CPUUsageNSec,TasksCurrent,Result,User",
  ]);
  const status = {};
  if (!result.ok) return status;
  for (const line of result.stdout.split("\n")) {
    const index = line.indexOf("=");
    if (index > 0) status[line.slice(0, index)] = line.slice(index + 1);
  }
  return status;
}

async function listUnits(patterns) {
  const result = await hostSudo("systemctl", [
    "list-units",
    "--type=service",
    "--all",
    "--no-legend",
    "--plain",
    ...patterns,
  ]);
  if (!result.ok) return [];
  return result.stdout
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => {
      const parts = line.split(/\s+/);
      return { unit: parts[0], load: parts[1], active: parts[2], sub: parts[3], description: parts.slice(4).join(" ") };
    });
}

/** Map RAST user id -> Linux identity (rast-u<id>) from the passwd database. */
async function linuxIdentity(userId) {
  const result = await hostSudo("getent", ["passwd"]);
  if (!result.ok) return null;
  for (const line of result.stdout.split("\n")) {
    const parts = line.split(":");
    if (parts.length < 7) continue;
    if (parts[0] === `rast-u${userId}`) {
      return { username: parts[0], uid: Number(parts[2]), gid: Number(parts[3]), home: parts[5] };
    }
  }
  return null;
}

async function pgrepCount(pattern, { uid = null } = {}) {
  const argv = uid === null ? ["-c", "-f", pattern] : ["-c", "-u", String(uid), "-f", pattern];
  const result = await hostSudo("pgrep", argv);
  const count = Number(String(result.stdout).trim());
  return Number.isFinite(count) ? count : 0;
}

async function hostSnapshot({ userId, label }) {
  const identity = await linuxIdentity(userId);
  const uid = identity?.uid ?? null;
  const sessionUnit = `rast-xpra-u${userId}`;
  const session = await systemctlShow(sessionUnit);
  const appUnits = (await listUnits(["rast-app-*"])).filter((unit) => unit.active === "active");
  const appStatuses = [];
  for (const unit of appUnits) {
    const status = await systemctlShow(unit.unit);
    appStatuses.push({
      unit: unit.unit,
      active: status.ActiveState,
      sub: status.SubState,
      memoryBytes: Number(status.MemoryCurrent) || 0,
      tasks: Number(status.TasksCurrent) || 0,
    });
  }
  return {
    label,
    at: new Date().toISOString(),
    identity,
    sessionUnit,
    session: {
      active: session.ActiveState,
      sub: session.SubState,
      mainPid: Number(session.MainPID) || 0,
      memoryBytes: Number(session.MemoryCurrent) || 0,
      tasks: Number(session.TasksCurrent) || 0,
      cpuNs: Number(session.CPUUsageNSec) || 0,
    },
    appUnits: appStatuses,
    // Processes of the app (Chrome) run as the user's Linux identity, in the
    // app/session units' cgroups. Count only that uid to stay read-only and
    // exact; fall back to 0 when the identity is unknown.
    chromeProcesses: uid === null ? 0 : await pgrepCount("google-chrome|chrome", { uid }),
  };
}

// ---------------------------------------------------------------------------
// Browser helpers
// ---------------------------------------------------------------------------
async function launchBrowser() {
  const executablePath = args.chromium || CACHED_CHROMIUM;
  // Use the cached Playwright Chromium when present; otherwise let Playwright
  // resolve its own browser bundle.
  const resolved = existsSync(executablePath) ? executablePath : undefined;
  return chromium.launch({
    headless: args.headless,
    executablePath: resolved,
    args: [
      "--no-sandbox",
      "--disable-gpu",
      "--disable-dev-shm-usage",
      "--disable-background-timer-throttling",
      "--disable-renderer-backgrounding",
    ],
  });
}

async function newContext(browser) {
  const context = await browser.newContext({ viewport: VIEWPORT, ignoreHTTPSErrors: true });
  const page = await context.newPage();
  page.setDefaultTimeout(30_000);
  const logs = [];
  page.on("console", (message) => {
    if (message.type() === "error" || message.type() === "warning") {
      logs.push({ type: message.type(), text: message.text().slice(0, 500) });
    }
  });
  page.on("pageerror", (error) => logs.push({ type: "pageerror", text: String(error).slice(0, 500) }));
  return { context, page, logs };
}

async function submitLoginForm(page, user) {
  const submit = page.locator('.auth-form button[type="submit"], button:has-text("Sign in")').first();
  await page.fill('input[name="username"]', user.username);
  await page.fill('input[type="password"]', user.password);
  const [response] = await Promise.all([
    page
      .waitForResponse(
        (candidate) => candidate.url().includes("/api/auth/login") && candidate.request().method() === "POST",
        { timeout: 60_000 },
      )
      .catch(() => null),
    submit.click(),
  ]);
  return {
    status: response?.status() ?? 0,
    retryAfter: Number(response?.headers()?.["retry-after"] ?? 0) || 0,
  };
}

async function signIn(browser, user) {
  const { context, page, logs } = await newContext(browser);
  // The SPA renders the auth form (or the desktop for an existing cookie)
  // asynchronously after DOMContentLoaded: wait for whichever comes first
  // instead of probing the DOM immediately (a plain `count()` here raced the
  // boot and silently skipped the login).
  await page.goto(`${args.url}/`, { waitUntil: "domcontentloaded" });
  await page.waitForSelector(".auth-form, .taskbar", { timeout: 90_000 });

  for (let attempt = 1; attempt <= 3; attempt += 1) {
    if (await page.locator(".taskbar").count()) break;
    if (attempt > 1) {
      // Re-open a clean login screen before retrying.
      await page.goto(`${args.url}/`, { waitUntil: "domcontentloaded" });
      await page.waitForSelector(".auth-form, .taskbar", { timeout: 90_000 });
      if (await page.locator(".taskbar").count()) break;
    }
    const loginInput = page.locator('input[name="username"]');
    await loginInput.waitFor({ state: "visible", timeout: 30_000 });
    const submitted = await submitLoginForm(page, user);
    if (submitted.status === 401) {
      await screenshot(page, `login-rejected-${user.username}`).catch(() => {});
      throw new Error(`sign-in as ${user.username} was rejected (HTTP 401: wrong credentials or locked account)`);
    }
    let reached = true;
    try {
      await page.waitForSelector(".taskbar", { timeout: 60_000 });
    } catch {
      reached = false;
    }
    if (reached) {
      const me = await page.evaluate(async () => {
        const response = await fetch("/api/auth/me", { credentials: "same-origin" });
        return { status: response.status, body: response.ok ? await response.json() : null };
      });
      if (me.status !== 200) throw new Error(`sign-in as ${user.username} failed (HTTP ${me.status})`);
      const username = me.body?.username ?? me.body?.user?.username;
      if (username !== user.username) {
        throw new Error(`signed in as ${username ?? "?"} instead of ${user.username}`);
      }
      return { context, page, logs, me: me.body };
    }
    // The production login endpoint is rate limited (10 hits/IP per 300 s,
    // shared with earlier runs): wait out `Retry-After` and try again. This is
    // infrastructure polish, not a product failure.
    if (submitted.status === 429) {
      const waitSecs = Math.min((submitted.retryAfter || 60) + 3, 120);
      rec.note(
        `login for ${user.username} throttled (HTTP 429, Retry-After ${submitted.retryAfter || "?"}s, attempt ${attempt}/3); ` +
          `waiting ${waitSecs}s before retrying`,
      );
      await sleep(waitSecs * 1000);
      continue;
    }
    const text = await page.evaluate(() => (document.body.innerText || "").slice(0, 400)).catch(() => "");
    await screenshot(page, `login-failure-${user.username}`).catch(() => {});
    throw new Error(
      `sign-in as ${user.username} did not reach the desktop (HTTP ${submitted.status || "?"}); page: ${text.replace(/\s+/g, " ").slice(0, 250)}`,
    );
  }
  if (!(await page.locator(".taskbar").count())) {
    throw new Error(`sign-in as ${user.username} still throttled after retries (production login rate limit)`);
  }
  const me = await page.evaluate(async () => {
    const response = await fetch("/api/auth/me", { credentials: "same-origin" });
    return { status: response.status, body: response.ok ? await response.json() : null };
  });
  return { context, page, logs, me: me.body };
}

/** Same-origin GET from the page (browser sends its real session cookie). */
async function apiGet(page, path) {
  return page.evaluate(async (requestPath) => {
    const response = await fetch(requestPath, { credentials: "same-origin", headers: { accept: "application/json" } });
    let body = null;
    try {
      body = await response.json();
    } catch {
      body = null;
    }
    return { status: response.status, body };
  }, path);
}

// -- windows ---------------------------------------------------------------
/**
 * All RAST windows with the native-integration facts the live verification
 * needs: is the body a `.linux-native-window`, which remote Xpra windows are
 * adopted there (and do they carry a painted canvas/video), and whether an
 * iframe/toolbar leaked inside.
 */
async function windowsInfo(page) {
  return page.evaluate(() => {
    const isVisible = (el) => {
      const rect = el.getBoundingClientRect();
      const style = getComputedStyle(el);
      return rect.width > 0 && rect.height > 0 && style.display !== "none" && style.visibility !== "hidden";
    };
    return [...document.querySelectorAll(".window[data-window-id]")].map((el) => {
      const body = el.querySelector(".win-body");
      const rect = el.getBoundingClientRect();
      const adopted = body
        ? [...body.querySelectorAll("[data-rast-native-wid]")].map((node) => ({
            wid: node.getAttribute("data-rast-native-wid"),
            canvasCount: node.querySelectorAll("canvas").length,
            videoCount: node.querySelectorAll("video").length,
            canvases: [...node.querySelectorAll("canvas")].map((canvas) => ({
              width: canvas.width,
              height: canvas.height,
              clientWidth: canvas.clientWidth,
              clientHeight: canvas.clientHeight,
              visible: isVisible(canvas),
            })),
            windowheads: [...node.querySelectorAll(".windowhead")].map((head) => ({
              display: getComputedStyle(head).display,
              visible: isVisible(head),
            })),
          }))
        : [];
      const visibleIframes = body ? [...body.querySelectorAll("iframe")].filter(isVisible).length : 0;
      return {
        id: el.dataset.windowId,
        appId: el.dataset.appId ?? "",
        title: el.querySelector(".win-title")?.textContent?.trim() ?? "",
        focused: el.classList.contains("is-focused") || el.classList.contains("focused"),
        minimized: el.classList.contains("is-minimized") || el.classList.contains("minimized"),
        maximized: el.classList.contains("is-maximized") || el.classList.contains("maximized"),
        rect: {
          x: Math.round(rect.x),
          y: Math.round(rect.y),
          width: Math.round(rect.width),
          height: Math.round(rect.height),
        },
        native: !!body?.classList.contains("linux-native-window"),
        starting: /^Starting\b/.test(el.querySelector(".win-title")?.textContent?.trim() ?? ""),
        problem: !!body?.querySelector(".linux-native-problem"),
        iframeCount: body ? body.querySelectorAll("iframe").length : 0,
        visibleIframes,
        adopted,
        toolbarNodes: body
          ? [
              ...body.querySelectorAll(
                '[class*="toolbar" i], [id*="toolbar" i], .xpra-toolbar, #xpra-toolbar, .xpra-desktop',
              ),
            ]
              .filter(isVisible)
              .map((node) => (node.id ? `#${node.id}` : `.${String(node.className).split(" ")[0]}`))
          : [],
      };
    });
  });
}

function isReadyNativeWindow(window) {
  return (
    window.native &&
    !window.starting &&
    !window.problem &&
    window.adopted.length > 0 &&
    window.adopted.some(
      (node) => node.canvasCount + node.videoCount > 0 && node.canvases.some((canvas) => canvas.visible),
    )
  );
}

async function nativeWindows(page) {
  return (await windowsInfo(page)).filter((window) => window.native);
}

async function readyNativeWindows(page) {
  return (await windowsInfo(page)).filter(isReadyNativeWindow);
}

async function waitForReadyNativeWindows(page, { min = 1, max = Infinity, timeoutMs = WINDOW_TIMEOUT_MS, what } = {}) {
  return waitFor(
    async () => {
      const windows = await readyNativeWindows(page);
      if (windows.length >= min && windows.length <= max) return windows;
      return null;
    },
    { timeoutMs, intervalMs: 250, what: what || `${min} adopted native window(s)` },
  );
}

async function taskbarInfo(page) {
  return page.evaluate(() =>
    [...document.querySelectorAll(".taskbar-app")].map((el) => ({
      label: el.querySelector(".taskbar-app-label")?.textContent?.trim() ?? el.textContent?.trim() ?? "",
      title: el.getAttribute("data-tip") ?? "",
      appId: el.dataset.appId ?? el.querySelector(".taskbar-app-icon")?.dataset?.app ?? null,
      active: el.classList.contains("is-active"),
      minimized: el.classList.contains("is-minimized"),
      pinned: el.classList.contains("is-pinned"),
    })),
  );
}

async function clickWindowTitleBar(page, windowId) {
  await page.locator(`.window[data-window-id="${windowId}"]`).click({ position: { x: 60, y: 8 }, force: true });
  await page.waitForTimeout(350);
}

/**
 * Close a window through its taskbar entry (middle click = close). This is the
 * reliable path when another window covers the target's title bar: a forced
 * click on the close control would land on the covering window instead.
 */
async function closeWindowViaTaskbar(page, window) {
  const entries = page.locator(".taskbar-app");
  const count = await entries.count();
  const target = String(window.title ?? window.appId ?? "");
  for (let index = 0; index < count; index += 1) {
    const entry = entries.nth(index);
    const facts = await entry.evaluate((el) => ({
      label: el.querySelector(".taskbar-app-label")?.textContent?.trim() ?? "",
      tip: el.getAttribute("data-tip") ?? "",
      active: el.classList.contains("is-active"),
      minimized: el.classList.contains("is-minimized"),
    }));
    if (facts.tip === target || facts.label === target || (target && facts.tip.startsWith(target))) {
      await entry.click({ button: "middle", force: true });
      return true;
    }
  }
  return false;
}

async function closeWindow(page, windowId) {
  const info = (await windowsInfo(page)).find((window) => window.id === windowId);
  const window = page.locator(`.window[data-window-id="${windowId}"]`);
  const close = window.locator(".win-control-close");
  if (await close.count()) {
    await close.first().click({ force: true }).catch(() => {});
    const detached = await window
      .waitFor({ state: "detached", timeout: 4_000 })
      .then(() => true)
      .catch(() => false);
    if (detached) return;
  }
  if (info && (await closeWindowViaTaskbar(page, info))) {
    await window.waitFor({ state: "detached", timeout: 10_000 }).catch(() => {});
    return;
  }
  await clickWindowTitleBar(page, windowId);
  await page.locator(".taskbar-app.is-active").first().click({ button: "middle" }).catch(() => {});
}

async function closeAllNativeWindows(page, { keep = [] } = {}) {
  for (let round = 0; round < 30; round += 1) {
    const windows = (await windowsInfo(page)).filter(
      (window) => window.native && !window.starting && !keep.includes(window.id),
    );
    if (windows.length === 0) return true;
    await closeWindow(page, windows[windows.length - 1].id);
    await page.waitForTimeout(300);
  }
  return (await nativeWindows(page)).filter((window) => !window.starting).length === 0;
}

// -- start menu / App Center ------------------------------------------------
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
  const clickedAt = Date.now();
  await tile.click();
  await page.waitForSelector(".start-menu", { state: "detached", timeout: 10_000 }).catch(() => {});
  return clickedAt;
}

async function appCenterWindowId(page) {
  const windows = await windowsInfo(page);
  const appCenter = windows.find((window) => /App Center/i.test(window.title));
  return appCenter?.id ?? null;
}

async function openAppCenter(page) {
  await launchFromApplications(page, "App Center");
  const id = await waitFor(() => appCenterWindowId(page), { timeoutMs: 30_000, what: "the App Center window" });
  return id;
}

/**
 * App Center is the `packages` application (adminOnly in the shell). Ordinary
 * accounts only get it through an explicit group grant, so the live script
 * checks before trying to open it and reports the permission facts instead of
 * failing a product-independent check.
 */
async function appCenterAvailability(page) {
  const me = await apiGet(page, "/api/auth/me");
  const permission = me.body?.permissions?.apps?.packages ?? null;
  const user = me.body?.user ?? null;
  const isAdmin = user?.role === "admin" || user?.superAdmin === true;
  const packages = await apiGet(page, "/api/packages");
  return {
    allowed: Boolean(isAdmin || permission?.allowed),
    isAdmin,
    permission,
    packagesStatus: packages.status,
  };
}

/**
 * What is still running on the host when the verification finishes. The Lead
 * tears these down after the release; the script must never stop them.
 */
async function captureHostAfter() {
  // Leave everything running (the Lead tears it down): report the active native
  // units and related processes so the report states exactly what is still
  // alive when the verification finishes.
  const units = [...(await listUnits(["rast-xpra-*"])), ...(await listUnits(["rast-app-*"]))];
  const runningUnits = [];
  for (const unit of units) {
    if (unit.active !== "active") continue;
    const status = await systemctlShow(unit.unit);
    runningUnits.push({
      unit: unit.unit,
      active: status.ActiveState,
      sub: status.SubState,
      mainPid: Number(status.MainPID) || 0,
      memoryBytes: Number(status.MemoryCurrent) || 0,
      tasks: Number(status.TasksCurrent) || 0,
    });
  }
  const rastProcesses = (await hostSudo("pgrep", ["-af", "rast --config"]))
    .stdout.split("\n")
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => {
      const [pid, ...command] = line.split(/\s+/);
      return { pid: Number(pid) || 0, command: command.join(" ") };
    });
  const chromeHostWide = await pgrepCount("google-chrome", {});
  return {
    capturedAt: new Date().toISOString(),
    activeNativeUnits: runningUnits,
    rastProcesses,
    chromeProcessesHostWide: chromeHostWide,
    note: "App windows were closed through the UI; native session/app units were deliberately left running for teardown.",
  };
}

// ---------------------------------------------------------------------------
// Main verification
// ---------------------------------------------------------------------------
let browser = null;
let alice = null;
let bob = null;
/** Host state at the end of the run (reported, not cleaned up). */
let hostAfter = null;
const metric = rec.metric;

async function main() {
  mkdirSync(RUN_DIR, { recursive: true });
  mkdirSync(SHOTS_DIR, { recursive: true });
  const overallStarted = Date.now();

  const credentials = loadCredentials(args.creds);
  // Never print the credentials themselves; usernames are the only safe part.
  rec.note(`credentials loaded for users: ${Object.keys(credentials).join(", ")}`);
  rec.note(`target: ${args.url}`);

  browser = await launchBrowser();
  rec.note(`browser: ${browser.version()} (headless: ${args.headless})`);

  // -- phase 0: production reachable and accounts usable --------------------
  await phase("00-preflight", "Production reachability, version and native app availability", async () => {
    const health = await fetch(`${args.url}/healthz`, { redirect: "manual" }).catch((error) => ({ status: 0, error }));
    await required("prod.health", "production /healthz answers 200", async () => {
      if (health.status !== 200) throw new Error(`/healthz returned ${health.status}`);
      return "HTTP 200";
    });
    const bootstrap = await fetch(`${args.url}/api/bootstrap`).then((response) => response.json());
    rec.note(`production RAST ${bootstrap.version ?? "?"} (${bootstrap.product ?? "RAST"})`);

    // Never store the /api/auth/me payload in the report: it contains the CSRF
    // token. Only the account id/role are recorded as evidence.
    const aliceLogin = await step("prod.login.alice", "t-alice signs in through the real form", async () => {
      alice = await signIn(browser, credentials.alice);
      const user = alice.me?.user ?? {};
      return `user id ${user.id ?? "?"}, role ${user.role ?? "?"}`;
    });
    if (!aliceLogin.ok) throw new PhaseAbort(`prod.login.alice: ${aliceLogin.error.message}`);
    rec.note(`t-alice user id ${alice.me?.user?.id ?? "?"} (role ${alice.me?.user?.role ?? "?"})`);

    const bobLogin = await step("prod.login.bob", "t-bob signs in (for the cross-user isolation check)", async () => {
      bob = await signIn(browser, credentials.bob);
      const user = bob.me?.user ?? {};
      return `user id ${user.id ?? "?"}, role ${user.role ?? "?"}`;
    });
    if (!bobLogin.ok) throw new PhaseAbort(`prod.login.bob: ${bobLogin.error.message}`);
    rec.note(`t-bob user id ${bob.me?.user?.id ?? "?"} (role ${bob.me?.user?.role ?? "?"})`);

    await required("prod.native.enabled", "the server offers the native Linux app API", async () => {
      const listing = await apiGet(alice.page, "/api/linux-apps");
      if (listing.status === 404 || listing.status === 501) {
        throw new Error(
          `GET /api/linux-apps -> HTTP ${listing.status}: native_enabled/linux_apps_enabled is not on (deployment prerequisite)`,
        );
      }
      if (listing.status !== 200) throw new Error(`GET /api/linux-apps -> HTTP ${listing.status}`);
      const apps = listing.body?.apps ?? listing.body?.items ?? [];
      const chrome = apps.find((app) => /google chrome/i.test(String(app.name ?? "")) || app.id === "chrome");
      if (!chrome) throw new Error(`Google Chrome is not installed/visible for t-alice (apps: ${apps.map((app) => app.id).join(", ")})`);
      rec.note(`Google Chrome: id=${chrome.id} backend=${chrome.backend ?? "?"} status=${chrome.status ?? "?"}`);
      if (chrome.backend && chrome.backend !== "native") {
        throw new Error(`Google Chrome backend is '${chrome.backend}', expected 'native'`);
      }
      return { appId: chrome.id, backend: chrome.backend ?? null, status: chrome.status ?? null };
    });

    await required("prod.native.session-api", "GET /api/linux-apps/session answers for t-alice", async () => {
      const state = await apiGet(alice.page, "/api/linux-apps/session");
      if (state.status !== 200) throw new Error(`HTTP ${state.status}`);
      return { hasSession: Boolean(state.body?.session) };
    });

    await screenshot(alice.page, "00-desktop-signed-in");
  });

  if (!args.preflightOnly) {
  // -- phase 1: one native window from Applications -------------------------
  let firstWindow = null;
  let sessionUrl = null;
  await phase("01-native-window", "Applications -> Google Chrome becomes one adopted native window", async () => {
    // Deterministic starting point: close leftovers the test account may have
    // from an earlier verification (a fresh page load re-adopts running apps).
    await step("cleanup.leftover-windows", "no leftover native app window from earlier runs", async () => {
      const before = (await windowsInfo(alice.page)).filter((window) => window.native && !window.starting);
      if (before.length === 0) return "none";
      await closeAllNativeWindows(alice.page);
      await waitFor(async () => (await windowsInfo(alice.page)).filter((window) => window.native && !window.starting).length === 0, {
        timeoutMs: 60_000,
        what: "leftover native windows to close",
      });
      return `closed ${before.length} leftover window(s)`;
    });

    const clickedAt = await required("01.launch.applications", "open Google Chrome from the Applications menu", () =>
      launchFromApplications(alice.page, "Google Chrome"),
    );
    const ready = await required("01.window.adopted", "exactly one adopted `.linux-native-window` appears", async () => {
      const windows = await waitForReadyNativeWindows(alice.page, { min: 1, timeoutMs: WINDOW_TIMEOUT_MS, what: "the adopted Chrome window" });
      return windows;
    });
    if (!ready || ready.length === 0) throw new PhaseAbort("no adopted native window");
    firstWindow = ready[0];
    let latencyMs = Date.now() - clickedAt;
    if (ready.length === 1) {
      // Prefer the real click→window time only when the window we waited for is
      // the one created by that click (repeats of the test may have a warm app).
      metric("clickToFirstWindowMs", latencyMs, "ms");
    }
    await required("01.window.single", "exactly one RAST window hosts Chrome (no duplicates)", async () => {
      const windows = await readyNativeWindows(alice.page);
      if (windows.length !== 1) throw new Error(`expected exactly 1 adopted window, found ${windows.length}`);
      return windows[0].id;
    });

    await required("01.window.native-body", "the window body is `.linux-native-window` with an adopted remote surface", async () => {
      const window = (await nativeWindows(alice.page)).find((candidate) => candidate.id === firstWindow.id);
      if (!window) throw new Error("the window disappeared");
      if (!window.native) throw new Error("window body is not `.linux-native-window`");
      const node = window.adopted[0];
      if (!node) throw new Error("no adopted remote Xpra node ([data-rast-native-wid]) in the body");
      const painted = node.canvases.some((canvas) => canvas.visible && canvas.width > 0 && canvas.height > 0);
      if (!painted) throw new Error("the adopted surface has no painted, visible canvas/video");
      return `${window.title} | wid ${node.wid} | canvas ${node.canvases.map((canvas) => `${canvas.width}x${canvas.height}`).join(",")}`;
    });

    await required("01.window.no-iframe", "the app window body contains no iframe (transport stays outside)", async () => {
      const window = (await nativeWindows(alice.page)).find((candidate) => candidate.id === firstWindow.id);
      if (!window) throw new Error("the window disappeared");
      if (window.iframeCount !== 0) throw new Error(`window body contains ${window.iframeCount} iframe(s)`);
      return "0 iframes";
    });

    await required("01.window.no-xpra-chrome", "no Xpra toolbar/remote-desktop chrome is visible inside the window", async () => {
      const window = (await nativeWindows(alice.page)).find((candidate) => candidate.id === firstWindow.id);
      if (!window) throw new Error("the window disappeared");
      if (window.toolbarNodes.length > 0) throw new Error(`visible toolbar/desktop nodes: ${window.toolbarNodes.join(", ")}`);
      for (const node of window.adopted) {
        for (const head of node.windowheads) {
          if (head.visible) throw new Error("an Xpra window decoration (.windowhead) is visible inside the RAST window");
        }
      }
      return "no toolbar nodes, Xpra decorations hidden";
    });

    await required("01.transport.hidden", "the single `.linux-native-session-frame` transport iframe is off-screen and outside every window", async () => {
      const facts = await alice.page.evaluate(() => {
        const iframes = [...document.querySelectorAll("iframe.linux-native-session-frame")];
        return {
          count: iframes.length,
          items: iframes.map((iframe) => {
            const rect = iframe.getBoundingClientRect();
            const style = getComputedStyle(iframe);
            return {
              insideWindow: Boolean(iframe.closest(".window")),
              x: Math.round(rect.x),
              right: Math.round(rect.right),
              width: Math.round(rect.width),
              display: style.display,
              visibility: style.visibility,
            };
          }),
        };
      });
      if (facts.count !== 1) throw new Error(`expected exactly one hidden transport iframe, found ${facts.count}`);
      const item = facts.items[0];
      if (item.insideWindow) throw new Error("the transport iframe is inside a RAST window");
      if (!(item.right <= 0 || item.x <= -1000)) {
        throw new Error(`the transport iframe is on screen (x=${item.x}, right=${item.right})`);
      }
      if (item.display === "none") throw new Error("the transport iframe is display:none (it must stay laid out)");
      return `off-screen at x=${item.x} (right=${item.right}), outside all windows, display:${item.display}`;
    });

    await required("01.surface.only-native", "no remote viewer surface exists outside the adopted window", async () => {
      const stray = await alice.page.evaluate(() => {
        const visible = (el) => {
          const rect = el.getBoundingClientRect();
          const style = getComputedStyle(el);
          return rect.width > 0 && rect.height > 0 && style.display !== "none" && style.visibility !== "hidden";
        };
        return [...document.querySelectorAll("canvas, video")]
          .filter((el) => !el.closest(".linux-native-window") && visible(el))
          .map((el) => el.tagName.toLowerCase());
      });
      if (stray.length > 0) throw new Error(`visible remote surface outside the native window: ${stray.join(", ")}`);
      return "all visible canvases/videos are inside the app window";
    });

    // Evidence: full desktop + close-up of the window body.
    await screenshot(alice.page, "01-desktop-chrome-window");
    await elementScreenshot(
      alice.page.locator(`.window[data-window-id="${firstWindow.id}"] .win-body`),
      "01-window-body-closeup",
    );

    // Metrics + session URL (redacted) for the isolation check later.
    const snapshot = await hostSnapshot({ userId: alice.me.id ?? alice.me.user?.id, label: "after-first-window" });
    rec.note(`host state: ${JSON.stringify(redactDeep({
      session: snapshot.session,
      appUnits: snapshot.appUnits,
      chromeProcesses: snapshot.chromeProcesses,
    }))}`);
    metric("sessionMemoryBytes", snapshot.session.memoryBytes, "bytes");
    metric("sessionTasks", snapshot.session.tasks);
    metric("appProcessCount", snapshot.chromeProcesses);
    metric("appUnitCount", snapshot.appUnits.length);

    const state = await apiGet(alice.page, "/api/linux-apps/session");
    sessionUrl = state.body?.session?.url ?? null;
    if (!sessionUrl) throw new PhaseAbort("the native session has no viewer URL");
    rec.note(`session viewer URL: ${redactSessionUrl(sessionUrl)}`);
  });

  // -- phase 2: App Center launch, closing App Center keeps the window ------
  await phase("02-appcenter", "App Center tile launch; closing App Center keeps the app window", async () => {
    // App Center is the admin-only `packages` app. Use an optional admin
    // account (RAST_LIVE_ADMIN_USER/PASSWORD) when the ordinary test account
    // has no grant; otherwise record a precise, evidence-backed skip and keep
    // the Applications-launched window for the next phase.
    let target = alice;
    let adminSession = null;
    let availability = await appCenterAvailability(target.page);
    if (!availability.allowed && process.env.RAST_LIVE_ADMIN_USER && process.env.RAST_LIVE_ADMIN_PASSWORD) {
      adminSession = await signIn(browser, {
        username: process.env.RAST_LIVE_ADMIN_USER,
        password: process.env.RAST_LIVE_ADMIN_PASSWORD,
      });
      target = adminSession;
      rec.note(`App Center check using administrator account ${process.env.RAST_LIVE_ADMIN_USER}`);
      availability = await appCenterAvailability(target.page);
    }
    if (!availability.allowed) {
      rec.skip(
        "02.appcenter",
        "App Center tile launch path",
        `App Center is admin-only in RAST 0.18.3 (packages app). ${target.me?.user?.username ?? "account"}: ` +
          `permissions.apps.packages=${JSON.stringify(availability.permission)}, GET /api/packages -> HTTP ${availability.packagesStatus}. ` +
          "Pass RAST_LIVE_ADMIN_USER/RAST_LIVE_ADMIN_PASSWORD to exercise this path with an administrator.",
      );
      await screenshot(target.page, "02-appcenter-unavailable").catch(() => {});
      if (adminSession) await adminSession.context.close().catch(() => {});
      return;
    }

    // Start from no app window for THIS account so the tile launch is the one
    // that creates the window.
    await required("02.reset", "no leftover app window before the tile launch", async () => {
      const windows = (await nativeWindows(target.page)).filter((window) => !window.starting);
      for (const window of windows) await closeWindow(target.page, window.id);
      await waitFor(async () => (await nativeWindows(target.page)).filter((window) => !window.starting).length === 0, {
        timeoutMs: 60_000,
        what: "leftover app windows to close",
      });
      return `closed ${windows.length} window(s)`;
    });

    const appCenterId = await required("02.appcenter.open", "App Center opens from Applications", () => openAppCenter(target.page));
    await screenshot(target.page, "02-app-center");

    const clickedAt = await required("02.tile.launch", "the Google Chrome app tile opens the app", async () => {
      const appCenter = target.page.locator(`.window[data-window-id="${appCenterId}"]`);
      const tile = appCenter.locator(".pkg-tile", { hasText: "Google Chrome" }).first();
      await tile.waitFor({ timeout: 60_000 });
      await tile.scrollIntoViewIfNeeded().catch(() => {});
      const openButton = tile.locator('button[title^="Open"], button:has-text("Open")').first();
      const clicked = Date.now();
      if (await openButton.count()) await openButton.click();
      else await tile.click();
      return clicked;
    });

    await required("02.tile.native-ready", "the App Center tile shows Google Chrome as a ready native app", async () => {
      const appCenter = target.page.locator(`.window[data-window-id="${appCenterId}"]`);
      const tile = appCenter.locator(".pkg-tile", { hasText: "Google Chrome" }).first();
      const text = (await tile.innerText()).replace(/\s+/g, " ").trim();
      if (!/native/i.test(text)) throw new Error(`the tile does not report the native backend: ${text.slice(0, 200)}`);
      return text.slice(0, 200);
    });

    await required("02.appcenter.no-viewer", "App Center does not embed an app viewer/iframe", async () => {
      const facts = await target.page.evaluate((windowId) => {
        const el = document.querySelector(`.window[data-window-id="${windowId}"]`);
        if (!el) return null;
        const body = el.querySelector(".win-body");
        return {
          iframes: body ? body.querySelectorAll("iframe").length : -1,
          nativeWindows: body ? body.querySelectorAll(".linux-native-window").length : -1,
        };
      }, appCenterId);
      if (!facts) throw new Error("App Center window disappeared");
      if (facts.iframes !== 0) throw new Error(`App Center contains ${facts.iframes} iframe(s)`);
      if (facts.nativeWindows !== 0) throw new Error("App Center contains an embedded native window");
      return "0 iframes, 0 embedded native windows";
    });

    await required("02.window.opened", "a native Chrome window appears from the App Center tile", async () => {
      const windows = await waitForReadyNativeWindows(target.page, { min: 1, timeoutMs: WINDOW_TIMEOUT_MS, what: "Chrome window from App Center" });
      return windows.map((window) => window.id).join(",");
    });
    metric("appCenterLaunchMs", Date.now() - clickedAt, "ms");

    await required("02.appcenter.close", "closing App Center keeps the Chrome window alive", async () => {
      const windows = await windowsInfo(target.page);
      const appCenter = windows.find((window) => window.id === appCenterId);
      if (!appCenter) throw new Error("App Center window not found");
      // The just-opened Chrome window covers App Center; a forced click on
      // App Center's close control would hit the covering window. Close it via
      // its own taskbar entry instead (middle click = close).
      const viaTaskbar = await closeWindowViaTaskbar(target.page, appCenter);
      if (!viaTaskbar) await closeWindow(target.page, appCenterId);
      await waitFor(async () => !(await windowsInfo(target.page)).some((window) => window.id === appCenterId), {
        timeoutMs: 30_000,
        what: "App Center to close",
      });
      const chrome = await readyNativeWindows(target.page);
      if (chrome.length !== 1) throw new Error(`expected the Chrome window to survive, found ${chrome.length} adopted window(s)`);
      const window = (await nativeWindows(target.page)).find((candidate) => candidate.id === chrome[0].id);
      if (!window?.native) throw new Error("the surviving window is not a native window");
      return `Chrome window ${chrome[0].id} survived App Center`;
    });
    await screenshot(target.page, "02-chrome-after-appcenter-closed");

    if (adminSession) {
      // Leave the administrator account tidy: the tile-launched window is not
      // needed by the following t-alice phases.
      await closeAllNativeWindows(target.page).catch(() => {});
      await adminSession.context.close().catch(() => {});
      rec.note("closed the administrator verification window");
    }
  });

  // -- phase 3: second window and window-management parity -------------------
  let secondWindow = null;
  await phase("03-multiwindow", "Ctrl+N gives a second independent window; taskbar, drag, resize, minimize, maximize, close", async () => {
    rec.note(
      "03.resize note: the first live attempt failed because `.win-resize-se` sat at ~(1354,896), under the ~40px taskbar overlay (desktop 1480x920); the product resize path is fine (dev matrix scenario d asserts RAST + remote Xpra geometry). This run repositions the window and uses the vertically centred west-edge handle (`.win-resize-w`).",
    );
    const first = (await readyNativeWindows(alice.page))[0];
    if (!first) throw new PhaseAbort("no first Chrome window");
    const widsBefore = new Set((await windowsInfo(alice.page)).flatMap((window) => window.adopted.map((node) => node.wid)));

    await required("03.ctrl-n", "Ctrl+N inside the Chrome window creates a second independent RAST window", async () => {
      let windows = [];
      for (let attempt = 0; attempt < 4; attempt += 1) {
        await clickWindowTitleBar(alice.page, (await readyNativeWindows(alice.page))[0]?.id ?? first.id);
        await alice.page.keyboard.press("Control+n");
        await alice.page.waitForTimeout(1000);
        try {
          windows = await waitForReadyNativeWindows(alice.page, { min: 2, timeoutMs: 30_000, what: "the second Chrome window" });
        } catch {
          windows = await readyNativeWindows(alice.page);
        }
        if (windows.length >= 2) break;
      }
      if (windows.length < 2) throw new Error(`expected 2 adopted windows, found ${windows.length}`);
      const widsAfter = new Set((await windowsInfo(alice.page)).flatMap((window) => window.adopted.map((node) => node.wid)));
      const newWids = [...widsAfter].filter((wid) => !widsBefore.has(wid));
      if (newWids.length < 1) throw new Error("no new remote Xpra window was adopted");
      if (windows.length !== 2) throw new Error(`expected exactly 2 adopted windows, found ${windows.length}`);
      secondWindow = windows.find((window) => window.id !== first.id) ?? windows[1];
      return `${windows.map((window) => window.id).join(",")} (new wid ${newWids.join(",")})`;
    });
    metric("adoptedWindowCount", 2);
    await screenshot(alice.page, "03-two-chrome-windows");

    await required("03.taskbar", "each window has its own taskbar entry", async () => {
      const entries = (await taskbarInfo(alice.page)).filter(
        (entry) => !entry.pinned && /chrome|new tab/i.test(entry.label || entry.title),
      );
      if (entries.length < 2) throw new Error(`expected 2 Chrome taskbar entries, found ${entries.length}`);
      return entries.map((entry) => entry.label).join(" | ");
    });

    const dragTarget = (await windowsInfo(alice.page)).find((window) => window.id === secondWindow.id);
    const beforeDrag = dragTarget ? { ...dragTarget.rect } : null;
    await step("03.drag", "a window can be dragged by its title bar", async () => {
      if (!beforeDrag) throw new Error("second window vanished");
      const box = await alice.page.locator(`.window[data-window-id="${secondWindow.id}"]`).boundingBox();
      if (!box) throw new Error("no window box");
      await alice.page.mouse.move(box.x + box.width / 2, box.y + 10);
      await alice.page.mouse.down();
      await alice.page.mouse.move(box.x + box.width / 2 - 120, box.y + 80, { steps: 12 });
      await alice.page.mouse.up();
      await alice.page.waitForTimeout(500);
      const after = (await windowsInfo(alice.page)).find((window) => window.id === secondWindow.id);
      if (!after) throw new Error("window disappeared during drag");
      const moved = Math.abs(after.rect.x - beforeDrag.x) > 30 || Math.abs(after.rect.y - beforeDrag.y) > 30;
      if (!moved) throw new Error(`window did not move (${JSON.stringify(beforeDrag)} -> ${JSON.stringify(after.rect)})`);
      return `${JSON.stringify(beforeDrag)} -> ${JSON.stringify(after.rect)}`;
    });

    await step("03.resize", "the window resizes and the remote surface follows", async () => {
      // The bottom-right handle can end up under the taskbar (or off-screen)
      // after the drag, so put the window in a known position first and use the
      // WEST edge: it is always on-screen while the left edge is positive.
      const windowLocator = () => alice.page.locator(`.window[data-window-id="${secondWindow.id}"]`);
      const titleBox = await windowLocator().boundingBox();
      if (!titleBox) throw new Error("second window has no box");
      await alice.page.mouse.move(titleBox.x + Math.min(500, titleBox.width / 2), titleBox.y + 10);
      await alice.page.mouse.down();
      await alice.page.mouse.move(200 + Math.min(500, titleBox.width / 2), 90 + 10, { steps: 12 });
      await alice.page.mouse.up();
      await alice.page.waitForTimeout(400);
      await clickWindowTitleBar(alice.page, secondWindow.id);

      const target = (await windowsInfo(alice.page)).find((window) => window.id === secondWindow.id);
      if (!target) throw new Error("second window vanished");
      const beforeCanvas = target.adopted[0]?.canvases[0] ?? null;
      const handle = windowLocator().locator(".win-resize-w");
      if (!(await handle.count())) throw new Error("the window has no west resize handle");
      const box = await handle.first().boundingBox();
      if (!box) throw new Error("resize handle has no box");
      await alice.page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
      await alice.page.mouse.down();
      // Shrink horizontally only using the WEST edge: it stays on screen
      // (left edge is positive) and its centre is vertically clear of the
      // bottom taskbar. The first live attempt used `.win-resize-se`, whose
      // centre sat under the taskbar overlay at y~896 — a script geometry
      // issue, not a product bug.
      await alice.page.mouse.move(box.x + box.width / 2 + 110, box.y + box.height / 2, { steps: 12 });
      await alice.page.mouse.up();
      const resized = await waitFor(
        async () => {
          const after = (await windowsInfo(alice.page)).find((window) => window.id === secondWindow.id);
          if (!after) return null;
          if (!(after.rect.width < target.rect.width - 40 || after.rect.height > target.rect.height + 40)) return null;
          return after;
        },
        { timeoutMs: 15_000, intervalMs: 250, what: "the window to resize" },
      );
      // The remote Xpra surface must follow the RAST resize (the shim sends the
      // new content size; the remote window then repaints at the new size).
      const remote = await waitFor(
        async () => {
          const after = (await windowsInfo(alice.page)).find((window) => window.id === secondWindow.id);
          const canvas = after?.adopted[0]?.canvases[0];
          if (!canvas || canvas.width <= 0) return null;
          if (!beforeCanvas) return canvas;
          if (canvas.width === beforeCanvas.width && canvas.height === beforeCanvas.height) return null;
          return canvas;
        },
        { timeoutMs: 15_000, intervalMs: 300, what: "the adopted canvas to follow the resize" },
      );
      rec.note(
        `resize: ${JSON.stringify(target.rect)} -> ${JSON.stringify(resized.rect)}; ` +
          `remote canvas ${beforeCanvas ? `${beforeCanvas.width}x${beforeCanvas.height}` : "?"} -> ${remote.width}x${remote.height}`,
      );
      return `${target.rect.width}x${target.rect.height} -> ${resized.rect.width}x${resized.rect.height} (remote ${remote.width}x${remote.height})`;
    });

    await step("03.minimize-restore", "the window minimizes and restores", async () => {
      const window = alice.page.locator(`.window[data-window-id="${secondWindow.id}"]`);
      await window.locator(".win-control-minimize").click();
      await alice.page.waitForTimeout(500);
      const minimized = (await windowsInfo(alice.page)).find((item) => item.id === secondWindow.id);
      if (!minimized?.minimized) throw new Error("the window is not minimized");
      const entry = alice.page.locator(".taskbar-app.is-minimized").first();
      if (!(await entry.count())) throw new Error("no minimized taskbar entry to restore from");
      await entry.click();
      await alice.page.waitForTimeout(500);
      const restored = (await windowsInfo(alice.page)).find((item) => item.id === secondWindow.id);
      if (!restored || restored.minimized) throw new Error("the window did not restore");
      return "minimized and restored through the taskbar";
    });

    await step("03.maximize", "the window maximizes to the desktop area", async () => {
      const window = alice.page.locator(`.window[data-window-id="${secondWindow.id}"]`);
      await window.locator(".win-control-maximize").click();
      await alice.page.waitForTimeout(700);
      const maximized = (await windowsInfo(alice.page)).find((item) => item.id === secondWindow.id);
      if (!maximized?.maximized) throw new Error("the window did not maximize");
      if (maximized.rect.width < ViewportWidth() * 0.9) {
        throw new Error(`maximized window is only ${maximized.rect.width}px wide`);
      }
      return `${maximized.rect.width}x${maximized.rect.height}`;
    });

    await step("03.close-one", "closing one window leaves the other one running", async () => {
      const remaining = await readyNativeWindows(alice.page);
      const keep = remaining.find((window) => window.id !== secondWindow.id) ?? remaining[0];
      if (!keep) throw new Error("no window to keep");
      const other = remaining.find((window) => window.id !== keep.id);
      if (!other) throw new Error("no window to close");
      await closeWindow(alice.page, other.id);
      await waitFor(async () => (await windowsInfo(alice.page)).every((window) => window.id !== other.id), {
        timeoutMs: 30_000,
        what: "the closed window to disappear",
      });
      const left = await readyNativeWindows(alice.page);
      if (left.length !== 1) throw new Error(`expected 1 surviving window, found ${left.length}`);
      if (!left[0].adopted.some((node) => node.canvases.some((canvas) => canvas.visible))) {
        throw new Error("the surviving window lost its adopted surface");
      }
      return `closed ${other.id}; ${left[0].id} still adopted`;
    });
    await screenshot(alice.page, "03-after-closing-one-window");
  });

  // -- phase 4: refresh, re-adoption and host evidence -----------------------
  await phase("04-refresh", "Page reload re-adopts the same windows without duplicating processes", async () => {
    const userId = alice.me.id ?? alice.me.user?.id;
    const before = await required("04.before.host", "host state is captured before the reload", () =>
      hostSnapshot({ userId, label: "before-refresh" }),
    );
    const beforeWindows = await required("04.before.windows", "the adopted window state is captured before the reload", async () => {
      const windows = await readyNativeWindows(alice.page);
      if (windows.length < 1) throw new Error("no adopted window before the reload");
      return {
        count: windows.length,
        ids: windows.map((window) => window.id),
        wids: windows.flatMap((window) => window.adopted.map((node) => node.wid)),
      };
    });

    const reloadStarted = Date.now();
    await required("04.reload", "the desktop page reloads", async () => {
      await alice.page.reload({ waitUntil: "domcontentloaded" });
      await alice.page.waitForSelector(".taskbar", { timeout: 90_000 });
      await alice.page
        .waitForFunction(() => !document.querySelector(".boot-screen, .loading-screen, .splash"), undefined, { timeout: 30_000 })
        .catch(() => {});
      return "reloaded";
    });
    await required("04.readopted", "the same windows are re-adopted without duplication", async () => {
      const windows = await waitForReadyNativeWindows(alice.page, {
        min: beforeWindows.count,
        max: beforeWindows.count,
        timeoutMs: 180_000,
        what: `${beforeWindows.count} re-adopted window(s)`,
      });
      const wids = windows.flatMap((window) => window.adopted.map((node) => node.wid)).sort();
      const expected = [...beforeWindows.wids].sort();
      if (JSON.stringify(wids) !== JSON.stringify(expected)) {
        throw new Error(`window set changed across reload: before ${expected.join(",")}, after ${wids.join(",")}`);
      }
      metric("refreshReAdoptMs", Date.now() - reloadStarted, "ms");
      return `${windows.length} window(s), same remote window ids`;
    });
    await required("04.after.host", "the same Linux session/processes still run after the reload", async () => {
      const after = await hostSnapshot({ userId, label: "after-refresh" });
      if (after.session.active !== "active") throw new Error(`session unit is ${after.session.active}`);
      if (before.session.mainPid && after.session.mainPid !== before.session.mainPid) {
        throw new Error(`session MainPID changed (${before.session.mainPid} -> ${after.session.mainPid}); the session was restarted`);
      }
      const beforeUnits = before.appUnits.map((unit) => unit.unit).sort();
      const afterUnits = after.appUnits.map((unit) => unit.unit).sort();
      if (JSON.stringify(beforeUnits) !== JSON.stringify(afterUnits)) {
        throw new Error(`app units changed across reload: before ${beforeUnits.join(",")}, after ${afterUnits.join(",")}`);
      }
      if (after.chromeProcesses < Math.max(1, before.chromeProcesses - 2)) {
        throw new Error(`Chrome process count dropped (${before.chromeProcesses} -> ${after.chromeProcesses}); the app may have restarted`);
      }
      metric("sessionMemoryBytesAfterRefresh", after.session.memoryBytes, "bytes");
      metric("processCountAfterRefresh", after.chromeProcesses);
      return `unit ${after.sessionUnit} active, MainPID ${after.session.mainPid}, ${after.chromeProcesses} chrome processes`;
    });
    await screenshot(alice.page, "04-after-refresh");
  });

  // -- phase 5: cross-user isolation ----------------------------------------
  await phase("05-isolation", "Another signed-in user (and anonymous) cannot open t-alice's session URL", async () => {
    const target = sessionUrl
      ? new URL(sessionUrl, `${args.url}/`).href
      : (await required("05.session-url", "t-alice's session viewer URL is available", async () => {
          const state = await apiGet(alice.page, "/api/linux-apps/session");
          const url = state.body?.session?.url;
          if (!url) throw new Error("no session url in /api/linux-apps/session");
          return new URL(url, `${args.url}/`).href;
        }));

    await required("05.bob.403", "t-bob gets 403 when requesting t-alice's session URL", async () => {
      const bobPage = bob.page;
      const response = await bobPage.goto(target, { waitUntil: "domcontentloaded", timeout: 60_000 }).catch((error) => ({ status: () => 0, _error: error }));
      const status = typeof response.status === "function" ? response.status() : 0;
      if (![401, 403].includes(status)) throw new Error(`expected 403 (or 401) for t-bob, got HTTP ${status}`);
      await screenshot(bobPage, "05-bob-forbidden");
      return `HTTP ${status}`;
    });

    await required("05.anonymous.401", "an anonymous browser gets 401 when requesting the session URL", async () => {
      const anonymous = await newContext(browser);
      try {
        const response = await anonymous.page
          .goto(target, { waitUntil: "domcontentloaded", timeout: 60_000 })
          .catch((error) => ({ status: () => 0, _error: error }));
        const status = typeof response.status === "function" ? response.status() : 0;
        if (status !== 401) throw new Error(`expected 401 for an anonymous request, got HTTP ${status}`);
        await screenshot(anonymous.page, "05-anonymous-unauthorized");
        return "HTTP 401";
      } finally {
        await anonymous.context.close();
      }
    });

    await required("05.bob.session-own", "t-bob's own session API never returns t-alice's session", async () => {
      const state = await apiGet(bob.page, "/api/linux-apps/session");
      if (state.status === 200) {
        const url = state.body?.session?.url;
        if (url && sessionUrl && url === sessionUrl) throw new Error("t-bob's session API returned t-alice's session");
      }
      return `HTTP ${state.status}${state.status === 200 ? " (own session only)" : ""}`;
    });
  });

  }

  // -- phase 6: metrics and report -----------------------------------------
  await phase("06-metrics", "Collect session metrics and write the JSON/markdown report", async () => {
    const userId = alice.me.id ?? alice.me.user?.id;
    const snapshot = await hostSnapshot({ userId, label: "final" });
    const windows = await readyNativeWindows(alice.page);
    metric("adoptedWindowCountFinal", windows.length);
    metric("sessionMemoryBytesFinal", snapshot.session.memoryBytes, "bytes");
    metric("sessionTasksFinal", snapshot.session.tasks);
    metric("processCountFinal", snapshot.chromeProcesses);
    metric("appUnitCountFinal", snapshot.appUnits.length);
    const appUnitMemory = snapshot.appUnits.reduce((sum, unit) => sum + unit.memoryBytes, 0);
    metric("appUnitsMemoryBytes", appUnitMemory, "bytes");
    rec.note(
      `final: ${windows.length} adopted window(s), session unit ${snapshot.sessionUnit} ${snapshot.session.active}/${snapshot.session.sub}, ` +
        `memory ${(snapshot.session.memoryBytes / 1024 / 1024).toFixed(1)} MiB, ${snapshot.chromeProcesses} Chrome processes`,
    );

  });

  // Cleanup: leave the test account tidy (the session idles out afterwards).
  if (!args.keepWindows && !args.preflightOnly && alice) {
    await closeAllNativeWindows(alice.page).catch(() => {});
    rec.note("closed the verification windows on the t-alice desktop");
  }
  hostAfter = await captureHostAfter().catch((error) => {
    rec.note(`could not capture the final host state: ${error.message}`);
    return null;
  });
  if (hostAfter) {
    rec.note(
      `host left running: ${hostAfter.activeNativeUnits.length} active native unit(s) ` +
        `[${hostAfter.activeNativeUnits
          .map((unit) => `${unit.unit} pid=${unit.mainPid} ${(unit.memoryBytes / 1024 / 1024).toFixed(0)}MiB`)
          .join(", ")}]; ` +
        `${hostAfter.rastProcesses.length} rast process(es); ${hostAfter.chromeProcessesHostWide} chrome process(es) host-wide`,
    );
  }

  const report = {
    kind: "rast-live-native-production-verification",
    url: args.url,
    ranAt: new Date().toISOString(),
    durationMs: Date.now() - overallStarted,
    bootstrap: null,
    users: {
      alice: { username: "t-alice", id: alice?.me?.id ?? alice?.me?.user?.id ?? null },
      bob: { username: "t-bob", id: bob?.me?.id ?? bob?.me?.user?.id ?? null },
    },
    checks: redactDeep(rec.checks),
    phases: redactDeep(rec.phases),
    metrics: rec.metrics,
    details: redactDeep(rec.details),
    screenshots: rec.screenshots,
    failures: redactDeep(rec.failures),
    hostAfter: redactDeep(hostAfter),
  };
  const passed = rec.checks.filter((check) => check.status === "pass").length;
  const failed = rec.checks.filter((check) => check.status === "fail").length;
  const skipped = rec.checks.filter((check) => check.status === "skip").length;
  writeFileSync(join(RUN_DIR, "report.json"), JSON.stringify(report, null, 2));

  const lines = [
    "# RAST live production verification — native Linux apps",
    "",
    `- Target: ${args.url}`,
    `- Ran at: ${report.ranAt}`,
    `- Duration: ${(report.durationMs / 1000).toFixed(1)} s`,
    `- Checks: **${passed} passed, ${failed} failed, ${skipped} skipped**`,
    "",
    "## Checks",
    "",
    "| Check | Result | Detail |",
    "|---|---|---|",
    ...rec.checks.map((check) => {
      const detail = check.detail !== null && check.detail !== undefined ? String(typeof check.detail === "string" ? check.detail : JSON.stringify(check.detail)) : check.error || "";
      return `| ${check.id} — ${check.what.replace(/\|/g, "\\|")} | **${check.status.toUpperCase()}** | ${redactSessionUrl(detail).replace(/\|/g, "\\|").slice(0, 400)} |`;
    }),
    "",
    "## Metrics",
    "",
    "| Metric | Value | Unit |",
    "|---|---|---|",
    ...Object.entries(rec.metrics).map(([name, item]) => `| ${name} | ${item.value} | ${item.unit || ""} |`),
    "",
    "## Phases",
    "",
    "| Phase | Status | Duration |",
    "|---|---|---|",
    ...rec.phases.map((phaseRecord) => `| ${phaseRecord.id} — ${phaseRecord.title.replace(/\|/g, "\\|")} | **${phaseRecord.status.toUpperCase()}** | ${(phaseRecord.durationMs / 1000).toFixed(1)} s |`),
    "",
    "## Still running on the host when the run finished (left for teardown)",
    "",
    ...(hostAfter
      ? [
          "| Unit | State | PID | Memory | Tasks |",
          "|---|---|---|---|---|",
          ...hostAfter.activeNativeUnits.map(
            (unit) =>
              `| ${unit.unit} | ${unit.active}/${unit.sub} | ${unit.mainPid} | ${(unit.memoryBytes / 1024 / 1024).toFixed(1)} MiB | ${unit.tasks} |`,
          ),
          "",
          `- rast processes: ${hostAfter.rastProcesses.map((proc) => `pid ${proc.pid} \`${proc.command}\``).join(", ") || "none"}`,
          `- Chrome processes host-wide: ${hostAfter.chromeProcessesHostWide}`,
        ]
      : ["- (not captured)"]),
    "",
    "## Screenshots",
    "",
    ...rec.screenshots.map((path) => `- \`${path}\``),
    "",
    "## Notes",
    "",
    ...rec.details.map((line) => `- ${redactSessionUrl(line)}`),
    "",
    failed > 0 ? "## Failures\n\n" + rec.failures.map((failure) => `- **${failure.id}** ${failure.what}: ${redactSessionUrl(failure.error ?? "?").slice(0, 800)}`).join("\n") : "No failures.",
    "",
  ];
  writeFileSync(join(RUN_DIR, "report.md"), lines.join("\n"));

  console.log(`\n${lines.slice(0, 12).join("\n")}`);
  console.log(`\nreport: ${join(RUN_DIR, "report.json")}`);
  console.log(`screenshots: ${SHOTS_DIR}`);
  return failed === 0 ? 0 : 1;
}

// Small helpers used above but defined here to keep the phase bodies readable.
function ViewportWidth() {
  return VIEWPORT.width;
}

main()
  .then(async (code) => {
    await browser?.close().catch(() => {});
    process.exit(code);
  })
  .catch(async (error) => {
    console.error("live verification crashed:", error);
    try {
      writeFileSync(
        join(RUN_DIR, "crash.txt"),
        `${new Date().toISOString()}\n${error?.stack ?? error}\n\nchecks so far:\n${JSON.stringify(redactDeep(rec.checks), null, 2)}\n`,
      );
    } catch {
      /* ignore */
    }
    await browser?.close().catch(() => {});
    process.exit(2);
  });
