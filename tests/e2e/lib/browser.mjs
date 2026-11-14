// Playwright helpers: browser/context lifecycle, login (cookie jar and form),
// the RAST desktop shell and generic window/taskbar introspection.
//
// The selectors are the shell's public DOM contract (see frontend/src/core/
// wm.ts and frontend/src/shell/desktop.ts): `.window[data-window-id]`,
// `.win-title`, `.win-control-*`, `.taskbar-app`, `.menu-app`. The scenarios
// avoid internal app markup where possible so they survive UI changes.

import { mkdirSync, readdirSync } from "node:fs";
import { join } from "node:path";
import { chromium } from "playwright";

import { config, SCREENSHOTS_DIR } from "./config.mjs";

const CACHED_CHROMIUM = "/home/lufria/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome";

export function chromiumExecutable() {
  if (process.env.RAST_E2E_CHROMIUM) return process.env.RAST_E2E_CHROMIUM;
  try {
    readdirSync("/home/lufria/.cache/ms-playwright");
    return CACHED_CHROMIUM;
  } catch {
    return undefined; // let Playwright resolve its own browser
  }
}

export async function launchBrowser({ headless = true, slowMo = 0 } = {}) {
  const executablePath = chromiumExecutable();
  const browser = await chromium.launch({
    headless,
    slowMo,
    executablePath,
    args: [
      "--no-sandbox",
      "--disable-gpu",
      "--disable-dev-shm-usage",
      "--disable-background-timer-throttling",
      "--disable-renderer-backgrounding",
    ],
  });
  return browser;
}

export async function newDesktopSession(browser, { api = null, viewport = { width: 1480, height: 920 } } = {}) {
  const context = await browser.newContext({ viewport, ignoreHTTPSErrors: true });
  const logs = [];
  if (api) {
    // Reuse the API session in the browser: the shell checks the same cookie.
    const cookieUrl = new URL(config.baseUrl);
    const cookies = [...api.cookies.entries()].map(([name, value]) => ({
      name,
      value,
      domain: cookieUrl.hostname,
      path: "/",
      httpOnly: name === "rast_session",
      secure: false,
      sameSite: "Lax",
      expires: Math.floor(Date.now() / 1000) + 3600,
    }));
    if (cookies.length > 0) await context.addCookies(cookies);
  }
  const page = await context.newPage();
  page.setDefaultTimeout(30_000);
  page.on("console", (message) => {
    const text = message.text();
    if (message.type() === "error" || message.type() === "warning") {
      logs.push({ type: message.type(), text: text.slice(0, 500) });
    }
  });
  page.on("pageerror", (error) => logs.push({ type: "pageerror", text: String(error).slice(0, 500) }));
  return { context, page, logs };
}

export async function gotoDesktop(page) {
  await page.goto(`${config.baseUrl}/`, { waitUntil: "domcontentloaded" });
  await page.waitForSelector(".taskbar", { timeout: 30_000 });
  await page.waitForFunction(
    () => !document.querySelector(".boot-screen, .loading-screen, .splash"),
    undefined,
    { timeout: 30_000 },
  ).catch(() => {});
  return page;
}

/** Login through the real form (evidence that the browser flow works). */
export async function loginViaForm(page, username, password) {
  await page.goto(`${config.baseUrl}/`, { waitUntil: "domcontentloaded" });
  await page.waitForSelector('input[name="username"], .auth-form', { timeout: 20_000 });
  await page.fill('input[name="username"]', username);
  await page.fill('input[type="password"]', password);
  await page.click('.auth-form button[type="submit"], button:has-text("Sign in")');
  await page.waitForSelector(".taskbar", { timeout: 30_000 });
  return page;
}

