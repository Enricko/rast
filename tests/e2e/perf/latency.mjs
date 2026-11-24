#!/usr/bin/env node
// Stage-by-stage input/clipboard latency profile for RAST native apps.
//
// Task context (docs/BENCHMARKS.md §8): the end-to-end input→display latency
// measured by `bench.mjs` is an upper bound limited by screenshot polling. This
// script decomposes the path into measured stages on the real production
// deployment (public HTTPS URL, Cloudflare tunnel → nginx → RAST → Xpra):
//
//   1. input event → RAST handler       Event Timing API (input delay,
//                                       processing) + a runtime wrapper around
//                                       the shim's `sendKey` (monkey-patched in
//                                       the page only; product code untouched)
//   2. RAST handler → WebSocket frame   CDP Network.webSocketFrameSent times
//   3. transport RTT / server damage    `xpra info` of the test identity's own
//                                       session: client ping latency p50/p90,
//                                       damage client/frame-total/target latency,
//                                       batch config and adaptive delay
//   4. hidden-viewer rAF vs main rAF    rAF counters in the desktop document and
//                                       in the off-screen same-origin Xpra
//                                       viewer iframe (idle vs during typing) —
//                                       tests whether the off-screen viewer is
//                                       render-throttled (suspected latency
//                                       contributor)
//   5. application acknowledgement      keypress → `document.title` change of
//                                       the controlled remote page, mirrored into
//                                       the adopted RAST window title (a full
//                                       keypress→application→client round trip,
//                                       no screenshot polling)
//   6. tunnel round-trip                keep-alive HTTP RTT to the loopback API
//                                       (http://127.0.0.1:8720/healthz) vs the
//                                       public HTTPS URL (/healthz), plus curl
//                                       DNS/TCP/TLS/TTFB breakdown and the
//                                       inferred tunnel overhead
//
// The estimated floor is derived from the measured stages with stated
// assumptions (one-way transport ≈ ping/2 in each direction, server damage
// pipeline from `xpra info`, client paint ≈ one viewer frame); it is an
// estimate, not a measurement of an individual keystroke.
//
// Usage (from tests/e2e):
//   node perf/latency.mjs                        # production, t-alice
//   node perf/latency.mjs --keys=30 --idle=5
//   node perf/latency.mjs --keep-session         # leave the test session up
//
// Options: --url, --creds, --user, --admin, --keys, --idle, --keep-session,
//          --headless=false, --timeout-scale.
//
// Safety: only the test account's own session/windows are used; the live user
// session (rast-xpra-u1) is never touched and no service/unit is started or
// stopped. Results: tests/e2e/results/perf-input-latency-<date>.{json,md} and
// -evidence.txt. The session is ended through the RAST API unless
// --keep-session is given.

import { execFile } from "node:child_process";
import { join } from "node:path";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);

import {
  DEFAULT_BASE_URL,
  DEFAULT_CREDS,
  HERE,
  REPO,
  calibratePage,
  clickRemotePage,
  detectControlledPage,
  discoverIdentity,
  ensureDir,
  formLogin,
  fmtMs,
  launchApp,
  launchBrowser,
  loopbackApi,
  newClientContext,
  installTitleObserver,
  pageNow,
  parseArgs,
  parseInfo,
  readSecrets,
  resolveCredentials,
  screenshot,
  sha8,
  sleep,
  startPageServer,
  stats,
  titleLog,
  waitFor,
  waitForAdoptedWindows,
  waitForWindowTitle,
  windowBox,
  windowsInfo,
  writeJson,
  writeText,
} from "../clipboard/lib.mjs";

/* ------------------------------------------------------------------------ */
/* arguments                                                                */
/* ------------------------------------------------------------------------ */

const args = parseArgs(process.argv.slice(2), {
  url: DEFAULT_BASE_URL,
  creds: DEFAULT_CREDS,
  user: "t-alice",
  admin: "t-admin",
  keys: "24",
  idle: "5",
  "keep-session": false,
  headless: true,
});
const BASE_URL = String(args.url).replace(/\/$/, "");
const KEYS = Math.max(6, Number(args.keys) || 24);
const IDLE_SECONDS = Math.max(2, Number(args.idle) || 5);
const DATE = new Date().toISOString().slice(0, 10);
const STAMP = new Date().toISOString().replace(/[:.]/g, "-").replace("Z", "");
const RESULTS_DIR = join(REPO, "tests/e2e/results");
const OUT_PREFIX = `perf-input-latency-${DATE}`;
const JSON_PATH = join(RESULTS_DIR, `${OUT_PREFIX}.json`);
const EVIDENCE_PATH = join(RESULTS_DIR, `${OUT_PREFIX}-evidence.txt`);
const SHOTS_DIR = join(RESULTS_DIR, `${OUT_PREFIX}-screenshots`);
ensureDir(RESULTS_DIR);
ensureDir(SHOTS_DIR);

const sleepMs = sleep;
const pct = (values, fraction) => stats(values)[fraction === 0.5 ? "p50" : "p95"];

/* ------------------------------------------------------------------------ */
/* small async host helper (execFile, so the browser keeps running while a   */
/* `xpra info` snapshot is taken mid-interaction)                            */
/* ------------------------------------------------------------------------ */

async function sudoAsync(argv, { timeoutMs = 30_000 } = {}) {
  try {
    const { stdout } = await execFileAsync("sudo", ["-n", ...argv], {
      encoding: "utf8",
      timeout: timeoutMs,
      maxBuffer: 16 * 1024 * 1024,
    });
    return { ok: true, stdout };
  } catch (error) {
    return { ok: false, stdout: error.stdout ?? "", stderr: error.stderr ?? String(error) };
  }
}

async function xpraInfoAsync(identity) {
  if (!identity?.available) return { ok: false, error: "identity/display not known yet" };
  const result = await sudoAsync([
    "-u",
    identity.username,
    "env",
    `XDG_RUNTIME_DIR=${identity.runtimeDir}`,
    `HOME=${identity.home}`,
    "xpra",
    "info",
    `--socket-dir=${identity.socketDir}`,
    `:${identity.display}`,
  ]);
  if (!result.ok) return { ok: false, error: result.stderr?.slice(0, 300) };
  return { ok: true, text: result.stdout, map: parseInfo(result.stdout) };
}

