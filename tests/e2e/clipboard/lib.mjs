// Shared helpers for the RAST clipboard/input verification and the input-latency
// stage profile.
//
// Everything here runs against the *public production desktop* through the real
// login form (browser) plus the loopback API (http://127.0.0.1:8720) for
// read-only session state. Host-side commands are read-only evidence helpers
// (`xdotool`/`xclip` on the test identity's own Xpra display, `xpra info`); no
// helper ever starts/stops a service, a unit or a session.
//
// The remote application is a canvas, so the harness never inspects remote DOM:
// the controlled test page reports state through `document.title`, which the
// RAST window manager mirrors into `.win-title` of the adopted window.

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { inflateSync } from "node:zlib";

import { chromium } from "playwright";

import { RastApi } from "../lib/rast-api.mjs";

export const HERE = dirname(fileURLToPath(import.meta.url));
export const REPO = resolve(HERE, "../../..");
export const RESULTS_DIR = join(HERE, "results");
export const SCREENSHOTS_DIR = join(HERE, "screenshots");
export const CHROMIUM = "/home/lufria/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome";
export const DEFAULT_BASE_URL = "https://desktop.lufria.tech";
export const DEFAULT_CREDS = "/home/lufria/.config/rast-test/creds";
export const LOOPBACK_API = "http://127.0.0.1:8720";
export const VIEWPORT = { width: 1480, height: 920 };
export const APP_NAME = "Google Chrome";
export const APP_SLUG = "google-chrome";

export const sleep = (ms) => new Promise((done) => setTimeout(done, ms));

export function parseArgs(argv, defaults = {}) {
  const args = { ...defaults };
  for (const item of argv) {
    if (!item.startsWith("--")) throw new Error(`unexpected argument: ${item}`);
    const body = item.slice(2);
    const index = body.indexOf("=");
    if (index < 0) args[body] = true;
    else args[body.slice(0, index)] = body.slice(index + 1);
  }
  return args;
}

export function percentile(values, fraction) {
  const clean = values.filter((value) => Number.isFinite(value)).sort((a, b) => a - b);
  if (clean.length === 0) return null;
  const position = (clean.length - 1) * fraction;
  const lower = Math.floor(position);
  const upper = Math.ceil(position);
  if (lower === upper) return clean[lower];
  return clean[lower] + (clean[upper] - clean[lower]) * (position - lower);
}

export function stats(values) {
  const clean = values.filter((value) => Number.isFinite(value));
  if (clean.length === 0) return { n: 0, p50: null, p95: null, min: null, max: null, mean: null };
  const sum = clean.reduce((total, value) => total + value, 0);
  return {
    n: clean.length,
    p50: percentile(clean, 0.5),
    p95: percentile(clean, 0.95),
    min: Math.min(...clean),
    max: Math.max(...clean),
    mean: sum / clean.length,
  };
}

export const fmtMs = (value) => (Number.isFinite(value) ? Number(value).toFixed(value < 10 ? 2 : 1) : "n/a");

export function sha8(text) {
  return createHash("sha256").update(String(text)).digest("hex").slice(0, 8);
}

export async function waitFor(fn, { timeoutMs = 30_000, intervalMs = 150, what = "condition" } = {}) {
  const started = Date.now();
  let last = null;
  for (;;) {
    try {
      last = await fn();
      if (last) return last;
    } catch (error) {
      last = error;
    }
    if (Date.now() - started > timeoutMs) {
      const detail = last instanceof Error ? last.message : JSON.stringify(last)?.slice(0, 400);
      throw new Error(`timed out after ${timeoutMs} ms waiting for ${what}${detail ? `: ${detail}` : ""}`);
    }
    await sleep(intervalMs);
  }
}

/* ------------------------------------------------------------------ secrets */

export function readSecrets(path) {
  const values = {};
  for (const line of readFileSync(path, "utf8").split("\n")) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const index = trimmed.indexOf("=");
    if (index <= 0) continue;
    values[trimmed.slice(0, index).trim()] = trimmed.slice(index + 1).trim();
  }
  return values;
}

/** Production `username=password` file (or the dev `RAST_E2E_*` variables). */
export function resolveCredentials(secrets, user = "t-alice", admin = "t-admin") {
  if (secrets[user]) {
    return {
      user: { username: user, password: secrets[user] },
      admin: { username: admin, password: secrets[admin] ?? secrets[user] },
    };
  }
  const passwordOf = (name) => {
    const pairs = [
      [secrets.RAST_E2E_ADMIN_USER, secrets.RAST_E2E_ADMIN_PASSWORD],
      [secrets.RAST_E2E_USER1_USER, secrets.RAST_E2E_USER1_PASSWORD],
      [secrets.RAST_E2E_USER2_USER, secrets.RAST_E2E_USER2_PASSWORD],
    ];
    return pairs.find(([candidate]) => candidate === name)?.[1];
  };
  const devUser = user === "t-alice" ? secrets.RAST_E2E_USER1_USER || "alice" : user;
  const devAdmin = admin === "t-admin" ? secrets.RAST_E2E_ADMIN_USER || "admin" : admin;
  return {
    user: { username: devUser, password: passwordOf(devUser) },
    admin: { username: devAdmin, password: passwordOf(devAdmin) },
  };
}

