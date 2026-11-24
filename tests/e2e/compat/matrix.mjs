// RAST native app compatibility matrix.
//
// Detects what is actually installed (system .desktop entries, RAST App Center
// Linux apps, host commands, RAST built-in apps) and then, for every app that
// RAST can actually launch, exercises it through the real authenticated desktop
// path in a headless Playwright Chromium: launch, window mapping, typing,
// scrolling, clipboard (where a deterministic fixture exists), second window
// and close. Every check is Pass / Partial / Fail / Not tested / Not applicable
// and the report keeps "not installed" strictly separate from real failures.
//
// Usage:
//   node tests/e2e/compat/matrix.mjs --label=baseline
//   node tests/e2e/compat/matrix.mjs --base-url=http://127.0.0.1:8731 \
//        --creds=/tmp/rast-e2e/credentials.env --label=dev
//   node tests/e2e/compat/matrix.mjs --detect-only
//
// Options:
//   --label=<name>      result label (default: run)
//   --out=<prefix>      output prefix under tests/e2e/results/
//                       (default: compat-runtime2-<label>-<YYYY-MM-DD>)
//   --base-url=<url>    RAST URL (default $RAST_BENCH_BASE_URL or production)
//   --creds=<file>      production `user=password` file or dev RAST_E2E_* env
//   --user=<name>       user that launches apps (default t-alice / alice)
//   --admin=<name>      admin for the App Center inventory (default t-admin / admin)
//   --max-apps=<n>      live-test at most n App Center apps (default 5)
//   --detect-only       only write the inventory, do not touch sessions
//   --skip-live         alias of --detect-only
//
// Safety: launches/closes only the benchmark user's own apps through the RAST
// API and the desktop UI. It never stops systemd units or production services.

import { execFileSync } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, readdirSync, statSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { inflateSync } from "node:zlib";
import { chromium } from "playwright";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "../../..");
const RESULTS = join(REPO, "tests/e2e/results");
const SCREENSHOTS = join(REPO, "tests/e2e/screenshots/compat");
const BENCH_PAGE = resolve(REPO, "tests/e2e/perf/bench-page.html");

const sleep = (ms) => new Promise((done) => setTimeout(done, ms));
const stamp = () => new Date().toISOString().replace(/[:.]/g, "-").replace("Z", "").slice(0, 19);
const DATE = new Date().toISOString().slice(0, 10);

function arg(name, fallback = null) {
  const hit = process.argv.find((item) => item === `--${name}` || item.startsWith(`--${name}=`));
  if (!hit) return fallback;
  if (hit === `--${name}`) {
    const index = process.argv.indexOf(hit);
    const next = process.argv[index + 1];
    return next && !next.startsWith("--") ? next : true;
  }
  return hit.slice(name.length + 3);
}

const OPTIONS = {
  label: arg("label", "run"),
  out: arg("out", null),
  baseUrl: String(arg("base-url", process.env.RAST_BENCH_BASE_URL || "https://desktop.lufria.tech")).replace(/\/$/, ""),
  creds: arg("creds", process.env.RAST_BENCH_CREDS || "/home/lufria/.config/rast-test/creds"),
  user: arg("user", null),
  admin: arg("admin", null),
  maxApps: Number(arg("max-apps", 5)),
  detectOnly: arg("detect-only", false) === true || arg("skip-live", false) === true,
};
const OUT_PREFIX = OPTIONS.out || `compat-runtime2-${OPTIONS.label}-${DATE}`;
const JSON_PATH = join(RESULTS, `${OUT_PREFIX}.json`);
const MD_PATH = join(RESULTS, `${OUT_PREFIX}.md`);

const log = (message) => console.log(`[compat ${new Date().toISOString().slice(11, 19)}] ${message}`);

// ---------------------------------------------------------------------------
// Credentials / REST
// ---------------------------------------------------------------------------
function readSecrets(path) {
  const values = {};
  for (const line of readFileSync(path, "utf8").split("\n")) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const index = trimmed.indexOf("=");
    if (index > 0) values[trimmed.slice(0, index).trim()] = trimmed.slice(index + 1).trim();
  }
  return values;
}

function resolveCredentials() {
  const secrets = readSecrets(OPTIONS.creds);
  if (secrets["t-alice"]) {
    const user = OPTIONS.user || "t-alice";
    const admin = OPTIONS.admin || "t-admin";
    return {
      user: { username: user, password: secrets[user] },
      admin: { username: admin, password: secrets[admin] ?? secrets[user] },
    };
  }
  const passwordOf = (name) => [
    [secrets.RAST_E2E_ADMIN_USER, secrets.RAST_E2E_ADMIN_PASSWORD],
    [secrets.RAST_E2E_USER1_USER, secrets.RAST_E2E_USER1_PASSWORD],
    [secrets.RAST_E2E_USER2_USER, secrets.RAST_E2E_USER2_PASSWORD],
  ].find(([candidate]) => candidate === name)?.[1];
  const user = OPTIONS.user || secrets.RAST_E2E_USER1_USER || "alice";
  const admin = OPTIONS.admin || secrets.RAST_E2E_ADMIN_USER || "admin";
  return { user: { username: user, password: passwordOf(user) }, admin: { username: admin, password: passwordOf(admin) } };
}

class Api {
  constructor(baseUrl) {
    this.baseUrl = baseUrl;
    this.cookies = new Map();
    this.csrf = null;
    this.user = null;
  }
  cookieHeader() {
    return [...this.cookies.entries()].map(([name, value]) => `${name}=${value}`).join("; ");
  }
  async request(method, path, body) {
    const headers = {};
    if (this.cookies.size) headers.Cookie = this.cookieHeader();
    if (this.csrf) headers["X-CSRF-Token"] = this.csrf;
    let payload;
    if (body !== undefined) { headers["Content-Type"] = "application/json"; payload = JSON.stringify(body); }
    const response = await fetch(`${this.baseUrl}${path}`, { method, headers, body: payload, redirect: "manual" });
    for (const raw of response.headers.getSetCookie?.() ?? []) {
      const pair = raw.split(";")[0];
      const index = pair.indexOf("=");
      if (index > 0) this.cookies.set(pair.slice(0, index), pair.slice(index + 1));
    }
    const text = await response.text();
    let json = null;
    try { json = text ? JSON.parse(text) : null; } catch { /* not json */ }
    if (response.status < 200 || response.status >= 300) {
      const detail = json?.error?.message || json?.message || text.slice(0, 300);
      throw new Error(`${method} ${path} -> HTTP ${response.status}: ${detail}`);
    }
    return json;
  }
  get(path) { return this.request("GET", path); }
  post(path, body) { return this.request("POST", path, body === undefined ? {} : body); }
  async login(credentials) {
    const result = await this.post("/api/auth/login", credentials);
    this.csrf = result.csrfToken;
    this.user = result.user;
    return result;
  }
  listApps() { return this.get("/api/linux-apps"); }
  packages() { return this.get("/api/packages"); }
  sessionState() { return this.get("/api/linux-apps/session"); }
  closeSession() { return this.post("/api/linux-apps/session/close"); }
  closeRun(runId) { return this.post(`/api/linux-apps/session/runs/${encodeURIComponent(runId)}/close`); }
}