function latencyFromXpra(map) {
  const num = (key) => {
    const value = Number(map?.[key]);
    return Number.isFinite(value) ? value : null;
  };
  return {
    pingP50: num("client.0.connection.client.ping_latency.50p"),
    pingP90: num("client.0.connection.client.ping_latency.90p"),
    pingAvg: num("client.0.connection.client.ping_latency.avg"),
    pingMax: num("client.0.connection.client.ping_latency.max"),
    damageClientLatency: num("client.0.damage.client-latency"),
    damageFrameTotalLatency: num("client.0.damage.frame-total-latency"),
    damageTargetLatency: num("client.0.damage.target-latency"),
    damageFps: num("client.0.damage.fps"),
    batchDelayAvg: num("client.0.batch.delay.avg"),
    batchDelayMax: num("client.0.batch.delay.max"),
    batchDelayCur: num("client.0.batch.delay.cur"),
    batchMinDelay: num("client.0.batch.min-delay"),
    batchMaxDelay: num("client.0.batch.max-delay"),
    batchExpire: num("client.0.batch.expire"),
    compressor: map?.["client.0.connection.compressor"] ?? null,
    windowDamage: Object.fromEntries(
      Object.entries(map ?? {})
        .filter(([key]) => /^client\.0\.window\.\d+\.damage\.(target-latency|in_latency\.(avg|90p)|fps|events)$/.test(key))
        .map(([key, value]) => [key, num(key) ?? value]),
    ),
    windowEncoder: Object.fromEntries(
      Object.entries(map ?? {})
        .filter(([key]) => /^client\.0\.window\.\d+\.encoding\.pipeline_option\.0\.encoder$/.test(key))
        .map(([key, value]) => [key, value]),
    ),
    windowBatch: Object.fromEntries(
      Object.entries(map ?? {})
        .filter(([key]) => /^client\.0\.window\.\d+\.batch\.(delay\.(avg|max|cur)|actual_delays\.(avg|max|cur)|client-latency\.(avg|target))$/.test(key))
        .map(([key, value]) => [key, num(key) ?? value]),
    ),
  };
}

/* ------------------------------------------------------------------------ */
/* tunnel round-trip measurement                                            */
/* ------------------------------------------------------------------------ */

async function httpRtt(url, samples = 40) {
  const values = [];
  // Warm-up: one request so DNS/TLS setup does not dominate every sample (the
  // keep-alive pool is reused by Node's fetch implementation).
  await fetch(url).then((response) => response.arrayBuffer()).catch(() => undefined);
  for (let index = 0; index < samples; index += 1) {
    const started = performance.now();
    try {
      const response = await fetch(url, { keepalive: true });
      await response.arrayBuffer();
      if (!response.ok) continue;
      values.push(performance.now() - started);
    } catch {
      /* skip failed sample */
    }
    await sleepMs(10);
  }
  return values;
}

async function curlTimings(url, samples = 6) {
  const format = "%{time_namelookup}|%{time_connect}|%{time_appconnect}|%{time_starttransfer}|%{time_total}|%{http_code}";
  const rows = [];
  for (let index = 0; index < samples; index += 1) {
    const started = Date.now();
    const result = await sudoAsync(
      ["-u", "lufria", "curl", "-sS", "-o", "/dev/null", "-w", format, "--max-time", "20", url],
      { timeoutMs: 25_000 },
    ).catch(() => null);
    // sudo -u lufria is only used to keep a clean environment; fall back to a
    // direct curl if sudo has no rule for it.
    const text = result?.ok ? result.stdout.trim() : "";
    const direct = text ? null : await execFileAsync("curl", ["-sS", "-o", "/dev/null", "-w", format, "--max-time", "20", url]).then(
      (value) => value.stdout.trim(),
      () => "",
    );
    const line = text || direct || "";
    const [dns, connect, tls, ttfb, total, code] = line.split("|").map((value) => Number(value));
    if (!Number.isFinite(total)) continue;
    rows.push({ dns, connect, tls, ttfb, total, code, wallMs: Date.now() - started });
    await sleepMs(40);
  }
  return rows;
}

/* ------------------------------------------------------------------------ */
/* browser instrumentation                                                  */
/* ------------------------------------------------------------------------ */

/**
 * Install all page-side probes (diagnostic only; no product code is changed):
 *  - Event Timing entries for keydown (input delay / handler processing)
 *  - a wrapper around the shim's `sendKey` (the RAST handler → shim boundary)
 *  - rAF counters in the desktop document and in the off-screen viewer iframe
 *  - a paste-event counter (proves whether a trusted paste event ever exists)
 */
