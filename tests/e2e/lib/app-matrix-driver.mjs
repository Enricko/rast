// Playwright side of the app matrix: a restricted API client, the desktop
// session, per-window input and screen helpers, and the three responsiveness
// measurements. The numbers are computed by the pure functions of
// app-matrix-core.mjs from the raw samples taken here.

import {
  assertAllowedApiCall,
  expandSteps,
  latenciesFromEvents,
  movesPerSecond,
  round1,
  stats,
} from "./app-matrix-core.mjs";
import { installProbe, readXpraInfo } from "./app-matrix-probe.mjs";
import * as browser from "./browser.mjs";

export const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

// ---------------------------------------------------------------------------
// API client restricted to the allow-list of the matrix
// ---------------------------------------------------------------------------
export class SafeApi {
  constructor(inner, { cleanup = false } = {}) {
    this.inner = inner;
    this.cleanup = cleanup;
  }

  get user() {
    return this.inner.user;
  }

  get cookies() {
    return this.inner.cookies;
  }

  get(path) {
    assertAllowedApiCall("GET", path, { cleanup: this.cleanup });
    return this.inner.get(path);
  }

  post(path, body) {
    assertAllowedApiCall("POST", path, { cleanup: this.cleanup });
    return this.inner.post(path, body);
  }

  async rawGet(path) {
    assertAllowedApiCall("GET", path, { cleanup: this.cleanup });
    return this.inner.raw("GET", path);
  }

  /** Binary download through the same session cookie. */
  async download(path) {
    assertAllowedApiCall("GET", path, { cleanup: this.cleanup });
    const response = await fetch(`${this.inner.baseUrl}${path}`, { headers: { Cookie: this.inner.cookieHeader() } });
    if (!response.ok) throw new Error(`GET ${path.split("?")[0]} -> HTTP ${response.status}`);
    return Buffer.from(await response.arrayBuffer());
  }
}

// ---------------------------------------------------------------------------
// Desktop session
// ---------------------------------------------------------------------------
export async function openMatrixDesktop(pwBrowser, api, credentials) {
  const session = await browser.newDesktopSession(pwBrowser, { api });
  await browser.gotoDesktop(session.page);
  if (!(await browser.isAuthenticated(session.page))) {
    await browser.loginViaForm(session.page, credentials.username, credentials.password);
  }
  await session.page.evaluate(installProbe);
  return session;
}

export async function readEnvironment(page) {
  return page.evaluate(() => ({
    devicePixelRatio: window.devicePixelRatio,
    viewport: `${window.innerWidth}x${window.innerHeight}`,
    userAgent: navigator.userAgent,
  }));
}

export async function readXpra(page) {
  for (const frame of page.frames()) {
    if (!frame.url().includes("/linux-apps/session/")) continue;
    try {
      return await frame.evaluate(readXpraInfo);
    } catch {
      return null;
    }
  }
  return null;
}

// ---------------------------------------------------------------------------
// Launcher and windows
// ---------------------------------------------------------------------------
/** Open the start menu, search, and return the tile (or null). Does not click it. */
export async function findLauncherTile(page, name) {
  const menu = await browser.openStartMenu(page);
  const search = menu.locator(".menu-search");
  if (await search.count()) await search.fill(name);
  await page.waitForTimeout(300);
  const tile = menu.locator(".menu-app", { hasText: name }).first();
  try {
    await tile.waitFor({ timeout: 8_000 });
  } catch {
    return null;
  }
  return tile;
}

export async function closeStartMenuQuietly(page) {
  await browser.closeStartMenu(page).catch(() => {});
}

const winSel = (id) => `.window[data-window-id="${id}"]`;

export async function bodyBox(page, windowId) {
  const body = page.locator(`${winSel(windowId)} .win-body`);
  await body.waitFor({ state: "visible", timeout: 30_000 });
  const box = await body.boundingBox();
  if (!box) throw new Error(`window ${windowId} has no visible body`);
  return box;
}

function clampClip(box, viewport) {
  const x = Math.max(0, box.x);
  const y = Math.max(0, box.y);
  return {
    x,
    y,
    width: Math.max(1, Math.min(box.width, viewport.width - x)),
    height: Math.max(1, Math.min(box.height, viewport.height - y)),
  };
}

/** PNG buffer of the window body (pixel comparison). */
export async function bodyBuffer(page, windowId) {
  const box = await bodyBox(page, windowId);
  const clip = clampClip(box, page.viewportSize() ?? { width: 1480, height: 920 });
  return page.screenshot({ clip, animations: "disabled" });
}

