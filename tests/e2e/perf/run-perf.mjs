// RAST native-vs-Docker performance measurement harness.
//
// Method (also documented in tests/e2e/results/README.md):
//   * cold launch  — no session/copy for the user: from the click on the app in
//                    the desktop start menu to the first painted pixel of the
//                    app window (window visible + its pixels changed).
//   * warm launch  — the session/copy already exists (the app was opened and
//                    its windows closed); same start/end points.
//   * time-to-first-pixel — from the RAST window appearing to its first pixel
//                    change (subset of cold/warm).
//   * identity provisioning (native only) — first-ever launch per user, which
//                    includes useradd/groupadd and must not be mistaken for
//                    Xpra/Chrome startup.
//   * idle memory  — 60 s after the last input, process memory of the user's
//                    session: native from the per-user cgroup; Docker from
//                    `docker stats` for the user's copy containers.
//   * active memory/CPU — sampled during a scripted 8 s interaction
//                    (typing + scrolling) from the cgroup cpu.stat deltas
//                    (native) or `docker stats` (docker).
//   * input latency — Playwright timestamp before a key press/mouse click to
//                    the first changed screenshot pixel of the app window;
//                    considerable Node/Playwright and encode overhead is
//                    included (documented as an upper bound).
//   * one-user and two-user cases, same host, same apps, same fixture.
//
// Usage:
//   node tests/e2e/perf/run-perf.mjs --mode=docker
//   node tests/e2e/perf/run-perf.mjs --mode=native --label=after-fix
//   node tests/e2e/perf/run-perf.mjs --compare      # docker vs native tables
//
// Outputs JSON + markdown in tests/e2e/results/.