/* ------------------------------------------------------------ static server */

/**
 * Tiny 127.0.0.1 static server for the controlled remote pages. The remote
 * Chrome runs on this host, so a loopback URL is reachable from the session.
 * Returns {port, url(path), close()}.
 */
export async function startPageServer({ port = 8798, routes = {}, host = "127.0.0.1" } = {}) {
  const resolved = {};
  for (const [route, value] of Object.entries(routes)) {
    resolved[route] = typeof value === "string" ? readFileSync(value) : Buffer.from(value.body, "utf8");
  }
  const server = createServer((request, response) => {
    const path = new URL(request.url, `http://${host}`).pathname;
    const body = resolved[path] ?? resolved["/"];
    if (!body) {
      response.writeHead(404, { "content-type": "text/plain" });
      response.end("not found");
      return;
    }
    response.writeHead(200, {
      "content-type": "text/html; charset=utf-8",
      "cache-control": "no-store",
      "content-length": body.length,
      connection: "close",
    });
    response.end(body);
  });
  let chosen = port;
  for (let attempt = 0; attempt < 20; attempt += 1) {
    try {
      await new Promise((resolveListen, rejectListen) => {
        server.once("error", rejectListen);
        server.listen(chosen, host, resolveListen);
      });
      break;
    } catch (error) {
      if (error.code !== "EADDRINUSE") throw error;
      chosen += 1;
    }
  }
  return {
    port: chosen,
    host,
    url: (path = "/clipboard-page.html") => `http://${host}:${chosen}${path}`,
    close: () => new Promise((done) => server.close(done)),
  };
}

/* ---------------------------------------------------------------- browser */

export async function launchBrowser({ headless = true } = {}) {
  let executablePath;
  try {
    readFileSync(CHROMIUM);
    executablePath = CHROMIUM;
  } catch {
    executablePath = undefined;
  }
  return chromium.launch({
    headless,
    executablePath,
    args: [
      "--no-sandbox",
      "--disable-gpu",
      "--disable-dev-shm-usage",
      "--disable-background-timer-throttling",
      "--disable-renderer-backgrounding",
      "--disable-backgrounding-occluded-windows",
    ],
  });
}

const MAC_USER_AGENT =
  "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/131.0.0.0 Safari/537.36";

/**
 * A browser context for the desktop shell: Clipboard API permissions for the
 * RAST origin, console/error collection, and optional macOS platform emulation
 * (the shim's `keyboardSwap` is derived from `navigator.platform` /
 * `navigator.userAgentData.platform`, so both must be overridden).
 */