export async function clickBody(page, windowId) {
  const box = await bodyBox(page, windowId);
  await page.mouse.click(box.x + box.width / 2, box.y + box.height / 2);
  await page.waitForTimeout(250);
}

/** Poll the body until it differs from `baseline`. */
export async function waitBodyChange(page, windowId, baseline, { timeoutMs = 5000, intervalMs = 40 } = {}) {
  const started = Date.now();
  for (;;) {
    const current = await bodyBuffer(page, windowId);
    if (!current.equals(baseline)) return { changed: true, ms: Date.now() - started, bytes: current.length };
    if (Date.now() - started > timeoutMs) return { changed: false, ms: Date.now() - started, bytes: current.length };
    await sleep(intervalMs);
  }
}

export async function runSteps(page, windowId, steps, vars) {
  for (const step of expandSteps(steps, vars)) {
    switch (step.op) {
      case "click":
        await clickBody(page, windowId);
        break;
      case "key":
        await page.keyboard.press(step.keys);
        await sleep(120);
        break;
      case "type":
        await page.keyboard.type(step.text, { delay: step.delay ?? 30 });
        await sleep(150);
        break;
      case "lines":
        for (let i = 1; i <= step.count; i += 1) {
          await page.keyboard.type(step.text.replace(/\{i\}/g, String(i).padStart(5, "0")), { delay: 2 });
          await page.keyboard.press("Enter");
        }
        await sleep(300);
        break;
      case "wait":
        await sleep(step.ms);
        break;
      default:
        throw new Error(`unknown step op: ${step.op}`);
    }
  }
}

// ---------------------------------------------------------------------------
// Closing
// ---------------------------------------------------------------------------
export async function closeWindowByControl(page, windowId) {
  const control = page.locator(`${winSel(windowId)} .win-control-close`);
  if (!(await control.count())) throw new Error(`window ${windowId} has no close control`);
  await control.first().click();
}

export async function waitWindowDetached(page, windowId, timeoutMs) {
  try {
    await page.locator(winSel(windowId)).waitFor({ state: "detached", timeout: timeoutMs });
    return true;
  } catch {
    return false;
  }
}

// ---------------------------------------------------------------------------
// Responsiveness
// ---------------------------------------------------------------------------
const LETTERS = "abcdefghijklmnopqrstuvwxyz";

async function startProbe(page, windowId, canvas) {
  return page.evaluate(([id, useCanvas]) => window.__rastProbe.start(id, { canvas: useCanvas }), [windowId, canvas]);
}

const stopProbe = (page) => page.evaluate(() => window.__rastProbe.stop());

/** Typing: `keys` key presses 300 ms apart, keydown to first changed pixel. */
export async function measureTyping(page, windowId, { keys = 30 } = {}) {
  const started = await startProbe(page, windowId, true);
  const method = started.canvas ? "canvas" : "screenshot";
  const samples = [];
  let missed = 0;
  if (started.canvas) {
    await sleep(400);
    for (let i = 0; i < keys; i += 1) {
      await page.keyboard.press(LETTERS[i % LETTERS.length]);
      await sleep(300);
    }
    const data = await stopProbe(page);
    const result = latenciesFromEvents(data.keydowns, data.changes, 1000);
    samples.push(...result.samples);
    missed = result.missed;
  } else {
    await stopProbe(page);
    for (let i = 0; i < keys; i += 1) {
      const baseline = await bodyBuffer(page, windowId);
      const t0 = Date.now();
      await page.keyboard.press(LETTERS[i % LETTERS.length]);
      const change = await waitBodyChange(page, windowId, baseline, { timeoutMs: 1000, intervalMs: 20 });
      if (!change.changed) missed += 1;
      samples.push(Date.now() - t0);
      await sleep(200);
    }
  }
  return { method, keys, samples: samples.map(round1), missed, stats: stats(samples) };
}

/**
 * Scroll: (a) 12 single wheel ticks 700 ms apart, wheel to first changed
 * frame; (b) three bursts of 30 small ticks 16 ms apart, changed frames per
 * second. Direction alternates; `first` is the direction that has room.
 */