/** A logged-in desktop page: API cookie jar first, form login as fallback. */
export async function openDesktop(browser, api) {
  const session = await newDesktopSession(browser, { api });
  await gotoDesktop(session.page);
  if (!(await isAuthenticated(session.page))) {
    const role = api?.user;
    if (!role) throw new Error("desktop did not authenticate and no credentials were supplied");
    await loginViaForm(session.page, role.username, config.users.find((u) => u.username === role.username)?.password ?? config.admin.password);
  }
  return session;
}

export async function isAuthenticated(page) {
  return page.evaluate(async () => {
    try {
      const response = await fetch("/api/auth/me", { credentials: "same-origin" });
      return response.ok;
    } catch {
      return false;
    }
  });
}

// ---------------------------------------------------------------------------
// Start menu and app launch
// ---------------------------------------------------------------------------
export async function openStartMenu(page) {
  if (await page.locator(".start-menu").count()) return page.locator(".start-menu").first();
  await page.locator(".taskbar-start").click();
  await page.waitForSelector(".start-menu", { timeout: 10_000 });
  return page.locator(".start-menu").first();
}

export async function closeStartMenu(page) {
  if (await page.locator(".start-menu").count()) await page.keyboard.press("Escape");
  await page.waitForSelector(".start-menu", { state: "detached", timeout: 5_000 }).catch(() => {});
}

/** Open an app from the start menu by visible name (e.g. "App Center"). */
export async function launchApp(page, name) {
  const menu = await openStartMenu(page);
  const search = menu.locator(".menu-search");
  if (await search.count()) await search.fill(name);
  const tile = menu.locator(".menu-app", { hasText: name }).first();
  await tile.waitFor({ timeout: 10_000 });
  await tile.click();
  await page.waitForSelector(".start-menu", { state: "detached", timeout: 10_000 }).catch(() => {});
}

// ---------------------------------------------------------------------------
// Windows and taskbar
// ---------------------------------------------------------------------------
export function windowsInfo(page) {
  return page.evaluate(() => {
    const windows = [...document.querySelectorAll(".window[data-window-id]")];
    return windows.map((el) => {
      const rect = el.getBoundingClientRect();
      return {
        id: el.dataset.windowId,
        appId: el.dataset.appId,
        title: el.querySelector(".win-title")?.textContent?.trim() ?? "",
        focused: el.classList.contains("is-focused") || el.classList.contains("focused"),
        minimized: el.classList.contains("is-minimized") || el.classList.contains("minimized"),
        maximized: el.classList.contains("is-maximized") || el.classList.contains("maximized"),
        rect: { x: Math.round(rect.x), y: Math.round(rect.y), width: Math.round(rect.width), height: Math.round(rect.height) },
      };
    });
  });
}

export function taskbarInfo(page) {
  return page.evaluate(() => {
    const entries = [...document.querySelectorAll(".taskbar-app")];
    return entries.map((el) => ({
      label: el.querySelector(".taskbar-app-label")?.textContent?.trim() ?? el.textContent?.trim() ?? "",
      appId: el.dataset.appId ?? null,
      active: el.classList.contains("is-active"),
      minimized: el.classList.contains("is-minimized"),
      pinned: el.classList.contains("is-pinned"),
    }));
  });
}

export async function waitForWindows(page, predicate, { timeoutMs = 60_000, what = "windows" } = {}) {
  const started = Date.now();
  let last;
  for (;;) {
    last = await windowsInfo(page);
    if (predicate(last)) return last;
    if (Date.now() - started > timeoutMs) {
      throw new Error(
        `timed out waiting for ${what}; windows now: ${JSON.stringify(last).slice(0, 800)}`,
      );
    }
    await page.waitForTimeout(300);
  }
}

export async function waitForTaskbar(page, predicate, { timeoutMs = 60_000, what = "taskbar entries" } = {}) {
  const started = Date.now();
  let last;
  for (;;) {
    last = await taskbarInfo(page);
    if (predicate(last)) return last;
    if (Date.now() - started > timeoutMs) {
      throw new Error(
        `timed out waiting for ${what}; taskbar now: ${JSON.stringify(last).slice(0, 800)}`,
      );
    }
    await page.waitForTimeout(300);
  }
}