async function installProbes(page) {
  return page.evaluate(() => {
    if (window.__latencyProbe) return window.__latencyProbe.ready;
    const probe = {
      ready: true,
      events: [],
      rawKeydowns: [],
      sendKey: [],
      pasteEvents: 0,
      raf: { running: false, main: 0, viewer: 0, startedAt: 0 },
    };
    window.__latencyProbe = probe;
    try {
      const observer = new PerformanceObserver((list) => {
        for (const entry of list.getEntries()) {
          if (entry.name !== "keydown" && entry.name !== "paste" && entry.name !== "input") continue;
          probe.events.push({
            name: entry.name,
            startTime: entry.startTime,
            processingStart: entry.processingStart ?? null,
            processingEnd: entry.processingEnd ?? null,
            duration: entry.duration,
            eventCount: entry.target ?? null,
          });
        }
      });
      observer.observe({ type: "event", buffered: true, durationThreshold: 0 });
      window.__latencyObserver = observer;
    } catch (error) {
      probe.eventTimingError = String(error);
    }
    document.addEventListener(
      "paste",
      () => {
        probe.pasteEvents += 1;
      },
      true,
    );
    // Raw capture listener: gives N samples even when the Event Timing API
    // reports only a subset; `timeStamp` is the browser's own event creation
    // time on the same performance.now() clock.
    document.addEventListener(
      "keydown",
      (event) => {
        probe.rawKeydowns.push({ timeStamp: event.timeStamp, at: performance.now(), key: event.key });
        if (probe.rawKeydowns.length > 500) probe.rawKeydowns.splice(0, probe.rawKeydowns.length - 500);
      },
      true,
    );
    const frame = document.querySelector("iframe.linux-native-session-frame");
    const shim = frame?.contentWindow?.RASTNative;
    if (shim && typeof shim.sendKey === "function" && !shim.__latencyWrapped) {
      const original = shim.sendKey.bind(shim);
      shim.sendKey = function (pressed, init) {
        if (pressed === true) {
          probe.sendKey.push({
            at: performance.now(),
            key: init?.key ?? null,
            code: init?.code ?? null,
            ctrlKey: Boolean(init?.ctrlKey),
            metaKey: Boolean(init?.metaKey),
          });
          if (probe.sendKey.length > 500) probe.sendKey.splice(0, probe.sendKey.length - 500);
        }
        return original(pressed, init);
      };
      shim.__latencyWrapped = true;
    }
    const startRaf = () => {
      probe.raf.running = true;
      probe.raf.main = 0;
      probe.raf.viewer = 0;
      probe.raf.startedAt = performance.now();
      const mainTick = () => {
        if (!probe.raf.running) return;
        probe.raf.main += 1;
        requestAnimationFrame(mainTick);
      };
      requestAnimationFrame(mainTick);
      const viewerWindow = frame?.contentWindow;
      if (viewerWindow) {
        const viewerTick = () => {
          if (!probe.raf.running) return;
          probe.raf.viewer += 1;
          viewerWindow.requestAnimationFrame(viewerTick);
        };
        viewerWindow.requestAnimationFrame(viewerTick);
        probe.raf.viewerAvailable = true;
      } else {
        probe.raf.viewerAvailable = false;
      }
    };
    probe.startRaf = startRaf;
    startRaf();
    return true;
  });
}

async function rafWindow(page, ms) {
  const before = await page.evaluate(() => ({ ...window.__latencyProbe.raf }));
  await sleepMs(ms);
  const after = await page.evaluate(() => ({ ...window.__latencyProbe.raf }));
  const elapsed = after.startedAt ? (Date.now() - Date.now()) : 0;
  // startedAt is a page performance.now value; use the counters only.
  return {
    mainFps: (after.main - before.main) / (ms / 1000),
    viewerFps: (after.viewer - before.viewer) / (ms / 1000),
    mainFrames: after.main - before.main,
    viewerFrames: after.viewer - before.viewer,
    viewerAvailable: after.viewerAvailable !== false,
    elapsedMs: ms,
  };
}

async function probeState(page) {
  return page.evaluate(() => {
    const probe = window.__latencyProbe;
    if (!probe) return null;
    return {
      events: probe.events.slice(),
      rawKeydowns: probe.rawKeydowns.slice(),
      sendKey: probe.sendKey.slice(),
      pasteEvents: probe.pasteEvents,
      eventTimingError: probe.eventTimingError ?? null,
    };
  });
}

async function resetProbes(page) {
  await page.evaluate(() => {
    const probe = window.__latencyProbe;
    if (!probe) return;
    probe.events = [];
    probe.rawKeydowns = [];
    probe.sendKey = [];
    probe.pasteEvents = 0;
  });
}

/**
 * Wait until the title observer records a *fresh* title (attributed after
 * `before`) with the given prefix. Robust against a non-empty fixture field:
 * the app response is the title change itself, not a guessed exact value.
 */
async function waitForFreshTitle(page, windowId, before, prefix, timeoutMs = 6_000) {
  const started = Date.now();
  for (;;) {
    const entry = await titleLog(page, windowId);
    if (entry && Number.isFinite(entry.at) && entry.at >= before && String(entry.title ?? "").startsWith(prefix)) {
      return { ok: true, title: entry.title, at: entry.at };
    }
    if (Date.now() - started > timeoutMs) return { ok: false, title: entry?.title ?? "", at: entry?.at ?? null };
    await sleepMs(15);
  }
}

async function focusProbeSafe() {
  try {
    return await page.evaluate(() => {
      const element = document.activeElement;
      return {
        activeElement: element
          ? {
              tag: element.tagName,
              className: typeof element.className === "string" ? element.className : "",
            }
          : null,
      };
    });
  } catch {
    return { activeElement: null };
  }
}

/* ------------------------------------------------------------------------ */
/* navigation to the controlled page                                         */
/* ------------------------------------------------------------------------ */

async function navigateControlled(page, windowId, url) {
  const box = await windowBox(page, windowId);
  if (!box) throw new Error("adopted window disappeared");
  for (const attempt of [
    { name: "Alt+D", focus: () => page.keyboard.press("Alt+KeyD") },
    { name: "omnibox pointer", focus: () => page.mouse.click(box.x + box.width * 0.45, box.y + 72) },
    { name: "Ctrl+K", focus: () => page.keyboard.press("Control+KeyK") },
  ]) {
    for (let round = 1; round <= 2; round += 1) {
      await page.keyboard.press("Escape").catch(() => undefined);
      await page.mouse.click(box.x + box.width / 2, box.y + box.height - 60).catch(() => undefined);
      await sleepMs(250);
      await attempt.focus();
      await sleepMs(350);
      await page.keyboard.type(url, { delay: 8 });
      await sleepMs(120);
      await page.keyboard.press("Enter");
      const started = Date.now();
      while (Date.now() - started < 20_000) {
        const detection = await detectControlledPage(page, windowId).catch(() => null);
        if (detection?.ok) return { ok: true, method: attempt.name, layout: detection.layout };
        await sleepMs(250);
      }
    }
  }
  return { ok: false };
}

/* ------------------------------------------------------------------------ */
/* main                                                                     */
/* ------------------------------------------------------------------------ */

const credentials = resolveCredentials(readSecrets(String(args.creds)), args.user, args.admin);
if (!credentials.user.password) throw new Error(`no password for ${args.user} in ${args.creds}`);
const server = await startPageServer({
  port: Number(process.env.RAST_LATENCY_PORT || 8797),
  routes: { "/clipboard-page.html": join(HERE, "clipboard-page.html") },
});
const PAGE_URL = `${server.url("/clipboard-page.html")}?run=${STAMP}&role=A`;