export async function newClientContext(browser, { baseUrl = DEFAULT_BASE_URL, mac = false } = {}) {
  const context = await browser.newContext({ viewport: VIEWPORT, ignoreHTTPSErrors: true });
  await context
    .grantPermissions(["clipboard-read", "clipboard-write"], { origin: new URL(baseUrl).origin })
    .catch(() => undefined);
  const page = await context.newPage();
  page.setDefaultTimeout(30_000);
  const consoleLog = [];
  page.on("console", (message) => {
    const entry = { type: message.type(), text: message.text().slice(0, 600) };
    consoleLog.push(entry);
  });
  page.on("pageerror", (error) => consoleLog.push({ type: "pageerror", text: String(error).slice(0, 600) }));
  let cdp = null;
  if (mac) {
    cdp = await context.newCDPSession(page);
    await cdp.send("Emulation.setUserAgentOverride", {
      userAgent: MAC_USER_AGENT,
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
  }
  return { context, page, consoleLog, cdp };
}

export function npmConsoleErrors(consoleLog) {
  return consoleLog.filter(
    (entry) =>
      entry.type === "error" &&
      /clipboard|keyboard|forwardKey|RAST native shim|native session|linux-app/i.test(entry.text),
  );
}

/* ------------------------------------------------------------------- login */

export async function formLogin(page, { baseUrl, credentials }) {
  for (let attempt = 1; attempt <= 3; attempt += 1) {
    await page.goto(`${baseUrl}/`, { waitUntil: "domcontentloaded" }).catch(() => undefined);
    const booted = await page
      .waitForSelector('input[name="username"], .auth-form, .taskbar', { timeout: 45_000 })
      .then(() => true)
      .catch(() => false);
    if (!booted) {
      await sleep(2_000);
      continue;
    }
    if (await page.locator(".taskbar").count()) {
      if (await authenticated(page)) return { attempts: attempt, alreadySignedIn: true };
    }
    await page.fill('input[name="username"]', credentials.username);
    await page.fill('input[type="password"]', credentials.password);
    const [response] = await Promise.all([
      page
        .waitForResponse(
          (candidate) => candidate.url().includes("/api/auth/login") && candidate.request().method() === "POST",
          { timeout: 60_000 },
        )
        .catch(() => null),
      page.locator('.auth-form button[type="submit"], button:has-text("Sign in")').first().click(),
    ]);
    const status = response?.status() ?? 0;
    if (status === 429) {
      const retryAfter = Number(response?.headers()?.["retry-after"] ?? 60) || 60;
      await sleep(Math.min(retryAfter, 180) * 1000);
      continue;
    }
    try {
      await page.waitForSelector(".taskbar", { timeout: 45_000 });
      if (await authenticated(page)) return { attempts: attempt, alreadySignedIn: false, status };
    } catch {
      /* retry */
    }
  }
  throw new Error(`could not sign in as ${credentials.username} through the real form (rate limit?)`);
}

export async function authenticated(page) {
  return page
    .evaluate(async () => {
      try {
        const response = await fetch("/api/auth/me", { credentials: "same-origin" });
        return response.ok;
      } catch {
        return false;
      }
    })
    .catch(() => false);
}

/** Read-only loopback API client (session state, app list, identity checks). */
export async function loopbackApi(credentials) {
  const api = new RastApi(LOOPBACK_API);
  await api.loginAs(credentials);
  return api;
}

export async function deployedVersion(credentials, { baseUrl = DEFAULT_BASE_URL } = {}) {
  const facts = {};
  try {
    const admin = new RastApi(LOOPBACK_API);
    await admin.loginAs(credentials);
    for (const path of ["/api/system/info", "/api/about", "/api/system/version"]) {
      try {
        facts.info = await admin.get(path);
        facts.source = path;
        break;
      } catch {
        /* try next */
      }
    }
    await admin.logout().catch(() => undefined);
  } catch (error) {
    facts.error = String(error.message ?? error);
  }
  try {
    const response = await fetch(`${baseUrl}/healthz`);
    facts.healthzStatus = response.status;
  } catch (error) {
    facts.healthzError = String(error.message ?? error);
  }
  return facts;
}

/* --------------------------------------------------------------- desktop */

export function windowsInfo(page) {
  return page.evaluate(() =>
    [...document.querySelectorAll(".window[data-window-id]")].map((element) => {
      const rect = element.getBoundingClientRect();
      return {
        id: element.dataset.windowId,
        appId: element.dataset.appId ?? "",
        title: element.querySelector(".win-title")?.textContent?.trim() ?? "",
        focused: element.classList.contains("is-focused"),
        minimized: element.classList.contains("is-minimized"),
        rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
        hasCanvas: Boolean(element.querySelector(".linux-native-window canvas")),
      };
    }),
  );
}

export const isAdopted = (window, slug = APP_SLUG) => String(window.appId ?? "").startsWith(`linux:${slug}:`);
export const adoptedWindows = (windows, slug = APP_SLUG) => windows.filter((window) => isAdopted(window, slug));

export async function waitForAdoptedWindows(page, { slug = APP_SLUG, min = 1, max = Infinity, timeoutMs = 240_000 } = {}) {
  return waitFor(
    async () => {
      const windows = adoptedWindows(await windowsInfo(page), slug);
      const ready = windows.filter((window) => window.hasCanvas);
      if (ready.length >= min && ready.length <= max) return ready;
      return null;
    },
    { timeoutMs, intervalMs: 250, what: `${min === max ? min : `>=${min}`} adopted ${slug} window(s)` },
  );
}

export async function openStartMenu(page) {
  if (await page.locator(".start-menu").count()) return page.locator(".start-menu").first();
  for (let attempt = 1; attempt <= 4; attempt += 1) {
    await page.keyboard.press("Escape").catch(() => undefined);
    await page.locator(".taskbar-start").click({ force: attempt > 1 }).catch(() => undefined);
    const opened = await page
      .waitForSelector(".start-menu", { timeout: 8_000, state: "visible" })
      .then(() => true)
      .catch(() => false);
    if (opened) return page.locator(".start-menu").first();
    await sleep(600);
  }
  throw new Error("the RAST start menu did not open");
}

/** Launch an app by its start-menu tile name (the real user path). */
export async function launchApp(page, name = APP_NAME) {
  const menu = await openStartMenu(page);
  const search = menu.locator(".menu-search");
  if (await search.count()) {
    await search.fill(name).catch(() => undefined);
    await sleep(400);
  }
  const tile = menu.locator(".menu-app", { hasText: name }).first();
  await tile.waitFor({ timeout: 20_000 });
  await tile.click();
  await page.waitForSelector(".start-menu", { state: "detached", timeout: 10_000 }).catch(() => undefined);
  return Date.now();
}

/* --------------------------------------------------------------- geometry */

export async function windowBox(page, windowId) {
  return page.evaluate((id) => {
    const element = document.querySelector(`.window[data-window-id="${id}"]`);
    if (!element) return null;
    const rect = element.getBoundingClientRect();
    return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
  }, windowId);
}

export function windowTitle(page, windowId) {
  return page.evaluate((id) => {
    const element = document.querySelector(`.window[data-window-id="${id}"]`);
    return element?.querySelector(".win-title")?.textContent?.trim() ?? "";
  }, windowId);
}

/**
 * Title observer: records every `.win-title` change per adopted window with a
 * page-performance timestamp, so the harness measures *when the application
 * answered* without screenshot polling granularity.
 */
export async function installTitleObserver(page) {
  await page.evaluate(() => {
    if (window.__rastTitleLog) return;
    window.__rastTitleLog = {};
    const scan = () => {
      for (const element of document.querySelectorAll(".window[data-window-id]")) {
        const id = element.dataset.windowId;
        if (!id) continue;
        const title = element.querySelector(".win-title")?.textContent?.trim() ?? "";
        const previous = window.__rastTitleLog[id];
        if (!previous || previous.title !== title) {
          window.__rastTitleLog[id] = {
            title,
            at: performance.now(),
            wall: Date.now(),
            n: (previous?.n ?? 0) + 1,
          };
        }
      }
    };
    scan();
    const observer = new MutationObserver(scan);
    observer.observe(document.body, { subtree: true, childList: true, characterData: true });
    window.__rastTitleObserver = observer;
    window.__rastNow = () => performance.now();
  });
}

/** All title changes for one window, newest last. */
export function titleLog(page, windowId) {
  return page.evaluate((id) => window.__rastTitleLog?.[id] ?? null, windowId).catch(() => null);
}

export function pageNow(page) {
  return page.evaluate(() => performance.now());
}

/** Wait until the adopted window title matches `predicate`, using the observer. */
export async function waitForWindowTitle(page, windowId, predicate, { timeoutMs = 15_000, pollMs = 40 } = {}) {
  const started = Date.now();
  let last = "";
  for (;;) {
    const entry = await titleLog(page, windowId);
    last = entry?.title ?? (await windowTitle(page, windowId));
    let matched = false;
    try {
      matched = typeof predicate === "string" ? last.includes(predicate) : predicate(last);
    } catch {
      matched = false;
    }
    if (matched) return { title: last, at: entry?.at ?? null, wall: entry?.wall ?? Date.now(), elapsedMs: Date.now() - started };
    if (Date.now() - started > timeoutMs) return { title: last, at: entry?.at ?? null, matched: false, elapsedMs: Date.now() - started };
    await sleep(pollMs);
  }
}

/** Wait until the title *changes* from `previous` (fresh event evidence). */
export async function waitForTitleNot(page, windowId, previous, { timeoutMs = 10_000 } = {}) {
  return waitForWindowTitle(page, windowId, (title) => title !== previous, { timeoutMs });
}

export function decodePng(buffer) {
  if (buffer.readUInt32BE(0) !== 0x89504e47) throw new Error("not a PNG");
  let offset = 8;
  let width = 0;
  let height = 0;
  let colorType = 0;
  let bitDepth = 0;
  const idat = [];
  while (offset < buffer.length) {
    const length = buffer.readUInt32BE(offset);
    const type = buffer.toString("ascii", offset + 4, offset + 8);
    const data = buffer.subarray(offset + 8, offset + 8 + length);
    if (type === "IHDR") {
      width = data.readUInt32BE(0);
      height = data.readUInt32BE(4);
      bitDepth = data[8];
      colorType = data[9];
      if (data[12] !== 0) throw new Error("interlaced PNG");
    } else if (type === "IDAT") idat.push(data);
    else if (type === "IEND") break;
    offset += 12 + length;
  }
  if (bitDepth !== 8) throw new Error(`unsupported PNG bit depth ${bitDepth}`);
  const channels = colorType === 6 ? 4 : colorType === 2 ? 3 : colorType === 0 ? 1 : -1;
  if (channels < 0) throw new Error(`unsupported PNG color type ${colorType}`);
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
        const pa = Math.abs(p - left);
        const pb = Math.abs(p - up);
        const pc = Math.abs(p - upLeft);
        value = (value + (pa <= pb && pa <= pc ? left : pb <= pc ? up : upLeft)) & 0xff;
      } else if (filter !== 0) throw new Error(`unsupported PNG filter ${filter}`);
      out[x] = value;
    }
    previous = out;
  }
  return { width, height, channels, pixels };
}