/** Windows of one app (by app id prefix, e.g. "linux-chrome"). */
export function appWindows(windows, appIdPart) {
  return windows.filter((window) => (window.appId ?? "").includes(appIdPart));
}

export async function focusWindow(page, windowId) {
  await page.locator(`.window[data-window-id="${windowId}"]`).click({ position: { x: 30, y: 8 } });
}

export async function closeWindowFromTaskbar(page, selector) {
  const entry = page.locator(selector);
  await entry.click({ button: "middle" }).catch(async () => {
    await entry.click();
    await page.keyboard.press("Control+w");
  });
}

// ---------------------------------------------------------------------------
// The embedded Linux-app frame
// ---------------------------------------------------------------------------
export function appFrames(page) {
  return page.locator('iframe.linux-frame, iframe[src*="/linux-apps/"]');
}

export async function waitForAppFrame(page, { timeoutMs = 90_000 } = {}) {
  const frame = appFrames(page).last();
  await frame.waitFor({ state: "attached", timeout: timeoutMs });
  const handle = await frame.elementHandle();
  // Wait for the streamed page itself to load (Xpra HTML5 shell or VNC page).
  await page.waitForTimeout(500);
  return { frame, handle };
}

export async function frameBox(page) {
  const frame = appFrames(page).last();
  return frame.boundingBox();
}

/** Click into the streamed app (the frame swallows input; Xpra forwards it). */
export async function clickInApp(page, { x = 0.5, y = 0.5, position } = {}) {
  const box = await frameBox(page);
  if (!box) throw new Error("no Linux app frame is visible");
  await page.mouse.click(box.x + (position?.x ?? box.width * x), box.y + (position?.y ?? box.height * y));
}

export async function typeInApp(page, text) {
  await page.keyboard.type(text, { delay: 25 });
}

// ---------------------------------------------------------------------------
// Screenshots
// ---------------------------------------------------------------------------
export function ensureScreenshotDir(dir = SCREENSHOTS_DIR) {
  mkdirSync(dir, { recursive: true });
  return dir;
}

export async function screenshot(page, name, { dir = SCREENSHOTS_DIR, fullPage = false } = {}) {
  ensureScreenshotDir(dir);
  const path = join(dir, `${name}.png`);
  await page.screenshot({ path, fullPage, animations: "disabled" });
  return path;
}

export async function screenshotClip(page, clip, name, { dir = SCREENSHOTS_DIR } = {}) {
  ensureScreenshotDir(dir);
  const path = join(dir, `${name}.png`);
  await page.screenshot({ path, clip, animations: "disabled" });
  return path;
}

/** Screenshot of the Linux app frame area, used for pixel-change detection. */
export async function frameScreenshot(page, name, { dir = SCREENSHOTS_DIR } = {}) {
  const box = await frameBox(page);
  if (!box) throw new Error("no Linux app frame to screenshot");
  return screenshotClip(page, box, name, { dir });
}

/** Raw PNG buffer of the app frame, for cheap before/after comparisons. */
export async function frameBuffer(page) {
  const box = await frameBox(page);
  if (!box) throw new Error("no Linux app frame to screenshot");
  return page.screenshot({ clip: box, animations: "disabled" });
}

/** Poll the app frame until its pixels change from `baseline` (first pixel). */
export async function waitForFrameChange(page, baseline, { timeoutMs = 15_000, intervalMs = 100 } = {}) {
  const started = Date.now();
  let polls = 0;
  for (;;) {
    await page.waitForTimeout(intervalMs);
    polls += 1;
    const current = await frameBuffer(page);
    if (!current.equals(baseline)) {
      return { changed: true, elapsedMs: Date.now() - started, polls, bytes: current.length };
    }
    if (Date.now() - started > timeoutMs) {
      return { changed: false, elapsedMs: Date.now() - started, polls };
    }
  }
}
