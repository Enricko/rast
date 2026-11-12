// Fast, robust 5-point backend performance baseline (docker and native).
//
// It deliberately bypasses the RAST desktop shell: launches go through the
// API and input is sent to the streamed app page directly
// (/linux-apps/<key>/ or /linux-apps/session/<key>/), so the numbers measure
// the backend + stream, not the window-manager UI. Method:
//   launch      POST open/launch -> backend reports ready (poll 100 ms)
//   idle        memory 60 s after the last input
//   active CPU  cgroup cpu.stat delta (native) / docker stats samples (docker)
//               during a scripted 10 s typing+scrolling interaction
//   input       keydown -> first changed page pixel (100 ms polling)
//   two-user    both users concurrently, combined memory/CPU
//
// Usage: node tests/e2e/perf/quick-perf.mjs --mode=docker|native --instance-root=...

import { execFile } from "node:child_process";
import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";
import { promisify } from "node:util";

function prescanArg(argv, name) {
  for (let index = 0; index < argv.length; index += 1) {
    if (argv[index] === `--${name}` && argv[index + 1]) return argv[index + 1];
    if (argv[index].startsWith(`--${name}=`)) return argv[index].slice(name.length + 3);
  }
  return null;
}
const earlyRoot = prescanArg(process.argv.slice(2), "instance-root");
if (earlyRoot) process.env.RAST_E2E_ROOT = earlyRoot;

const { config, modeFromInstance, RESULTS_DIR } = await import("../lib/config.mjs");
const { loginClients, sleep } = await import("../lib/rast-api.mjs");
const browser = await import("../lib/browser.mjs");
const host = await import("../lib/host.mjs");

const execFileAsync = promisify(execFile);
const arg = (name, fallback) => process.argv.find((item) => item.startsWith(`--${name}=`))?.split("=")[1] ?? fallback;

function median(values) {
  const clean = values.filter((value) => Number.isFinite(value)).sort((a, b) => a - b);
  if (!clean.length) return NaN;
  const middle = Math.floor(clean.length / 2);
  return clean.length % 2 ? clean[middle] : (clean[middle - 1] + clean[middle]) / 2;
}
function fmt(ms) { return Number.isFinite(ms) ? `${(ms / 1000).toFixed(2)} s` : "n/a"; }
function mb(bytes) { return Number.isFinite(bytes) ? `${(bytes / 1024 / 1024).toFixed(1)} MiB` : "n/a"; }

function parseDockerBytes(text) {
  const match = String(text ?? "").match(/([\d.]+)\s*([KMGT]?i?B)/i);
  if (!match) return NaN;
  const factors = { b: 1, kb: 1000, kib: 1024, mb: 1e6, mib: 1024 ** 2, gb: 1e9, gib: 1024 ** 3 };
  return Number(match[1]) * (factors[match[2].toLowerCase()] ?? 1);
}

async function dockerStatsFor(names) {
  if (!names.length) return { memoryBytes: NaN, cpuPercent: NaN };
  const stats = await host.dockerStats(names);
  return {
    memoryBytes: stats.reduce((total, item) => total + parseDockerBytes(item.memory), 0),
    cpuPercent: stats.reduce((total, item) => total + (Number.parseFloat(item.cpu) || 0), 0),
    raw: stats,
  };
}

async function dockerNamesFor(client) {
  const copies = await host.dockerPs(["--filter", "label=rast.managed=linux-app"]);
  return copies
    .filter((copy) => String(copy.labels["rast.linux.user"]) === String(client.user.id))
    .map((copy) => copy.name);
}

async function nativeUnitsFor(mode, client, app) {
  if (mode !== "native") return { units: [], sessionUnit: null };
  const identity = (await host.linuxIdentities()).find((entry) => entry.userId === client.user.id);
  if (!identity) return { units: [], sessionUnit: null };
  const sessionUnit = `rast-xpra-u${client.user.id}`;
  const units = [sessionUnit];
  for (const unit of await host.appUnits()) {
    const status = await host.unitStatus(unit.unit);
    if (String(status.Uid ?? "") === String(identity.uid)) units.push(unit.unit);
  }
  return { units, sessionUnit, identity };
}