/**
 * Locate the remote page origin inside the adopted-window screenshot. The
 * controlled page starts with a pure-black 8 px strip; RAST/Chrome chrome above
 * it is neither pure black nor uniform, so the first row of near-#000 pixels is
 * the page origin.
 */
export function findPageOrigin(image) {
  const { width, height, channels, pixels } = image;
  const nearBlack = (x, y) => {
    const index = (y * width + x) * channels;
    return Math.max(pixels[index], pixels[index + 1], pixels[index + 2]) < 22;
  };
  for (let y = 0; y < height; y += 1) {
    let hits = 0;
    let total = 0;
    for (let x = 0; x < width; x += 2) {
      total += 1;
      if (nearBlack(x, y)) hits += 1;
    }
    if (hits / total > 0.92) {
      let left = 0;
      let right = width - 1;
      while (left < width - 1 && !nearBlack(left, y)) left += 1;
      while (right > left && !nearBlack(right, y)) right -= 1;
      return { top: y, left, right, bottom: height - 1 };
    }
  }
  return null;
}

/**
 * Content rect of the remote page inside the adopted RAST window, in page
 * coordinates. `content` is the screenshot area of the remote page (origin =
 * page's black strip); clicks map 1:1 because the Xpra window is rendered at
 * scale 1.
 */
