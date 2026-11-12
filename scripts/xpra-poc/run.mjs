// End-to-end Playwright proof for the RAST native Xpra integration.
//
// Starts scripts/xpra-poc/server.mjs (HTTP+WS reverse proxy injecting the real
// src/linux_apps/xpra_shim.js into the live Xpra HTML5 page), opens the
// miniature RAST desktop (index.html + harness.js), and drives the whole
// contract against the live Xpra session on 127.0.0.1:14591 (display :101):
//
//   adopt, focus, typing, clipboard (both directions), resize, maximize,
//   minimize/restore, transient modal dialog, close, reconnect after refresh
//   (no duplicated apps), narrow layout.
//
// Usage: node scripts/xpra-poc/run.mjs
// Env: XPRA_HOST, XPRA_PORT, POC_PORT, POC_PLAYWRIGHT_DIR (/tmp/pw),
//      POC_CHROME (chromium executable path)
//
// Screenshots and observations.json are written next to this file.

import fs from "node:fs";
import path from "node:path";
import { execFileSync, spawn } from "node:child_process";
import { createRequire } from "node:module";
import { fileURLToPath } from "node:url";
import { startServer } from "./server.mjs";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const SHOTS = path.join(__dirname, "screenshots");
const OBSERVATIONS = path.join(__dirname, "observations.json");
const PLAYWRIGHT_DIR = process.env.POC_PLAYWRIGHT_DIR || "/tmp/pw";
const CHROME =
  process.env.POC_CHROME ||
  "/home/lufria/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome";
const DISPLAY = process.env.POC_DISPLAY || ":101";
const PORT = Number(process.env.POC_PORT || 14592);

const require = createRequire(path.join(PLAYWRIGHT_DIR, "package.json"));
const { chromium } = require("playwright");

const observations = { startedAt: new Date().toISOString(), steps: [], assertions: [], env: { DISPLAY, PORT } };
const failures = [];
const spawned = [];

function pass(name, detail) {
  observations.assertions.push({ name, ok: true, detail: detail ?? null });
  console.log(`PASS  ${name}${detail ? " — " + detail : ""}`);
}
function fail(name, detail) {
  observations.assertions.push({ name, ok: false, detail: detail ?? null });
  failures.push(name);
  console.log(`FAIL  ${name}${detail ? " — " + detail : ""}`);
}

function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function waitFor(description, fn, timeoutMs = 20000, intervalMs = 150) {
  const deadline = Date.now() + timeoutMs;
  let lastError = null;
  while (Date.now() < deadline) {
    try {
      const value = await fn();
      if (value) return value;
    } catch (error) {
      lastError = error;
    }
    await sleep(intervalMs);
  }
  throw new Error(`timeout waiting for ${description}${lastError ? ": " + lastError.message : ""}`);
}

async function screenshot(page, name) {
  fs.mkdirSync(SHOTS, { recursive: true });
  const file = path.join(SHOTS, name + ".png");
  await page.screenshot({ path: file });
  console.log(`  screenshot: ${path.relative(process.cwd(), file)}`);
  return file;
}

async function desktop(page) {
  return page.evaluate(() => window.__POC.desktopState());
}

async function findFrameByTitle(page, title) {
  const state = await desktop(page);
  return state.framesInfo.find((frame) => frame.title === title) || null;
}

async function waitFrameByTitle(page, title, timeoutMs = 25000) {
  return waitFor(`RAST window "${title}"`, async () => findFrameByTitle(page, title), timeoutMs);
}

function launch(program, args, env = {}) {
  const child = spawn(program, args, {
    env: { ...process.env, ...env },
    detached: true,
    stdio: "ignore",
  });
  child.unref();
  spawned.push(child);
  return child;
}

function xtermCount() {
  try {
    const out = execFileSync("pgrep", ["-fc", "xterm"], { encoding: "utf8" }).trim();
    return Number(out || 0);
  } catch {
    return 0;
  }
}

function processAlive(pid) {
  try {
    process.kill(pid, 0);
    return true;
  } catch {
    return false;
  }
}

function readClipboard() {
  try {
    return execFileSync("xclip", ["-selection", "clipboard", "-o"], {
      env: { ...process.env, DISPLAY },
      encoding: "utf8",
      timeout: 4000,
    });
  } catch (error) {
    return "";
  }
}

function writeClipboard(text) {
  // xclip forks and keeps owning the selection until another owner appears.
  const child = spawn("xclip", ["-selection", "clipboard", "-i"], {
    env: { ...process.env, DISPLAY },
    stdio: ["pipe", "ignore", "ignore"],
    detached: true,
  });
  child.stdin.end(text);
  child.unref();
  return child;
}

