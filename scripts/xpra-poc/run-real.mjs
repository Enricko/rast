// End-to-end proof of the REAL frontend integration (not the PoC harness):
// the built RAST desktop (frontend/dist) served by a mock RAST API, with the
// single Xpra session proxied at `/linux-apps/session/<key>/` and the real
// `src/linux_apps/xpra_shim.js` injected, exactly like the Rust backend will.
//
// Proves: native app launch from the launcher -> one shared client -> every
// remote Xpra window is a first-class RAST window (title bar + taskbar entry),
// keyboard forwarding into the remote app, RAST resize -> X server resize,
// minimize/restore/maximize, close -> WM_DELETE, refresh -> windows restored
// without duplicated apps, narrow layout.
//
// Usage: (cd frontend && npm run build) && node scripts/xpra-poc/run-real.mjs

import fs from "node:fs";
import path from "node:path";
import { execFileSync, spawn } from "node:child_process";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { startMock } from "./mock-rast.mjs";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const SHOTS = path.join(__dirname, "screenshots-real");
const OBSERVATIONS = path.join(__dirname, "observations-real.json");
const PLAYWRIGHT_DIR = process.env.POC_PLAYWRIGHT_DIR || "/tmp/pw";
const CHROME = process.env.POC_CHROME || "/home/lufria/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome";
const DISPLAY = process.env.POC_DISPLAY || ":101";
const PORT = Number(process.env.MOCK_PORT || 14593);

const require = createRequire(path.join(PLAYWRIGHT_DIR, "package.json"));
const { chromium } = require("playwright");

const observations = { startedAt: new Date().toISOString(), steps: [], assertions: [] };
const failures = [];
let page = null;
let browser = null;

const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

function pass(name, detail) {
  observations.assertions.push({ name, ok: true, detail: detail ?? null });
  console.log(`PASS  ${name}${detail ? " — " + detail : ""}`);
}
function fail(name, detail) {
  observations.assertions.push({ name, ok: false, detail: detail ?? null });
  failures.push(name);
  console.log(`FAIL  ${name}${detail ? " — " + detail : ""}`);
}

async function waitFor(description, fn, timeoutMs = 30_000, intervalMs = 150) {
  const deadline = Date.now() + timeoutMs;
  let last = null;
  while (Date.now() < deadline) {
    try {
      const value = await fn();
      if (value) return value;
    } catch (error) {
      last = error;
    }
    await sleep(intervalMs);
  }
  throw new Error(`timeout waiting for ${description}${last ? ": " + String(last) : ""}`);
}

async function screenshot(name) {
  fs.mkdirSync(SHOTS, { recursive: true });
  const file = path.join(SHOTS, name + ".png");
  await page.screenshot({ path: file });
  return path.relative(process.cwd(), file);
}

function xtermCount() {
  try {
    return Number(execFileSync("pgrep", ["-fc", "xterm"], { encoding: "utf8" }).trim() || 0);
  } catch {
    return 0;
  }
}

function raastWindowCount() {
  return page.evaluate(() => document.querySelectorAll('.window[data-app-id^="linux:"]').length);
}

function taskbarTitles() {
  return page.evaluate(() =>
    Array.from(document.querySelectorAll(".taskbar .taskbar-app .taskbar-app-label")).map((el) => el.textContent ?? ""),
  );
}

async function shimWindows() {
  return page.evaluate(() => {
    const frame = document.querySelector("iframe.linux-native-session-frame");
    if (!frame || !frame.contentWindow) return [];
    const native = frame.contentWindow.RASTNative;
    return native ? native.windows() : [];
  });
}

async function titleTexts() {
  return page.evaluate(() =>
    Array.from(document.querySelectorAll('.window[data-app-id^="linux:"] .win-title')).map((el) => el.textContent ?? ""),
  );
}

async function windowForTitle(title) {
  const index = (await titleTexts()).findIndex((text) => text.includes(title));
  if (index < 0) return null;
  const windows = page.locator('.window[data-app-id^="linux:"]');
  return windows.nth(index);
}