export async function calibratePage(page, windowId) {
  const box = await windowBox(page, windowId);
  if (!box || box.width < 40 || box.height < 40) return null;
  const clip = {
    x: Math.round(box.x),
    y: Math.round(box.y),
    width: Math.round(box.width),
    height: Math.round(box.height),
  };
  const image = decodePng(await page.screenshot({ clip, animations: "disabled" }));
  const origin = findPageOrigin(image);
  if (!origin) return null;
  return {
    windowId,
    window: box,
    content: {
      x: box.x + origin.left,
      y: box.y + origin.top,
      width: origin.right - origin.left + 1,
      height: origin.bottom - origin.top + 1,
    },
  };
}

/**
 * Find a point that actually hits `layout.windowId` (not an overlapping RAST
 * window drawn above it). Adopted Chrome windows can overlap — the popup window
 * the matrix opens on purpose sits on top of the main window — so blind centre
 * clicks would silently drive the wrong application. `elementsFromPoint` gives
 * the true hit-test order; the candidates are tried in a deterministic order.
 */
export async function findClickablePoint(page, layout, { mode = "content", preferred = null } = {}) {
  if (!layout?.windowId) return null;
  return page.evaluate(
    ({ windowId, content, mode, preferred }) => {
      const target = document.querySelector(`.window[data-window-id="${windowId}"]`);
      if (!target) return null;
      const candidates = [];
      const push = (x, y) => {
        if (Number.isFinite(x) && Number.isFinite(y)) candidates.push([x, y]);
      };
      if (mode === "titlebar") {
        const bar = target.querySelector(".win-titlebar")?.getBoundingClientRect();
        if (bar) for (const fraction of [0.2, 0.5, 0.8, 0.05, 0.95]) push(bar.left + bar.width * fraction, bar.top + bar.height / 2);
      } else if (mode === "absolute") {
        for (const point of preferred ?? []) push(point.x, point.y);
      } else if (content) {
        if (preferred) push(content.x + content.width * (preferred.fx ?? 0.5), content.y + content.height * (preferred.fy ?? 0.5));
        for (const fy of [0.85, 0.7, 0.95, 0.55, 0.3, 0.15]) {
          for (const fx of [0.5, 0.25, 0.75, 0.1, 0.9]) push(content.x + content.width * fx, content.y + content.height * fy);
        }
      }
      for (const [x, y] of candidates) {
        if (x < 4 || y < 4 || x > window.innerWidth - 4 || y > window.innerHeight - 4) continue;
        const stack = document.elementsFromPoint(x, y);
        let topWindow = null;
        for (const element of stack) {
          const windowElement = element.closest?.(".window[data-window-id]");
          if (windowElement) {
            topWindow = windowElement;
            break;
          }
        }
        if (topWindow === target) return { x, y };
      }
      return null;
    },
    { windowId: layout.windowId, content: layout.content ?? null, mode, preferred },
  );
}

/** Click the remote page's textarea (below the toolbar) at a hit-tested point. */
export async function clickRemotePage(page, layout, point = {}) {
  const preferred = { fx: point.x ?? 0.5, fy: point.y ?? 0.5 };
  const hit = await findClickablePoint(page, layout, { mode: "content", preferred });
  if (!hit) throw new Error(`no clickable point on the remote page of window ${layout.windowId} (fully covered)`);
  await page.mouse.click(hit.x, hit.y);
  await sleep(250);
  return hit;
}

/** Click the controlled page's NEW WINDOW button (toolbar row, 150x42 px). */
export async function clickNewWindowButton(page, layout) {
  const content = layout.content;
  const points = [
    [content.x + 8 + 75, content.y + 8 + 27],
    [content.x + 8 + 40, content.y + 8 + 27],
    [content.x + 8 + 120, content.y + 8 + 27],
    [content.x + 8 + 75, content.y + 8 + 14],
    [content.x + 8 + 75, content.y + 8 + 40],
  ].map(([x, y]) => ({ x, y }));
  const hit = await findClickablePoint(page, layout, { mode: "absolute", preferred: points });
  if (!hit) return null;
  await page.mouse.click(hit.x, hit.y);
  await sleep(400);
  return hit;
}