export async function measureScroll(page, windowId, { first = "down" } = {}) {
  const box = await bodyBox(page, windowId);
  await page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
  const sign = (n) => (((n % 2 === 0) === (first === "down")) ? 1 : -1);
  const started = await startProbe(page, windowId, true);
  const method = started.canvas ? "canvas" : "screenshot";
  const LATENCY_TICKS = 12;
  const BURSTS = 3;
  const BURST_TICKS = 30;
  if (!started.canvas) {
    await stopProbe(page);
    const samples = [];
    let missed = 0;
    for (let i = 0; i < LATENCY_TICKS; i += 1) {
      const baseline = await bodyBuffer(page, windowId);
      const t0 = Date.now();
      await page.mouse.wheel(0, 120 * sign(i));
      const change = await waitBodyChange(page, windowId, baseline, { timeoutMs: 1000, intervalMs: 20 });
      if (!change.changed) missed += 1;
      samples.push(Date.now() - t0);
      await sleep(500);
    }
    return { method, samples: samples.map(round1), missed, latency: stats(samples), fps: null, changedFrames: null };
  }
  await sleep(400);
  for (let i = 0; i < LATENCY_TICKS; i += 1) {
    await page.mouse.wheel(0, 120 * sign(i));
    await sleep(700);
  }
  for (let burst = 0; burst < BURSTS; burst += 1) {
    for (let i = 0; i < BURST_TICKS; i += 1) {
      await page.mouse.wheel(0, 40 * sign(burst));
      await sleep(16);
    }
    await sleep(400);
  }
  const data = await stopProbe(page);
  const latencyWheels = data.wheels.slice(0, LATENCY_TICKS);
  const burstWheels = data.wheels.slice(LATENCY_TICKS);
  const latency = latenciesFromEvents(latencyWheels, data.changes, 1000);
  // Frame rate: changed frames inside each burst, from its first tick to 150 ms
  // after its last, summed over the bursts.
  let frames = 0;
  let spanMs = 0;
  for (let burst = 0; burst < BURSTS; burst += 1) {
    const ticks = burstWheels.slice(burst * BURST_TICKS, (burst + 1) * BURST_TICKS);
    if (ticks.length < 2) continue;
    const from = ticks[0];
    const to = ticks[ticks.length - 1] + 150;
    frames += data.changes.filter((t) => t >= from && t <= to).length;
    spanMs += to - from;
  }
  const fps = spanMs > 0 ? round1((frames * 1000) / spanMs) : 0;
  return {
    method,
    samples: latency.samples.map(round1),
    missed: latency.missed,
    latency: stats(latency.samples),
    fps,
    changedFrames: frames,
    burstSpanMs: round1(spanMs),
  };
}

/** Drag the RAST title bar along a path; frame rate and repaint after the drop. */
export async function measureDrag(page, windowId) {
  const el = page.locator(winSel(windowId));
  if (await el.evaluate((node) => node.classList.contains("is-maximized") || node.classList.contains("maximized"))) {
    await el.locator(".win-control-maximize").click();
    await page.waitForTimeout(500);
  }
  const bar = el.locator(".win-titlebar").first();
  const box = await bar.boundingBox();
  if (!box) throw new Error(`window ${windowId} has no title bar to drag`);
  const viewport = page.viewportSize() ?? { width: 1480, height: 920 };
  const dx = box.x + box.width + 260 < viewport.width ? 240 : -240;
  const dy = box.y + box.height + 80 < viewport.height ? 70 : -70;
  const startX = box.x + Math.min(120, box.width * 0.3);
  const startY = box.y + box.height / 2;
  const started = await startProbe(page, windowId, true);
  const method = started.canvas ? "canvas" : "screenshot";
  await sleep(300);
  await page.mouse.move(startX, startY);
  await page.mouse.down();
  const STEPS = 90;
  for (let i = 1; i <= STEPS; i += 1) {
    const t = i / STEPS;
    const eased = t * t * (3 - 2 * t);
    const tick = Date.now();
    await page.mouse.move(startX + dx * eased, startY + dy * eased);
    const spent = Date.now() - tick;
    if (spent < 16) await sleep(16 - spent);
  }
  await page.mouse.up();
  await sleep(1200);
  const data = await stopProbe(page);
  const moves = movesPerSecond(data.rects);
  const drop = data.pointerups.length > 0 ? data.pointerups[data.pointerups.length - 1] : null;
  let repaint = "unmeasured";
  let repaintMs = null;
  if (data.canvas && drop !== null) {
    const hit = data.changes.find((t) => t >= drop && t < drop + 1000);
    if (hit === undefined) {
      repaint = "none-needed";
    } else {
      repaint = "repainted";
      repaintMs = round1(hit - drop);
    }
  }
  return {
    method,
    fps: moves.fps,
    movedFrames: moves.frames,
    durationMs: moves.durationMs,
    repaint,
    repaintMs,
    // Frame count of the whole page, to tell a slow drag from a slow page.
    pageFrames: data.frames,
    path: { dx, dy, steps: STEPS },
  };
}