import { execFile } from "node:child_process";
import { existsSync, mkdirSync, readFileSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { promisify } from "node:util";

// --instance-root is applied before the configuration module is loaded.
function prescanArg(argv, name) {
  for (let index = 0; index < argv.length; index += 1) {
    if (argv[index] === `--${name}` && argv[index + 1]) return argv[index + 1];
    if (argv[index].startsWith(`--${name}=`)) return argv[index].slice(name.length + 3);
  }
  return null;
}
const earlyRoot = prescanArg(process.argv.slice(2), "instance-root");
if (earlyRoot) process.env.RAST_E2E_ROOT = earlyRoot;

const { config, modeFromInstance, RESULTS_DIR, SCREENSHOTS_DIR } = await import("../lib/config.mjs");
const { loginClients, sleep } = await import("../lib/rast-api.mjs");
const browser = await import("../lib/browser.mjs");
const host = await import("../lib/host.mjs");
const { ensureChrome, ensurePortableApp } = await import("../scenarios/_helpers.mjs");

const execFileAsync = promisify(execFile);
const PERF_SHOTS = join(SCREENSHOTS_DIR, "perf");

function parseArgs(argv) {
  const args = { mode: null, label: null, compare: false, runs: 3, verbose: false, instanceRoot: null };
  for (const item of argv) {
    if (item === "--compare") args.compare = true;
    else if (item === "--verbose") args.verbose = true;
    else if (item.startsWith("--mode=")) args.mode = item.slice("--mode=".length);
    else if (item.startsWith("--label=")) args.label = item.slice("--label=".length);
    else if (item.startsWith("--runs=")) args.runs = Math.max(1, Number(item.split("=")[1]));
    else if (item === "--instance-root") args.instanceRoot = argv[argv.indexOf(item) + 1];
    else if (item.startsWith("--instance-root=")) args.instanceRoot = item.slice("--instance-root=".length);
    else {
      console.error(`unknown argument: ${item}`);
      process.exit(2);
    }
  }
  return args;
}

function stamp() {
  return new Date().toISOString().replace(/[:.]/g, "-").replace("Z", "");
}

function median(values) {
  const sorted = values.filter((value) => Number.isFinite(value)).sort((a, b) => a - b);
  if (sorted.length === 0) return NaN;
  const middle = Math.floor(sorted.length / 2);
  return sorted.length % 2 ? sorted[middle] : (sorted[middle - 1] + sorted[middle]) / 2;
}

function stats(values) {
  const clean = values.filter((value) => Number.isFinite(value));
  if (clean.length === 0) return { n: 0 };
  return {
    n: clean.length,
    median: median(clean),
    min: Math.min(...clean),
    max: Math.max(...clean),
    values: clean,
  };
}

function parseDockerBytes(text) {
  // "123.4MiB / 2GiB" or "1.2GB" etc.
  if (!text) return NaN;
  const match = String(text).match(/([\d.]+)\s*([KMGT]?i?B)/i);
  if (!match) return NaN;
  const value = Number(match[1]);
  const unit = match[2].toLowerCase();
  const factors = {
    b: 1,
    kb: 1000,
    kib: 1024,
    mb: 1000 ** 2,
    mib: 1024 ** 2,
    gb: 1000 ** 3,
    gib: 1024 ** 3,
    tb: 1000 ** 4,
    tib: 1024 ** 4,
  };
  return value * (factors[unit] ?? 1);
}

/** Sums cgroup counters over units; custom slices live under rast.slice. */
class CgroupSampler {
  constructor(units) {
    this.units = units;
    this.dirs = null;
  }
  async resolve() {
    const dirs = [];
    for (const unit of this.units) {
      const dir = await host.unitCgroupDir(unit);
      if (dir) dirs.push(dir);
    }
    this.dirs = dirs;
    return dirs.length > 0;
  }
  async sumFile(name, pattern) {
    if (!this.dirs && !(await this.resolve())) return NaN;
    let total = 0;
    let found = false;
    for (const dir of this.dirs) {
      const text = await host.cgroupFile(`${dir}/${name}`);
      if (!text) continue;
      if (pattern) {
        const match = text.match(pattern);
        if (!match) continue;
        total += Number(match[1]);
      } else {
        const value = Number(text.trim());
        if (!Number.isFinite(value)) continue;
        total += value;
      }
      found = true;
    }
    return found ? total : NaN;
  }
  cpuUsec() {
    return this.sumFile("cpu.stat", /^usage_usec (\d+)$/m);
  }
  memoryBytes() {
    return this.sumFile("memory.current");
  }
  memoryPeakBytes() {
    return this.sumFile("memory.peak");
  }
  async sliceMemory() {
    const dir = await host.unitCgroupDir("rast-linux.slice");
    const text = await host.cgroupFile(dir ? `${dir}/memory.current` : null);
    return text ? Number(text.trim()) : NaN;
  }
  async sliceCpuUsec() {
    const dir = await host.unitCgroupDir("rast-linux.slice");
    const text = await host.cgroupFile(dir ? `${dir}/cpu.stat` : null);
    const match = text?.match(/^usage_usec (\d+)$/m);
    return match ? Number(match[1]) : NaN;
  }
  async sliceMemoryPeak() {
    const dir = await host.unitCgroupDir("rast-linux.slice");
    const text = await host.cgroupFile(dir ? `${dir}/memory.peak` : null);
    return text ? Number(text.trim()) : NaN;
  }
}

async function nativeUnits(ctx, client) {
  const units = [];
  const identity = (await host.linuxIdentities()).find((entry) => entry.userId === client.user.id);
  if (!identity) return { units, identity: null, appUnits: [] };
  const sessionUnit = `rast-xpra-u${client.user.id}`;
  units.push(sessionUnit);
  const appUnits = [];
  for (const unit of await host.appUnits()) {
    const status = await host.unitStatus(unit.unit);
    if (String(status.Uid ?? "") === String(identity.uid)) {
      appUnits.push(unit.unit);
      units.push(unit.unit);
    }
  }
  return { units, identity, appUnits, sessionUnit };
}

async function dockerCopyNames(ctx, app, client) {
  const copies = (await host.dockerPs(["--filter", "label=rast.managed=linux-app"])).filter(
    (container) =>
      container.labels["rast.linux.app"] === app.id &&
      String(container.labels["rast.linux.user"]) === String(client.user.id),
  );
  return copies.map((copy) => copy.name);
}

async function dockerStatsFor(names) {
  if (names.length === 0) return { memoryBytes: NaN, cpuPercent: NaN, names: [] };
  const stats = await host.dockerStats(names);
  const memoryBytes = stats.reduce((total, item) => total + parseDockerBytes(item.memory), 0);
  const cpuPercent = stats.reduce((total, item) => total + (Number.parseFloat(item.cpu) || 0), 0);
  return { memoryBytes, cpuPercent, names, raw: stats };
}

/** Wait until the user's session/copy is gone (cold start condition). */
async function resetUserSession(ctx, client, app) {
  if (ctx.mode === "native") {
    await client.closeNativeSession().catch(() => {});
    const started = Date.now();
    while (Date.now() - started < 120_000) {
      const state = await client.nativeSessionState().catch(() => ({ session: null }));
      if (!state.session) break;
      await sleep(500);
    }
    const unit = `rast-xpra-u${client.user.id}`;
    while (Date.now() - started < 180_000) {
      const status = await host.unitStatus(unit);
      if (status.ActiveState !== "active") break;
      await sleep(500);
    }
  } else {
    const listing = await client.listApps();
    for (const session of (listing.sessions ?? []).filter((item) => item.status !== "ended")) {
      await client.closeSession(session.id).catch(() => {});
    }
    const started = Date.now();
    while (Date.now() - started < 120_000) {
      const copies = await dockerCopyNames(ctx, app, client);
      if (copies.length === 0) break;
      await sleep(500);
    }
  }
}

/** Wait for the app window and return timing: appearMs, firstPixelMs, usableMs. */
async function waitForUsableWindow(ctx, page, app, startedAt) {
  let appearedAt = null;
  let pixelAt = null;
  const deadline = Date.now() + 240_000;
  let firstBuffer = null;
  let windowId = null;
  while (Date.now() < deadline) {
    const windows = await browser.windowsInfo(page);
    const match = windows.find((window) => String(window.appId).includes(app.id));
    if (match && !appearedAt) {
      appearedAt = Date.now();
      windowId = match.id;
    }
    if (match) {
      const box = await page.locator(`.window[data-window-id="${match.id}"]`).boundingBox().catch(() => null);
      if (box) {
        const buffer = await page.screenshot({ clip: box, animations: "disabled" }).catch(() => null);
        if (buffer) {
          if (!firstBuffer) firstBuffer = buffer;
          else if (!buffer.equals(firstBuffer)) {
            pixelAt = Date.now();
            break;
          }
        }
      }
    }
    await page.waitForTimeout(100);
  }
  return {
    windowId,
    appearMs: appearedAt ? appearedAt - startedAt : NaN,
    firstPixelMs: pixelAt ? pixelAt - startedAt : NaN,
    timeToFirstPixelMs: pixelAt && appearedAt ? pixelAt - appearedAt : NaN,
    usableMs: pixelAt ? pixelAt - startedAt : NaN,
  };
}

/** Poll the backend until the launched app's session/copy is ready. */
async function waitBackendReady(ctx, client, app, startedAt) {
  const deadline = Date.now() + 240_000;
  while (Date.now() < deadline) {
    if (ctx.mode === "native") {
      const state = await client.nativeSessionState().catch(() => null);
      const session = state?.session ?? null;
      const run = (session?.apps ?? []).find((entry) => entry.appId === app.id);
      if (session && session.status === "ready" && run && run.status === "running") {
        return { sessionReadyMs: Date.now() - startedAt, session, run };
      }
    } else {
      const listing = await client.listApps().catch(() => null);
      const mine = (listing?.sessions ?? []).find(
        (entry) => entry.appId === app.id && entry.status !== "ended",
      );
      if (mine) {
        const fresh = await client.session(mine.id).catch(() => null);
        if (fresh?.status === "ready") {
          return { sessionReadyMs: Date.now() - startedAt, session: fresh, run: null };
        }
      }
    }
    await sleep(200);
  }
  return { sessionReadyMs: NaN, session: null, run: null };
}

/** Measure one launch through the UI; assumes the desktop page is already open. */
async function measureLaunch(ctx, page, app, { cold }) {
  if (cold) await resetUserSession(ctx, ctx.userClient, app);
  // Close leftover windows of the app so the click opens a fresh one.
  const leftovers = (await browser.windowsInfo(page)).filter((window) =>
    String(window.appId).includes(app.id),
  );
  for (const window of leftovers) {
    const close = page.locator(`.window[data-window-id="${window.id}"] .win-control-close`);
    if (await close.count()) await close.first().click().catch(() => {});
  }
  await page.waitForTimeout(500);

  const apiStarted = Date.now();
  const openPromise = ctx.userClient.openApp(app.id).catch(() => null);
  const startedAt = Date.now();
  await browser.launchApp(page, app.name);
  const backendPromise = waitBackendReady(ctx, ctx.userClient, app, startedAt);
  const timing = await waitForUsableWindow(ctx, page, app, startedAt);
  const backend = await backendPromise;
  const apiResult = await openPromise;
  const apiMs = Date.now() - apiStarted;
  return {
    ...timing,
    apiMs,
    apiResponseMs: apiMs,
    sessionReadyMs: backend.sessionReadyMs,
    apiResult: apiResult ? "ok" : "error",
    cold,
  };
}

/** Input latency: press a key / click, measure until the app window pixels change. */
async function measureInputLatency(ctx, page, app, { kind = "key", repeats = 3 } = {}) {
  const windows = (await browser.windowsInfo(page)).filter((window) =>
    String(window.appId).includes(app.id),
  );
  if (windows.length === 0) return { samples: [], note: "no app window" };
  const box = await page.locator(`.window[data-window-id="${windows[0].id}"]`).boundingBox();
  const clip = { x: box.x, y: box.y, width: Math.min(box.width, 1200), height: Math.min(box.height, 800) };
  const samples = [];
  for (let index = 0; index < repeats; index += 1) {
    const baseline = await page.screenshot({ clip, animations: "disabled" });
    const t0 = Date.now();
    if (kind === "key") {
      await page.mouse.click(box.x + box.width * 0.5, box.y + box.height * 0.62);
      await page.waitForTimeout(150);
      await page.keyboard.press("KeyA");
    } else {
      const target =
        ctx.mode === "docker"
          ? { x: box.x + 100, y: box.y + 96 }
          : { x: box.x + 100, y: box.y + 96 };
      await page.mouse.click(target.x, target.y);
    }
    let elapsed = NaN;
    for (let poll = 0; poll < 100; poll += 1) {
      const current = await page.screenshot({ clip, animations: "disabled" });
      if (!current.equals(baseline)) {
        elapsed = Date.now() - t0;
        break;
      }
      await page.waitForTimeout(20);
    }
    samples.push(elapsed);
    await page.waitForTimeout(300);
  }
  return { samples: samples.filter(Number.isFinite), note: kind === "key" ? "keydown -> pixel change" : "click -> pixel change" };
}

/** Sample CPU/memory during a scripted interaction. */
async function measureActiveResources(ctx, page, app, client, { durationMs = 8000 } = {}) {
  let sampler = null;
  let startCpu = NaN;
  let dockerNames = [];
  if (ctx.mode === "native") {
    const { units } = await nativeUnits(ctx, client);
    sampler = new CgroupSampler(units);
    startCpu = await sampler.cpuUsec();
  } else {
    dockerNames = await dockerCopyNames(ctx, app, client);
  }
  const startAt = Date.now();
  const memorySamples = [];
  const cpuSamples = [];
  let typing = true;
  // Interact through the streamed window for the whole window.
  const windows = (await browser.windowsInfo(page)).filter((window) =>
    String(window.appId).includes(app.id),
  );
  if (windows.length > 0) {
    const box = await page.locator(`.window[data-window-id="${windows[0].id}"]`).boundingBox();
    await page.mouse.click(box.x + box.width * 0.5, box.y + box.height * 0.62);
    await page.waitForTimeout(200);
  }
  while (Date.now() - startAt < durationMs) {
    if (ctx.mode === "native") {
      memorySamples.push(await sampler.memoryBytes());
    } else {
      const stats = await dockerStatsFor(dockerNames);
      memorySamples.push(stats.memoryBytes);
      cpuSamples.push(stats.cpuPercent);
    }
    if (typing) await page.keyboard.type("rast perf test ", { delay: 20 }).catch(() => {});
    await page.mouse.wheel(0, typing ? 400 : -400).catch(() => {});
    typing = !typing;
    await sleep(400);
  }
  const elapsedMs = Date.now() - startAt;
  let cpuPercent = NaN;
  if (ctx.mode === "native") {
    const endCpu = await sampler.cpuUsec();
    if (Number.isFinite(startCpu) && Number.isFinite(endCpu)) {
      cpuPercent = ((endCpu - startCpu) / 1000 / elapsedMs) * 100;
    }
  } else {
    cpuPercent = cpuSamples.filter(Number.isFinite).reduce((a, b) => a + b, 0) / Math.max(1, cpuSamples.length);
  }
  return {
    durationMs: elapsedMs,
    memoryBytes: stats(memorySamples),
    cpuPercent: stats([cpuPercent]),
  };
}

function readEncoderInfo(identity) {
  // Xpra logs the encoder/encoding it selects; read the newest log best-effort.
  if (!identity) return null;
  const candidates = [
    `/run/rast-linux/${identity.uid}/xpra.log`,
    `/run/rast-linux/${identity.uid}/xpra-server.log`,
  ];
  for (const path of candidates) {
    if (existsSync(path)) {
      try {
        const text = readFileSync(path, "utf8");
        const lines = text
          .split("\n")
          .filter((line) => /encoder|encoding|codec|video region|scaling/i.test(line))
          .slice(-12);
        return { path, lines };
      } catch {
        /* not readable */
      }
    }
  }
  return null;
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.compare) return compareResults();

  if (args.instanceRoot) config.instanceRoot = args.instanceRoot;
  const mode = args.mode || modeFromInstance();
  if (!["docker", "native"].includes(mode)) {
    console.error(`invalid mode '${mode}' (docker|native)`);
    return 2;
  }
  if (!config.admin.password) {
    console.error("no credentials — start the dev instance first (scripts/e2e/dev-instance.sh)");
    return 2;
  }
  mkdirSync(PERF_SHOTS, { recursive: true });

  const clients = await loginClients(config);
  const pwBrowser = await browser.launchBrowser({ headless: true });
  const admin = clients.admin;
  const alice = clients.users[0];
  const bob = clients.users[1];
  // Reuse the scenario helpers with a minimal context.
  const ctx = {
    mode,
    config,
    browser: pwBrowser,
    admin,
    users: clients.users,
    user: (index) => clients.users[index],
    host,
    fixtures: config.fixtures,
    shared: {},
    record: (line) => console.log(`    ${line}`),
    log: (line) => console.log(`    ${line}`),
    metric: () => {},
    skip: (reason) => {
      throw new Error(`skip: ${reason}`);
    },
    screenshot: async (page, name) => {
      mkdirSync(join(PERF_SHOTS, mode), { recursive: true });
      return browser.screenshot(page, name, { dir: join(PERF_SHOTS, mode) });
    },
  };

  console.log(`\nRAST perf — mode ${mode} (${config.baseUrl})`);
  const { app: portable } = await ensurePortableApp(ctx);
  const { app: chrome } = await ensureChrome(ctx);
  console.log(`fixture app ${portable.id}, Chrome ${chrome.id}`);

  const results = {
    mode,
    label: args.label,
    startedAt: new Date().toISOString(),
    baseUrl: config.baseUrl,
    measurements: {},
    notes: [],
  };
  const m = results.measurements;

  // Native: first-ever launch includes identity provisioning (useradd ~8 s).
  if (mode === "native") {
    const identity = (await host.linuxIdentities()).find((entry) => entry.userId === alice.user.id);
    m.identity = {
      existing: Boolean(identity),
      detail: identity ?? null,
    };
    if (!identity) {
      console.log("first-ever launch for alice: measuring identity provisioning separately");
      const started = Date.now();
      await alice.openApp(portable.id).catch((error) => console.log(`    launch error: ${error.message}`));
      const session = await alice.nativeSessionState().catch(() => ({ session: null }));
      m.identity.provisionAndLaunchMs = Date.now() - started;
      m.identity.sessionAfter = session.session ?? null;
      await resetUserSession(ctx, alice, portable);
    }
  }

  const desktopSession = await browser.openDesktop(ctx.browser, alice);
  const page = desktopSession.page;
  ctx.userClient = alice;

  // ---- cold launch (runs)
  const cold = [];
  for (let run = 0; run < args.runs; run += 1) {
    console.log(`cold launch ${run + 1}/${args.runs}`);
    const sample = await measureLaunch(ctx, page, portable, { cold: true });
    cold.push(sample);
    await ctx.screenshot(page, `cold-launch-${run + 1}`);
  }
  m.coldLaunch = {
    usableMs: stats(cold.map((sample) => sample.usableMs)),
    appearMs: stats(cold.map((sample) => sample.appearMs)),
    timeToFirstPixelMs: stats(cold.map((sample) => sample.timeToFirstPixelMs)),
    apiMs: stats(cold.map((sample) => sample.apiMs)),
    sessionReadyMs: stats(cold.map((sample) => sample.sessionReadyMs)),
    samples: cold,
  };

  // ---- idle memory after 60 s without input (app open)
  console.log("idle memory after 60 s…");
  await sleep(60_000);
  if (mode === "native") {
    const { units, identity, appUnits, sessionUnit } = await nativeUnits(ctx, alice);
    const sampler = new CgroupSampler(units);
    const sessionResources = await host.unitResources(sessionUnit);
    const appResources = [];
    for (const unit of appUnits) appResources.push(await host.unitResources(unit));
    m.idle = {
      sessionMemoryBytes: await sampler.memoryBytes(),
      sessionMemoryPeakBytes: await sampler.memoryPeakBytes(),
      sessionUnitResources: sessionResources,
      sessionCgroupMemoryCurrent: sessionResources.memoryBytes,
      sessionCgroupMemoryPeak: sessionResources.memoryPeakBytes,
      appResources,
      sliceMemoryBytes: await sampler.sliceMemory(),
      sliceMemoryPeakBytes: await sampler.sliceMemoryPeak(),
      units,
      appUnits,
      identity,
      sessionUnit,
    };
  } else {
    // Chrome rather than the fixture: it is the realistic idle workload.
    await browser.launchApp(page, chrome.name).catch(() => {});
    await page.waitForTimeout(3000);
    const names = await dockerCopyNames(ctx, chrome, alice);
    m.idle = await dockerStatsFor(names);
    m.idle.names = names;
  }
  await ctx.screenshot(page, "idle");

  // ---- warm launch
  const warm = [];
  for (let run = 0; run < args.runs; run += 1) {
    console.log(`warm launch ${run + 1}/${args.runs}`);
    const sample = await measureLaunch(ctx, page, portable, { cold: false });
    warm.push(sample);
  }
  m.warmLaunch = {
    usableMs: stats(warm.map((sample) => sample.usableMs)),
    appearMs: stats(warm.map((sample) => sample.appearMs)),
    timeToFirstPixelMs: stats(warm.map((sample) => sample.timeToFirstPixelMs)),
    apiMs: stats(warm.map((sample) => sample.apiMs)),
    sessionReadyMs: stats(warm.map((sample) => sample.sessionReadyMs)),
    samples: warm,
  };
  // Keep a fixture window open for the interaction measurements.
  await measureLaunch(ctx, page, portable, { cold: false });

  // ---- active memory + CPU during scripted interaction (one user)
  console.log("active resources, one user…");
  m.activeOneUser = await measureActiveResources(ctx, page, portable, alice, { durationMs: 8000 });
  m.activeOneUser.dockerStats = mode === "docker" ? await dockerStatsFor(await dockerCopyNames(ctx, portable, alice)) : null;
  if (mode === "native") {
    const { units, sessionUnit, appUnits } = await nativeUnits(ctx, alice);
    m.activeOneUser.units = units;
    m.activeOneUser.sessionUnitResources = await host.unitResources(sessionUnit);
    m.activeOneUser.appUnitResources = [];
    for (const unit of appUnits) m.activeOneUser.appUnitResources.push(await host.unitResources(unit));
    m.activeOneUser.unitsMemoryBytes = await new CgroupSampler(units).memoryBytes();
  }

  // ---- input latency
  console.log("input latency…");
  m.inputLatency = {
    key: await measureInputLatency(ctx, page, portable, { kind: "key", repeats: 4 }),
    click: await measureInputLatency(ctx, page, portable, { kind: "click", repeats: 3 }),
  };

  // ---- two-user case: both users use Chrome at the same time
  console.log("two-user case…");
  await resetUserSession(ctx, alice, portable).catch(() => {});
  await resetUserSession(ctx, bob, portable).catch(() => {});
  const bobDesktop = await browser.openDesktop(ctx.browser, bob);
  const twoStarted = Date.now();
  await Promise.all([
    alice.openApp(chrome.id).catch(() => null),
    bob.openApp(chrome.id).catch(() => null),
  ]);
  await Promise.all([
    browser
      .waitForWindows(page, (list) => list.some((window) => String(window.appId).includes(chrome.id)), {
        timeoutMs: 240_000,
        what: "alice Chrome",
      })
      .catch(() => {}),
    browser
      .waitForWindows(bobDesktop.page, (list) => list.some((window) => String(window.appId).includes(chrome.id)), {
        timeoutMs: 240_000,
        what: "bob Chrome",
      })
      .catch(() => {}),
  ]);
  m.twoUserOpenMs = Date.now() - twoStarted;
  if (mode === "native") {
    const aliceUnits = await nativeUnits(ctx, alice);
    const bobUnits = await nativeUnits(ctx, bob);
    const aliceSampler = new CgroupSampler(aliceUnits.units);
    const bobSampler = new CgroupSampler(bobUnits.units);
    m.twoUser = {
      aliceUnits: aliceUnits.units,
      bobUnits: bobUnits.units,
      aliceSessionMemoryBytes: await aliceSampler.memoryBytes(),
      bobSessionMemoryBytes: await bobSampler.memoryBytes(),
      aliceSessionResources: await host.unitResources(aliceUnits.sessionUnit),
      bobSessionResources: await host.unitResources(bobUnits.sessionUnit),
      sliceMemoryBytes: await aliceSampler.sliceMemory(),
      sliceMemoryPeakBytes: await aliceSampler.sliceMemoryPeak(),
      displays: {
        alice: (await alice.nativeSessionState()).session?.display ?? null,
        bob: (await bob.nativeSessionState()).session?.display ?? null,
      },
    };
  } else {
    const aliceCopies = await dockerCopyNames(ctx, chrome, alice);
    const bobCopies = await dockerCopyNames(ctx, chrome, bob);
    m.twoUser = {
      alice: await dockerStatsFor(aliceCopies),
      bob: await dockerStatsFor(bobCopies),
      names: { alice: aliceCopies, bob: bobCopies },
    };
  }
  // Two users interacting at the same time (CPU contention, one socket each).
  const combinedCpu = [];
  for (let round = 0; round < 6; round += 1) {
    await page.mouse.wheel(0, 300).catch(() => {});
    await bobDesktop.page.mouse.wheel(0, 300).catch(() => {});
    if (mode === "docker") {
      const statsA = await dockerStatsFor(await dockerCopyNames(ctx, chrome, alice));
      const statsB = await dockerStatsFor(await dockerCopyNames(ctx, chrome, bob));
      combinedCpu.push((statsA.cpuPercent || 0) + (statsB.cpuPercent || 0));
    }
    await sleep(500);
  }
  if (mode === "native") {
    const sliceSampler = new CgroupSampler([]);
    const before = await sliceSampler.sliceCpuUsec();
    await sleep(2000);
    const after = await sliceSampler.sliceCpuUsec();
    m.twoUser.cpuPercent = ((after - before) / 1000 / 2000) * 100;
  } else {
    m.twoUser.cpuPercent = stats(combinedCpu);
  }
  await ctx.screenshot(page, "two-user-alice");
  await ctx.screenshot(bobDesktop.page, "two-user-bob");

  // ---- context: identities, units, encoder, journal
  if (mode === "native") {
    const identities = await host.linuxIdentities();
    m.identities = identities;
    m.sessionUnits = [];
    for (const unit of await host.sessionUnits()) {
      m.sessionUnits.push({ ...unit, resources: await host.unitResources(unit.unit) });
    }
    m.appUnits = [];
    for (const unit of await host.appUnits()) {
      m.appUnits.push({ ...unit, resources: await host.unitResources(unit.unit) });
    }
    m.encoder = readEncoderInfo(identities.find((entry) => entry.userId === alice.user.id));
    const journal = await host.sudo("journalctl", ["-u", `rast-xpra-u${alice.user.id}`, "-n", "40", "--no-pager"]);
    m.journal = (journal.stdout || journal.stderr || "").split("\n").slice(-40);
  } else {
    m.containers = await host.dockerPs(["--filter", "label=rast.managed=linux-app"]);
    m.xpraInsideContainers = [];
  }

  results.finishedAt = new Date().toISOString();
  results.missing = [
    "GPU/encoder utilisation and frames-per-second are not exposed by either backend",
    "Network/WAN latency to the browser is not measured (loopback only)",
    "Xpra encoder/encoding selection is read from the Xpra log when present; no per-frame statistics",
    "Keyboard/mouse latency includes Playwright, Chromium and screenshot polling overhead (upper bound)",
    "Docker image build time is an install metric and is measured in scenario a, not here",
  ];
  const jsonPath = join(RESULTS_DIR, `perf-${mode}-latest.json`);
  writeFileSync(jsonPath, JSON.stringify(results, null, 2));
  writeFileSync(join(RESULTS_DIR, `perf-${mode}-${stamp()}.json`), JSON.stringify(results, null, 2));
  const markdown = renderMarkdown(results);
  writeFileSync(join(RESULTS_DIR, `perf-${mode}-latest.md`), markdown);
  console.log(`\n${markdown}`);
  console.log(`\nraw results: ${jsonPath}`);
  await desktopSession.context.close();
  await bobDesktop.context.close();
  await pwBrowser.close();
  return 0;
}