/**
 * Positive pixel identification of the controlled page inside an adopted
 * window. A stale/restored tab or a Google search page must never be mistaken
 * for the fixture: the controlled page is dark, has the pure-black origin strip
 * and a blue/red button bar, while Chrome New Tab / Google results are light.
 */
export async function detectControlledPage(page, windowId) {
  const layout = await calibratePage(page, windowId);
  if (!layout) return { ok: false, reason: "page origin (black strip) not found", layout: null };
  const box = layout.window;
  const image = decodePng(
    await page.screenshot({
      clip: {
        x: Math.max(0, Math.round(box.x)),
        y: Math.max(0, Math.round(box.y)),
        width: Math.round(Math.min(box.width, 1400)),
        height: Math.round(Math.min(box.height, 800)),
      },
      animations: "disabled",
    }),
  );
  const originTop = Math.max(0, Math.round(layout.content.y - box.y));
  const contentTop = Math.round(layout.content.y - box.y);
  const isDark = (x, y) => {
    const index = (y * image.width + x) * image.channels;
    return Math.max(image.pixels[index], image.pixels[index + 1], image.pixels[index + 2]) < 70;
  };
  const clamp = (value, max) => Math.max(0, Math.min(max - 1, Math.round(value)));
  // mean luminance of the textarea region (below the 62 px toolbar)
  let sum = 0;
  let count = 0;
  for (let y = contentTop + 80; y < image.height; y += 4) {
    for (let x = 2; x < image.width - 2; x += 4) {
      const index = (y * image.width + x) * image.channels;
      sum += Math.max(image.pixels[index], image.pixels[index + 1], image.pixels[index + 2]);
      count += 1;
    }
  }
  const mean = count ? sum / count : 0;
  // blue/red count in the 62 px toolbar band right below the origin strip
  let colored = 0;
  for (let y = clamp(originTop + 10, image.height); y < clamp(originTop + 60, image.height); y += 2) {
    for (let x = 2; x < image.width - 2; x += 2) {
      const index = (y * image.width + x) * image.channels;
      const r = image.pixels[index];
      const g = image.pixels[index + 1];
      const b = image.pixels[index + 2];
      if (b > 90 && b > r + 30 && b > g + 20) colored += 1;
      else if (r > 80 && r > g + 30 && r > b + 20) colored += 1;
    }
  }
  const ok = mean > 2 && mean < 80 && colored >= 3;
  return { ok, mean: Number(mean.toFixed(1)), colored, layout, reason: ok ? "controlled page signature" : `unexpected page (mean=${mean.toFixed(1)}, colored=${colored})` };
}

/** Focus an adopted window: hit-tested content click, titlebar fallback. */
export async function focusWindowContent(page, windowId, layout = null) {
  const resolved = layout ?? (await calibratePage(page, windowId));
  if (!resolved) {
    const box = await windowBox(page, windowId);
    if (!box) throw new Error(`window ${windowId} is not visible`);
    await page.mouse.click(box.x + box.width / 2, box.y + 10);
    await sleep(300);
    return null;
  }
  const contentHit = await findClickablePoint(page, resolved, { mode: "content" });
  if (contentHit) {
    await page.mouse.click(contentHit.x, contentHit.y);
    await sleep(300);
    return contentHit;
  }
  // The window is covered in its content area: raise it through its RAST
  // titlebar (the window manager owns that DOM) and click the content again.
  const titleHit = await findClickablePoint(page, resolved, { mode: "titlebar" });
  if (!titleHit) throw new Error(`window ${windowId} is fully covered by another window`);
  await page.mouse.click(titleHit.x, titleHit.y);
  await sleep(350);
  const second = await findClickablePoint(page, resolved, { mode: "content" });
  if (second) {
    await page.mouse.click(second.x, second.y);
    await sleep(300);
    return second;
  }
  return titleHit;
}

export function focusProbe(page) {
  return page.evaluate(() => {
    const frame = document.querySelector("iframe.linux-native-session-frame");
    const describe = (element) =>
      element
        ? {
            tag: element.tagName,
            id: element.id || "",
            className: typeof element.className === "string" ? element.className : "",
          }
        : null;
    let snapshot = null;
    try {
      snapshot = frame?.contentWindow?.RASTNative?.snapshot?.() ?? null;
    } catch (error) {
      snapshot = { error: String(error) };
    }
    return {
      activeElement: describe(document.activeElement),
      activeIsViewer: Boolean(frame && document.activeElement === frame),
      viewerActiveElement: describe(frame?.contentDocument?.activeElement ?? null),
      viewer: snapshot
        ? {
            version: snapshot.version,
            connected: snapshot.connected,
            clipboardEnabled: snapshot.clipboardEnabled,
            clipboardDirection: snapshot.clipboardDirection,
            clipboardPending: snapshot.clipboardPending,
            captureKeyboard: snapshot.captureKeyboard,
            keyboardSwap: snapshot.keyboardSwap,
            windows: Array.isArray(snapshot.windows) ? snapshot.windows.length : -1,
          }
        : null,
      nativeWindows: document.querySelectorAll(".linux-native-window").length,
    };
  });
}