async function cgroupCpuUsec(units) {
  let total = 0;
  let found = false;
  for (const unit of units) {
    const dir = await host.unitCgroupDir(unit);
    const text = await host.cgroupFile(dir ? `${dir}/cpu.stat` : null);
    const match = text?.match(/^usage_usec (\d+)$/m);
    if (match) { total += Number(match[1]); found = true; }
  }
  return found ? total : NaN;
}
async function cgroupMemory(units) {
  let total = 0;
  let found = false;
  for (const unit of units) {
    const dir = await host.unitCgroupDir(unit);
    const text = await host.cgroupFile(dir ? `${dir}/memory.current` : null);
    const value = text ? Number(text.trim()) : NaN;
    if (Number.isFinite(value)) { total += value; found = true; }
  }
  return found ? total : NaN;
}

async function resetSessions(mode, client) {
  if (mode === "native") {
    await client.closeNativeSession().catch(() => {});
    const started = Date.now();
    while (Date.now() - started < 120_000) {
      const state = await client.nativeSessionState().catch(() => ({ session: null }));
      if (!state.session) return;
      await sleep(300);
    }
  } else {
    const listing = await client.listApps();
    for (const session of (listing.sessions ?? []).filter((item) => item.status !== "ended")) {
      await client.closeSession(session.id).catch(() => {});
    }
    const started = Date.now();
    while (Date.now() - started < 120_000) {
      if ((await dockerNamesFor(client)).length === 0) return;
      await sleep(300);
    }
  }
}

/** Launch through the API and report timing: response, session ready, stream URL. */
async function launch(mode, client, app) {
  const t0 = Date.now();
  let response = null;
  if (mode === "native") {
    response = await client.launch(app.id);
  } else {
    response = await client.openApp(app.id);
  }
  const apiMs = Date.now() - t0;
  const deadline = Date.now() + 240_000;
  while (Date.now() < deadline) {
    if (mode === "native") {
      const state = await client.nativeSessionState();
      const session = state?.session;
      const run = (session?.apps ?? []).find((entry) => entry.appId === app.id && entry.status === "running");
      if (session?.url && session.status === "ready" && run) {
        return { apiMs, readyMs: Date.now() - t0, url: session.url, session, run };
      }
    } else {
      const listing = await client.listApps();
      const mine = (listing.sessions ?? []).find((entry) => entry.appId === app.id && entry.status !== "ended");
      if (mine?.status === "ready" && mine.url) {
        return { apiMs, readyMs: Date.now() - t0, url: mine.url, session: mine };
      }
    }
    await sleep(100);
  }
  throw new Error(`launch did not become ready in 240 s (${app.id})`);
}

/** Open the streamed app page directly (bypasses the desktop shell). */
async function openStreamPage(pwBrowser, client, url, label) {
  const context = await pwBrowser.newContext({ viewport: { width: 1280, height: 800 } });
  const base = new URL(config.baseUrl);
  await context.addCookies(
    [...client.cookies.entries()].map(([name, value]) => ({
      name,
      value,
      domain: base.hostname,
      path: "/",
      httpOnly: name === "rast_session",
      secure: false,
      sameSite: "Lax",
      expires: Math.floor(Date.now() / 1000) + 3600,
    })),
  );
  const page = await context.newPage();
  page.setDefaultTimeout(60_000);
  await page.goto(`${config.baseUrl}${url}`, { waitUntil: "domcontentloaded" });
  await page.waitForTimeout(4000); // let the stream client connect
  return { context, page, label };
}

async function interact(page, durationMs) {
  const viewport = page.viewportSize();
  await page.mouse.click(viewport.width / 2, viewport.height / 2);
  await page.waitForTimeout(300);
  const started = Date.now();
  let typed = 0;
  while (Date.now() - started < durationMs) {
    await page.keyboard.type("rast perf ", { delay: 15 }).catch(() => {});
    await page.mouse.wheel(0, 500).catch(() => {});
    typed += 1;
    await sleep(300);
  }
  return { durationMs: Date.now() - started, iterations: typed };
}

async function inputLatency(page, repeats = 3) {
  const samples = [];
  for (let index = 0; index < repeats; index += 1) {
    const viewport = page.viewportSize();
    const clip = { x: 0, y: 0, width: viewport.width, height: viewport.height };
    const baseline = await page.screenshot({ clip, animations: "disabled" });
    const t0 = Date.now();
    await page.mouse.click(viewport.width / 2, viewport.height / 2);
    await page.keyboard.press("KeyA");
    let elapsed = NaN;
    for (let poll = 0; poll < 60; poll += 1) {
      await page.waitForTimeout(25);
      const current = await page.screenshot({ clip, animations: "disabled" });
      if (!current.equals(baseline)) { elapsed = Date.now() - t0; break; }
    }
    samples.push(elapsed);
    await page.waitForTimeout(400);
  }
  return samples.filter(Number.isFinite);
}