const results = {
  label: `input-latency-${DATE}`,
  startedAt: new Date().toISOString(),
  finishedAt: null,
  target: BASE_URL,
  pageUrl: PAGE_URL,
  user: args.user,
  keys: KEYS,
  environment: {},
  idle: null,
  typing: null,
  appResponse: null,
  raf: null,
  xpra: { idle: null, midTyping: null, after: null },
  tunnel: null,
  estimatedFloor: null,
  observations: [],
  limitations: [],
};

function observation(id, detail) {
  results.observations.push({ id, detail: String(detail) });
  console.log(`  · ${id}: ${detail}`);
}

let browser = null;
let context = null;
let page = null;
let api = null;
let identity = null;
let windowId = null;
let keepSession = args["keep-session"] === true;

try {
  browser = await launchBrowser({ headless: args.headless !== false });
  const client = await newClientContext(browser, { baseUrl: BASE_URL, mac: false });
  context = client.context;
  page = client.page;
  // CDP WebSocket frame timestamps (the local client ↔ RAST server leg).
  const cdp = await context.newCDPSession(page);
  await cdp.send("Network.enable");
  const wsFrames = { sent: [], received: [] };
  page.on("websocket", (websocket) => {
    websocket.on("framesent", (event) => {
      wsFrames.sent.push({ at: Date.now(), length: String(event.payload ?? "").length });
      if (wsFrames.sent.length > 5000) wsFrames.sent.splice(0, wsFrames.sent.length - 5000);
    });
    websocket.on("framereceived", (event) => {
      wsFrames.received.push({ at: Date.now(), length: String(event.payload ?? "").length });
      if (wsFrames.received.length > 20000) wsFrames.received.splice(0, wsFrames.received.length - 20000);
    });
  });
  cdp.on("Network.webSocketFrameSent", (event) => {
    if (event.response?.opcode === 1 || event.response?.opcode === 2) {
      wsFrames.sent.push({ at: Date.now(), length: (event.response.payloadData ?? "").length });
      if (wsFrames.sent.length > 5000) wsFrames.sent.splice(0, wsFrames.sent.length - 5000);
    }
  });
  cdp.on("Network.webSocketFrameReceived", (event) => {
    if (event.response?.opcode === 1 || event.response?.opcode === 2) {
      wsFrames.received.push({ at: Date.now(), length: (event.response.payloadData ?? "").length });
      if (wsFrames.received.length > 20000) wsFrames.received.splice(0, wsFrames.received.length - 20000);
    }
  });
  results.environment.wsFrameLog = { sent: wsFrames.sent.length, received: wsFrames.received.length };

  await formLogin(page, { baseUrl: BASE_URL, credentials: credentials.user });
  await installTitleObserver(page);
  console.log(`signed in as ${args.user}; target ${BASE_URL}`);
  api = await loopbackApi(credentials.user);
  identity = await discoverIdentity(api);
  results.environment.deployed = await (async () => {
    try {
      const admin = await loopbackApi(credentials.admin);
      for (const path of ["/api/system/info", "/api/about"]) {
        try {
          const info = await admin.get(path);
          await admin.logout().catch(() => undefined);
          return { source: path, info };
        } catch {
          /* next */
        }
      }
      await admin.logout().catch(() => undefined);
    } catch (error) {
      return { error: String(error.message ?? error) };
    }
    return null;
  })();
  results.environment.host = {
    release: (await execFileAsync("/usr/local/bin/rast", ["version"], { encoding: "utf8" }).catch(() => ({ stdout: "" }))).stdout.split("\n")[0] || null,
    xpraVersion: (await execFileAsync("xpra", ["--version"], { encoding: "utf8" })).stdout.split("\n")[0] || null,
    chromeVersion: (await execFileAsync("google-chrome-stable", ["--version"], { encoding: "utf8" })).stdout.trim(),
    chromiumExecutable: (await import("playwright")).chromium.executablePath?.() ?? null,
    cpus: (await execFileAsync("nproc", [], { encoding: "utf8" })).stdout.trim(),
    loadAverageStart: (await execFileAsync("cat", ["/proc/loadavg"], { encoding: "utf8" })).stdout.trim(),
  };

  // ---- launch/reuse the test user's Chrome ------------------------------
  let adopted = [];
  try {
    adopted = await waitForAdoptedWindows(page, { min: 1, timeoutMs: 10_000 });
  } catch {
    adopted = [];
  }
  if (adopted.length === 0) {
    const clickAt = Date.now();
    await launchApp(page);
    adopted = await waitForAdoptedWindows(page, { min: 1, timeoutMs: 300_000 });
    results.environment.launchMs = Date.now() - clickAt;
  }
  const target = adopted[adopted.length - 1];
  windowId = target.id;
  identity = await discoverIdentity(api);
  results.environment.session = identity.session;
  console.log(`using adopted window ${windowId} (session display :${identity.display})`);

  const navigation = await navigateControlled(page, windowId, PAGE_URL);
  if (!navigation.ok) throw new Error("could not load the controlled page in the remote Chrome");
  results.environment.navigation = navigation.method;
  await installProbes(page);
  await installTitleObserver(page);
  let layout = navigation.layout;
  const ensureLayout = async () => {
    if (!layout) layout = await calibratePage(page, windowId);
    return layout;
  };
  await ensureLayout();
  console.log(`controlled page loaded via ${navigation.method}`);

  // ---- 1. idle: hidden-viewer vs main-document rAF ----------------------
  await clickRemotePage(page, layout, {});
  await sleepMs(1_000);
  const idleRaf = await rafWindow(page, IDLE_SECONDS * 1000);
  results.raf = { idle: idleRaf };
  // xpra info during idle
  results.xpra.idle = latencyFromXpra((await xpraInfoAsync(identity)).map ?? null);
  console.log(
    `idle rAF: main=${idleRaf.mainFps.toFixed(1)} fps viewer=${idleRaf.viewerFps.toFixed(1)} fps (viewer available=${idleRaf.viewerAvailable})`,
  );

  // ---- 2. typing burst with stage probes --------------------------------
  // Focus handshake: the session may have been left warm by another run with a
  // second restored window behind the fixture, and a forwarded key that lands
  // in the wrong browser window silently produces no title change. Probe the
  // title channel with one disposable key and re-focus/re-point the click until
  // the fixture provably receives input.
  let focusOk = false;
  const handshake = [];
  for (const point of [{}, { fy: 0.75 }, { fy: 0.25, fx: 0.25 }, { fy: 0.75, fx: 0.75 }]) {
    await clickRemotePage(page, layout, point).catch(() => undefined);
    await sleepMs(300);
    const probe = await focusProbeSafe();
    await resetProbes(page);
    await page.keyboard.press("Control+KeyA");
    await page.keyboard.press("Backspace");
    await sleepMs(250);
    const handshakeBefore = await pageNow(page);
    await page.keyboard.press("KeyQ");
    const watch = await waitForFreshTitle(page, windowId, handshakeBefore, "INPUT:", 3_000);
    handshake.push({ point, title: watch.title, ok: watch.ok, active: probe.activeElement?.className ?? null });
    if (handshake[handshake.length - 1].ok) {
      focusOk = true;
      break;
    }
  }
  results.focusHandshake = handshake;
  if (!focusOk) {
    observation("focus-handshake", "the fixture did not receive a probe key in any attempt; latency samples would be meaningless");
    throw new Error("focus handshake failed: forwarded keys do not reach the controlled remote page (see focusHandshake in results)");
  }
  // Clear the field so each key appends one character deterministically.
  await clickRemotePage(page, layout, {});
  await page.keyboard.press("Control+KeyA");
  await page.keyboard.press("Backspace");
  await sleepMs(400);
  await resetProbes(page);

  const keystrokes = [];
  const rafBefore = await page.evaluate(() => ({ ...window.__latencyProbe.raf }));
  const midTypingSnapshot = xpraInfoAsync(identity); // started before typing; awaited after
  const typingStartedAt = Date.now();
  for (let index = 0; index < KEYS; index += 1) {
    const before = await pageNow(page);
    const nodeStart = Date.now();
    await page.keyboard.press("KeyA");
    // app acknowledgement: the remote page reports a fresh INPUT:<value> title
    const watch = await waitForFreshTitle(page, windowId, before, "INPUT:", 6_000);
    keystrokes.push({
      index,
      beforePageNow: before,
      nodeStart,
      title: watch.title,
      titleAt: watch.at,
      appResponseMs: watch.ok ? watch.at - before : null,
      nodeResponseMs: Date.now() - nodeStart,
    });
    if (!watch.ok) {
      observation("typing.app-response-miss", `keystroke ${index} produced no fresh INPUT title; stopping the burst to avoid a meaningless measurement`);
      break;
    }
    await sleepMs(90);
  }
  const typingDurationMs = Date.now() - typingStartedAt;
  const rafAfter = await page.evaluate(() => ({ ...window.__latencyProbe.raf }));
  const typingRaf = {
    mainFps: ((rafAfter.main - rafBefore.main) / typingDurationMs) * 1000,
    viewerFps: ((rafAfter.viewer - rafBefore.viewer) / typingDurationMs) * 1000,
    mainFrames: rafAfter.main - rafBefore.main,
    viewerFrames: rafAfter.viewer - rafBefore.viewer,
    viewerAvailable: rafAfter.viewerAvailable !== false,
    elapsedMs: typingDurationMs,
  };
  results.raf.typing = typingRaf;
  const probes = (await probeState(page)) ?? { events: [], sendKey: [], pasteEvents: 0 };
  const midTyping = await midTypingSnapshot;
  results.xpra.midTyping = latencyFromXpra(midTyping.map ?? null);
  await sleepMs(400);
  const afterInfo = await xpraInfoAsync(identity);
  results.xpra.after = latencyFromXpra(afterInfo.map ?? null);

  // ---- 3. correlate stages ---------------------------------------------
  const eventTiming = probes.events.filter((entry) => entry.name === "keydown");
  const inputDelay = eventTiming.filter((entry) => entry.processingStart != null).map((entry) => entry.processingStart - entry.startTime);
  const handlerProcessing = eventTiming
    .filter((entry) => entry.processingStart != null && entry.processingEnd != null)
    .map((entry) => entry.processingEnd - entry.processingStart);
  const sendKeyToEvent = [];
  const sendKeyTimes = probes.sendKey.map((entry) => entry.at);
  for (const entry of eventTiming) {
    let best = null;
    for (const value of sendKeyTimes) {
      if (value >= entry.startTime && (best === null || value < best)) best = value;
    }
    if (best !== null) sendKeyToEvent.push(best - entry.startTime);
  }
  const rawKeydowns = probes.rawKeydowns ?? [];
  const handlerFromRaw = [];
  const dispatchFromRaw = [];
  const used = new Set();
  for (const send of probes.sendKey) {
    let bestIndex = -1;
    for (let index = 0; index < rawKeydowns.length; index += 1) {
      if (used.has(index)) continue;
      if (rawKeydowns[index].timeStamp > send.at) continue;
      if (bestIndex < 0 || rawKeydowns[index].timeStamp > rawKeydowns[bestIndex].timeStamp) bestIndex = index;
    }
    if (bestIndex < 0) continue;
    used.add(bestIndex);
    handlerFromRaw.push(send.at - rawKeydowns[bestIndex].timeStamp);
    dispatchFromRaw.push(rawKeydowns[bestIndex].at - rawKeydowns[bestIndex].timeStamp);
  }
  const appResponse = keystrokes.map((entry) => entry.appResponseMs).filter((value) => Number.isFinite(value));
  // keypress (Node) → first outgoing WebSocket frame after it
  const sendLatency = [];
  const roundTripLatency = [];
  for (const stroke of keystrokes) {
    const sent = wsFrames.sent.find((frame) => frame.at >= stroke.nodeStart);
    if (sent) {
      sendLatency.push(sent.at - stroke.nodeStart);
      const received = wsFrames.received.find((frame) => frame.at >= sent.at);
      if (received) roundTripLatency.push(received.at - stroke.nodeStart);
    }
  }
  results.typing = {
    keys: KEYS,
    durationMs: typingDurationMs,
    eventTiming: {
      samples: eventTiming.length,
      inputDelay: stats(inputDelay),
      handlerProcessing: stats(handlerProcessing),
      eventToSendKey: stats(sendKeyToEvent),
      rawKeydownSamples: rawKeydowns.length,
      dispatchToCaptureListener: stats(dispatchFromRaw),
      eventToSendKeyRaw: stats(handlerFromRaw),
    },
    sendKeyCalls: probes.sendKey.length,
    pasteEventsSeen: probes.pasteEvents,
    appResponse: stats(appResponse),
    nodeKeypressToWsSent: stats(sendLatency),
    nodeKeypressToWsReceived: stats(roundTripLatency),
    perKey: keystrokes.map((entry) => ({ i: entry.index, appResponseMs: entry.appResponseMs, nodeResponseMs: entry.nodeResponseMs })),
    wsFrames: { sentTotal: wsFrames.sent.length, receivedTotal: wsFrames.received.length },
    eventTimingError: probes.eventTimingError ?? null,
  };
  console.log(`typing: inputDelay p50=${fmtMs(results.typing.eventTiming.inputDelay.p50)} ms, handler p50=${fmtMs(results.typing.eventTiming.handlerProcessing.p50)} ms`);
  console.log(`typing: event→sendKey p50=${fmtMs(results.typing.eventTiming.eventToSendKey.p50)} ms, keypress→WS-sent p50=${fmtMs(sendLatency.length ? stats(sendLatency).p50 : null)} ms`);
  console.log(`typing: key→app title p50=${fmtMs(results.typing.appResponse.p50)} ms p95=${fmtMs(results.typing.appResponse.p95)} ms`);
  console.log(`typing rAF: main=${typingRaf.mainFps.toFixed(1)} fps viewer=${typingRaf.viewerFps.toFixed(1)} fps`);

  // ---- 4. tunnel RTT: loopback vs public -------------------------------
  console.log("measuring HTTP RTT (loopback vs public)...");
  const loopbackSamples = await httpRtt("http://127.0.0.1:8720/healthz", 40);
  const publicSamples = await httpRtt(`${BASE_URL}/healthz`, 40);
  const loopbackCurl = await curlTimings("http://127.0.0.1:8720/healthz", 6);
  const publicCurl = await curlTimings(`${BASE_URL}/healthz`, 6);
  let cfRay = null;
  try {
    const response = await fetch(`${BASE_URL}/healthz`);
    cfRay = response.headers.get("cf-ray") ?? null;
    results.tunnelResponseHeaders = {
      "cf-ray": cfRay,
      server: response.headers.get("server") ?? null,
      "cf-cache-status": response.headers.get("cf-cache-status") ?? null,
    };
  } catch {
    cfRay = null;
  }
  const loopbackStats = stats(loopbackSamples);
  const publicStats = stats(publicSamples);
  const tunnelOverheadMs =
    loopbackStats.p50 != null && publicStats.p50 != null ? publicStats.p50 - loopbackStats.p50 : null;
  results.tunnel = {
    loopback: { url: "http://127.0.0.1:8720/healthz", samples: loopbackStats, curl: loopbackCurl },
    public: { url: `${BASE_URL}/healthz`, samples: publicStats, curl: publicCurl },
    inferredOverheadMs: tunnelOverheadMs,
    cfRay,
    note: "RTT measured from the same host that runs the RAST server: the public leg includes the host→Cloudflare edge→tunnel→origin path; it does not include any end user's last mile.",
  };
  console.log(
    `HTTP RTT p50: loopback=${fmtMs(loopbackStats.p50)} ms public=${fmtMs(publicStats.p50)} ms → tunnel overhead ≈ ${fmtMs(tunnelOverheadMs)} ms (cf-ray ${cfRay ?? "n/a"})`,
  );

  // ---- 5. estimated floor ---------------------------------------------
  const ping = results.xpra.midTyping?.pingP50 ?? results.xpra.after?.pingP50 ?? results.xpra.idle?.pingP50 ?? null;
  const damageFrame = results.xpra.midTyping?.damageFrameTotalLatency ?? results.xpra.after?.damageFrameTotalLatency ?? null;
  const handlerMs = results.typing.eventTiming.eventToSendKey.p50;
  const wsSendMs = stats(sendLatency).p50;
  const viewerFrameMs = results.raf.typing.viewerFps > 0 ? 1000 / results.raf.typing.viewerFps : 16.7;
  const floorParts = {
    raastHandlerMs: handlerMs,
    clientToServerOneWayMs: ping != null ? ping / 2 : null,
    serverDamageFrameTotalMs: damageFrame,
    serverToClientOneWayMs: ping != null ? ping / 2 : null,
    clientPaintMs: viewerFrameMs,
  };
  const knownFloor = Object.values(floorParts).every((value) => value != null)
    ? Object.values(floorParts).reduce((total, value) => total + value, 0)
    : null;
  results.estimatedFloor = {
    parts: floorParts,
    totalMs: knownFloor,
    assumptions: [
      "one-way transport latency ≈ xpra client ping p50 / 2 in each direction",
      "server damage → encoded frame time = `xpra info` client.0.damage.frame-total-latency (mid-interaction snapshot)",
      "client decode/paint ≈ one hidden-viewer rAF interval at the measured viewer rAF rate",
      "RAST handler = measured keydown event → shim.sendKey call; WebSocket send queue not included",
    ],
    caveat: "Estimate for one keystroke to become a painted frame; Xpra batching can add up to batch.max-delay under continuous damage. Compare with the measured application-acknowledgement p50/p95.",
  };
  if (knownFloor != null) {
    console.log(
      `estimated floor ≈ ${fmtMs(knownFloor)} ms = handler ${fmtMs(handlerMs)} + ping/2 ${fmtMs(ping / 2)}×2 + damage ${fmtMs(damageFrame)} + paint ${fmtMs(viewerFrameMs)}`,
    );
  }

  // ---- screenshots + evidence -----------------------------------------
  await screenshot(page, SHOTS_DIR, "typing-profile").catch(() => undefined);
  await screenshot(page, SHOTS_DIR, "desktop-profile").catch(() => undefined);
  const evidenceLines = [];
  evidenceLines.push(`# RAST input-latency profile evidence — ${results.startedAt}`);
  evidenceLines.push(`target: ${BASE_URL}`);
  evidenceLines.push(`session: ${JSON.stringify(results.environment.session)}`);
  evidenceLines.push(`\n## xpra info (idle)\n${JSON.stringify(results.xpra.idle, null, 2)}`);
  evidenceLines.push(`\n## xpra info (mid-typing)\n${JSON.stringify(results.xpra.midTyping, null, 2)}`);
  evidenceLines.push(`\n## xpra info (after)\n${JSON.stringify(results.xpra.after, null, 2)}`);
  const rawInfoAfter = await xpraInfoAsync(identity);
  if (rawInfoAfter.ok) {
    const raw = rawInfoAfter.text;
    const selected = raw
      .split("\n")
      .filter((line) =>
        /^(client\.0\.(connection\.client\.ping_latency|damage\.|batch\.|connection\.(compressor|encoder|protocol-type)|window\.[0-9]+\.(damage\.|batch\.(delay|actual_delays|client-latency)|encoding\.pipeline_option\.0\.encoder|total_frames)))/.test(
          line,
        ),
      )
      .sort();
    evidenceLines.push(`\n## raw xpra info selected lines (after)\n${selected.join("\n")}`);
  }
  evidenceLines.push(`\n## tunnel\n${JSON.stringify(results.tunnel, null, 2)}`);
  writeText(EVIDENCE_PATH, `${evidenceLines.join("\n")}\n`);

  results.finishedAt = new Date().toISOString();
  results.limitations = [
    "Single host: the browser used for measurement, RAST, Xpra and the remote Chrome all run on one host; the public leg still crosses the real Cloudflare tunnel but there is no end-user last mile.",
    "The application-acknowledgement metric uses the remote page's document.title → RAST window title path; it is a full keypress→application→client round trip and can include Xpra window-metadata propagation, so it is an upper bound for app acknowledgement, not screen paint latency.",
    "The trusted browser paste event cannot be produced by CDP key injection; pasteEventsSeen reports the truth for this run (expected 0) and no paste-path stage is inferred from it.",
    "`xpra info` snapshots are instantaneous samples of adaptive state; mid-typing is taken while the burst is in flight but the batch delay statistics are windowed.",
    "rAF rates are for headless Chromium (--disable-background-timer-throttling / --disable-renderer-backgrounding enabled); a headed browser may render the off-screen viewer differently.",
  ];
} catch (error) {
  results.error = String(error?.stack ?? error);
  console.error(`\nFATAL: ${error?.message ?? error}`);
} finally {
  try {
    if (api && !keepSession) {
      const state = await api.nativeSession().catch(() => null);
      for (const run of state?.session?.apps ?? []) {
        if (run.status === "running" && run.runId) await api.closeNativeRun(run.runId).catch(() => undefined);
      }
      await api.closeNativeSession().catch(() => undefined);
      for (let poll = 0; poll < 30; poll += 1) {
        const after = await api.nativeSession().catch(() => null);
        if (!after?.session) {
          results.sessionClosed = true;
          break;
        }
        await sleepMs(500);
      }
    }
  } catch {
    /* best effort */
  }
  await server.close().catch(() => undefined);
  await context?.close().catch(() => undefined);
  await browser?.close().catch(() => undefined);
}