export function shimSnapshot(page) {
  return page.evaluate(() => {
    const frame = document.querySelector("iframe.linux-native-session-frame");
    try {
      return frame?.contentWindow?.RASTNative?.snapshot?.() ?? null;
    } catch (error) {
      return { error: String(error) };
    }
  });
}

/* -------------------------------------------------------------- clipboard */

export async function writeLocalClipboard(page, text) {
  await page.bringToFront().catch(() => undefined);
  return page
    .evaluate((value) => navigator.clipboard.writeText(value), text)
    .then(() => true)
    .catch(() => false);
}

export async function readLocalClipboard(page) {
  return page
    .evaluate(() => navigator.clipboard.readText())
    .then((text) => text)
    .catch(() => null);
}

export async function waitForLocalClipboard(page, expected, { timeoutMs = 12_000, intervalMs = 100 } = {}) {
  const started = Date.now();
  let actual = null;
  for (;;) {
    actual = await readLocalClipboard(page);
    if (actual === expected) return { ok: true, actual, elapsedMs: Date.now() - started };
    if (Date.now() - started > timeoutMs) return { ok: false, actual, elapsedMs: Date.now() - started };
    await sleep(intervalMs);
  }
}

export async function waitForPasteTitle(page, windowId, expected, { timeoutMs = 15_000 } = {}) {
  const result = await waitForWindowTitle(page, windowId, (title) => title.includes(`PASTED:${expected}`), { timeoutMs });
  return { ...result, ok: Boolean(result.title?.includes(`PASTED:${expected}`)) };
}

/* ------------------------------------------------------- host-side evidence */

export function sudoRun(argv, { timeoutMs = 20_000 } = {}) {
  try {
    const stdout = execFileSync("sudo", ["-n", ...argv], {
      encoding: "utf8",
      timeout: timeoutMs,
      maxBuffer: 16 * 1024 * 1024,
    });
    return { ok: true, stdout, stderr: "" };
  } catch (error) {
    return { ok: false, stdout: error.stdout ?? "", stderr: error.stderr ?? String(error) };
  }
}

/** The test identity discovered from the API session (uid/base layout). */
export async function discoverIdentity(api) {
  const state = await api.nativeSession().catch(() => null);
  const session = state?.session ?? null;
  const userId = api.user?.id;
  const uid = 200000 + Number(userId);
  const identity = {
    userId: Number(userId),
    username: `rast-u${userId}`,
    uid,
    home: `/var/lib/rast/linux-users/${userId}`,
    xauthority: `/var/lib/rast/linux-users/${userId}/.Xauthority`,
    runtimeDir: `/run/rast-linux/${uid}`,
    socketDir: `/run/rast-linux/${uid}/sockets`,
    display: session?.display ?? null,
    session: session
      ? {
          id: session.id,
          status: session.status,
          display: session.display,
          profile: session.profile,
          displayBackend: session.displayBackend,
          startedAt: session.startedAt,
          apps: (session.apps ?? []).map((app) => ({ appId: app.appId, status: app.status, runId: app.runId })),
          viewers: session.viewers,
        }
      : null,
  };
  identity.available = Boolean(identity.display && identity.userId && Number.isFinite(identity.uid));
  return identity;
}

/** Read-only command as the identity on its own X display. */
export function asIdentity(identity, argv, options = {}) {
  return sudoRun(
    ["-u", identity.username, "env", `DISPLAY=:${identity.display}`, `XAUTHORITY=${identity.xauthority}`, ...argv],
    options,
  );
}

/** Current contents of the remote X11 CLIPBOARD selection (read-only). */
export function remoteClipboardText(identity) {
  for (const target of ["UTF8_STRING", "STRING"]) {
    const result = asIdentity(identity, ["xclip", "-selection", "clipboard", "-o", "-t", target]);
    if (result.ok) return { ok: true, text: result.stdout, target };
  }
  return { ok: false, text: null, error: "the remote X11 clipboard has no owner" };
}