function renderMarkdown(results) {
  const m = results.measurements;
  const f = (value) => (Number.isFinite(value) ? value.toFixed(1) : "n/a");
  const mb = (bytes) => (Number.isFinite(bytes) ? (bytes / 1024 / 1024).toFixed(1) : "n/a");
  const lines = [
    `# RAST performance — ${results.mode} backend${results.label ? ` (${results.label})` : ""}`,
    "",
    `Generated ${results.finishedAt} against ${results.baseUrl}. Raw JSON: \`perf-${results.mode}-latest.json\`.`,
    "",
    "| Measurement | n | median | min | max | unit |",
    "|---|---|---|---|---|---|",
  ];
  const row = (label, stat, unit) => {
    if (!stat || !stat.n) return lines.push(`| ${label} | 0 | n/a | n/a | n/a | ${unit} |`);
    lines.push(`| ${label} | ${stat.n} | ${f(stat.median)} | ${f(stat.min)} | ${f(stat.max)} | ${unit} |`);
  };
  row("Cold launch → first usable window", m.coldLaunch?.usableMs, "ms");
  row("Cold launch → window visible", m.coldLaunch?.appearMs, "ms");
  row("Cold launch → time to first pixel", m.coldLaunch?.timeToFirstPixelMs, "ms");
  row("Cold launch → launch API response", m.coldLaunch?.apiMs, "ms");
  row("Cold launch → backend session ready", m.coldLaunch?.sessionReadyMs, "ms");
  row("Warm launch → first usable window", m.warmLaunch?.usableMs, "ms");
  row("Warm launch → window visible", m.warmLaunch?.appearMs, "ms");
  row("Warm launch → time to first pixel", m.warmLaunch?.timeToFirstPixelMs, "ms");
  row("Warm launch → launch API response", m.warmLaunch?.apiMs, "ms");
  row("Warm launch → backend session ready", m.warmLaunch?.sessionReadyMs, "ms");
  row("Key press → pixel change", m.inputLatency?.key?.samples ? stats(m.inputLatency.key.samples) : null, "ms");
  row("Mouse click → pixel change", m.inputLatency?.click?.samples ? stats(m.inputLatency.click.samples) : null, "ms");
  row("Active CPU, one user interacting", m.activeOneUser?.cpuPercent, "% of one core");
  row("Active memory, one user interacting", m.activeOneUser?.memoryBytes, "bytes");
  lines.push("");
  lines.push("## Memory and CPU");
  lines.push("");
  lines.push("| Case | Value |");
  lines.push("|---|---|");
  if (results.mode === "native") {
    lines.push(`| Idle memory after 60 s (units: ${(m.idle?.units ?? []).join(", ") || "n/a"}) | ${mb(m.idle?.sessionMemoryBytes)} MiB |`);
    lines.push(`| Idle memory peak (cgroup memory.peak) | ${mb(m.idle?.sessionMemoryPeakBytes)} MiB |`);
    if (m.idle?.sessionUnitResources) {
      lines.push(
        `| Session unit rast-xpra-u<id>: MemoryCurrent / MemoryPeak / tasks | ${mb(m.idle.sessionUnitResources.memoryBytes)} MiB / ${mb(m.idle.sessionUnitResources.memoryPeakBytes)} MiB / ${m.idle.sessionUnitResources.tasks} |`,
      );
    }
    for (const app of m.idle?.appResources ?? []) {
      lines.push(`| App unit ${app.unit}: memory / peak | ${mb(app.memoryBytes)} / ${mb(app.memoryPeakBytes)} MiB |`);
    }
    lines.push(`| Idle memory of the whole rast-linux slice | ${mb(m.idle?.sliceMemoryBytes)} MiB |`);
    lines.push(`| Two-user session memory (alice / bob) | ${mb(m.twoUser?.aliceSessionMemoryBytes)} / ${mb(m.twoUser?.bobSessionMemoryBytes)} MiB |`);
    lines.push(`| Two-user slice memory | ${mb(m.twoUser?.sliceMemoryBytes)} MiB |`);
    lines.push(`| Two-user displays (alice / bob) | :${m.twoUser?.displays?.alice ?? "?"} / :${m.twoUser?.displays?.bob ?? "?"} |`);
    lines.push(`| Two-user CPU while interacting | ${f(m.twoUser?.cpuPercent)} % |`);
    if (m.identity) {
      lines.push(
        `| Identity provisioning (first-ever launch) | ${m.identity.existing ? "identity existed (not re-measured)" : `${f(m.identity.provisionAndLaunchMs)} ms (identity + first launch)`} |`,
      );
    }
  } else {
    lines.push(`| Idle memory after 60 s (Docker copy) | ${mb(m.idle?.memoryBytes)} MiB |`);
    lines.push(`| Idle CPU (Docker stats) | ${f(m.idle?.cpuPercent)} % |`);
    lines.push(`| Two-user memory (alice / bob) | ${mb(m.twoUser?.alice?.memoryBytes)} / ${mb(m.twoUser?.bob?.memoryBytes)} MiB |`);
    lines.push(`| Two-user CPU while interacting | ${f(typeof m.twoUser?.cpuPercent?.median === "number" ? m.twoUser.cpuPercent.median : m.twoUser?.cpuPercent)} % |`);
    lines.push(`| Two-user open (both apps ready) | ${f(m.twoUserOpenMs)} ms |`);
  }
  lines.push("");
  lines.push("## Not measured");
  lines.push("");
  for (const item of results.missing ?? []) lines.push(`* ${item}`);
  lines.push("");
  return lines.join("\n");
}