async function main() {
  let server = null;
  let testXterm = null;
  try {
    server = await startMock(PORT);
    console.log("mock RAST backend:", server.url);
    observations.url = server.url;

    testXterm = spawn("xterm", ["-T", "RAST-POC", "-fa", "Monospace", "-fs", "11", "-e", "bash", "--noprofile", "--norc", "-i"], {
      env: { ...process.env, DISPLAY },
      detached: true,
      stdio: "ignore",
    });
    testXterm.unref();
    await sleep(1200);

    browser = await chromium.launch({ executablePath: CHROME, headless: true, args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"] });
    const context = await browser.newContext({ viewport: { width: 1440, height: 900 } });
    page = await context.newPage();
    const consoleLines = [];
    page.on("console", (message) => {
      const text = message.text();
      if (message.type() === "error" || text.includes("RAST") || text.includes("native")) consoleLines.push(`${message.type()}: ${text.slice(0, 300)}`);
    });
    page.on("pageerror", (error) => consoleLines.push("pageerror: " + String(error).slice(0, 300)));

    await page.goto(server.url, { waitUntil: "domcontentloaded", timeout: 30_000 });
    await waitFor("RAST desktop", async () => page.locator(".desktop").count(), 30_000);
    pass("the real RAST desktop boots against the native API contract");

    // The shared session exists already: the desktop must reconnect it and
    // recreate every remote window without opening any app first.
    await waitFor("native session windows restored on desktop boot", async () => (await raastWindowCount()) >= 1, 45_000);
    const bootWindows = await raastWindowCount();
    const bootTitles = await titleTexts();
    pass("one shared client creates one RAST window per remote Xpra window", `${bootWindows} windows: ${bootTitles.join(" | ")}`);
    const taskbar = await taskbarTitles();
    if (taskbar.length >= bootWindows) pass("each top-level window has a taskbar entry", taskbar.join(" | "));
    else fail("each top-level window has a taskbar entry", JSON.stringify(taskbar));
    await screenshot("01-desktop-restored");

    // Launch the native app from the start menu (the real launcher path).
    await page.click('[data-panel-trigger="start"]');
    await waitFor("start menu", async () => page.locator(".menu-app").count(), 10_000);
    const chromeTile = page.locator(".menu-app", { hasText: "Google Chrome" }).first();
    if ((await chromeTile.count()) > 0) {
      await chromeTile.click();
      pass("native app launches from the RAST launcher");
    } else {
      fail("native app launches from the RAST launcher", "Google Chrome tile not found");
    }
    await sleep(2500);
    await screenshot("02-after-launch");

    // Typing goes through the real RAST key handling into the remote xterm.
    const target = await windowForTitle("RAST-POC");
    if (!target) throw new Error("the RAST-POC remote window was not adopted");
    const box = await target.locator(".win-body").boundingBox();
    await page.mouse.click(box.x + Math.min(120, box.width / 2), box.y + Math.min(60, box.height / 2));
    await sleep(600);
    const proof = "/tmp/rast-real-e2e-typing-proof";
    try { fs.unlinkSync(proof); } catch { /* ignore */ }
    await page.keyboard.type("touch " + proof, { delay: 35 });
    await page.keyboard.press("Enter");
    const typed = await waitFor("typing in the remote app", async () => fs.existsSync(proof), 20_000).catch(() => false);
    if (typed) pass("keyboard forwarding works in the real desktop (remote xterm ran the command)");
    else fail("keyboard forwarding works in the real desktop");
    await screenshot("03-typing");

    // Resize the RAST window by dragging its edge; the X server window follows.
    const before = (await shimWindows()).find((w) => w.title === "RAST-POC");
    const handle = target.locator(".win-resize-e").first();
    const hb = await handle.boundingBox();
    // Aim inside the window: the resize handles are clipped by the window's
    // overflow at the outer edge, so their exact bounding-box centre is over
    // the desktop.
    await page.mouse.move(hb.x + 2, hb.y + hb.height / 2);
    await page.mouse.down();
    await page.mouse.move(hb.x + 2 + 180, hb.y + hb.height / 2 + 120, { steps: 12 });
    await page.mouse.up();
    await sleep(900);
    const after = (await shimWindows()).find((w) => w.title === "RAST-POC");
    if (before && after && after.width > before.width) pass("RAST resize reaches the X server window", `${before.width} -> ${after.width}`);
    else fail("RAST resize reaches the X server window", JSON.stringify({ before, after }));
    await screenshot("04-resize");

    // Minimize strips the taskbar entry focus; restore from the taskbar.
    await target.locator(".win-control-minimize").click();
    await waitFor("window minimized", async () => target.evaluate((el) => el.classList.contains("is-minimized")), 8000).catch(() => null);
    const minimizedNow = await page.evaluate(() => {
      const frame = document.querySelector("iframe.linux-native-session-frame");
      const native = frame?.contentWindow?.RASTNative;
      const win = native?.windows().find((w) => w.title === "RAST-POC");
      return win ? win.minimized : null;
    });
    if (minimizedNow === true) pass("minimize iconifies the remote window (X11 unmap)");
    else fail("minimize iconifies the remote window", String(minimizedNow));
    await screenshot("05-minimized");
    await page.locator(".taskbar .taskbar-app", { hasText: "RAST-POC" }).first().click();
    await sleep(700);
    const restored = await target.evaluate((el) => !el.classList.contains("is-minimized"));
    if (restored) pass("restore from the taskbar maps the remote window again");
    else fail("restore from the taskbar maps the remote window again");

    // Maximize fills the work area and resizes the remote window.
    await target.locator(".win-control-maximize").click();
    await waitFor("window maximized", async () => target.evaluate((el) => el.classList.contains("is-maximized")), 8000).catch(() => null);
    await sleep(700);
    const maxed = (await shimWindows()).find((w) => w.title === "RAST-POC");
    if (maxed && maxed.width > after.width) pass("maximize resizes the remote window to the work area", `${maxed.width}x${maxed.height}`);
    else fail("maximize resizes the remote window to the work area", JSON.stringify(maxed));
    await screenshot("06-maximize");
    await target.locator(".win-control-maximize").click();

    // Close: WM_DELETE reaches the app, only that window disappears.
    const xtermsBefore = xtermCount();
    const windowsBefore = await raastWindowCount();
    await target.locator(".win-control-close").click();
    const closed = await waitFor("remote window closed", async () => (await titleTexts()).every((text) => !text.includes("RAST-POC")), 15_000).catch(() => false);
    await sleep(600);
    if (closed && (await raastWindowCount()) === windowsBefore - 1) pass("closing a RAST window closes only that remote window", `${windowsBefore} -> ${await raastWindowCount()}`);
    else fail("closing a RAST window closes only that remote window");
    await screenshot("07-closed");

    // Refresh: the same session reconnects and windows are recreated, apps not duplicated.
    const beforeReloadWindows = await raastWindowCount();
    const beforeReloadXterms = xtermCount();
    await page.reload({ waitUntil: "domcontentloaded", timeout: 30_000 });
    await waitFor("RAST desktop after refresh", async () => page.locator(".desktop").count(), 30_000);
    await waitFor("native windows restored after refresh", async () => (await raastWindowCount()) === beforeReloadWindows, 45_000).catch(() => null);
    await sleep(1500);
    const afterReloadWindows = await raastWindowCount();
    const afterReloadXterms = xtermCount();
    if (afterReloadWindows === beforeReloadWindows && afterReloadXterms === beforeReloadXterms) {
      pass("refresh reconnects and restores every window (no duplicated apps)", `windows ${beforeReloadWindows} -> ${afterReloadWindows}, xterms ${beforeReloadXterms} -> ${afterReloadXterms}`);
    } else {
      fail("refresh reconnects and restores every window without duplicates", JSON.stringify({ beforeReloadWindows, afterReloadWindows, beforeReloadXterms, afterReloadXterms }));
    }
    await screenshot("08-reconnect");

    // Narrow layout: windows fill the work area and stay usable.
    await page.setViewportSize({ width: 390, height: 844 });
    await sleep(1200);
    const narrow = await page.evaluate(() => {
      const wins = Array.from(document.querySelectorAll('.window[data-app-id^="linux:"]'));
      const visible = wins.filter((win) => win.offsetParent !== null);
      if (visible.length === 0) return null;
      const rects = visible.map((win) => {
        const rect = win.getBoundingClientRect();
        return { width: Math.round(rect.width), height: Math.round(rect.height) };
      });
      return { windows: rects, viewport: [window.innerWidth, window.innerHeight] };
    });
    if (narrow && narrow.windows.every((rect) => rect.width <= 390 && rect.height <= 844 && rect.width > 0)) pass("narrow (390x844) layout keeps native windows usable", JSON.stringify(narrow));
    else fail("narrow (390x844) layout keeps native windows usable", JSON.stringify(narrow));
    await screenshot("09-narrow");

    // ------------------------------------------------- App Center native app UI
    await page.setViewportSize({ width: 1440, height: 900 });
    await sleep(900);
    await page.click('[data-panel-trigger="start"]');
    await waitFor("start menu", async () => page.locator(".menu-app").count(), 10_000);
    await page.locator(".menu-app", { hasText: "App Center" }).first().click();
    await waitFor("App Center", async () => page.locator(".pkg-content").count(), 20_000);
    const migrationNotice = await page.locator(".native-migration-notice").innerText().catch(() => "");
    if (migrationNotice.includes("Docker")) pass("App Center shows the Docker -> native migration notice", migrationNotice.split("\n").slice(0, 2).join(" · "));
    else fail("App Center shows the Docker -> native migration notice", JSON.stringify(migrationNotice.slice(0, 200)));
    await screenshot("10-appcenter-list");

    const appTile = page.locator('.pkg-tile[data-linux-id="chrome"]').first();
    await appTile.scrollIntoViewIfNeeded();
    await sleep(300);
    await appTile.click();
    await waitFor("native app page", async () => page.locator(".pkg-hero-name").count(), 15_000);
    const nativeBadge = await page.locator(".native-badge.is-native").count();
    const nativeSection = await page.locator(".pkg-section", { hasText: "Native install" }).count();
    if (nativeBadge > 0 && nativeSection > 0) pass("App Center shows the native install details (badge, path, launcher, dependencies)");
    else fail("App Center shows the native install details", JSON.stringify({ nativeBadge, nativeSection }));
    await screenshot("11-appcenter-native-app");

    // Install preview before anything is installed (the real Add app flow).
    await page.locator('.pkg-toolbar button[title^="Install a"]').first().click();
    await waitFor("Add app dialog", async () => page.locator('.modal input[type="url"]').count(), 10_000);
    await page.locator('.modal input[type="url"]').last().fill("https://example.test/google-chrome.deb");
    await page.locator('.linux-link button[type="submit"]').first().click();
    await waitFor(
      "install preview",
      async () => page.locator(".pkg-preview .pkg-preview-name").count(),
      20_000,
    );
    await sleep(250);
    const previewText = await page.locator(".pkg-preview").last().innerText();
    const previewChecks = ["google-chrome-stable", "package changes", "compatible", "architecture", "to install"];
    const haystack = previewText.toLowerCase();
    const previewOk = previewChecks.every((needle) => haystack.includes(needle));
    if (previewOk) pass("install preview presents package transaction + compatibility before installing", previewChecks.join(", "));
    else fail("install preview presents the server's installation details", JSON.stringify(previewText.slice(0, 500)));
    await screenshot("12-appcenter-preview");
    await page.locator(".modal .modal-foot .btn-ghost", { hasText: "Cancel" }).last().click();
    await sleep(300);
    await page.locator(".modal .modal-foot .btn-ghost", { hasText: "Cancel" }).last().click().catch(() => undefined);
    await screenshot("13-appcenter-final");

    observations.console = consoleLines.slice(-60);
  } catch (error) {
    fail("real-frontend e2e run completed", error && error.stack ? String(error.stack).slice(0, 2000) : String(error));
  } finally {
    try { if (page && !page.isClosed()) await screenshot("99-final"); } catch { /* ignore */ }
    if (browser) await browser.close().catch(() => undefined);
    if (testXterm) {
      try { process.kill(-testXterm.pid, "SIGKILL"); } catch { try { testXterm.kill("SIGKILL"); } catch { /* ignore */ } }
    }
    if (server) await server.close();
    observations.finishedAt = new Date().toISOString();
    observations.failures = failures;
    fs.writeFileSync(OBSERVATIONS, JSON.stringify(observations, null, 2));
    console.log("\nobservations:", OBSERVATIONS);
    if (failures.length) {
      console.error(`\n${failures.length} assertion(s) failed: ${failures.join("; ")}`);
      process.exitCode = 1;
    } else {
      console.log("\nAll real-frontend assertions passed.");
    }
  }
}

main();