/** Visible remote Chrome window titles from the identity's own X server. */
export function remoteWindowTitles(identity) {
  const search = asIdentity(identity, ["xdotool", "search", "--class", "chrome"]);
  if (!search.ok) return { ok: false, titles: [], error: search.stderr.slice(0, 200) };
  const ids = search.stdout
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean);
  const titles = [];
  for (const id of ids) {
    const name = asIdentity(identity, ["xdotool", "getwindowname", id]);
    titles.push({ id, title: name.ok ? name.stdout.trim() : "" });
  }
  return { ok: true, titles };
}

/** `xpra info` snapshot of the identity's own shared session. */
export function xpraInfo(identity) {
  if (!identity.available) return { ok: false, error: "identity/display unknown" };
  const result = sudoRun(
    [
      "-u",
      identity.username,
      "env",
      `XDG_RUNTIME_DIR=${identity.runtimeDir}`,
      `HOME=${identity.home}`,
      "xpra",
      "info",
      `--socket-dir=${identity.socketDir}`,
      `:${identity.display}`,
    ],
    { timeoutMs: 30_000 },
  );
  if (!result.ok) return { ok: false, error: result.stderr.slice(0, 300) };
  return { ok: true, text: result.stdout, map: parseInfo(result.stdout) };
}

export function parseInfo(text) {
  const map = {};
  for (const line of text.split("\n")) {
    const index = line.indexOf("=");
    if (index <= 0) continue;
    map[line.slice(0, index)] = line.slice(index + 1);
  }
  return map;
}

const num = (value) => {
  const parsed = Number(value);
  return Number.isFinite(parsed) ? parsed : null;
};

/** Selected latency/encoding fields of an `xpra info` snapshot. */
export function xpraLatencySnapshot(map) {
  if (!map) return null;
  const pick = (prefix) =>
    Object.fromEntries(
      Object.entries(map)
        .filter(([key]) => key.startsWith(prefix))
        .map(([key, value]) => [key.slice(prefix.length), num(value) ?? value]),
    );
  return {
    clientPing: {
      p50: num(map["client.0.connection.client.ping_latency.50p"]),
      p90: num(map["client.0.connection.client.ping_latency.90p"]),
      avg: num(map["client.0.connection.client.ping_latency.avg"]),
      max: num(map["client.0.connection.client.ping_latency.max"]),
      count: num(map["client.0.connection.client.ping_latency.count"]),
    },
    serverPing: {
      p50: num(map["client.0.connection.server.ping_latency.50p"]),
      p90: num(map["client.0.connection.server.ping_latency.90p"]),
    },
    damage: {
      clientLatency: num(map["client.0.damage.client-latency"]),
      frameTotalLatency: num(map["client.0.damage.frame-total-latency"]),
      targetLatency: num(map["client.0.damage.target-latency"]),
      lastLatency: num(map["client.0.damage.last-latency"]),
      fps: num(map["client.0.damage.fps"]),
    },
    batch: {
      delayAvg: num(map["client.0.batch.delay.avg"]),
      delayMax: num(map["client.0.batch.delay.max"]),
      delayCur: num(map["client.0.batch.delay.cur"]),
      minDelay: num(map["client.0.batch.min-delay"]),
      maxDelay: num(map["client.0.batch.max-delay"]),
      expire: num(map["client.0.batch.expire"]),
    },
    compressor: map["client.0.connection.compressor"] ?? null,
    encoder: map["client.0.connection.encoder"] ?? null,
    protocol: map["client.0.connection.protocol-type"] ?? null,
    packetCounts: {
      input: num(map["client.0.connection.input.packetcount"]),
      output: num(map["client.0.connection.output.packetcount"]),
    },
    windows: Object.fromEntries(
      Object.entries(map)
        .filter(([key]) => /^client\.0\.window\.\d+\.(title|width|height|encoding\.pipeline_option\.0\.encoder|total_frames\.)/.test(key))
        .map(([key, value]) => [key, num(value) ?? value]),
    ),
    batchWindow: pick("client.0.window."),
  };
}

/* --------------------------------------------------------------- screenshots */

export function ensureDir(dir) {
  mkdirSync(dir, { recursive: true });
  return dir;
}

export async function screenshot(page, dir, name) {
  ensureDir(dir);
  const path = join(dir, `${name}.png`);
  await page.screenshot({ path, animations: "disabled" }).catch(() => undefined);
  return path;
}

export async function screenshotWindow(page, dir, name, windowId) {
  ensureDir(dir);
  const path = join(dir, `${name}.png`);
  const box = await windowBox(page, windowId);
  if (!box) return null;
  await page
    .screenshot({
      path,
      clip: {
        x: Math.max(0, Math.round(box.x) - 2),
        y: Math.max(0, Math.round(box.y) - 2),
        width: Math.round(box.width) + 4,
        height: Math.round(box.height) + 4,
      },
      animations: "disabled",
    })
    .catch(() => undefined);
  return path;
}

export function writeJson(path, value) {
  writeFileSync(path, `${JSON.stringify(value, null, 2)}\n`);
}

export function writeText(path, value) {
  writeFileSync(path, value);
}