async function main() {
  const mode = arg("mode", modeFromInstance());
  if (!["docker", "native"].includes(mode)) throw new Error(`invalid mode ${mode}`);
  if (!config.admin.password) throw new Error("no credentials — start the dev instance first");
  console.log(`quick perf: mode=${mode} server=${config.baseUrl}`);
  const clients = await loginClients(config);
  const admin = clients.admin;
  const [alice, bob] = clients.users;

  // The fixture app gives deterministic input feedback; use it when installed.
  const listing = await admin.listApps();
  const app =
    listing.apps.find((candidate) => candidate.id === "chrome" && candidate.status === "ready") ??
    listing.apps.find((candidate) => candidate.status === "ready");
  if (!app) throw new Error("no ready app to measure");
  console.log(`app: ${app.id} (${app.name})`);

  const pwBrowser = await browser.launchBrowser({ headless: true });
  const results = { mode, startedAt: new Date().toISOString(), app: { id: app.id, name: app.name }, baseUrl: config.baseUrl };

  // --- cold launch (no session) ---
  await resetSessions(mode, alice);
  const cold = await launch(mode, alice, app);
  results.coldLaunch = cold;
  let stream = await openStreamPage(pwBrowser, alice, cold.url, "cold");
  results.coldFirstDrawMs = null;
  try {
    const baseline = await stream.page.screenshot({ animations: "disabled" });
    const t0 = Date.now();
    for (let poll = 0; poll < 200; poll += 1) {
      await stream.page.waitForTimeout(100);
      const current = await stream.page.screenshot({ animations: "disabled" });
      if (!current.equals(baseline)) { results.coldFirstDrawMs = Date.now() - t0; break; }
    }
  } catch (error) {
    results.coldFirstDrawNote = String(error).slice(0, 200);
  }
  await stream.context.close();

  // --- warm launch (session exists) ---
  await resetSessions(mode, alice); // stop first so warm means "session kept, app restarted"
  const first = await launch(mode, alice, app);
  results.warmLaunch = await launch(mode, alice, app);
  results.warmLaunch.sessionRetained = sessionKeyOf(first) === sessionKeyOf(results.warmLaunch);
  stream = await openStreamPage(pwBrowser, alice, results.warmLaunch.url, "warm");

  // --- idle memory after 60 s ---
  console.log("idle 60 s…");
  await sleep(60_000);
  if (mode === "native") {
    const { units, sessionUnit } = await nativeUnitsFor(mode, alice, app);
    results.idle = {
      units,
      sessionUnit,
      sessionResources: await host.unitResources(sessionUnit),
      memoryBytes: await cgroupMemory(units),
    };
  } else {
    const names = await dockerNamesFor(alice);
    const stats = await dockerStatsFor(names);
    const cgroup = await host.dockerCgroupResources(names);
    results.idle = {
      names,
      memoryBytes: Number.isFinite(cgroup.memoryBytes) ? cgroup.memoryBytes : stats.memoryBytes,
      cgroupMemoryBytes: cgroup.memoryBytes,
      statsMemoryBytes: stats.memoryBytes,
      cpuPercentFromStats: stats.cpuPercent,
      cgroups: cgroup.dirs,
    };
  }
  console.log(`idle: ${mb(results.idle.memoryBytes)}`);

  // --- input latency (streamed page) ---
  results.inputLatencyMs = await inputLatency(stream.page, 3);

  // --- active CPU/memory during a 10 s scripted interaction ---
  let cpuBefore = NaN;
  let units = [];
  let dockerNames = [];
  let dockerCpuBefore = NaN;
  if (mode === "native") {
    ({ units } = await nativeUnitsFor(mode, alice, app));
    cpuBefore = await cgroupCpuUsec(units);
  } else {
    dockerNames = await dockerNamesFor(alice);
    dockerCpuBefore = (await host.dockerCgroupResources(dockerNames)).cpuUsec;
  }
  const startedActive = Date.now();
  const samples = [];
  const interactionPromise = interact(stream.page, 10_000);
  if (mode === "docker") {
    while (Date.now() - startedActive < 10_500) {
      samples.push(await dockerStatsFor(dockerNames));
      await sleep(400);
    }
  }
  const interaction = await interactionPromise;
  const elapsed = Date.now() - startedActive;
  if (mode === "native") {
    const cpuAfter = await cgroupCpuUsec(units);
    results.active = {
      durationMs: elapsed,
      cpuPercent: ((cpuAfter - cpuBefore) / 1000 / elapsed) * 100,
      memoryBytes: await cgroupMemory(units),
      units,
      interaction,
    };
  } else {
    const memoryValues = samples.map((sample) => sample.memoryBytes);
    const cpuValues = samples.map((sample) => sample.cpuPercent);
    const cgroup = await host.dockerCgroupResources(dockerNames);
    const cgroupCpu =
      Number.isFinite(dockerCpuBefore) && Number.isFinite(cgroup.cpuUsec)
        ? ((cgroup.cpuUsec - dockerCpuBefore) / 1000 / elapsed) * 100
        : NaN;
    results.active = {
      durationMs: elapsed,
      cpuPercent: Number.isFinite(cgroupCpu) ? cgroupCpu : median(cpuValues),
      cpuPercentFromStats: median(cpuValues),
      memoryBytes: Number.isFinite(cgroup.memoryBytes) ? cgroup.memoryBytes : median(memoryValues),
      samples: samples.length,
      interaction,
    };
  }
  console.log(`active: cpu ${Number.isFinite(results.active.cpuPercent) ? results.active.cpuPercent.toFixed(1) : "n/a"} %, mem ${mb(results.active.memoryBytes)}`);
  await stream.context.close();

  // --- two users concurrently ---
  console.log("two-user case…");
  await resetSessions(mode, bob).catch(() => {});
  const t0 = Date.now();
  const [aliceLaunch, bobLaunch] = await Promise.all([launch(mode, alice, app), launch(mode, bob, app)]);
  results.twoUser = { openMs: Date.now() - t0, alice: { ...aliceLaunch }, bob: { ...bobLaunch } };
  const aliceStream = await openStreamPage(pwBrowser, alice, aliceLaunch.url, "alice2");
  const bobStream = await openStreamPage(pwBrowser, bob, bobLaunch.url, "bob2");
  await sleep(3000);
  let twoCpuBefore = NaN;
  if (mode === "docker") {
    twoCpuBefore = (await host.dockerCgroupResources(await dockerNamesFor(alice))).cpuUsec;
  }
  if (mode === "native") {
    const aliceUnits = (await nativeUnitsFor(mode, alice, app)).units;
    const bobUnits = (await nativeUnitsFor(mode, bob, app)).units;
    twoCpuBefore = await cgroupCpuUsec([...aliceUnits, ...bobUnits]);
    results.twoUser.memoryBytes =
      (await cgroupMemory(aliceUnits)) + (await cgroupMemory(bobUnits));
    results.twoUser.units = { alice: aliceUnits, bob: bobUnits };
  } else {
    const aliceNames = await dockerNamesFor(alice);
    const bobNames = await dockerNamesFor(bob);
    const [aliceStats, bobStats] = [await dockerStatsFor(aliceNames), await dockerStatsFor(bobNames)];
    const [aliceCg, bobCg] = [
      await host.dockerCgroupResources(aliceNames),
      await host.dockerCgroupResources(bobNames),
    ];
    results.twoUser.memoryBytes = Number.isFinite(aliceCg.memoryBytes + bobCg.memoryBytes)
      ? aliceCg.memoryBytes + bobCg.memoryBytes
      : aliceStats.memoryBytes + bobStats.memoryBytes;
    results.twoUser.cgroupMemoryBytes = { alice: aliceCg.memoryBytes, bob: bobCg.memoryBytes };
    results.twoUser.names = { alice: aliceNames, bob: bobNames };
  }
  const twoStart = Date.now();
  await Promise.all([interact(aliceStream.page, 6000), interact(bobStream.page, 6000)]);
  const twoElapsed = Date.now() - twoStart;
  if (mode === "native") {
    const aliceUnits = (await nativeUnitsFor(mode, alice, app)).units;
    const bobUnits = (await nativeUnitsFor(mode, bob, app)).units;
    const twoCpuAfter = await cgroupCpuUsec([...aliceUnits, ...bobUnits]);
    results.twoUser.cpuPercent = ((twoCpuAfter - twoCpuBefore) / 1000 / twoElapsed) * 100;
  } else {
    const aliceNames = await dockerNamesFor(alice);
    const bobNames = await dockerNamesFor(bob);
    const [aliceCg, bobCg] = [
      await host.dockerCgroupResources(aliceNames),
      await host.dockerCgroupResources(bobNames),
    ];
    if (Number.isFinite(twoCpuBefore) && Number.isFinite(aliceCg.cpuUsec + bobCg.cpuUsec)) {
      results.twoUser.cpuPercent =
        ((aliceCg.cpuUsec + bobCg.cpuUsec - twoCpuBefore) / 1000 / twoElapsed) * 100;
    } else {
      const aliceStats = await dockerStatsFor(aliceNames);
      const bobStats = await dockerStatsFor(bobNames);
      results.twoUser.cpuPercent = (aliceStats.cpuPercent || 0) + (bobStats.cpuPercent || 0);
    }
  }
  console.log(`two-user: ${mb(results.twoUser.memoryBytes)} memory, cpu ${Number.isFinite(results.twoUser.cpuPercent) ? results.twoUser.cpuPercent.toFixed(1) : "n/a"} %`);
  await aliceStream.context.close();
  await bobStream.context.close();
  await pwBrowser.close();

  results.finishedAt = new Date().toISOString();
  mkdirSync(RESULTS_DIR, { recursive: true });
  writeFileSync(join(RESULTS_DIR, `perf-quick-${mode}-latest.json`), JSON.stringify(results, null, 2));
  const table = render(results);
  writeFileSync(join(RESULTS_DIR, `perf-quick-${mode}-latest.md`), table + "\n");
  console.log(`\n${table}`);
}