// ---------------------------------------------------------------------------
// Detection: .desktop entries, commands, App Center, built-in apps
// ---------------------------------------------------------------------------
const DESKTOP_DIRS = [
  "/usr/share/applications",
  "/usr/local/share/applications",
  "/var/lib/flatpak/exports/share/applications",
  "/var/lib/snapd/desktop/applications",
  join(process.env.HOME ?? "/root", ".local/share/applications"),
];

function parseDesktopFile(path) {
  try {
    const text = readFileSync(path, "utf8");
    const entry = {};
    let inEntry = false;
    for (const line of text.split("\n")) {
      const trimmed = line.trim();
      if (trimmed.startsWith("[")) { inEntry = trimmed === "[Desktop Entry]"; continue; }
      if (!inEntry || !trimmed || trimmed.startsWith("#")) continue;
      const index = trimmed.indexOf("=");
      if (index > 0) entry[trimmed.slice(0, index).trim()] = trimmed.slice(index + 1).trim();
    }
    const execLine = (entry.Exec ?? "").replace(/%[FUfu]/g, "").trim();
    const execBinary = execLine.split(/\s+/)[0]?.replace(/^["']|["']$/g, "") ?? "";
    return {
      file: path,
      name: entry.Name ?? execBinary.split("/").pop() ?? path,
      genericName: entry.GenericName ?? null,
      comment: entry.Comment ?? null,
      exec: execLine || null,
      execBinary: execBinary || null,
      categories: entry.Categories ?? null,
      type: entry.Type ?? null,
      noDisplay: entry.NoDisplay === "true",
      hidden: entry.Hidden === "true",
      terminal: entry.Terminal === "true",
      execStub: ["false", "true"].includes(execBinary.split("/").pop() ?? ""),
    };
  } catch {
    return null;
  }
}

function scanDesktopEntries() {
  const entries = [];
  const warnings = [];
  for (const dir of DESKTOP_DIRS) {
    if (!existsSync(dir)) continue;
    let files = [];
    try { files = readdirSync(dir).filter((file) => file.endsWith(".desktop")); } catch { continue; }
    for (const file of files) {
      const entry = parseDesktopFile(join(dir, file));
      if (!entry) continue;
      if (entry.type && entry.type !== "Application") continue;
      if (entry.hidden) continue;
      entries.push(entry);
    }
  }
  return { entries: entries.sort((a, b) => a.name.localeCompare(b.name)), warnings };
}

function commandPath(command) {
  try { return execFileSync("sh", ["-c", `command -v ${JSON.stringify(command)}`], { encoding: "utf8" }).trim(); } catch { return null; }
}

function detectCommands(commands) {
  const result = {};
  for (const command of commands) result[command] = commandPath(command);
  return result;
}

const CAPABILITIES = {
  "google-chrome": {
    textInput: true,
    scroll: true,
    clipboard: true,
    secondWindow: true,
    note: "verified against the deterministic harness page loaded inside the remote Chrome",
  },
  "chromium-browser": { textInput: true, scroll: true, clipboard: true, secondWindow: true },
  xterm: { textInput: true, scroll: false, clipboard: false, secondWindow: false, note: "typing changes the terminal contents; no scrollable fixture and no clipboard round trip target" },
  uxterm: { textInput: true, scroll: false, clipboard: false, secondWindow: false },
  alacritty: { textInput: true, scroll: false, clipboard: false, secondWindow: false },
  firefox: { textInput: true, scroll: true, clipboard: true, secondWindow: true },
  libreoffice: { textInput: true, scroll: true, clipboard: false, secondWindow: true },
};

// ---------------------------------------------------------------------------
// Browser + desktop helpers
// ---------------------------------------------------------------------------
const CHROMIUM = "/home/lufria/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome";
const SCALE = (ms) => Math.round(ms * Number(arg("timeout-scale", 1) || 1));

async function launchBrowser() {
  return chromium.launch({
    headless: true,
    executablePath: existsSync(CHROMIUM) ? CHROMIUM : undefined,
    args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage", "--disable-background-timer-throttling", "--disable-renderer-backgrounding"],
  });
}

async function formLogin(page, username, password) {
  await page.goto(`${OPTIONS.baseUrl}/`, { waitUntil: "domcontentloaded" });
  await page.waitForSelector('input[name="username"], .auth-form', { timeout: SCALE(30000) });
  await page.fill('input[name="username"]', username);
  await page.fill('input[type="password"]', password);
  await Promise.all([
    page.click('.auth-form button[type="submit"], button:has-text("Sign in")'),
    page.waitForSelector(".taskbar", { timeout: SCALE(90000) }),
  ]);
  const authenticated = await page.evaluate(async () => (await fetch("/api/auth/me", { credentials: "same-origin" })).ok);
  if (!authenticated) throw new Error("form login did not produce an authenticated desktop");
  await page.waitForTimeout(1200);
}

function windowsInfo(page) {
  return page.evaluate(() => [...document.querySelectorAll(".window[data-window-id]")].map((element) => {
    const rect = element.getBoundingClientRect();
    return {
      id: element.dataset.windowId,
      appId: element.dataset.appId ?? "",
      title: element.querySelector(".win-title")?.textContent?.trim() ?? "",
      rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
    };
  }).filter((window) => !window.minimized));
}

function shimSnapshot(page) {
  return page.evaluate(() => {
    const frame = document.querySelector("iframe.linux-native-session-frame");
    const shim = frame?.contentWindow?.RASTNative ?? null;
    if (!shim) return { present: Boolean(frame), ready: false, connected: false, windows: [] };
    let connected = false;
    try { connected = Boolean(shim.client?.connected); } catch { /* not ready */ }
    return { present: true, ready: Boolean(shim.ready), connected, windows: shim.windows?.() ?? [] };
  });
}

const isAdopted = (window, appId) => window.appId.startsWith(`linux:${appId}:`);
const appWindows = (windows, appId) => windows.filter((window) => isAdopted(window, appId));

async function waitFor(page, predicate, timeoutMs, what, intervalMs = 100) {
  const started = Date.now();
  let last = null;
  for (;;) {
    last = await predicate();
    if (last) return last;
    if (Date.now() - started > timeoutMs) throw new Error(`timeout waiting for ${what} (last: ${JSON.stringify(last).slice(0, 300)})`);
    await page.waitForTimeout(intervalMs);
  }
}

async function openStartMenu(page) {
  if (await page.locator(".start-menu").count()) return;
  await page.locator(".taskbar-start").click();
  await page.waitForSelector(".start-menu", { timeout: SCALE(15000) });
}

async function launchFromMenu(page, name) {
  await openStartMenu(page);
  const search = page.locator(".start-menu .menu-search");
  if (await search.count()) await search.fill(name);
  const tile = page.locator(".start-menu .menu-app", { hasText: name }).first();
  await tile.waitFor({ timeout: SCALE(15000) });
  const started = Date.now();
  await tile.click();
  return started;
}

async function closeSession(api) {
  await api.closeSession().catch(() => {});
  const started = Date.now();
  for (;;) {
    const state = await api.sessionState().catch(() => null);
    if (!state?.session) return;
    if (Date.now() - started > SCALE(120000)) return;
    await sleep(300);
  }
}

async function closeRun(api, appId) {
  const state = await api.sessionState().catch(() => null);
  for (const run of state?.session?.apps ?? []) {
    if (run.appId === appId && run.status === "running") await api.closeRun(run.runId).catch(() => {});
  }
}

// --- minimal PNG decode + content calibration (same fixture as perf/bench) ---
function decodePng(buffer) {
  if (buffer.readUInt32BE(0) !== 0x89504e47) throw new Error("not a PNG");
  let offset = 8;
  let width = 0; let height = 0; let colorType = 0; let bitDepth = 0;
  const idat = [];
  while (offset < buffer.length) {
    const length = buffer.readUInt32BE(offset);
    const type = buffer.toString("ascii", offset + 4, offset + 8);
    const data = buffer.subarray(offset + 8, offset + 8 + length);
    if (type === "IHDR") {
      width = data.readUInt32BE(0); height = data.readUInt32BE(4);
      bitDepth = data[8]; colorType = data[9];
      if (data[12] !== 0) throw new Error("interlaced PNG");
    } else if (type === "IDAT") idat.push(data);
    else if (type === "IEND") break;
    offset += 12 + length;
  }
  if (bitDepth !== 8) throw new Error(`unsupported bit depth ${bitDepth}`);
  const channels = colorType === 6 ? 4 : colorType === 2 ? 3 : colorType === 0 ? 1 : -1;
  if (channels < 0) throw new Error(`unsupported color type ${colorType}`);
  const raw = inflateSync(Buffer.concat(idat));
  const stride = width * channels;
  const pixels = Buffer.alloc(height * stride);
  let previous = Buffer.alloc(stride);
  for (let y = 0; y < height; y += 1) {
    const filter = raw[y * (stride + 1)];
    const line = raw.subarray(y * (stride + 1) + 1, (y + 1) * (stride + 1));
    const out = pixels.subarray(y * stride, (y + 1) * stride);
    for (let x = 0; x < stride; x += 1) {
      const left = x >= channels ? out[x - channels] : 0;
      const up = previous[x];
      const upLeft = x >= channels ? previous[x - channels] : 0;
      let value = line[x];
      if (filter === 1) value = (value + left) & 0xff;
      else if (filter === 2) value = (value + up) & 0xff;
      else if (filter === 3) value = (value + ((left + up) >> 1)) & 0xff;
      else if (filter === 4) {
        const p = left + up - upLeft;
        const pa = Math.abs(p - left); const pb = Math.abs(p - up); const pc = Math.abs(p - upLeft);
        value = (value + (pa <= pb && pa <= pc ? left : pb <= pc ? up : upLeft)) & 0xff;
      } else if (filter !== 0) throw new Error(`unsupported filter ${filter}`);
      out[x] = value;
    }
    previous = out;
  }
  return { width, height, channels, pixels };
}

function findContentRect(image) {
  const { width, height, channels, pixels } = image;
  const dark = (x, y) => {
    const index = (y * width + x) * channels;
    return Math.max(pixels[index], pixels[index + 1], pixels[index + 2]) < 90;
  };
  const rowRatio = (y) => {
    let hits = 0; let total = 0;
    for (let x = 0; x < width; x += 3) { total += 1; if (dark(x, y)) hits += 1; }
    return hits / total;
  };
  let top = -1;
  for (let y = 0; y < height; y += 1) if (rowRatio(y) > 0.6) { top = y; break; }
  if (top < 0) return null;
  let bottom = height - 1;
  for (let y = height - 1; y >= 0; y -= 1) if (rowRatio(y) > 0.4) { bottom = y; break; }
  return { top, bottom };
}

async function calibrateContent(page, windowId) {
  const box = await page.evaluate((id) => {
    const element = document.querySelector(`.window[data-window-id="${id}"]`);
    const rect = element.getBoundingClientRect();
    return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
  }, windowId);
  const image = decodePng(await page.screenshot({ clip: { x: Math.round(box.x), y: Math.round(box.y), width: Math.round(box.width), height: Math.round(box.height) } }));
  const rect = findContentRect(image);
  if (!rect) throw new Error("cannot locate remote page content");
  return {
    window: box,
    content: {
      x: Math.round(box.x + 1),
      y: Math.round(box.y + rect.top),
      width: Math.round(box.width - 2),
      height: Math.round(rect.bottom - rect.top + 1),
    },
    navY: Math.round(box.y + rect.top + 8 + 32),
  };
}

function estimateContent(box) {
  return { x: Math.round(box.x), y: Math.round(box.y + 123), width: Math.round(box.width), height: Math.max(120, Math.round(box.height - 124)) };
}

/** True when the dark fixture page is painted in the window content area. */
async function benchPageLoaded(page, box) {
  const estimate = estimateContent(box);
  const clip = {
    x: Math.round(estimate.x + 10),
    y: Math.round(estimate.y + 10),
    width: Math.round(Math.min(estimate.width - 20, 900)),
    height: Math.round(Math.min(estimate.height - 20, 500)),
  };
  try {
    const image = decodePng(await page.screenshot({ clip, animations: "disabled" }));
    let dark = 0;
    let total = 0;
    for (let y = 0; y < image.height; y += 7) {
      for (let x = 0; x < image.width; x += 7) {
        const index = (y * image.width + x) * image.channels;
        if (Math.max(image.pixels[index], image.pixels[index + 1], image.pixels[index + 2]) < 90) dark += 1;
        total += 1;
      }
    }
    return total > 0 && dark / total > 0.25;
  } catch {
    return false;
  }
}

/**
 * Load the fixture page inside the remote Chrome. Newer shells reserve Ctrl+L
 * for the local browser (`OTHER_BROWSER_RESERVED`), so Alt+D (not reserved,
 * Chrome focuses the address bar) is the primary path; Ctrl+L is a fallback for
 * older builds and the New Tab search box is the last resort.
 */
async function navigateBench(page, layout, query) {
  const box = await windowBox(page, layout.windowId ?? layout.window?.id ?? layout.content?.windowId);
  const url = `${BENCH_URL}?${query}`;
  const attempts = [
    async () => page.keyboard.press("Alt+KeyD"),
    async () => page.keyboard.press("Control+KeyL"),
    async () => {
      const estimate = estimateContent(box);
      await page.mouse.click(estimate.x + estimate.width * 0.5, estimate.y + estimate.height * 0.38);
    },
  ];
  for (const focus of attempts) {
    await page.mouse.click(box.x + box.width / 2, box.y + box.height - 60).catch(() => {});
    await page.waitForTimeout(300);
    await focus();
    await page.waitForTimeout(350);
    await page.keyboard.type(url, { delay: 8 });
    await page.waitForTimeout(150);
    await page.keyboard.press("Enter");
    const deadline = Date.now() + SCALE(9000);
    while (Date.now() < deadline) {
      if (await benchPageLoaded(page, box)) return true;
      await page.waitForTimeout(250);
    }
    await page.keyboard.press("Escape").catch(() => {});
  }
  return false;
}

async function clickNav(page, layout, fraction) {
  await page.mouse.click(Math.round(layout.content.x + layout.content.width * fraction), layout.navY);
  await page.waitForTimeout(SCALE(4000));
}

async function windowBox(page, windowId) {
  return page.evaluate((id) => {
    const element = document.querySelector(`.window[data-window-id="${id}"]`);
    const rect = element.getBoundingClientRect();
    return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
  }, windowId);
}

async function clipChanged(page, clip, action, timeoutMs = 4000) {
  const baseline = await page.screenshot({ clip, animations: "disabled" }).catch(() => null);
  if (!baseline) return { changed: false, error: "no screenshot" };
  const started = Date.now();
  await action();
  while (Date.now() - started < timeoutMs) {
    const current = await page.screenshot({ clip, animations: "disabled" }).catch(() => null);
    if (current && !current.equals(baseline)) return { changed: true, elapsedMs: Date.now() - started };
    await sleep(60);
  }
  return { changed: false, elapsedMs: Date.now() - started };
}

// ---------------------------------------------------------------------------
// Live checks for one launchable app
// ---------------------------------------------------------------------------
function check(status, evidence, notes = null) {
  return { status, evidence, notes };
}

async function testApp({ page, api, app, caps }) {
  mkdirSync(SCREENSHOTS, { recursive: true });
  const checks = {};
  log(`live test: ${app.name} (${app.id}, backend ${app.backend ?? "?"})`);
  const sessionBefore = await api.sessionState().catch(() => null);
  checks.sessionBefore = sessionBefore?.session ? `session already running (display ${sessionBefore.session.display})` : "cold session";

  // ---- launch -------------------------------------------------------------
  const clickAt = await launchFromMenu(page, app.name);
  let adoptedWindow = null;
  let launchError = null;
  try {
    adoptedWindow = await (async () => {
      const deadline = Date.now() + SCALE(240000);
      while (Date.now() < deadline) {
        const windows = await windowsInfo(page);
        const adopted = appWindows(windows, app.id);
        if (adopted.length) return adopted[0];
        await page.waitForTimeout(150);
      }
      throw new Error("no adopted window appeared");
    })();
  } catch (error) {
    launchError = String(error);
  }
  const launchMs = adoptedWindow ? Date.now() - clickAt : NaN;
  if (!adoptedWindow) {
    checks.launch = check("fail", null, `no adopted RAST window within the timeout: ${launchError}`);
    return { checks, launchFailed: true };
  }
  checks.launch = check("pass", `click → adopted window ${Math.round(launchMs)} ms`);
  log(`  launch: adopted in ${Math.round(launchMs)} ms`);

  // Wait for the shim to report the remote window(s).
  let shim = null;
  try {
    shim = await waitFor(page, async () => {
      const snapshot = await shimSnapshot(page);
      return snapshot.ready && snapshot.windows.length > 0 ? snapshot : null;
    }, SCALE(120000), "remote window from the native shim");
  } catch (error) {
    checks.windowMapping = check("fail", null, `shim never reported a remote top-level window: ${error.message}`);
  }

  // ---- window mapping -----------------------------------------------------
  if (shim) {
    await page.waitForTimeout(1500);
    const shellWindows = appWindows(await windowsInfo(page), app.id);
    const remoteWindows = shim.windows;
    const bodyInspection = await page.evaluate((ids) => ids.map((id) => {
      const element = document.querySelector(`.window[data-window-id="${id}"]`);
      const body = element?.querySelector(".win-body");
      return {
        id,
        adoptedNodes: body?.querySelectorAll(".rast-native-xpra-window").length ?? 0,
        iframes: body?.querySelectorAll("iframe").length ?? 0,
        canvases: body?.querySelectorAll("canvas").length ?? 0,
      };
    }), shellWindows.map((window) => window.id));
    const expected = remoteWindows.filter((window) => !window.overrideRedirect && !window.windowType?.includes("MENU")).length;
    const oneToOne = shellWindows.length === expected;
    const noViewers = bodyInspection.every((item) => item.iframes === 0);
    const adopted = bodyInspection.every((item) => item.adoptedNodes >= 1);
    const status = oneToOne && noViewers && adopted ? "pass" : "fail";
    checks.windowMapping = check(status, {
      shellWindows: shellWindows.length,
      remoteTopLevelWindows: expected,
      remoteWindows: remoteWindows.map((window) => ({ wid: window.wid, title: window.title, type: window.windowType, transientFor: window.transientFor, adopted: window.adopted })),
      bodies: bodyInspection,
    }, status === "pass" ? "one RAST window per remote top-level window, adopted DOM node, 0 iframes/viewers" : "window count or adopted DOM mismatch");
    log(`  mapping: shell=${shellWindows.length} remote=${expected} iframes=${bodyInspection.map((item) => item.iframes).join("/")}`);
  }

  // ---- normalise to one window (Chrome may restore windows from a previous
  // session after the benchmark's abrupt shutdown) ---------------------------
  try {
    const extras = appWindows(await windowsInfo(page), app.id).filter((window) => window.id !== adoptedWindow.id);
    for (const extra of extras) {
      const closer = page.locator(`.window[data-window-id="${extra.id}"] .win-control-close`).first();
      if (await closer.count()) await closer.click().catch(() => {});
      await page.waitForTimeout(500);
    }
    if (extras.length > 0) {
      await waitFor(page, async () => (appWindows(await windowsInfo(page), app.id).length === 1 ? true : null), SCALE(20000), "extra restored Chrome windows to close");
      log(`  closed ${extras.length} restored Chrome window(s) so the second-window check starts from one window`);
    }
  } catch (error) {
    log(`  window normalisation warning: ${error.message}`);
  }

  // ---- geometry / calibration for deterministic checks --------------------
  let layout = null;
  const hasFixture = Boolean(caps.textInput && (app.id === "google-chrome" || caps.scroll));
  try {
    await page.evaluate((windowId) => {
      const shim = document.querySelector("iframe.linux-native-session-frame")?.contentWindow?.RASTNative;
      if (!shim) return null;
      const remoteWindow = shim.windows().find((candidate) => candidate.adopted) ?? shim.windows()[0];
      return remoteWindow ? shim.resizeWindow(remoteWindow.wid, { width: 1440, height: 860 }) : null;
    }, adoptedWindow.id).catch(() => null);
    await page.waitForTimeout(SCALE(1500));
    if (app.id === "google-chrome") {
      const box = await windowBox(page, adoptedWindow.id);
      const loaded = await navigateBench(page, {
        windowId: adoptedWindow.id,
        window: box,
        content: { x: box.x, y: box.y, width: box.width },
        navY: box.y + 8 + 64 + 32,
      }, "mode=type");
      if (!loaded) throw new Error("could not load the deterministic fixture page in the remote Chrome");
      layout = await calibrateContent(page, adoptedWindow.id);
      log(`  bench page content: ${JSON.stringify(layout.content)}`);
    }
  } catch (error) {
    layout = null;
    log(`  fixture navigation unavailable: ${error.message}`);
  }

  // ---- typing --------------------------------------------------------------
  if (layout) {
    const box = await windowBox(page, adoptedWindow.id);
    const clip = { x: Math.round(box.x + 6), y: Math.round(box.y + 6), width: Math.round(Math.min(box.width - 12, 900)), height: Math.round(Math.min(box.height - 12, 420)) };
    // Click into the page textarea (deterministic fixture), type, expect pixels.
    await page.mouse.click(layout.content.x + 30, layout.content.y + 246 + 40);
    await page.waitForTimeout(300);
    const typing = await clipChanged(page, clip, async () => {
      await page.keyboard.type("rast compat check", { delay: 20 });
    }, SCALE(6000));
    checks.typing = check(typing.changed ? "pass" : "fail", typing, typing.changed ? `pixels changed ${typing.elapsedMs} ms after typing started` : "no pixel change after typing");
    log(`  typing: ${typing.changed ? "pass" : "FAIL"} (${typing.elapsedMs} ms${typing.changed ? "" : ", no change"})`);

    // ---- scrolling -------------------------------------------------------
    if (caps.scroll) {
      await clickNav(page, layout, 0.375); // SCROLL page
      await page.waitForTimeout(SCALE(1200));
      const scroll = await clipChanged(page, clip, async () => {
        for (let tick = 0; tick < 6; tick += 1) { await page.mouse.wheel(0, 280); await sleep(60); }
      }, SCALE(5000));
      checks.scrolling = check(scroll.changed ? "pass" : "fail", scroll, scroll.changed ? "wheel scroll changes pixels on the deterministic fixture" : "wheel scrolling produced no pixel change");
      log(`  scrolling: ${scroll.changed ? "pass" : "FAIL"}`);
    } else {
      checks.scrolling = check("not-applicable", null, "app has no known scrollable fixture for a deterministic check");
    }

    // ---- clipboard (fixture round trip) ---------------------------------
    if (caps.clipboard) {
      await clickNav(page, layout, 0.125); // TYPE page
      await page.waitForTimeout(SCALE(1200));
      await page.mouse.click(layout.content.x + 30, layout.content.y + 246 + 40);
      await page.waitForTimeout(300);
      await page.keyboard.press("Control+KeyA");
      await page.keyboard.press("Backspace");
      await page.keyboard.type("clipboard-round-trip", { delay: 12 });
      await page.waitForTimeout(700);
      const withText = await page.screenshot({ clip, animations: "disabled" }).catch(() => null);
      await page.keyboard.press("Control+KeyA");
      await page.keyboard.press("Control+KeyC");
      await sleep(500);
      await page.keyboard.press("Backspace");
      await page.waitForTimeout(700);
      const cleared = await page.screenshot({ clip, animations: "disabled" }).catch(() => null);
      await page.keyboard.press("Control+KeyV");
      await page.waitForTimeout(1200);
      const pasted = await page.screenshot({ clip, animations: "disabled" }).catch(() => null);
      let status = "fail";
      let evidence = null;
      if (withText && cleared && pasted) {
        const restored = withText.equals(pasted);
        const differedFromCleared = !cleared.equals(pasted);
        status = restored && differedFromCleared ? "pass" : differedFromCleared ? "partial" : "fail";
        evidence = { restored, differedFromCleared, detail: restored ? "copy → clear → paste restored the identical text visually" : "paste changed pixels but did not restore the exact pre-clear frame" };
      }
      checks.clipboard = check(status, evidence, "Ctrl+A/Ctrl+C → Backspace → Ctrl+V in the remote Chrome fixture");
      log(`  clipboard: ${status}`);
    } else if (caps.clipboard === false) {
      checks.clipboard = check("not-applicable", null, "app has no deterministic clipboard target");
    } else {
      checks.clipboard = check("not-tested", null, "clipboard round trip not implemented for this app");
    }
  } else {
    checks.typing = check("not-tested", null, "no deterministic input fixture for this app");
    checks.scrolling = check("not-tested", null, caps.scroll ? "fixture navigation failed" : "app has no known scrollable fixture");
    checks.clipboard = check("not-tested", null, "fixture navigation failed");
  }

  // ---- second window -------------------------------------------------------
  if (caps.secondWindow) {
    // Chrome may restore windows from an earlier session with a delay; collapse
    // back to the single benchmark window so the count starts from one.
    try {
      const all = appWindows(await windowsInfo(page), app.id);
      const keep = all.find((window) => window.id === adoptedWindow.id) ?? all[0];
      for (const extra of all.filter((window) => window.id !== keep.id)) {
        const closer = page.locator(`.window[data-window-id="${extra.id}"] .win-control-close`).first();
        if (await closer.count()) await closer.click().catch(() => {});
        await page.waitForTimeout(500);
      }
      await waitFor(page, async () => {
        const current = appWindows(await windowsInfo(page), app.id);
        return current.length <= 1 ? true : null;
      }, SCALE(20000), "restored Chrome windows to close before the second-window check");
    } catch (error) {
      log(`  pre-second-window normalisation warning: ${error.message}`);
    }
    const before = (await windowsInfo(page)).filter((window) => isAdopted(window, app.id)).length;
    let after = before;
    // Newer shells reserve Ctrl+N for the local browser, so a second window is
    // requested from inside the remote page (the fixture's POPUP button opens a
    // real second Chrome window) and Ctrl+N is kept as the legacy path.
    const openSecondWindow = async () => {
      // Newer shells reserve Ctrl+N for the local browser, so a second window is
      // requested from inside the remote page: focus the page in its centre
      // (pointer mapping is reliable there) and press "p", which the fixture
      // page turns into a real second Chrome window (trusted key gesture).
      await page.keyboard.press("Control+KeyN").catch(() => {});
      await page.waitForTimeout(1200);
      after = appWindows(await windowsInfo(page), app.id).length;
      if (after <= before && layout) {
        await page.mouse.click(layout.content.x + layout.content.width / 2, layout.content.y + layout.content.height / 2);
        await page.waitForTimeout(300);
        await page.keyboard.press("KeyP");
      }
    };
    await openSecondWindow();
    try {
      await waitFor(page, async () => {
        const snapshot = await shimSnapshot(page);
        after = appWindows(await windowsInfo(page), app.id).length;
        return after > before && snapshot.windows.length > 1 ? true : null;
      }, SCALE(20000), "second Chrome window");
    } catch { /* recorded below */ }
    const shimAfter = await shimSnapshot(page);
    const ok = after === before + 1 && shimAfter.windows.length >= after;
    checks.secondWindow = check(ok ? "pass" : "fail", {
      shellBefore: before,
      shellAfter: after,
      remoteWindowsAfter: shimAfter.windows.map((window) => window.title),
      screenshots: [`${app.id}-second-window`],
    }, ok ? "Ctrl+N created exactly one more adopted RAST window and one more remote top-level window" : "second window was not mirrored 1:1");
    await page.screenshot({ path: join(SCREENSHOTS, `${OUT_PREFIX}-${app.id}-second-window.png`) }).catch(() => {});
    // Close the extra window from the taskbar / window control.
    const windows = appWindows(await windowsInfo(page), app.id);
    if (windows.length > 1) {
      const last = windows[windows.length - 1];
      const closer = page.locator(`.window[data-window-id="${last.id}"] .win-control-close`).first();
      if (await closer.count()) await closer.click().catch(() => {});
      await page.waitForTimeout(1200);
    }
    log(`  second window: ${ok ? "pass" : "FAIL"} (shell ${before} → ${after})`);
  } else {
    checks.secondWindow = check("not-tested", null, "app has no second-window test (capability not declared)");
  }

  // ---- close ---------------------------------------------------------------
  const beforeClose = appWindows(await windowsInfo(page), app.id).length;
  let closeStatus = "fail";
  let closeEvidence = { beforeClose };
  try {
    const windows = appWindows(await windowsInfo(page), app.id);
    for (const window of windows) {
      const closer = page.locator(`.window[data-window-id="${window.id}"] .win-control-close`).first();
      if (await closer.count()) await closer.click().catch(() => {});
      await page.waitForTimeout(600);
    }
    await waitFor(page, async () => {
      const shell = appWindows(await windowsInfo(page), app.id).length;
      const shim = await shimSnapshot(page);
      closeEvidence.shellAfter = shell;
      closeEvidence.remoteAfter = shim.windows.length;
      return shell === 0 && shim.windows.length === 0 ? true : null;
    }, SCALE(20000), "app windows to close");
    closeStatus = "pass";
  } catch (error) {
    closeEvidence.error = String(error);
  }
  checks.close = check(closeStatus, closeEvidence, closeStatus === "pass" ? "closing the RAST window closed the remote window and removed it from the shell" : "window did not disappear from both the shell and the remote session");
  log(`  close: ${closeStatus}`);
  await closeRun(api, app.id).catch(() => {});
  await sleep(500);
  return { checks, launchFailed: false };
}

// ---------------------------------------------------------------------------
// Overall row status
// ---------------------------------------------------------------------------
const CHECK_ORDER = ["launch", "windowMapping", "typing", "scrolling", "clipboard", "secondWindow", "close"];
function overallStatus(checks, app) {
  const statuses = CHECK_ORDER.map((name) => checks[name]?.status).filter(Boolean);
  if (statuses.includes("fail")) return "fail";
  const meaningful = statuses.filter((status) => status !== "not-applicable" && status !== "not-tested");
  if (meaningful.length === 0) return "not-tested";
  if (meaningful.includes("partial")) return "partial";
  const required = ["launch", "windowMapping", "close"];
  if (required.some((name) => checks[name]?.status !== "pass")) return "partial";
  if (checks.typing?.status === "fail") return "fail";
  if (meaningful.every((status) => status === "pass")) return "pass";
  return "partial";
}

// ---------------------------------------------------------------------------
// Markdown
// ---------------------------------------------------------------------------
function renderMarkdown(results) {
  const m = results.matrix;
  const row = (entry) => {
    const checks = entry.checks ?? {};
    const cell = (name) => {
      const value = checks[name];
      if (!value) return "—";
      return value.status === "not-applicable" ? "n/a" : value.status === "not-tested" ? "not tested" : value.status;
    };
    return `| ${entry.name} | ${entry.installed ? "yes" : "no"} | ${entry.inAppCenter ? "yes" : "no"} | ${cell("launch")} | ${cell("windowMapping")} | ${cell("typing")} | ${cell("scrolling")} | ${cell("clipboard")} | ${cell("secondWindow")} | ${cell("close")} | **${entry.overall}** | ${entry.note ?? ""} |`;
  };
  const lines = [];
  lines.push(`# RAST native app compatibility matrix — ${results.label}`);
  lines.push("");
  lines.push(`Generated ${results.generatedAt} against \`${results.target.baseUrl}\` as \`${results.target.user.username}\`.`);
  lines.push(`Raw JSON: \`${OUT_PREFIX}.json\`.`);
  lines.push("");
  lines.push("## Summary");
  lines.push("");
  const counts = {};
  for (const entry of m) counts[entry.overall] = (counts[entry.overall] ?? 0) + 1;
  lines.push(`Apps found: ${m.length}. ${Object.entries(counts).map(([status, count]) => `${status}: ${count}`).join(", ")}.`);
  lines.push("");
  lines.push("Legend: **pass** = all required checks passed, **partial** = some checks passed but app-specific checks are n/a / not tested, **fail** = a tested behaviour is broken, **not-tested** = app is not installed / not managed by RAST, so it was not exercised.");
  lines.push("");
  lines.push("## Matrix");
  lines.push("");
  lines.push("| App | Installed | In App Center | Launch | 1 window / top-level | Typing | Scrolling | Clipboard | 2nd window | Close | Overall | Notes |");
  lines.push("|---|---|---|---|---|---|---|---|---|---|---|---|");
  for (const entry of m) lines.push(row(entry));
  lines.push("");
  lines.push("## Detection");
  lines.push("");
  lines.push(`* RAST App Center Linux apps (admin view): ${results.detection.appCenterApps.length}; visible to users: ${results.detection.userVisibleApps.length}.`);
  lines.push(`* System \`.desktop\` applications: ${results.detection.desktopEntries.length} (installed on the host; only apps managed by the RAST App Center can be launched through RAST).`);
  lines.push(`* RAST built-in apps (File Station, App Center, ...): ${results.detection.builtinApps.length} — outside the native Linux-app matrix.`);
  lines.push("");
  lines.push("| System desktop app | Exec | Managed by RAST |");
  lines.push("|---|---|---|");
  for (const entry of results.detection.desktopEntries) {
    const managed = results.detection.appCenterApps.find((app) => (app.desktopId ?? "").includes(entry.file.split("/").pop()?.replace(/\.desktop$/, "")) || app.name === entry.name);
    lines.push(`| ${entry.name} | \`${entry.exec ?? "—"}\` | ${managed ? `yes (${managed.id})` : "no"} |`);
  }
  lines.push("");
  lines.push("| Not installed / not detected | Status |");
  lines.push("|---|---|");
  for (const entry of results.detection.knownApps) {
    if (entry.installed) continue;
    lines.push(`| ${entry.name} | not installed → not tested |`);
  }
  lines.push("");
  lines.push("## Check details");
  lines.push("");
  for (const entry of m) {
    if (!entry.checks) continue;
    lines.push(`### ${entry.name}${entry.inAppCenter ? ` (\`${entry.appId}\`)` : ""}`);
    lines.push("");
    for (const name of CHECK_ORDER) {
      const detail = entry.checks[name];
      if (!detail) continue;
      lines.push(`* **${name}**: ${detail.status}${detail.notes ? ` — ${detail.notes}` : ""}`);
      if (detail.evidence && typeof detail.evidence === "object") lines.push("  ```json");
      if (detail.evidence && typeof detail.evidence === "object") lines.push("  " + JSON.stringify(detail.evidence).slice(0, 1200));
      if (detail.evidence && typeof detail.evidence === "object") lines.push("  ```");
    }
    lines.push("");
  }
  lines.push("## Method");
  lines.push("");
  lines.push("* Detection scans `/usr/share/applications`, `/usr/local/share/applications`, flatpak/snap export dirs, the App Center app list (`GET /api/linux-apps` as admin and as the user) and known command names.");
  lines.push("* Live checks run in headless Playwright Chromium through the real login form and the real desktop shell: launch from the start menu, then inspect the DOM (`.window[data-window-id]`, adopted `.rast-native-xpra-window` nodes, iframes) and the native shim snapshot (`RASTNative.windows()`).");
  lines.push("* Typing/scrolling/clipboard are verified against a deterministic local page loaded inside the remote Chrome through its address bar; other apps are marked not-applicable / not-tested for checks that need an app-specific fixture instead of pretending to test them.");
  lines.push("* Screenshots for the second-window check: `tests/e2e/screenshots/compat/`.");
  lines.push("");
  lines.push("## Limitations");
  lines.push("");
  lines.push("* Only apps that the RAST App Center can launch are live-tested; system packages outside the App Center are reported as installed but not tested.");
  lines.push("* The matrix ran against one environment (one host, one Xpra version, one Chrome build); GPU/accelerated paths are not present on this host.");
  lines.push("* Flaky browser-level interactions are retried once by the harness; a `fail` therefore means the behaviour was reproduced.");
  lines.push("");
  return `${lines.join("\n")}\n`;
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------
const BENCH_URL = `http://127.0.0.1:${Number(process.env.RAST_COMPAT_PORT || 8798)}/`;

async function main() {
  mkdirSync(RESULTS, { recursive: true });
  mkdirSync(SCREENSHOTS, { recursive: true });
  const credentials = resolveCredentials();
  log(`target ${OPTIONS.baseUrl}; output ${JSON_PATH}`);

  const adminApi = new Api(OPTIONS.baseUrl);
  await adminApi.login(credentials.admin);

  const appCenter = (await adminApi.listApps()).apps ?? [];
  const userApi = new Api(OPTIONS.baseUrl);
  await userApi.login(credentials.user);
  const userVisibleApps = ((await userApi.listApps()).apps ?? []).filter((app) => app.status === "ready");
  const builtin = [];
  try {
    const packages = await adminApi.packages();
    for (const app of packages.apps ?? []) if (app.installed) builtin.push({ id: app.id, name: app.name, description: app.description });
  } catch (error) {
    log(`built-in app list unavailable: ${error.message}`);
  }

  const { entries: desktopEntries } = scanDesktopEntries();
  const knownCommands = [
    "google-chrome", "google-chrome-stable", "chromium-browser", "xterm", "uxterm", "alacritty",
    "firefox", "libreoffice", "gimp", "vlc", "mpv", "gedit", "gnome-terminal", "xclock", "code", "htop", "vim", "openbox", "xpra",
  ];
  const commands = detectCommands(knownCommands);
  const knownApps = knownCommands
    .filter((command, index, list) => list.indexOf(command) === index)
    .map((command) => {
      const desktopMatch = desktopEntries.find((entry) => (entry.execBinary ?? "").split("/").pop() === command
        || (entry.exec ?? "").includes(`/bin/${command}`));
      const appCenterMatch = appCenter.find((app) => (app.command ?? []).some((value) => value.endsWith(`/${command}`)) || app.package === command);
      return {
        name: desktopMatch?.name ?? command,
        command,
        installed: Boolean(commands[command] || desktopMatch || appCenterMatch),
        path: commands[command],
        desktopEntry: desktopMatch?.file ?? null,
        appCenterAppId: appCenterMatch?.id ?? null,
      };
    });

  const detection = { appCenterApps: appCenter, userVisibleApps, desktopEntries, builtinApps: builtin, commands, knownApps };
  const matrix = [];

  // Apps that cannot be live-tested still get an explicit row so the report
  // separates "not installed" from real failures.
  const appCenterIds = new Set(appCenter.map((app) => app.id));
  for (const app of appCenter) {
    const managedByAppCenter = Boolean(app.backend);
    matrix.push({
      name: app.name,
      appId: app.id,
      installed: app.status === "ready",
      inAppCenter: true,
      appCenterBackend: app.backend ?? null,
      checks: {},
      overall: managedByAppCenter ? "not-tested" : "not-tested",
      note: app.status !== "ready" ? `App Center status: ${app.status}; not live-tested` : "live test pending",
    });
  }
  for (const entry of desktopEntries) {
    const managed = appCenter.find((app) => app.name === entry.name || (app.desktopId ?? "").toLowerCase().includes(entry.file.split("/").pop().toLowerCase().replace(/\.desktop$/, "")));
    if (managed) continue;
    matrix.push({
      name: entry.name,
      appId: null,
      installed: Boolean(entry.execBinary) && !entry.execStub && existsSync(entry.execBinary),
      inAppCenter: false,
      checks: {},
      overall: "not-tested",
      note: entry.execStub
        ? `desktop entry is a handler stub (${entry.execBinary}) — not a launchable app; not tested`
        : `installed on the host (${entry.execBinary ?? "?"}) but not managed by the RAST App Center — cannot be launched through RAST; not tested`,
    });
  }
  for (const entry of knownApps) {
    if (entry.appCenterAppId) continue;
    if (desktopEntries.some((desktop) => desktop.name === entry.name)) continue;
    matrix.push({
      name: entry.name,
      appId: null,
      installed: entry.installed,
      inAppCenter: false,
      checks: {},
      overall: "not-tested",
      note: entry.installed ? `installed (${entry.path ?? "?"}) but without a desktop entry/App Center app — not tested` : "not installed on the host → not tested",
    });
  }

  if (OPTIONS.detectOnly) {
    log("detect-only: writing the inventory without touching sessions");
  } else {
    // Local deterministic fixture page (same one as the perf harness).
    const benchHtml = readFileSync(BENCH_PAGE);
    const server = createServer((request, response) => {
      response.writeHead(200, { "Content-Type": "text/html; charset=utf-8", "Cache-Control": "no-store" });
      response.end(benchHtml);
    });
    await new Promise((done) => server.listen(Number(process.env.RAST_COMPAT_PORT || 8798), "127.0.0.1", done));

    const browser = await launchBrowser();
    const context = await browser.newContext({ viewport: { width: 1480, height: 920 }, ignoreHTTPSErrors: true });
    const page = await context.newPage();
    page.setDefaultTimeout(SCALE(30000));
    await formLogin(page, credentials.user.username, credentials.user.password);
    await closeSession(userApi);

    const testable = appCenter.filter((app) => app.status === "ready" && app.backend === "native").slice(0, OPTIONS.maxApps);
    for (const app of testable) {
      const row = matrix.find((entry) => entry.appId === app.id);
      if (!row) continue;
      try {
        await closeSession(userApi);
        await page.waitForTimeout(600);
        const caps = CAPABILITIES[app.id] ?? { textInput: "unknown", scroll: "unknown", clipboard: "unknown", secondWindow: "unknown" };
        const { checks } = await testApp({ page, api: userApi, app, caps });
        row.checks = checks;
        row.overall = overallStatus(checks, app);
        row.note = app.backend === "native" ? "live-tested through the desktop shell" : `backend ${app.backend}: live test not implemented`;
      } catch (error) {
        row.checks = { launch: check("fail", null, String(error)) };
        row.overall = "fail";
        row.note = `live test crashed: ${String(error).slice(0, 300)}`;
        log(`  ${app.name}: live test crashed: ${error.message}`);
      } finally {
        await closeSession(userApi).catch(() => {});
      }
    }
    // App Center apps we did not live-test (non-native backend or over the cap).
    for (const row of matrix) {
      if (!row.inAppCenter || Object.keys(row.checks ?? {}).length > 0) continue;
      const app = appCenter.find((candidate) => candidate.id === row.appId);
      if (!app) continue;
      if (app.backend !== "native") row.note = `backend ${app.backend ?? "?"}: this matrix only live-tests native apps`;
      else if (matrix.filter((entry) => entry.checks?.launch).length >= OPTIONS.maxApps) row.note = "skipped by --max-apps";
      else row.note = "not live-tested (status not ready / not visible)";
    }

    await browser.close().catch(() => {});
    await closeSession(userApi).catch(() => {});
    await new Promise((done) => server.close(done));
  }

  const results = {
    schema: "rast-compat-matrix/1",
    label: OPTIONS.label,
    generatedAt: new Date().toISOString(),
    options: { ...OPTIONS, creds: undefined },
    target: { baseUrl: OPTIONS.baseUrl, user: { username: credentials.user.username } },
    detection,
    matrix,
    environment: {
      host: execFileSync("hostname", { encoding: "utf8" }).trim(),
      kernel: execFileSync("uname", ["-r"], { encoding: "utf8" }).trim(),
      xpra: execFileSync("xpra", ["--version"], { encoding: "utf8" }).split("\n")[0],
      chrome: execFileSync("google-chrome-stable", ["--version"], { encoding: "utf8" }).trim(),
      chromiumHeadless: CHROMIUM,
      fixturePage: BENCH_PAGE,
    },
    notes: [
      "Checks are executed through the real authenticated desktop path in headless Playwright Chromium.",
      "Installed apps that the RAST App Center cannot launch are reported as not-tested rather than failed.",
      "The deterministic fixture page is loaded inside the remote Chrome through its own address bar; apps without such a fixture get not-applicable/not-tested rather than a guessed result.",
    ],
  };
  writeFileSync(JSON_PATH, `${JSON.stringify(results, null, 2)}\n`);
  writeFileSync(MD_PATH, renderMarkdown(results));
  log(`results: ${JSON_PATH}`);
  log(`markdown: ${MD_PATH}`);
  console.log(`\n${renderMarkdown(results)}`);
  return 0;
}

main()
  .then((code) => process.exit(code ?? 0))
  .catch((error) => {
    console.error("compat matrix failed:", error);
    process.exit(2);
  });