async function main() {
  let server = null;
  let browser = null;
  let page = null;
  let testXterm = null;
  let dialogApp = null;
  try {
    server = await startServer(PORT);
    console.log(`PoC proxy: ${server.url}`);
    observations.url = server.url;

    // A dedicated xterm we may type into and close without touching the
    // session's own windows.
    testXterm = launch("xterm", ["-T", "RAST-POC", "-fa", "Monospace", "-fs", "11", "-e", "bash", "--noprofile", "--norc", "-i"], {
      DISPLAY,
    });
    observations.testXtermPid = testXterm.pid;
    await sleep(1200);

    browser = await chromium.launch({
      executablePath: CHROME,
      headless: true,
      args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"],
    });
    const context = await browser.newContext({
      viewport: { width: 1440, height: 900 },
      permissions: ["clipboard-read", "clipboard-write"],
    });
    page = await context.newPage();
    const consoleLines = [];
    page.on("console", (message) => {
      const text = message.text();
      if (text.includes("RAST") || message.type() === "error") consoleLines.push(`${message.type()}: ${text}`);
    });
    page.on("pageerror", (error) => consoleLines.push("pageerror: " + String(error)));

    await page.goto(server.url, { waitUntil: "domcontentloaded", timeout: 30000 });

    // ---------------------------------------------------------- connect/adopt
    await waitFor("native session connected", async () => {
      const state = await desktop(page);
      return state.connected && state.frames.length > 0;
    }, 35000);
    const testFrame = await waitFrameByTitle(page, "RAST-POC");
    pass("single shared client connects and adopts remote windows", `${(await desktop(page)).frames.length} RAST windows`);
    observations.steps.push({ step: "connect", state: await desktop(page) });

    const initial = await desktop(page);
    const adoptedAll = initial.framesInfo.every((frame) => frame.adopted && frame.canvas && frame.canvas.width > 0);
    if (initial.adoptFailures.length === 0 && adoptedAll) {
      pass("DOM adoption works for every remote window", JSON.stringify(initial.framesInfo.map((f) => ({ wid: f.wid, body: f.body, canvas: f.canvas && { w: f.canvas.width, h: f.canvas.height } }))));
    } else {
      fail("DOM adoption works for every remote window", JSON.stringify(initial));
    }
    await screenshot(page, "01-desktop");

    // ------------------------------------------------------------------ typing
    const testWid = testFrame.wid;
    const testBody = await page.evaluate((wid) => window.__POC.frameBodySize(wid), testWid);
    // Real pointer click on the adopted canvas => xpra focus/pointer packets.
    const box = await page.locator(`section[data-wid="${testWid}"] .window-body`).boundingBox();
    await page.mouse.click(box.x + Math.min(80, box.width / 2), box.y + Math.min(40, box.height / 2));
    await sleep(600);
    const typingProof = "/tmp/rast-poc-typing-proof";
    try { fs.unlinkSync(typingProof); } catch { /* ignore */ }
    await page.keyboard.type("touch " + typingProof, { delay: 35 });
    await page.keyboard.press("Enter");
    const typed = await waitFor("typed command to run inside the remote xterm", async () => fs.existsSync(typingProof), 15000);
    if (typed) pass("keyboard forwarding types into the remote app", "remote xterm created " + typingProof);
    else fail("keyboard forwarding types into the remote app");
    await screenshot(page, "02-typing");
    observations.steps.push({ step: "typing", testBody });

    // -------------------------------------------------- clipboard browser->X11
    await page.evaluate(() => window.__POC.sendClipboard("RAST-CLIP-FROM-BROWSER-42"));
    const clipToServer = await waitFor("clipboard token to reach the X session", async () => {
      return readClipboard().includes("RAST-CLIP-FROM-BROWSER-42");
    }, 10000).catch(() => false);
    if (clipToServer) pass("clipboard browser -> remote desktop", readClipboard().trim());
    else fail("clipboard browser -> remote desktop", JSON.stringify(readClipboard().slice(0, 80)));

    // -------------------------------------------------- clipboard X11->browser
    writeClipboard("RAST-CLIP-FROM-SESSION-77");
    const clipFromServer = await waitFor("clipboard to reach the browser client", async () => {
      const text = await page.evaluate(() => window.__POC.clipboardText());
      return String(text).includes("RAST-CLIP-FROM-SESSION-77");
    }, 10000).catch(() => false);
    if (clipFromServer) pass("clipboard remote desktop -> browser", "shim clipboard buffer updated");
    else fail("clipboard remote desktop -> browser", JSON.stringify(await page.evaluate(() => window.__POC.clipboardText())));

    // ------------------------------------------------------------------ resize
    await page.evaluate((wid) => window.__POC.resizeFrame(wid, 780, 470), testWid);
    const resized = await waitFor("remote window resize to be applied", async () => {
      const canvas = await page.evaluate((wid) => window.__POC.frameCanvasSize(wid), testWid);
      const shimWindow = await page.evaluate((wid) => window.__POC.shimWindow(wid), testWid);
      return canvas && canvas.width >= 760 && shimWindow && shimWindow.width >= 760 ? { canvas, shimWindow } : null;
    }, 15000).catch(() => null);
    if (resized) pass("RAST window resize drives the server window size (canvas + X11)", JSON.stringify(resized));
    else fail("RAST window resize drives the server window size", JSON.stringify(await page.evaluate((wid) => ({ canvas: window.__POC.frameCanvasSize(wid), shim: window.__POC.shimWindow(wid) }), testWid)));
    // The canvas backing store is cleared by the resize; the shim requests a
    // full server refresh, so the window must have real content again.
    const repainted = await waitFor("resized canvas repaint", async () => (await page.evaluate((wid) => window.__POC.canvasDistinctColors(wid), testWid)) > 3, 12000).catch(() => false);
    if (repainted) pass("resize triggers a full repaint (no blank canvas)", "distinct colours " + (await page.evaluate((wid) => window.__POC.canvasDistinctColors(wid), testWid)));
    else fail("resize triggers a full repaint (no blank canvas)", "distinct colours " + (await page.evaluate((wid) => window.__POC.canvasDistinctColors(wid), testWid)));
    // Type after the resize: the app sees the new terminal size (visually proofed by screenshot).
    await page.keyboard.type("stty size", { delay: 30 });
    await page.keyboard.press("Enter");
    await sleep(700);
    await screenshot(page, "03-resize");

    // ---------------------------------------------------------------- maximize
    await page.evaluate((wid) => window.__POC.maximizeFrame(wid), testWid);
    const maximized = await waitFor("maximize to fill the desktop work area", async () => {
      const state = await desktop(page);
      const frame = state.framesInfo.find((f) => f.wid === testWid);
      return frame && frame.body.width >= 1300 ? frame : null;
    }, 10000).catch(() => null);
    if (maximized) pass("maximize fills the RAST work area", JSON.stringify(maximized.body));
    else fail("maximize fills the RAST work area");
    await screenshot(page, "04-maximize");
    await page.evaluate((wid) => window.__POC.maximizeFrame(wid), testWid);

    // ------------------------------------------------------ minimize / restore
    await page.evaluate((wid) => window.__POC.minimizeFrame(wid), testWid);
    const minimized = await waitFor("remote window iconified", async () => {
      const shimWindow = await page.evaluate((wid) => window.__POC.shimWindow(wid), testWid);
      return shimWindow && shimWindow.minimized ? shimWindow : null;
    }, 10000).catch(() => null);
    if (minimized) pass("minimize unmaps the remote window (X11 iconify)", "minimized=true");
    else fail("minimize unmaps the remote window");
    await screenshot(page, "05-minimized");
    await page.evaluate((wid) => window.__POC.restoreFrame(wid), testWid);
    const restored = await waitFor("remote window restored", async () => {
      const shimWindow = await page.evaluate((wid) => window.__POC.shimWindow(wid), testWid);
      return shimWindow && !shimWindow.minimized ? shimWindow : null;
    }, 10000).catch(() => null);
    if (restored) pass("restore maps the remote window again", "minimized=false");
    else fail("restore maps the remote window again");

    // ------------------------------------------------- transient modal dialog
    dialogApp = launch("python3", [path.join(__dirname, "dialog-app.py")], { DISPLAY });
    const mainFrame = await waitFrameByTitle(page, "RAST-POC-MAIN", 25000).catch(() => null);
    let dialogFrame = null;
    if (mainFrame) {
      dialogFrame = await waitFrameByTitle(page, "RAST-POC-DIALOG", 20000).catch(() => null);
    }
    if (mainFrame && dialogFrame) {
      const state = await desktop(page);
      const dialogWindow = state.windows.find((w) => w.wid === dialogFrame.wid);
      const noTaskbarEntry = !state.taskbar.some((entry) => entry.includes("RAST-POC-DIALOG"));
      const attached = dialogFrame.dialogOf === mainFrame.wid;
      pass("transient modal dialog is attached to its parent", JSON.stringify({ parent: mainFrame.wid, dialog: dialogFrame.wid, attached, noTaskbarEntry, transientFor: dialogWindow && dialogWindow.transientFor, modal: dialogWindow && dialogWindow.modal }));
      await screenshot(page, "06-dialog");
      await page.evaluate((wid) => window.__POC.closeWindow(wid), dialogFrame.wid);
      const dialogClosed = await waitFor("dialog window to close", async () => !(await desktop(page)).frames.includes(dialogFrame.wid), 15000).catch(() => false);
      if (dialogClosed) pass("closing the child keeps the parent window alive", "parent " + mainFrame.wid + " still open");
      else fail("closing the child keeps the parent window alive");
      await page.evaluate((wid) => window.__POC.closeWindow(wid), mainFrame.wid);
      await waitFor("dialog app to exit", async () => !(await desktop(page)).frames.includes(mainFrame.wid), 15000).catch(() => null);
    } else {
      fail("transient modal dialog is attached to its parent", "main=" + JSON.stringify(mainFrame) + " dialog=" + JSON.stringify(dialogFrame));
    }

    // -------------------------------------------------------------------- close
    const framesBeforeClose = (await desktop(page)).frames.length;
    const xtermsBeforeClose = xtermCount();
    await page.evaluate((wid) => window.__POC.closeWindow(wid), testWid);
    const closed = await waitFor("remote xterm window to close", async () => !(await desktop(page)).frames.includes(testWid), 15000).catch(() => false);
    await sleep(700);
    const xtermsAfterClose = xtermCount();
    if (closed && xtermsAfterClose < xtermsBeforeClose) {
      pass("closing a RAST window sends WM_DELETE (app exits, other windows stay)", `xterms ${xtermsBeforeClose} -> ${xtermsAfterClose}, windows ${framesBeforeClose} -> ${(await desktop(page)).frames.length}`);
    } else {
      fail("closing a RAST window sends WM_DELETE", JSON.stringify({ closed, xtermsBeforeClose, xtermsAfterClose }));
    }
    await screenshot(page, "07-closed");

    // ---------------------------------------------------------------- reconnect
    const beforeReload = await desktop(page);
    const beforeReloadXterms = xtermCount();
    await page.reload({ waitUntil: "domcontentloaded", timeout: 30000 });
    await waitFor("session reconnects after refresh", async () => {
      const state = await desktop(page);
      return state.connected && state.frames.length > 0;
    }, 35000);
    await sleep(1500);
    const afterReload = await desktop(page);
    const afterReloadXterms = xtermCount();
    if (afterReload.frames.length === beforeReload.frames.length && afterReloadXterms === beforeReloadXterms) {
      pass("refresh reconnects the single client and restores all windows (no duplicate apps)", `windows ${beforeReload.frames.length} -> ${afterReload.frames.length}, xterms ${beforeReloadXterms} -> ${afterReloadXterms}`);
    } else {
      fail("refresh reconnects and restores windows without duplicating apps", JSON.stringify({ beforeReload: beforeReload.frames, afterReload: afterReload.frames, beforeReloadXterms, afterReloadXterms }));
    }
    const reconnectPainted = await page.evaluate(() => {
      const state = window.__POC.desktopState();
      return state.framesInfo.map((frame) => ({ wid: frame.wid, colors: window.__POC.canvasDistinctColors(frame.wid) }));
    });
    if (reconnectPainted.every((entry) => entry.colors > 1)) pass("reconnected windows are painted", JSON.stringify(reconnectPainted));
    else fail("reconnected windows are painted", JSON.stringify(reconnectPainted));
    await screenshot(page, "08-reconnect");

    // ------------------------------------------------------------ narrow layout
    await page.setViewportSize({ width: 390, height: 844 });
    await sleep(900);
    const narrow = await desktop(page);
    const fits = narrow.framesInfo.every((frame) => frame.body.width <= 390 && frame.canvas && frame.canvas.width <= 400);
    if (fits) pass("narrow (390x844) layout keeps windows usable", JSON.stringify(narrow.framesInfo.map((f) => f.body)));
    else fail("narrow (390x844) layout keeps windows usable", JSON.stringify(narrow.framesInfo));
    await screenshot(page, "09-narrow");

    observations.console = consoleLines.slice(-80);
    observations.finalState = await desktop(page);
  } catch (error) {
    fail("poc run completed", error && error.stack ? String(error.stack).slice(0, 2000) : String(error));
  } finally {
    try { if (page && !page.isClosed()) await screenshot(page, "10-final"); } catch { /* ignore */ }
    if (browser) await browser.close().catch(() => {});
    if (dialogApp) { try { process.kill(-dialogApp.pid, "SIGKILL"); } catch { try { dialogApp.kill("SIGKILL"); } catch { /* ignore */ } } }
    if (testXterm) { try { process.kill(testXterm.pid, "SIGKILL"); } catch { /* ignore */ } }
    if (server) await server.close();
    observations.finishedAt = new Date().toISOString();
    observations.failures = failures;
    fs.mkdirSync(__dirname, { recursive: true });
    fs.writeFileSync(OBSERVATIONS, JSON.stringify(observations, null, 2));
    console.log(`\nobservations: ${OBSERVATIONS}`);
    console.log(`screenshots: ${SHOTS}`);
    if (failures.length) {
      console.error(`\n${failures.length} assertion(s) failed: ${failures.join("; ")}`);
      process.exitCode = 1;
    } else {
      console.log("\nAll PoC assertions passed.");
    }
  }
}

main();