function sessionKeyOf(launchResult) {
  return launchResult?.session?.url ?? launchResult?.url ?? null;
}

function render(results) {
  const f = (value) => (Number.isFinite(value) ? value.toFixed(1) : "n/a");
  const lines = [
    `# Quick backend performance — ${results.mode}`,
    "",
    `Generated ${results.finishedAt}; app ${results.app.id}; server ${results.baseUrl}.`,
    "",
    "| Measurement | Value |",
    "|---|---|",
    `| Cold launch — launch API response | ${fmt(results.coldLaunch.apiMs)} |`,
    `| Cold launch — backend ready (stream URL available) | ${fmt(results.coldLaunch.readyMs)} |`,
    `| Cold launch — first painted frame after opening the stream page | ${fmt(results.coldFirstDrawMs)} |`,
    `| Warm launch — launch API response | ${fmt(results.warmLaunch.apiMs)} |`,
    `| Warm launch — backend ready | ${fmt(results.warmLaunch.readyMs)} |`,
    `| Warm launch kept the same session | ${results.warmLaunch.sessionRetained} |`,
    `| Idle memory after 60 s | ${mb(results.idle.memoryBytes)} |`,
    `| Active memory during 10 s interaction | ${mb(results.active.memoryBytes)} |`,
    `| CPU during 10 s interaction | ${f(results.active.cpuPercent)} % of one core |`,
    `| Input: key press → first page pixel | ${f(median(results.inputLatencyMs))} ms (samples ${JSON.stringify(results.inputLatencyMs)}) |`,
    `| Two-user combined memory | ${mb(results.twoUser.memoryBytes)} |`,
    `| Two-user CPU during interaction | ${f(results.twoUser.cpuPercent)} % |`,
    `| Two-user open (both ready) | ${fmt(results.twoUser.openMs)} |`,
  ];
  if (results.idle.sessionResources) {
    lines.push(
      `| Session unit memory (systemctl MemoryCurrent / MemoryPeak) | ${mb(results.idle.sessionResources.memoryBytes)} / ${mb(results.idle.sessionResources.memoryPeakBytes)} |`,
    );
  }
  lines.push("");
  lines.push("Method: see tests/e2e/results/README.md (quick backend variant: API launch + streamed page directly, desktop shell excluded).");
  return lines.join("\n");
}

main()
  .then((code) => process.exit(code ?? 0))
  .catch((error) => {
    console.error("quick perf failed:", error);
    process.exit(2);
  });