writeJson(JSON_PATH, results);
writeText(join(RESULTS_DIR, `${OUT_PREFIX}.md`), renderMarkdown(results));
console.log("");
console.log(`results: ${JSON_PATH}`);
console.log(`evidence: ${EVIDENCE_PATH}`);
if (results.error) process.exit(1);

function renderMarkdown(data) {
  const row = (label, value) => `| ${label} | ${value} |`;
  const s = (value) => (value == null ? "n/a" : fmtMs(value));
  const lines = [];
  lines.push(`# RAST input-latency stage profile — ${data.startedAt?.slice(0, 10) ?? ""}`);
  lines.push("");
  lines.push(`- Target: ${data.target ?? "n/a"} (public HTTPS URL ⇒ real Cloudflare tunnel → nginx → RAST)`);
  lines.push(`- Test user: ${data.user ?? "n/a"}; session: \`${JSON.stringify(data.environment?.session ?? null)}\``);
  lines.push(`- Controlled remote page: ${data.pageUrl ?? "n/a"}`);
  lines.push(`- Host: ${JSON.stringify(data.environment?.host ?? null)}`);
  lines.push("");
  lines.push("## Stage latencies (per keystroke, n = keys typed in one burst)");
  lines.push("");
  lines.push("| Stage | p50 | p95 | n |");
  lines.push("|---|---|---|---|");
  const timing = data.typing?.eventTiming;
  if (timing) {
    lines.push(`| input delay (event start → handler start) | ${s(timing.inputDelay.p50)} ms | ${s(timing.inputDelay.p95)} ms | ${timing.inputDelay.n} |`);
    lines.push(`| RAST handler execution (event timing processing) | ${s(timing.handlerProcessing.p50)} ms | ${s(timing.handlerProcessing.p95)} ms | ${timing.handlerProcessing.n} |`);
    lines.push(`| keydown event → shim.sendKey call | ${s(timing.eventToSendKey.p50)} ms | ${s(timing.eventToSendKey.p95)} ms | ${timing.eventToSendKey.n} |`);
  }
  lines.push(`| keypress → outgoing WebSocket frame | ${s(data.typing?.nodeKeypressToWsSent?.p50)} ms | ${s(data.typing?.nodeKeypressToWsSent?.p95)} ms | ${data.typing?.nodeKeypressToWsSent?.n ?? 0} |`);
  lines.push(`| keypress → incoming WebSocket frame | ${s(data.typing?.nodeKeypressToWsReceived?.p50)} ms | ${s(data.typing?.nodeKeypressToWsReceived?.p95)} ms | ${data.typing?.nodeKeypressToWsReceived?.n ?? 0} |`);
  lines.push(`| keypress → application acknowledgement (title) | ${s(data.typing?.appResponse?.p50)} ms | ${s(data.typing?.appResponse?.p95)} ms | ${data.typing?.appResponse?.n ?? 0} |`);
  lines.push("");
  if (data.focusHandshake) {
    lines.push("### Focus handshake (input really reaches the fixture)");
    lines.push("");
    lines.push("| click point | activeElement | title after probe key | ok |");
    lines.push("|---|---|---|---|");
    for (const attempt of data.focusHandshake) {
      lines.push(`| ${JSON.stringify(attempt.point)} | ${attempt.active ?? "n/a"} | ${JSON.stringify(attempt.title)} | ${attempt.ok} |`);
    }
    lines.push("");
  }
  lines.push("## Hidden-viewer vs main-document rAF");
  lines.push("");
  lines.push("| Condition | main document | off-screen viewer iframe | viewer available |");
  lines.push("|---|---|---|---|");
  const idle = data.raf?.idle;
  const typing = data.raf?.typing;
  if (idle) lines.push(`| idle (${(idle.elapsedMs / 1000).toFixed(0)} s) | ${idle.mainFps.toFixed(1)} fps | ${idle.viewerFps.toFixed(1)} fps | ${idle.viewerAvailable} |`);
  if (typing) lines.push(`| during typing | ${typing.mainFps.toFixed(1)} fps | ${typing.viewerFps.toFixed(1)} fps | ${typing.viewerAvailable} |`);
  lines.push("");
  lines.push("## Xpra transport / damage (test identity's own session)");
  lines.push("");
  lines.push("| Metric | idle | mid-typing | after |");
  lines.push("|---|---|---|---|");
  const x = (key) => [data.xpra?.idle, data.xpra?.midTyping, data.xpra?.after].map((snap) => s(snap?.[key])).join(" | ");
  lines.push(`| client ping latency p50 | ${x("pingP50")} |`);
  lines.push(`| client ping latency p90 | ${x("pingP90")} |`);
  lines.push(`| damage client-latency | ${x("damageClientLatency")} |`);
  lines.push(`| damage frame-total latency | ${x("damageFrameTotalLatency")} |`);
  lines.push(`| damage target-latency | ${x("damageTargetLatency")} |`);
  lines.push(`| batch delay avg / max (cur) | ${[data.xpra?.idle, data.xpra?.midTyping, data.xpra?.after].map((snap) => (snap ? `${s(snap.batchDelayAvg)} / ${s(snap.batchDelayMax)} (${s(snap.batchDelayCur)})` : "n/a")).join(" | ")} |`);
  lines.push(`| batch config min / max / expire | ${[data.xpra?.idle, data.xpra?.midTyping, data.xpra?.after].map((snap) => (snap ? `${s(snap.batchMinDelay)} / ${s(snap.batchMaxDelay)} / ${s(snap.batchExpire)}` : "n/a")).join(" | ")} |`);
  lines.push("");
  lines.push("## Cloudflare tunnel overhead (HTTP RTT from the RAST host)");
  lines.push("");
  lines.push("| Path | p50 | p95 | n |");
  lines.push("|---|---|---|---|");
  lines.push(`| loopback (http://127.0.0.1:8720/healthz) | ${s(data.tunnel?.loopback?.samples?.p50)} ms | ${s(data.tunnel?.loopback?.samples?.p95)} ms | ${data.tunnel?.loopback?.samples?.n ?? 0} |`);
  lines.push(`| public (${data.target}/healthz) | ${s(data.tunnel?.public?.samples?.p50)} ms | ${s(data.tunnel?.public?.samples?.p95)} ms | ${data.tunnel?.public?.samples?.n ?? 0} |`);
  lines.push(`| inferred tunnel overhead | ${s(data.tunnel?.inferredOverheadMs)} ms | — | — |`);
  lines.push("");
  lines.push(`Cloudflare colo (cf-ray): ${data.tunnel?.cfRay ?? "n/a"}`);
  lines.push("");
  lines.push("### curl components (new connection; ms)");
  lines.push("");
  lines.push("| Path | DNS | TCP connect | TLS | TTFB | total |");
  lines.push("|---|---|---|---|---|---|");
  const curlRow = (label, rows) => {
    if (!rows || rows.length === 0) return `| ${label} | n/a | n/a | n/a | n/a | n/a |`;
    const median = (key) => stats(rows.map((entry) => entry[key] * 1000)).p50;
    return `| ${label} | ${fmtMs(median("dns"))} | ${fmtMs(median("connect"))} | ${fmtMs(median("tls"))} | ${fmtMs(median("ttfb"))} | ${fmtMs(median("total"))} |`;
  };
  lines.push(curlRow("loopback", data.tunnel?.loopback?.curl));
  lines.push(curlRow("public", data.tunnel?.public?.curl));
  lines.push("");
  lines.push("## Estimated floor (estimate, see assumptions)");
  lines.push("");
  const floor = data.estimatedFloor;
  if (floor?.totalMs != null) {
    lines.push(`**≈ ${fmtMs(floor.totalMs)} ms** = RAST handler ${s(floor.parts.raastHandlerMs)} + one-way ${s(floor.parts.clientToServerOneWayMs)} + server damage ${s(floor.parts.serverDamageFrameTotalMs)} + one-way ${s(floor.parts.serverToClientOneWayMs)} + client paint ${s(floor.parts.clientPaintMs)} ms.`);
  } else {
    lines.push("n/a (missing xpra ping/damage fields)");
  }
  lines.push("");
  for (const assumption of floor?.assumptions ?? []) lines.push(`- ${assumption}`);
  lines.push(`- ${floor?.caveat ?? ""}`);
  lines.push("");
  lines.push("## Observations");
  lines.push("");
  for (const entry of data.observations ?? []) lines.push(`- ${entry.id}: ${entry.detail}`);
  lines.push("");
  lines.push("## Limitations");
  lines.push("");
  for (const line of data.limitations ?? []) lines.push(`- ${line}`);
  lines.push("");
  return `${lines.join("\n")}\n`;
}