function compareResults() {
  const docker = JSON.parse(readFileSync(join(RESULTS_DIR, "perf-docker-latest.json"), "utf8"));
  const native = JSON.parse(readFileSync(join(RESULTS_DIR, "perf-native-latest.json"), "utf8"));
  const f = (value) => (Number.isFinite(value) ? value.toFixed(1) : "n/a");
  const pick = (result) => result.measurements;
  const d = pick(docker);
  const n = pick(native);
  const rows = [
    ["Cold launch → first usable window (ms)", d.coldLaunch?.usableMs?.median, n.coldLaunch?.usableMs?.median],
    ["Cold launch → window visible (ms)", d.coldLaunch?.appearMs?.median, n.coldLaunch?.appearMs?.median],
    ["Cold launch → time to first pixel (ms)", d.coldLaunch?.timeToFirstPixelMs?.median, n.coldLaunch?.timeToFirstPixelMs?.median],
    ["Warm launch → first usable window (ms)", d.warmLaunch?.usableMs?.median, n.warmLaunch?.usableMs?.median],
    ["Warm launch → time to first pixel (ms)", d.warmLaunch?.timeToFirstPixelMs?.median, n.warmLaunch?.timeToFirstPixelMs?.median],
    ["Key press → pixel change (ms)", stats(d.inputLatency?.key?.samples ?? [])?.median, stats(n.inputLatency?.key?.samples ?? [])?.median],
    ["Mouse click → pixel change (ms)", stats(d.inputLatency?.click?.samples ?? [])?.median, stats(n.inputLatency?.click?.samples ?? [])?.median],
    ["Idle memory (MiB)", (d.idle?.memoryBytes ?? NaN) / 1024 / 1024, (n.idle?.sessionMemoryBytes ?? NaN) / 1024 / 1024],
    ["Active CPU one user (%)", d.activeOneUser?.cpuPercent?.median, n.activeOneUser?.cpuPercent?.median],
    ["Two-user CPU (%)", typeof d.twoUser?.cpuPercent?.median === "number" ? d.twoUser.cpuPercent.median : d.twoUser?.cpuPercent, n.twoUser?.cpuPercent],
  ];
  const lines = [
    "# Docker vs native — performance comparison",
    "",
    `Docker run: ${docker.finishedAt}; native run: ${native.finishedAt}. Same host, same apps, loopback browser.`,
    "",
    "| Measurement | Docker | Native | Delta |",
    "|---|---|---|---|",
  ];
  for (const [label, dockerValue, nativeValue] of rows) {
    const delta =
      Number.isFinite(dockerValue) && Number.isFinite(nativeValue) && dockerValue !== 0
        ? `${(((nativeValue - dockerValue) / dockerValue) * 100).toFixed(0)} %`
        : "n/a";
    lines.push(`| ${label} | ${f(dockerValue)} | ${f(nativeValue)} | ${delta} |`);
  }
  lines.push("");
  lines.push("Negative delta = native is faster/smaller. See the per-mode files for raw samples and notes.");
  const markdown = lines.join("\n") + "\n";
  writeFileSync(join(RESULTS_DIR, "perf-comparison.md"), markdown);
  console.log(markdown);
  return 0;
}

main()
  .then((code) => process.exit(code ?? 0))
  .catch((error) => {
    console.error("perf harness failed:", error);
    process.exit(2);
  });
