// RAST native-runtime performance benchmark (BEFORE/AFTER).
//
// Measures the real, authenticated user path against a RAST server: a headless
// Chromium logs in through the login form, drives the actual desktop shell and
// launches the native app from the start menu. Every timing below is taken in
// the Node process with `Date.now()`; the browser, the RAST server and the
// Xpra display all run on the same host (`--base-url` may still be the public
// HTTPS URL, which is the intended production measurement).
//
// Metrics (see docs/BENCHMARKS.md for the full method):
//   * app launch latency     click on the start-menu tile -> first ADOPTED RAST
//                            window (`.window[data-window-id]` whose appId is
//                            `linux:<app>:<wid>`); also records the "starting"
//                            placeholder window and the shim-ready moments
//   * input -> display       keydown -> first changed pixel of the app window
//                            (screenshot polling; reported p50/p95 and as an
//                            upper bound with the measured poll granularity)
//   * typing burst           40 chars typed as fast as Playwright can: dispatch
//                            time, backlog drain (last key -> stable final
//                            frame) and total first-key -> final frame
//   * scrolling smoothness   wheel bursts; counts (a) distinct painted
//                            compositor frames via a CDP screencast, (b)
//                            distinct screenshots sampled at a fixed interval,
//                            (c) server-side encoded frames from `xpra info`
//   * animation/video        same frame counting on a 60 Hz canvas fixture and
//                            on a locally served VP8 test video
//   * CPU / memory           cgroup v2 cpu.stat + memory.current/peak of the
//                            user's `rast-xpra-u<id>` session unit and every
//                            `rast-app-*` unit of that identity, per phase
//   * bandwidth              WebSocket payload bytes of the Xpra HTML5 client
//                            (Playwright `websocket` events) per phase
//   * idle overhead          CPU/memory/frames/bytes over a quiet interval
//   * negotiated encoding    `xpra info` of the test user's session (encoder,
//                            quality/speed, batch delay, ping latency) + the
//                            session unit command line + journal log lines
//
// A small benchmark page (`bench-page.html`) is served from this host on
// 127.0.0.1 and loaded inside the remote Chrome through its own address bar.
// The page paints deterministically: each keypress changes a large swatch and
// the page background, so a single changed window pixel proves the input
// reached the display. Its top strip is pure black to let the harness locate
// the remote page origin inside the adopted window (needed for clip
// screenshots and for clicking the in-page TYPE/SCROLL/ANIM/VIDEO buttons).
//
// Safety: only ever touches the benchmark user's own session (through the RAST
// API) — never systemd units or production services directly.
//
// Usage:
//   node tests/e2e/perf/bench.mjs --label=baseline
//   node tests/e2e/perf/bench.mjs --label=after --out=perf-runtime2-after-2026-11-25
//   node tests/e2e/perf/bench.mjs --base-url=http://127.0.0.1:8731 \
//        --creds=/tmp/rast-e2e/credentials.env --label=dev-baseline
//
// Options:
//   --label=<name>        result label (default: run)
//   --out=<prefix>        output file prefix under tests/e2e/results/
//                         (default: perf-runtime2-<label>-<YYYY-MM-DD>)
//   --base-url=<url>      RAST URL (default $RAST_BENCH_BASE_URL or the
//                         production public URL)
//   --creds=<file>        credentials file. Two formats are accepted:
//                         production `username=password` lines (also
//                         /home/lufria/.config/rast-test/creds) or the dev
//                         instance's RAST_E2E_* variables.
//   --user=<name>         benchmark user (default t-alice / alice)
//   --admin=<name>        admin user for environment facts (default t-admin / admin)
//   --app=<id>            Linux app id (default google-chrome)
//   --cold-runs=<n>       cold launches (session stopped first), default 2
//   --warm-runs=<n>       warm launches (session kept), default 4
//   --input-samples=<n>   key->pixel samples, default 20
//   --burst-samples=<n>   typing bursts, default 3
//   --idle-seconds=<n>    quiet interval for idle overhead, default 30
//   --skip-scroll/--skip-anim/--skip-video/--skip-direct
//   --keep-session        leave the benchmark session running at the end
//   --timeout-scale=<f>   multiply every wait (default 1; 2 on a slow host)

import { execFileSync } from "node:child_process";
import { createHash } from "node:crypto";
import { mkdirSync, readFileSync, statSync, writeFileSync } from "node:fs";
import { createServer } from "node:http";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { inflateSync } from "node:zlib";
import { chromium } from "playwright";

const HERE = dirname(fileURLToPath(import.meta.url));
const REPO = resolve(HERE, "../../..");
const RESULTS = join(REPO, "tests/e2e/results");

// ---------------------------------------------------------------------------
// Arguments
// ---------------------------------------------------------------------------
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
  app: arg("app", "google-chrome"),
  coldRuns: Number(arg("cold-runs", 2)),
  warmRuns: Number(arg("warm-runs", 4)),
  inputSamples: Number(arg("input-samples", 20)),
  burstSamples: Number(arg("burst-samples", 3)),
  idleSeconds: Number(arg("idle-seconds", 30)),
  skipScroll: arg("skip-scroll", false) === true,
  skipAnim: arg("skip-anim", false) === true,
  skipVideo: arg("skip-video", false) === true,
  skipDirect: arg("skip-direct", false) === true,
  keepSession: arg("keep-session", false) === true,
  timeoutScale: Number(arg("timeout-scale", 1)),
};

const STAMP = new Date().toISOString().replace(/[:.]/g, "-").replace("Z", "").slice(0, 19);
const DATE = new Date().toISOString().slice(0, 10);
const OUT_PREFIX = OPTIONS.out || `perf-runtime2-${OPTIONS.label}-${DATE}`;
const JSON_PATH = join(RESULTS, `${OUT_PREFIX}.json`);
const MD_PATH = join(RESULTS, `${OUT_PREFIX}.md`);
const EVIDENCE_PATH = join(RESULTS, `${OUT_PREFIX}-evidence.txt`);

const sleep = (ms) => new Promise((done) => setTimeout(done, ms));
const SCALE = (ms) => Math.max(1, Math.round(ms * OPTIONS.timeoutScale));

function log(message) {
  console.log(`[bench ${new Date().toISOString().slice(11, 19)}] ${message}`);
}
function warn(message) {
  console.warn(`[bench] WARNING: ${message}`);
}

// ---------------------------------------------------------------------------
// Small statistics helpers
// ---------------------------------------------------------------------------
function percentile(values, fraction) {
  const clean = values.filter((value) => Number.isFinite(value)).sort((a, b) => a - b);
  if (clean.length === 0) return NaN;
  const index = Math.min(clean.length - 1, Math.max(0, Math.ceil(fraction * clean.length) - 1));
  return clean[index];
}
function median(values) {
  return percentile(values, 0.5);
}
function stats(values) {
  const clean = values.filter((value) => Number.isFinite(value));
  if (clean.length === 0) return { n: 0 };
  return {
    n: clean.length,
    min: Math.min(...clean),
    p50: percentile(clean, 0.5),
    p95: percentile(clean, 0.95),
    mean: clean.reduce((total, value) => total + value, 0) / clean.length,
    max: Math.max(...clean),
    values: clean.map((value) => Math.round(value * 100) / 100),
  };
}
const fmt = (value, digits = 0) => (Number.isFinite(value) ? Number(value).toFixed(digits) : "n/a");
const mib = (bytes) => (Number.isFinite(bytes) ? (bytes / 1024 / 1024).toFixed(1) : "n/a");

// ---------------------------------------------------------------------------
// Minimal PNG decoder (Playwright screenshots: 8-bit RGB/RGBA, non-interlaced)
// Used only to locate the remote page origin inside the adopted window.
// ---------------------------------------------------------------------------
function decodePng(buffer) {
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
 * Locate the dark remote page area inside an adopted-window screenshot. The
 * benchmark page starts with a pure-black 8px strip, so the first row that is
 * mostly dark is the page origin; Chrome's own UI above it is light.
 */
function findContentRect(image) {
  const { width, height, channels, pixels } = image;
  const dark = (x, y) => {
    const index = (y * width + x) * channels;
    return Math.max(pixels[index], pixels[index + 1], pixels[index + 2]) < 90;
  };
  const rowDarkRatio = (y) => {
    let hits = 0;
    let total = 0;
    for (let x = 0; x < width; x += 3) {
      total += 1;
      if (dark(x, y)) hits += 1;
    }
    return hits / total;
  };
  let top = -1;
  for (let y = 0; y < height; y += 1) {
    if (rowDarkRatio(y) > 0.6) { top = y; break; }
  }
  if (top < 0) return null;
  let bottom = height - 1;
  for (let y = height - 1; y >= 0; y -= 1) {
    if (rowDarkRatio(y) > 0.4) { bottom = y; break; }
  }
  return { top, bottom, left: 0, right: width - 1 };
}

// ---------------------------------------------------------------------------
// Credentials / REST helpers
// ---------------------------------------------------------------------------
function readSecrets(path) {
  const text = readFileSync(path, "utf8");
  const values = {};
  for (const line of text.split("\n")) {
    const trimmed = line.trim();
    if (!trimmed || trimmed.startsWith("#")) continue;
    const index = trimmed.indexOf("=");
    if (index <= 0) continue;
    values[trimmed.slice(0, index).trim()] = trimmed.slice(index + 1).trim();
  }
  return values;
}

function resolveCredentials() {
  const secrets = readSecrets(OPTIONS.creds);
  const legacy = secrets.RAST_E2E_PASSWORD || secrets.password ? {} : null;
  // Production format: one line per account, `username=password` (the account
  // list lives in the filename's owner config; t-alice/t-admin/t-bob).
  if (secrets["t-alice"] || legacy === null) {
    const user = OPTIONS.user || (secrets["t-alice"] ? "t-alice" : Object.keys(secrets)[0]);
    const admin = OPTIONS.admin || (secrets["t-admin"] ? "t-admin" : user);
    if (!secrets[user]) throw new Error(`no password for user '${user}' in ${OPTIONS.creds}`);
    return {
      user: { username: user, password: secrets[user] },
      admin: { username: admin, password: secrets[admin] ?? secrets[user] },
      source: OPTIONS.creds,
    };
  }
  // Dev-instance format (RAST_E2E_*).
  const user = OPTIONS.user || secrets.RAST_E2E_USER1_USER || "alice";
  const admin = OPTIONS.admin || secrets.RAST_E2E_ADMIN_USER || "admin";
  const passwordOf = (name) => {
    const matches = [
      [secrets.RAST_E2E_ADMIN_USER, secrets.RAST_E2E_ADMIN_PASSWORD],
      [secrets.RAST_E2E_USER1_USER, secrets.RAST_E2E_USER1_PASSWORD],
      [secrets.RAST_E2E_USER2_USER, secrets.RAST_E2E_USER2_PASSWORD],
    ];
    return matches.find(([candidate]) => candidate === name)?.[1];
  };
  return {
    user: { username: user, password: passwordOf(user) },
    admin: { username: admin, password: passwordOf(admin) },
    source: OPTIONS.creds,
  };
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
    if (this.cookies.size > 0) headers.Cookie = this.cookieHeader();
    if (this.csrf) headers["X-CSRF-Token"] = this.csrf;
    let payload;
    if (body !== undefined) {
      headers["Content-Type"] = "application/json";
      payload = JSON.stringify(body);
    }
    const response = await fetch(`${this.baseUrl}${path}`, { method, headers, body: payload, redirect: "manual" });
    for (const raw of response.headers.getSetCookie?.() ?? []) {
      const pair = raw.split(";")[0];
      const index = pair.indexOf("=");
      if (index > 0) this.cookies.set(pair.slice(0, index), pair.slice(index + 1));
    }
    const text = await response.text();
    let json = null;
    try { json = text ? JSON.parse(text) : null; } catch { /* non-JSON */ }
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
  sessionState() { return this.get("/api/linux-apps/session"); }
  launch(appId) { return this.post(`/api/linux-apps/${encodeURIComponent(appId)}/launch`); }
  closeSession() { return this.post("/api/linux-apps/session/close"); }
  closeRun(runId) { return this.post(`/api/linux-apps/session/runs/${encodeURIComponent(runId)}/close`); }
}

// ---------------------------------------------------------------------------
// Host facts and cgroup sampling (read-only; uses passwordless sudo when a
// cgroup file is not readable by the operator account)
// ---------------------------------------------------------------------------
function runHost(command, args = [], timeoutMs = 30000) {
  try {
    return execFileSync(command, args, { encoding: "utf8", timeout: timeoutMs, maxBuffer: 64 * 1024 * 1024 }).trim();
  } catch {
    return null;
  }
}
function sudoHost(args, timeoutMs = 30000) {
  return runHost("sudo", ["-n", ...args], timeoutMs);
}
function sudoFile(path) {
  const direct = (() => {
    try { return readFileSync(path, "utf8"); } catch { return null; }
  })();
  return direct ?? sudoHost(["cat", path]);
}

async function hostFacts() {
  const cpuinfo = readFileSync("/proc/cpuinfo", "utf8");
  const model = cpuinfo.split("\n").find((line) => line.startsWith("model name"))?.split(":")[1]?.trim() ?? "unknown";
  const load = readFileSync("/proc/loadavg", "utf8").trim().split(" ").slice(0, 3).map(Number);
  const mem = Object.fromEntries(
    readFileSync("/proc/meminfo", "utf8").split("\n").filter(Boolean).map((line) => {
      const [key, value] = line.split(":");
      return [key.trim(), Number.parseInt(value, 10) * 1024];
    }),
  );
  return {
    hostname: runHost("hostname"),
    kernel: runHost("uname", ["-r"]),
    cpus: cpuinfo.split("\n").filter((line) => line.startsWith("processor")).length,
    cpuModel: model,
    load1: load[0], load5: load[1], load15: load[2],
    memTotalBytes: mem.MemTotal, memAvailableBytes: mem.MemAvailable,
    xpraVersion: (runHost("xpra", ["--version"]) ?? "").split("\n")[0],
    chromeVersion: (runHost("google-chrome-stable", ["--version"]) ?? "").split("\n")[0],
    chromeDebVersion: runHost("dpkg-query", ["-W", "-f=${Version}", "google-chrome-stable"]),
    rastVersion: runHost("/usr/local/bin/rast", ["version"])?.split("\n")[0] ?? null,
    xpraEncoders: (() => {
      const text = runHost("xpra", ["encoding"]) ?? "";
      const lines = text.split("\n").filter((line) => /enc_x264|enc_openh264|enc_webp|nvenc|nvfbc|nvdec|^codecs versions|^\* (x264|openh264|webp)/.test(line)).map((line) => line.replace(/\x1b\[[0-9;]*m/g, "").trim());
      return lines.slice(0, 20);
    })(),
    rastService: (() => {
      const text = sudoHost(["systemctl", "show", "rast.service", "-p", "ActiveState", "-p", "ExecMainStartTimestamp", "-p", "NRestarts"]);
      return Object.fromEntries((text ?? "").split("\n").filter(Boolean).map((line) => {
        const index = line.indexOf("=");
        return [line.slice(0, index), line.slice(index + 1)];
      }));
    })(),
    rastNativeService: (() => {
      const text = sudoHost(["systemctl", "show", "rast-native.service", "-p", "ActiveState", "-p", "ExecMainStartTimestamp", "-p", "NRestarts"]);
      return Object.fromEntries((text ?? "").split("\n").filter(Boolean).map((line) => {
        const index = line.indexOf("=");
        return [line.slice(0, index), line.slice(index + 1)];
      }));
    })(),
  };
}

class CgroupSampler {
  constructor(units) {
    this.units = units;
    this.dirs = [];
  }
  async resolve() {
    this.dirs = [];
    for (const unit of this.units) {
      const control = runHost("systemctl", ["show", unit, "--property=ControlGroup", "--value"]);
      let dir = control ? `/sys/fs/cgroup${control.trim()}` : null;
      if (!dir || control.trim() === "/") {
        dir = [`/sys/fs/cgroup/rast.slice/rast-linux.slice/${unit}`, `/sys/fs/cgroup/system.slice/${unit}`]
          .find((candidate) => statExists(candidate)) ?? null;
      }
      if (dir) this.dirs.push(dir);
    }
    return this.dirs.length;
  }
  async read(pattern) {
    if (this.dirs.length === 0 && !(await this.resolve())) return NaN;
    let total = 0;
    let found = false;
    for (const dir of this.dirs) {
      const text = sudoFile(`${dir}/${pattern.file}`);
      if (!text) continue;
      if (pattern.regex) {
        const match = text.match(pattern.regex);
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
  cpuUsec() { return this.read({ file: "cpu.stat", regex: /^usage_usec (\d+)$/m }); }
  memoryBytes() { return this.read({ file: "memory.current" }); }
  memoryPeak() { return this.read({ file: "memory.peak" }); }
}

function statExists(path) {
  try { statSync(path); return true; } catch { return false; }
}

async function discoverUnits(userId, uid) {
  const sessionUnit = `rast-xpra-u${userId}`;
  const appUnits = [];
  const listing = sudoHost(["systemctl", "list-units", "--type=service", "--all", "--no-legend", "--plain", "rast-app-*"]);
  for (const line of (listing ?? "").split("\n")) {
    const unit = line.trim().split(/\s+/)[0];
    if (!unit) continue;
    const status = runHost("systemctl", ["show", unit, "-p", "Uid", "-p", "ActiveState"]);
    if (!status) continue;
    const fields = Object.fromEntries(status.split("\n").filter(Boolean).map((row) => {
      const index = row.indexOf("=");
      return [row.slice(0, index), row.slice(index + 1)];
    }));
    if (String(fields.Uid ?? "") === String(uid) && fields.ActiveState === "active") appUnits.push(unit);
  }
  return { sessionUnit, appUnits, units: [sessionUnit, ...appUnits] };
}

async function unitSummary(unit) {
  const text = runHost("systemctl", ["show", unit, "-p", "ActiveState,SubState,MainPID,MemoryCurrent,MemoryPeak,TasksCurrent,ControlGroup"]);
  const fields = Object.fromEntries((text ?? "").split("\n").filter(Boolean).map((line) => {
    const index = line.indexOf("=");
    return [line.slice(0, index), line.slice(index + 1)];
  }));
  return fields;
}

// ---------------------------------------------------------------------------
// xpra info / session evidence
// ---------------------------------------------------------------------------
function parseInfo(text) {
  const map = {};
  for (const line of text.split("\n")) {
    const index = line.indexOf("=");
    if (index <= 0) continue;
    map[line.slice(0, index)] = line.slice(index + 1);
  }
  return map;
}

const INFO_SELECTED = [
  /^build\.version$/,
  /^display\.address$/,
  /^client\.0\.encodings\.core$/,
  /^client\.0\.connection\.client\.ping_latency\./,
  /^client\.0\.connection\.server\.ping_latency\./,
  /^client\.0\.connection\.(input|output)\.(packetcount|raw_packetcount)$/,
  /^client\.0\.connection\.(encoder|compressor|compression_level|protocol-type)$/,
  /^client\.0\.batch\./,
  /^client\.0\.damage\./,
  /^client\.0\.window\.[0-9]+\.batch\./,
  /^client\.0\.window\.[0-9]+\.(encoding\.pipeline_option\.0\.(encoder|encoder\.width|encoder\.height|format),encodings\.video|encodings\.non-video|damage\.(fps|target-latency|packets_sent|events)|quality\.(cur|avg)|speed\.(cur|avg)|total_frames\.|total_pixels\.)/,
  /^client\.0\.window\.[0-9]+\.encoding\.(quality|speed)\.(cur|avg)$/,
  /^client\.0\.window\.[0-9]+\.encoding\.pipeline_option\.0\.encoder$/,
];
function selectedInfoLines(map) {
  const lines = [];
  for (const [key, value] of Object.entries(map)) {
    if (INFO_SELECTED.some((pattern) => pattern.test(key))) lines.push(`${key}=${value}`);
  }
  return lines.sort();
}
function infoValue(map, key) {
  const value = map[key];
  if (value === undefined) return null;
  const number = Number(value);
  return Number.isFinite(number) ? number : value;
}
function frameCounters(map) {
  const counters = {};
  for (const [key, value] of Object.entries(map)) {
    const match = key.match(/^client\.0\.window\.(\d+)\.total_frames\.(.+)$/);
    if (match) counters[`${match[1]}:${match[2]}`] = Number(value);
  }
  return counters;
}
function frameDelta(before, after) {
  let total = 0;
  for (const [key, value] of Object.entries(after)) {
    if (!key.includes(":")) continue;
    total += Math.max(0, value - (before[key] ?? value));
  }
  return total;
}
function encoderOf(map) {
  // The pipeline list contains every codec Xpra has used for this window, so
  // the "negotiated" encoder is the one with the most painted pixels.
  const byWindow = new Map();
  for (const [key, value] of Object.entries(map)) {
    const pipeline = key.match(/^client\.0\.window\.(\d+)\.encoding\.pipeline_option\.\d+\.encoder$/);
    if (pipeline) {
      const entry = byWindow.get(pipeline[1]) ?? { encoders: [], pixels: {} };
      if (!entry.encoders.includes(value)) entry.encoders.push(value);
      byWindow.set(pipeline[1], entry);
    }
    const pixels = key.match(/^client\.0\.window\.(\d+)\.total_pixels\.(.+)$/);
    if (pixels) {
      const entry = byWindow.get(pixels[1]) ?? { encoders: [], pixels: {} };
      entry.pixels[pixels[2]] = Number(value);
      byWindow.set(pixels[1], entry);
    }
  }
  let best = null;
  let bestPixels = -1;
  for (const [wid, entry] of byWindow) {
    const total = Object.values(entry.pixels).reduce((sum, value) => sum + value, 0);
    if (total > bestPixels) {
      bestPixels = total;
      best = { wid, encoders: entry.encoders, pixels: entry.pixels, dominantEncoder: Object.entries(entry.pixels).sort((a, b) => b[1] - a[1])[0]?.[0] ?? entry.encoders[0] ?? null };
    }
  }
  return best?.dominantEncoder ?? null;
}
function pickWindowInfo(map) {
  const byWindow = new Map();
  for (const [key, value] of Object.entries(map)) {
    const pipeline = key.match(/^client\.0\.window\.(\d+)\.encoding\.pipeline_option\.0\.encoder$/);
    const pixels = key.match(/^client\.0\.window\.(\d+)\.total_pixels\.(.+)$/);
    if (!pipeline && !pixels) continue;
    const wid = (pipeline ?? pixels)[1];
    const entry = byWindow.get(wid) ?? { encoders: [], pixels: {} };
    if (pipeline && !entry.encoders.includes(value)) entry.encoders.push(value);
    if (pixels) entry.pixels[pixels[2]] = Number(value);
    byWindow.set(wid, entry);
  }
  if (byWindow.size === 0) return null;
  let bestWid = null;
  let bestTotal = -1;
  for (const [wid, entry] of byWindow) {
    const total = Object.values(entry.pixels).reduce((sum, value) => sum + value, 0);
    if (total > bestTotal) { bestTotal = total; bestWid = wid; }
  }
  const prefix = `client.0.window.${bestWid}.`;
  const pixels = byWindow.get(bestWid).pixels;
  const dominant = Object.entries(pixels).sort((a, b) => b[1] - a[1])[0]?.[0] ?? byWindow.get(bestWid).encoders[0] ?? null;
  return {
    wid: bestWid,
    encoder: dominant,
    pipelineEncoders: byWindow.get(bestWid).encoders,
    pixelsByEncoder: pixels,
    width: Number(map[`${prefix}encoding.pipeline_option.0.encoder.width`]),
    height: Number(map[`${prefix}encoding.pipeline_option.0.encoder.height`]),
    qualityCurrent: Number(map[`${prefix}encoding.quality.cur`]),
    qualityAverage: Number(map[`${prefix}encoding.quality.avg`]),
    speedCurrent: Number(map[`${prefix}encoding.speed.cur`]),
    speedAverage: Number(map[`${prefix}encoding.speed.avg`]),
    damageFps: Number(map[`${prefix}damage.fps`]),
    targetLatencyMs: Number(map[`${prefix}damage.target-latency`]),
    frames: Object.fromEntries(Object.entries(map).filter(([key]) => key.startsWith(`${prefix}total_frames.`)).map(([key, value]) => [key.split(".").pop(), Number(value)])),
  };
}

class SessionProbe {
  constructor({ baseUrl, userId, uid, display, runtimeDir }) {
    this.baseUrl = baseUrl;
    this.userId = userId;
    this.uid = uid;
    this.display = display;
    this.runtimeDir = runtimeDir;
    this.sessionUnit = `rast-xpra-u${userId}`;
    this.identityUser = `rast-u${userId}`;
  }
  async info() {
    const args = [
      "-u", this.identityUser,
      "env", `XDG_RUNTIME_DIR=${this.runtimeDir}`,
      "xpra", "info", `--socket-dir=${this.runtimeDir}/sockets`, `:${this.display}`,
    ];
    const text = sudoHost(args, 30000);
    if (!text) return null;
    return text;
  }
  async snapshot() {
    const text = await this.info();
    if (!text) return { ok: false, error: "xpra info unavailable" };
    const map = parseInfo(text);
    return {
      ok: true,
      bytes: text.length,
      window: pickWindowInfo(map),
      encoder: encoderOf(map),
      encodingsAvailable: map["client.0.encodings.core"] ?? null,
      displayAddress: map["display.address"] ?? null,
      damageFrameTotalLatencyMs: Number(map["client.0.damage.frame-total-latency"]),
      damageClientLatencyMs: Number(map["client.0.damage.client-latency"]),
      damageFps: Number(map["client.0.damage.fps"]),
      batchDelayAvgMs: Number(map["client.0.batch.delay.avg"]),
      batchDelayMaxMs: Number(map["client.0.batch.delay.max"]),
      batchDelayCurMs: Number(map["client.0.batch.delay.cur"]),
      batchMinDelayMs: Number(map["client.0.batch.min-delay"]),
      batchMaxDelayMs: Number(map["client.0.batch.max-delay"]),
      batchExpireMs: Number(map["client.0.batch.expire"]),
      batchNormalized: Number(map["client.0.batch.normalized"]),
      clientPingLatencyMs: {
        p50: Number(map["client.0.connection.client.ping_latency.50p"]),
        p90: Number(map["client.0.connection.client.ping_latency.90p"]),
        avg: Number(map["client.0.connection.client.ping_latency.avg"]),
        max: Number(map["client.0.connection.client.ping_latency.max"]),
      },
      outputPacketCount: Number(map["client.0.connection.output.packetcount"]),
      inputPacketCount: Number(map["client.0.connection.input.packetcount"]),
      compressor: map["client.0.connection.compressor"] ?? null,
      connectionEncoder: map["client.0.connection.encoder"] ?? null,
      selectedLines: selectedInfoLines(map),
      frames: frameCounters(map),
    };
  }
  async sessionUnitEvidence() {
    return {
      unit: this.sessionUnit,
      show: await unitSummary(this.sessionUnit),
      execStart: sudoHost(["systemctl", "show", this.sessionUnit, "-p", "ExecStart", "--value"]) ?? null,
      environment: sudoHost(["systemctl", "show", this.sessionUnit, "-p", "Environment", "--value"]) ?? null,
    };
  }
  async journalLines() {
    const text = sudoHost(["journalctl", "-u", this.sessionUnit, "-n", "600", "--no-pager", "--output", "cat"], 20000);
    if (!text) return [];
    return text.split("\n").filter((line) => /encod|codec|scal|batch|pixel|webp|h264|avif|jpeg|latency/i.test(line)).slice(-40);
  }
}

// ---------------------------------------------------------------------------
// Browser helpers (desktop shell + native shim)
// ---------------------------------------------------------------------------
const CHROMIUM = "/home/lufria/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome";

async function launchBrowser() {
  return chromium.launch({
    headless: true,
    executablePath: statExists(CHROMIUM) ? CHROMIUM : undefined,
    args: [
      "--no-sandbox",
      "--disable-gpu",
      "--disable-dev-shm-usage",
      "--disable-background-timer-throttling",
      "--disable-renderer-backgrounding",
    ],
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
  if (!(await page.evaluate(async () => (await fetch("/api/auth/me", { credentials: "same-origin" })).ok))) {
    throw new Error("form login did not produce an authenticated desktop");
  }
  await page.waitForTimeout(1500);
}

function windowsInfo(page) {
  return page.evaluate(() => [...document.querySelectorAll(".window[data-window-id]")].map((element) => {
    const rect = element.getBoundingClientRect();
    return {
      id: element.dataset.windowId,
      appId: element.dataset.appId ?? "",
      title: element.querySelector(".win-title")?.textContent?.trim() ?? "",
      focused: element.classList.contains("is-focused"),
      rect: { x: rect.x, y: rect.y, width: rect.width, height: rect.height },
    };
  }));
}

async function openStartMenu(page) {
  if (await page.locator(".start-menu").count()) return;
  await page.locator(".taskbar-start").click();
  await page.waitForSelector(".start-menu", { timeout: SCALE(15000) });
}

async function clickAppTile(page, appName) {
  await openStartMenu(page);
  const search = page.locator(".start-menu .menu-search");
  if (await search.count()) await search.fill(appName);
  const tile = page.locator(".start-menu .menu-app", { hasText: appName }).first();
  await tile.waitFor({ timeout: SCALE(15000) });
  const started = Date.now();
  await tile.click();
  return started;
}

async function waitForShellWindows(page, predicate, timeoutMs, what) {
  const started = Date.now();
  let last = [];
  for (;;) {
    last = await windowsInfo(page);
    if (predicate(last)) return { windows: last, elapsedMs: Date.now() - started };
    if (Date.now() - started > timeoutMs) {
      throw new Error(`timeout waiting for ${what}; windows: ${JSON.stringify(last).slice(0, 400)}`);
    }
    await page.waitForTimeout(100);
  }
}

const isAdopted = (window, appId) => window.appId.startsWith(`linux:${appId}:`);
const isStarting = (window, appId) => window.appId === `linuxnative-starting:${appId}`;
const appWindows = (windows, appId) => windows.filter((window) => isAdopted(window, appId));
const startingWindows = (windows, appId) => windows.filter((window) => isStarting(window, appId));

function shimInfo(page) {
  return page.evaluate(() => {
    const frame = document.querySelector("iframe.linux-native-session-frame");
    const shim = frame?.contentWindow?.RASTNative ?? null;
    if (!shim) return { present: Boolean(frame), ready: false, connected: false, windows: [], key: null };
    let connected = false;
    try { connected = Boolean(shim.client?.connected); } catch { /* not ready */ }
    return { present: true, ready: Boolean(shim.ready), connected, windows: shim.windows?.() ?? [], key: shim.key ?? null };
  });
}

async function waitForShim(page, predicate, timeoutMs, what) {
  const started = Date.now();
  let last = null;
  for (;;) {
    last = await shimInfo(page);
    if (predicate(last)) return { info: last, elapsedMs: Date.now() - started };
    if (Date.now() - started > timeoutMs) throw new Error(`timeout waiting for ${what}; shim: ${JSON.stringify(last).slice(0, 400)}`);
    await page.waitForTimeout(100);
  }
}

async function closeBenchRun(api, appId) {
  try {
    const state = await api.sessionState();
    const runs = (state?.session?.apps ?? []).filter((run) => run.appId === appId && run.status === "running");
    for (const run of runs) await api.closeRun(run.runId).catch(() => {});
  } catch { /* no session */ }
}

async function waitSessionGone(api, timeoutMs) {
  const started = Date.now();
  for (;;) {
    const state = await api.sessionState().catch(() => null);
    if (!state?.session) return true;
    if (Date.now() - started > timeoutMs) return false;
    await sleep(300);
  }
}

async function waitNoAppWindows(page, appId, timeoutMs) {
  const started = Date.now();
  for (;;) {
    const windows = await windowsInfo(page);
    if (appWindows(windows, appId).length === 0 && startingWindows(windows, appId).length === 0) return true;
    if (Date.now() - started > timeoutMs) return false;
    await page.waitForTimeout(150);
  }
}

/** Click -> adopted window, with intermediate milestones. */
async function measureLaunch(page, api, app, { appName }) {
  const clickAt = await clickAppTile(page, appName);
  const milestones = {};
  let firstAdoptedWindow = null;
  let sampledWindow = null;
  const deadline = Date.now() + SCALE(240000);
  while (Date.now() < deadline) {
    const windows = await windowsInfo(page);
    if (!milestones.startingMs && startingWindows(windows, app.id).length > 0) {
      milestones.startingMs = Date.now() - clickAt;
    }
    const adopted = appWindows(windows, app.id);
    if (!milestones.adoptedMs && adopted.length > 0) {
      milestones.adoptedMs = Date.now() - clickAt;
      firstAdoptedWindow = adopted[0];
    }
    if (firstAdoptedWindow && !sampledWindow) {
      const shim = await shimInfo(page).catch(() => null);
      if (shim?.ready) milestones.shimReadyMs = Date.now() - clickAt;
      if (shim?.connected) milestones.clientConnectedMs = Date.now() - clickAt;
      sampledWindow = firstAdoptedWindow;
    }
    if (firstAdoptedWindow && milestones.clientConnectedMs) break;
    await page.waitForTimeout(100);
  }
  if (!firstAdoptedWindow) throw new Error(`launch did not create an adopted window within the timeout (${app.id})`);
  if (!milestones.shimReadyMs) {
    const shim = await waitForShim(page, (info) => info.ready, SCALE(120000), "native shim");
    milestones.shimReadyMs = Date.now() - clickAt;
    if (shim.info.connected) milestones.clientConnectedMs = Date.now() - clickAt;
  }
  // First painted frame of the adopted window: the window is blank until the
  // Xpra client paints it, so wait for the first pixel change.
  let paintedMs = null;
  try {
    const box = await page.evaluate((windowId) => {
      const element = document.querySelector(`.window[data-window-id="${windowId}"]`);
      const rect = element.getBoundingClientRect();
      return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
    }, firstAdoptedWindow.id);
    if (box.width > 20 && box.height > 20) {
      const clip = { x: Math.round(box.x + 2), y: Math.round(box.y + 2), width: Math.round(Math.min(box.width - 4, 1200)), height: Math.round(Math.min(box.height - 4, 700)) };
      const baseline = await page.screenshot({ clip, animations: "disabled" }).catch(() => null);
      const paintStart = Date.now();
      if (baseline) {
        for (let poll = 0; poll < 300; poll += 1) {
          const current = await page.screenshot({ clip, animations: "disabled" }).catch(() => null);
          if (current && !current.equals(baseline)) { paintedMs = Date.now() - paintStart; break; }
          await page.waitForTimeout(50);
        }
      }
    }
  } catch { /* best effort */ }
  milestones.firstPaintedMs = paintedMs;
  const window = (await windowsInfo(page)).find((candidate) => candidate.id === firstAdoptedWindow.id) ?? firstAdoptedWindow;
  return { ...milestones, totalMs: milestones.adoptedMs, window, windows: await windowsInfo(page) };
}

// ---------------------------------------------------------------------------
// Navigation to the benchmark page and calibration of its origin
// ---------------------------------------------------------------------------
/** Rough content estimate used before the fixture page is loaded. */
function estimateContent(box) {
  // The adopted window is RAST titlebar + Chrome UI + content; measured on this
  // Chrome build the content starts 123 px below the window top and ends at the
  // window bottom.
  return { x: Math.round(box.x), y: Math.round(box.y + 123), width: Math.round(box.width), height: Math.max(120, Math.round(box.height - 124)) };
}

/** True when the fixture page (dark) is painted in the window content area. */
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
        const max = Math.max(image.pixels[index], image.pixels[index + 1], image.pixels[index + 2]);
        if (max < 90) dark += 1;
        total += 1;
      }
    }
    return total > 0 && dark / total > 0.25;
  } catch {
    return false;
  }
}

/**
 * Load the fixture page inside the remote Chrome.
 *
 * The shell reserves browser chords for the local browser in newer builds
 * (`Ctrl+L` is in `OTHER_BROWSER_RESERVED`, so it never reaches the remote
 * app). Alt+D is not reserved and Chrome maps it to "focus the address bar";
 * older builds accept Ctrl+L. A last fallback clicks the New Tab page's own
 * search box. Success is verified by the page actually becoming dark.
 */
async function navigateBenchPage(page, app, content, url) {
  const windowId = content.windowId ?? content.id;
  const box = await page.evaluate((windowId) => {
    const element = document.querySelector(`.window[data-window-id="${windowId}"]`);
    const rect = element.getBoundingClientRect();
    return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
  }, windowId);
  const attempts = [
    { name: "Alt+D", focus: async () => page.keyboard.press("Alt+KeyD") },
    { name: "Ctrl+L", focus: async () => page.keyboard.press("Control+KeyL") },
    {
      name: "New-Tab search box",
      focus: async () => {
        const estimate = estimateContent(box);
        // The New Tab page centres its search box roughly 40% down the viewport.
        await page.mouse.click(estimate.x + estimate.width * 0.5, estimate.y + estimate.height * 0.38);
      },
    },
  ];
  for (const attempt of attempts) {
    // Focus the adopted window first (RAST titlebar click is harmless and does
    // not move the remote app's own focus ring).
    await page.mouse.click(box.x + box.width / 2, box.y + box.height - 60).catch(() => {});
    await page.waitForTimeout(300);
    await attempt.focus();
    await page.waitForTimeout(350);
    await page.keyboard.type(url, { delay: 8 });
    await page.waitForTimeout(150);
    await page.keyboard.press("Enter");
    const deadline = Date.now() + SCALE(9000);
    while (Date.now() < deadline) {
      if (await benchPageLoaded(page, box)) {
        log(`fixture page loaded in the remote Chrome (navigation via ${attempt.name})`);
        await page.waitForTimeout(SCALE(1500));
        return;
      }
      await page.waitForTimeout(250);
    }
    log(`navigation via ${attempt.name} did not load the fixture page; trying the next method`);
    await page.mouse.click(box.x + box.width / 2, box.y + box.height - 60).catch(() => {});
    await page.keyboard.press("Escape").catch(() => {});
  }
  throw new Error("could not navigate the remote Chrome to the fixture page (tried Alt+D, Ctrl+L and the New Tab search box)");
}

function contentRectOf(windowBox) {
  const clip = {
    x: Math.round(windowBox.x),
    y: Math.round(windowBox.y),
    width: Math.max(20, Math.round(windowBox.width)),
    height: Math.max(20, Math.round(windowBox.height)),
  };
  return clip;
}

async function calibrateContent(page, windowId) {
  const box = await page.evaluate((id) => {
    const element = document.querySelector(`.window[data-window-id="${id}"]`);
    const rect = element.getBoundingClientRect();
    return { x: rect.x, y: rect.y, width: rect.width, height: rect.height };
  }, windowId);
  const clip = contentRectOf(box);
  const image = decodePng(await page.screenshot({ clip, animations: "disabled" }));
  const rect = findContentRect(image);
  if (!rect) throw new Error("could not locate the remote page content inside the adopted window");
  const navHeight = 8 + 64; // black strip + button row, see bench-page.html
  return {
    window: box,
    content: {
      x: Math.round(box.x + rect.left),
      y: Math.round(box.y + rect.top),
      width: Math.round(rect.right - rect.left + 1),
      height: Math.round(rect.bottom - rect.top + 1),
    },
    nav: {
      y: Math.round(box.y + rect.top + 8 + 32),
      fractions: { type: 0.125, scroll: 0.375, anim: 0.625, video: 0.875 },
    },
    navHeight,
  };
}

async function clickNav(page, layout, mode) {
  const x = layout.content.x + layout.content.width * layout.nav.fractions[mode];
  await page.mouse.click(Math.round(x), Math.round(layout.nav.y));
  await page.waitForTimeout(SCALE(4500));
}

// ---------------------------------------------------------------------------
// Screencast (distinct painted compositor frames)
// ---------------------------------------------------------------------------
class Screencast {
  constructor(cdp) {
    this.frames = [];
    this.active = false;
    this.cdp = cdp;
    cdp.on("Page.screencastFrame", (frame) => {
      if (this.active) {
        this.frames.push({ timestampMs: Math.round(frame.metadata.timestamp * 1000), hash: createHash("md5").update(frame.data).digest("hex") });
      }
      cdp.send("Page.screencastFrameAck", { sessionId: frame.sessionId }).catch(() => {});
    });
  }
  async start() {
    await this.cdp.send("Page.enable").catch(() => {});
    await this.cdp.send("Page.startScreencast", { format: "jpeg", quality: 55, maxWidth: 1200, maxHeight: 800, everyNthFrame: 1 });
  }
  async stop() {
    await this.cdp.send("Page.stopScreencast").catch(() => {});
  }
  begin() {
    this.frames = [];
    this.startedAt = Date.now();
    this.active = true;
  }
  end() {
    this.active = false;
    const finishedAt = Date.now();
    const frames = this.frames;
    const distinct = new Set(frames.map((frame) => frame.hash)).size;
    const deltas = [];
    for (let index = 1; index < frames.length; index += 1) deltas.push(frames[index].timestampMs - frames[index - 1].timestampMs);
    const durationMs = finishedAt - this.startedAt;
    return {
      durationMs,
      frames: frames.length,
      distinctFrames: distinct,
      distinctFps: durationMs > 0 ? (distinct / durationMs) * 1000 : NaN,
      frameDeltasMs: stats(deltas.filter((value) => value > 0)),
    };
  }
}

/** Count distinct screenshots sampled at a fixed interval (lower-bound FPS). */
async function sampleDistinctFrames(page, clip, durationMs, intervalMs) {
  const hashes = [];
  const started = Date.now();
  while (Date.now() - started < durationMs) {
    const shot = await page.screenshot({ clip, animations: "disabled" }).catch(() => null);
    if (shot) hashes.push(createHash("md5").update(shot).digest("hex"));
    const elapsed = Date.now() - started;
    const wait = Math.min(intervalMs, Math.max(0, durationMs - elapsed));
    if (wait > 0) await sleep(wait);
  }
  const duration = Date.now() - started;
  return {
    durationMs: duration,
    samples: hashes.length,
    distinct: new Set(hashes).size,
    distinctFps: duration > 0 ? (new Set(hashes).size / duration) * 1000 : NaN,
  };
}

// ---------------------------------------------------------------------------
// Main
// ---------------------------------------------------------------------------
async function main() {
  mkdirSync(RESULTS, { recursive: true });
  const credentials = resolveCredentials();
  if (!credentials.user.password || !credentials.admin.password) {
    throw new Error(`no credentials found in ${OPTIONS.creds}`);
  }
  log(`target ${OPTIONS.baseUrl} as ${credentials.user.username}; results ${JSON_PATH}`);

  const hostStart = await hostFacts();
  const api = new Api(OPTIONS.baseUrl);
  await api.login(credentials.user);
  const userId = api.user.id;
  const adminApi = new Api(OPTIONS.baseUrl);
  let adminFacts = null;
  try {
    await adminApi.login(credentials.admin);
    for (const path of ["/api/system/info", "/api/system/version", "/api/about"]) {
      try { adminFacts = await adminApi.get(path); break; } catch { /* try next */ }
    }
  } catch (error) {
    warn(`admin login failed (environment facts only): ${error.message}`);
  }

  // Resolve the user's Linux identity (uid) — needed for cgroups and xpra info.
  const identity = (() => {
    for (const line of (runHost("getent", ["passwd"]) ?? "").split("\n")) {
      const parts = line.split(":");
      if (parts[0] !== `rast-u${userId}`) continue;
      return { username: parts[0], uid: Number(parts[2]), gid: Number(parts[3]), home: parts[5] };
    }
    return null;
  })();
  if (!identity) throw new Error(`native identity rast-u${userId} not found on this host`);

  // ---- browser + real login -------------------------------------------------
  const browser = await launchBrowser();
  const context = await browser.newContext({ viewport: { width: 1480, height: 920 }, ignoreHTTPSErrors: true });
  const page = await context.newPage();
  page.setDefaultTimeout(SCALE(30000));
  let websocket = null;
  const traffic = { receivedBytes: 0, sentBytes: 0, receivedFrames: 0, sentFrames: 0 };
  const byteLength = (payload) => (typeof payload === "string" ? Buffer.byteLength(payload) : payload?.length ?? 0);
  page.on("websocket", (socket) => {
    websocket = socket.url;
    socket.on("framereceived", (frame) => {
      traffic.receivedBytes += byteLength(frame.payload);
      traffic.receivedFrames += 1;
    });
    socket.on("framesent", (frame) => {
      traffic.sentBytes += byteLength(frame.payload);
      traffic.sentFrames += 1;
    });
  });
  await formLogin(page, credentials.user.username, credentials.user.password);
  log("desktop authenticated through the login form");

  // ---- app discovery --------------------------------------------------------
  const listing = await api.listApps();
  const app = (listing.apps ?? []).find((candidate) => candidate.id === OPTIONS.app)
    ?? (listing.apps ?? []).find((candidate) => candidate.status === "ready");
  if (!app) throw new Error(`app '${OPTIONS.app}' is not installed/visible for ${credentials.user.username}`);
  const appId = app.id;
  const appName = app.name;
  log(`app: ${appId} (${appName}), backend=${app.backend ?? "?"}`);

  // ---- launch latency -------------------------------------------------------
  // Cold: the user's whole session is stopped first (what happens when the
  // first app is opened after a while). Warm: the shared session stays alive
  // and only the app process is restarted.
  const coldSamples = [];
  for (let run = 1; run <= OPTIONS.coldRuns; run += 1) {
    log(`cold launch ${run}/${OPTIONS.coldRuns}`);
    await api.closeSession().catch(() => {});
    if (!(await waitSessionGone(api, SCALE(120000)))) throw new Error("native session did not stop before a cold launch");
    await waitNoAppWindows(page, appId, SCALE(30000));
    const sample = await measureLaunch(page, api, app, { appName });
    coldSamples.push(sample);
    log(`  → adopted ${fmt(sample.adoptedMs)} ms (placeholder ${fmt(sample.startingMs)} ms, painted +${fmt(sample.firstPaintedMs)} ms)`);
    await closeBenchRun(api, appId);
    await waitNoAppWindows(page, appId, SCALE(30000)).catch(() => {});
    await sleep(500);
  }

  const warmSamples = [];
  for (let run = 1; run <= OPTIONS.warmRuns; run += 1) {
    log(`warm launch ${run}/${OPTIONS.warmRuns}`);
    await closeBenchRun(api, appId);
    await waitNoAppWindows(page, appId, SCALE(60000));
    const sample = await measureLaunch(page, api, app, { appName });
    warmSamples.push(sample);
    log(`  → adopted ${fmt(sample.adoptedMs)} ms (placeholder ${fmt(sample.startingMs)} ms, painted +${fmt(sample.firstPaintedMs)} ms)`);
    await sleep(500);
  }

  // Leave one app running for the remaining phases.
  let windows = await windowsInfo(page);
  if (appWindows(windows, appId).length === 0) {
    log("launching the app again for the interaction phases");
    await measureLaunch(page, api, app, { appName });
    windows = await windowsInfo(page);
  }
  const adoptedWindow = appWindows(windows, appId)[0];
  if (!adoptedWindow) throw new Error("no adopted app window for the interaction phases");

  // ---- session probe / negotiated encoding evidence -------------------------
  let sessionState = await api.sessionState().catch(() => null);
  let session = sessionState?.session ?? null;
  let probe = null;
  let sessionEvidence = null;
  let beforeInfo = null;
  let resourceSamples = [];
  if (session) {
    probe = new SessionProbe({
      baseUrl: OPTIONS.baseUrl,
      userId,
      uid: identity.uid,
      display: session.display,
      runtimeDir: `/run/rast-linux/${identity.uid}`,
    });
    sessionEvidence = await probe.sessionUnitEvidence();
    await probe.journalLines().then((lines) => { sessionEvidence.journalEncodingLines = lines; });
    beforeInfo = await probe.snapshot();
    log(`xpra info: encoder=${beforeInfo?.encoder ?? "?"} display=${beforeInfo?.displayAddress ?? "?"} ok=${beforeInfo?.ok}`);
  } else {
    warn("no native session state from the API; skipping xpra evidence");
  }

  // Resize the adopted window to a known geometry for the bench page.
  const resize = await page.evaluate((windowId) => {
    const shim = document.querySelector("iframe.linux-native-session-frame")?.contentWindow?.RASTNative;
    if (!shim) return { ok: false, reason: "no shim" };
    const remoteWindow = shim.windows().find((candidate) => candidate.adopted) ?? shim.windows()[0];
    if (!remoteWindow) return { ok: false, reason: "no remote window" };
    return { ok: shim.resizeWindow(remoteWindow.wid, { width: 1440, height: 860 }), wid: remoteWindow.wid };
  }, adoptedWindow.id);
  log(`resize: ${JSON.stringify(resize)}`);
  await page.waitForTimeout(SCALE(2000));

  // ---- local benchmark page -------------------------------------------------
  const benchServer = createServer((request, response) => {
    try {
      if (request.url.startsWith("/fixture.webm")) {
        const body = readFileSync(VIDEO_FIXTURE);
        response.writeHead(200, { "Content-Type": "video/webm", "Content-Length": body.length });
        response.end(body);
        return;
      }
      const body = readFileSync(join(HERE, "bench-page.html"));
      response.writeHead(200, { "Content-Type": "text/html; charset=utf-8", "Cache-Control": "no-store" });
      response.end(body);
    } catch (error) {
      response.writeHead(500);
      response.end(String(error));
    }
  });
  await new Promise((done) => benchServer.listen(BENCH_PORT, "127.0.0.1", done));
  const benchUrl = `http://127.0.0.1:${BENCH_PORT}/?mode=type`;
  log(`benchmark page on ${benchUrl}`);
  await navigateBenchPage(page, app, adoptedWindow, benchUrl);
  const layout = await calibrateContent(page, adoptedWindow.id);
  log(`content rect ${JSON.stringify(layout.content)} (window ${JSON.stringify(layout.window)})`);

  const content = layout.content;
  const pageClip = {
    x: content.x + 40,
    y: content.y + 120,
    width: Math.max(120, Math.min(content.width - 80, 640)),
    height: Math.max(80, Math.min(content.height - 160, 360)),
  };

  const cdp = await context.newCDPSession(page);
  const screencast = new Screencast(cdp);
  await screencast.start().catch((error) => warn(`screencast unavailable: ${error.message}`));

  const takeTraffic = () => ({ ...traffic });
  const trafficDelta = (before, after) => ({
    receivedBytes: after.receivedBytes - before.receivedBytes,
    sentBytes: after.sentBytes - before.sentBytes,
    receivedFrames: after.receivedFrames - before.receivedFrames,
    sentFrames: after.sentFrames - before.sentFrames,
  });

  async function phase(name, action) {
    const units = await discoverUnits(userId, identity.uid);
    const sampler = new CgroupSampler(units.units);
    await sampler.resolve();
    const trafficBefore = takeTraffic();
    const infoBefore = probe ? await probe.snapshot().catch(() => null) : null;
    const cpuBefore = await sampler.cpuUsec();
    const memoryBefore = await sampler.memoryBytes();
    const startedAt = Date.now();
    let result;
    let error = null;
    try {
      result = await action();
    } catch (caught) {
      error = String(caught);
      throw caught;
    } finally {
      const durationMs = Date.now() - startedAt;
      const cpuAfter = await sampler.cpuUsec();
      const memoryAfter = await sampler.memoryBytes();
      const memoryPeak = await sampler.memoryPeak();
      const infoAfter = probe ? await probe.snapshot().catch(() => null) : null;
      const trafficAfter = takeTraffic();
      resourceSamples.push({
        phase: name,
        durationMs,
        units: units.units,
        cpuPercent: Number.isFinite(cpuBefore) && Number.isFinite(cpuAfter) ? ((cpuAfter - cpuBefore) / 1000 / durationMs) * 100 : NaN,
        memoryStartBytes: memoryBefore,
        memoryEndBytes: memoryAfter,
        memoryPeakBytes: memoryPeak,
        xpraFrames: infoBefore && infoAfter ? frameDelta(infoBefore.frames, infoAfter.frames) : null,
        xpraFramesPerSecond: infoBefore && infoAfter && durationMs > 0 ? frameDelta(infoBefore.frames, infoAfter.frames) / (durationMs / 1000) : null,
        traffic: trafficDelta(trafficBefore, trafficAfter),
        error,
      });
      if (result !== undefined) result.phase = name;
    }
    return result;
  }

  // ---- input -> display latency --------------------------------------------
  log(`input latency: ${OPTIONS.inputSamples} samples`);
  // Click into the deterministic fixture textarea so keystrokes definitely land
  // inside the remote page (the page also focuses it with autofocus).
  await page.mouse.click(content.x + 30, content.y + 246 + 40);
  await page.waitForTimeout(300);
  const inputSamples = [];
  const pollGranularities = [];
  let midInteractionInfo = null;
  const inputLatencyPhase = await phase("inputLatency", async () => {
  for (let sample = 0; sample < OPTIONS.inputSamples; sample += 1) {
    if (sample === Math.floor(OPTIONS.inputSamples / 2) && probe) {
      // Capture the negotiated/batching state while the user is actively typing,
      // not between phases: this is the value that explains input lag.
      midInteractionInfo = await probe.snapshot().catch(() => null);
    }
    const baseline = await page.screenshot({ clip: pageClip, animations: "disabled" });
    await page.waitForTimeout(120);
    const keyAt = Date.now();
    await page.keyboard.press("KeyX");
    let elapsed = NaN;
    let polls = 0;
    let pollTotalMs = 0;
    for (let poll = 0; poll < 200; poll += 1) {
      const pollStart = Date.now();
      const current = await page.screenshot({ clip: pageClip, animations: "disabled" }).catch(() => null);
      pollTotalMs += Date.now() - pollStart;
      polls += 1;
      if (current && !current.equals(baseline)) {
        elapsed = Date.now() - keyAt;
        break;
      }
    }
    inputSamples.push({ elapsedMs: elapsed, polls });
    if (polls > 0) pollGranularities.push(pollTotalMs / polls);
    await page.waitForTimeout(180);
  }
  return { samples: inputSamples.length };
  });
  const inputLatency = {
    ...stats(inputSamples.map((sample) => sample.elapsedMs)),
    samples: inputSamples,
    pollGranularityMs: stats(pollGranularities),
    method: "keydown (Node Date.now) -> first changed screenshot pixel of the adopted app window; includes Playwright/Chromium/Xpra encode and the public-URL round trip; upper bound, resolution = poll granularity",
  };
  log(`  p50 ${fmt(inputLatency.p50)} ms, p95 ${fmt(inputLatency.p95)} ms (poll ${fmt(inputLatency.pollGranularityMs.p50)} ms)`);

  // ---- typing burst --------------------------------------------------------
  log(`typing burst: ${OPTIONS.burstSamples} × 40 chars`);
  const burstText = "abcdefghijklmnopqrstuvwxyz0123456789ABCD";
  const burstSamples = [];
  const typingBurstPhase = await phase("typingBurst", async () => {
  for (let sample = 0; sample < OPTIONS.burstSamples; sample += 1) {
    // Focus the textarea and clear it.
    await page.mouse.click(content.x + content.width * 0.25, content.y + 300);
    await page.waitForTimeout(150);
    await page.keyboard.press("Control+KeyA");
    await page.keyboard.press("Backspace");
    await page.waitForTimeout(250);
    const baseline = await page.screenshot({ clip: pageClip, animations: "disabled" });
    const firstKeyAt = Date.now();
    await page.keyboard.type(burstText, { delay: 0 });
    const dispatchEndAt = Date.now();
    let stableAt = NaN;
    let previous = null;
    let polls = 0;
    for (let poll = 0; poll < 200; poll += 1) {
      const current = await page.screenshot({ clip: pageClip, animations: "disabled" }).catch(() => null);
      polls += 1;
      if (current && !current.equals(baseline)) {
        if (previous && current.equals(previous)) { stableAt = Date.now(); break; }
        previous = current;
      } else {
        previous = null;
      }
      await page.waitForTimeout(35);
    }
    burstSamples.push({
      dispatchMs: dispatchEndAt - firstKeyAt,
      drainMs: Number.isFinite(stableAt) ? stableAt - dispatchEndAt : NaN,
      totalMs: Number.isFinite(stableAt) ? stableAt - firstKeyAt : NaN,
      polls,
      chars: burstText.length,
    });
    await page.waitForTimeout(300);
  }
  return { bursts: burstSamples.length };
  });
  const typingBurst = {
    chars: burstText.length,
    samples: burstSamples,
    dispatchMs: stats(burstSamples.map((sample) => sample.dispatchMs)),
    drainMs: stats(burstSamples.map((sample) => sample.drainMs)),
    totalMs: stats(burstSamples.map((sample) => sample.totalMs)),
    method: `type ${burstText.length} chars with Playwright delay=0; dispatch = first key -> last key written; drain = last key -> first stable (two identical) screenshot after the burst; total = first key -> stable frame; screenshot polling ~35 ms + round trip, so drain/total are upper bounds`,
  };
  log(`  dispatch ${fmt(typingBurst.dispatchMs.p50)} ms, drain ${fmt(typingBurst.drainMs.p50)} ms, total ${fmt(typingBurst.totalMs.p50)} ms`);

  // ---- idle overhead (before the moving phases) ----------------------------
  log(`idle overhead: ${OPTIONS.idleSeconds} s`);
  await clickNav(page, layout, "type");
  const idle = await phase("idle", async () => {
    const idleScreencast = (() => { screencast.begin(); return true; })();
    await sleep(OPTIONS.idleSeconds * 1000);
    const cast = screencast.end();
    const distinct = await sampleDistinctFrames(page, pageClip, 3000, 60);
    return { cast, sampled: distinct, idleSeconds: OPTIONS.idleSeconds };
  });

  // ---- scroll smoothness ---------------------------------------------------
  let scroll = null;
  if (!OPTIONS.skipScroll) {
    log("scroll smoothness");
    scroll = await phase("scroll", async () => {
      await clickNav(page, layout, "scroll");
      await page.waitForTimeout(SCALE(1500));
      const beforeShot = await page.screenshot({ clip: pageClip, animations: "disabled" }).catch(() => null);
      screencast.begin();
      const started = Date.now();
      for (let tick = 0; tick < 24; tick += 1) {
        await page.mouse.wheel(0, 260).catch(() => {});
        await sleep(55);
      }
      const scrollMs = Date.now() - started;
      await sleep(900);
      const cast = screencast.end();
      const sampled = await sampleDistinctFrames(page, pageClip, 2500, 50);
      const afterShot = await page.screenshot({ clip: pageClip, animations: "disabled" }).catch(() => null);
      return {
        wheelEvents: 24,
        wheelMs: scrollMs,
        changed: Boolean(beforeShot && afterShot && !beforeShot.equals(afterShot)),
        cast,
        sampled,
      };
    });
    log(`  painted ${fmt(scroll.cast.distinctFps, 1)} fps (${scroll.cast.distinctFrames} distinct screencast frames), sampled ${fmt(scroll.sampled.distinctFps, 1)} fps, server-encoded ${fmt(resourceSamples.at(-1)?.xpraFramesPerSecond, 1)} fps`);
  }

  // ---- animation smoothness (60 Hz canvas) ---------------------------------
  let animation = null;
  if (!OPTIONS.skipAnim) {
    log("animation smoothness (60 Hz canvas)");
    animation = await phase("animation", async () => {
      await clickNav(page, layout, "anim");
      await page.waitForTimeout(SCALE(2500));
      screencast.begin();
      await sleep(3000);
      const cast = screencast.end();
      const sampled = await sampleDistinctFrames(page, pageClip, 2500, 50);
      return { cast, sampled };
    });
    log(`  painted ${fmt(animation.cast.distinctFps, 1)} fps, sampled ${fmt(animation.sampled.distinctFps, 1)} fps, server-encoded ${fmt(resourceSamples.at(-1)?.xpraFramesPerSecond, 1)} fps`);
  }

  // ---- video playback smoothness (local VP8 fixture) -----------------------
  let video = null;
  if (!OPTIONS.skipVideo && statExists(VIDEO_FIXTURE)) {
    log("video playback smoothness (local VP8 fixture)");
    video = await phase("video", async () => {
      await clickNav(page, layout, "video");
      await page.waitForTimeout(SCALE(5000));
      screencast.begin();
      await sleep(4000);
      const cast = screencast.end();
      const sampled = await sampleDistinctFrames(page, pageClip, 2500, 50);
      return { cast, sampled };
    });
    log(`  painted ${fmt(video.cast.distinctFps, 1)} fps, sampled ${fmt(video.sampled.distinctFps, 1)} fps, server-encoded ${fmt(resourceSamples.at(-1)?.xpraFramesPerSecond, 1)} fps`);
  } else if (!OPTIONS.skipVideo) {
    warn(`video fixture missing (${VIDEO_FIXTURE}); run with --skip-video or generate it with ffmpeg`);
  }

  // ---- direct Xpra session page (comparison point) -------------------------
  let direct = null;
  if (!OPTIONS.skipDirect && session?.url) {
    log("direct Xpra session page first paint");
    try {
      const directPage = await context.newPage();
      directPage.setDefaultTimeout(SCALE(60000));
      const started = Date.now();
      await directPage.goto(`${OPTIONS.baseUrl}${session.url}?sound=false`, { waitUntil: "domcontentloaded" });
      const viewport = directPage.viewportSize() ?? { width: 1200, height: 800 };
      const clip = { x: 0, y: 0, width: Math.min(viewport.width, 1000), height: Math.min(viewport.height, 600) };
      const baseline = await directPage.screenshot({ clip, animations: "disabled" }).catch(() => null);
      let firstPaintMs = NaN;
      if (baseline) {
        for (let poll = 0; poll < 400; poll += 1) {
          const current = await directPage.screenshot({ clip, animations: "disabled" }).catch(() => null);
          if (current && !current.equals(baseline)) { firstPaintMs = Date.now() - started; break; }
          await directPage.waitForTimeout(50);
        }
      }
      direct = { firstPaintMs, note: "raw Xpra HTML5 session page opened directly (desktop shell and window manager excluded)" };
      await directPage.close().catch(() => {});
      log(`  first paint ${fmt(direct.firstPaintMs)} ms`);
    } catch (error) {
      direct = { firstPaintMs: NaN, error: String(error) };
      warn(`direct session page measurement failed: ${error.message}`);
    }
  }

  // ---- final evidence ------------------------------------------------------
  const afterInfo = probe ? await probe.snapshot().catch(() => null) : null;
  const sessionEvidenceFinal = sessionEvidence
    ? {
        ...sessionEvidence,
        journalEncodingLines: await probe.journalLines().catch(() => []),
        sessionUnitAfter: await unitSummary(`rast-xpra-u${userId}`),
      }
    : null;
  const hostEnd = await hostFacts();
  const trafficTotal = takeTraffic();
  const sessionStateEnd = await api.sessionState().catch(() => null);

  // ---- cleanup -------------------------------------------------------------
  if (!OPTIONS.keepSession) {
    await api.closeSession().catch(() => {});
    await waitSessionGone(api, SCALE(120000)).catch(() => {});
  }
  await browser.close().catch(() => {});
  await new Promise((done) => benchServer.close(done));
  if (session?.url) {
    // Record the session URL hash only (it is a bearer capability).
    session = { ...session, url: session.url.replace(/[0-9a-f]{32}/, "<key>") };
  }

  const measurements = {
    launch: {
      cold: { samples: coldSamples.map(({ windows: _windows, ...rest }) => rest), adoptedMs: stats(coldSamples.map((sample) => sample.adoptedMs)), startingMs: stats(coldSamples.map((sample) => sample.startingMs)), firstPaintedMs: stats(coldSamples.map((sample) => sample.firstPaintedMs)) },
      warm: { samples: warmSamples.map(({ windows: _windows, ...rest }) => rest), adoptedMs: stats(warmSamples.map((sample) => sample.adoptedMs)), startingMs: stats(warmSamples.map((sample) => sample.startingMs)), firstPaintedMs: stats(warmSamples.map((sample) => sample.firstPaintedMs)) },
      app: { id: appId, name: appName, backend: app.backend ?? "native (inferred from the adopted native window)" },
      method: "start-menu tile click (menu already open, excluded) -> first adopted RAST window (`linux:<app>:<wid>`); cold = user session stopped first, warm = shared session kept and only the app restarted; firstPainted = additional time until the adopted window's pixels change",
    },
    inputLatency,
    typingBurst,
    idle,
    scroll,
    animation,
    video,
    direct,
    resources: resourceSamples,
    bandwidth: trafficTotal,
    session: {
      stateStart: session,
      stateEnd: sessionStateEnd?.session ? { ...sessionStateEnd.session, url: sessionStateEnd.session.url?.replace(/[0-9a-f]{32}/, "<key>") } : null,
      evidence: sessionEvidenceFinal,
      xpraInfoBefore: beforeInfo ? { ...beforeInfo, frames: undefined } : null,
      xpraInfoMidInteraction: midInteractionInfo ? { ...midInteractionInfo, frames: undefined } : null,
      xpraInfoAfter: afterInfo ? { ...afterInfo } : null,
    },
  };

  const results = {
    schema: "rast-perf-bench/1",
    label: OPTIONS.label,
    generatedAt: new Date().toISOString(),
    options: { ...OPTIONS, creds: undefined, baseUrl: OPTIONS.baseUrl },
    target: {
      baseUrl: OPTIONS.baseUrl,
      user: { username: credentials.user.username, id: userId },
      identity,
      app: { id: appId, name: appName, backend: app.backend ?? "native (inferred from the adopted native window)" },
      sessionUnit: `rast-xpra-u${userId}`,
      display: session?.display ?? null,
      runtimeDir: `/run/rast-linux/${identity.uid}`,
    },
    environment: {
      hostStart,
      hostEnd,
      adminFacts,
      benchPage: benchUrl,
      chromium: CHROMIUM,
      note: "single host; headless Playwright Chromium logged in through the real login form and drove the real desktop shell",
    },
    measurements,
    notes: [
      "All timings are taken in the Node process with Date.now(); the browser and the RAST server are on the same host but --base-url defaults to the public HTTPS URL (Cloudflare/nginx path), which is the intended production measurement.",
      "Input latency and typing-burst drain are upper bounds: the display is sampled with screenshots, so the reported resolution is the poll granularity below.",
      "Scrolling/animation/video FPS are reported three ways: distinct CDP screencast frames (painted compositor frames), distinct fixed-interval screenshots (lower bound), and server-side encoded frames from xpra info deltas.",
      "The app window for the interaction phases is resized through the native shim to 1440x860; the benchmark page is served by this harness on 127.0.0.1 and loaded inside the remote Chrome through its own address bar.",
      "CPU/memory come from cgroup v2 of the user's rast-xpra-u<id> session unit plus every active rast-app-* unit of that identity.",
      "Bandwidth counts WebSocket payload bytes of the Xpra HTML5 client as observed by Playwright (application-level, after Xpra compression); it includes all channels of the session.",
      "The harness only touches the benchmark user's own session through the RAST API; it never stops systemd units or production services directly.",
    ],
  };

  writeFileSync(JSON_PATH, `${JSON.stringify(results, null, 2)}\n`);
  const markdown = renderMarkdown(results);
  writeFileSync(MD_PATH, markdown);
  writeFileSync(EVIDENCE_PATH, renderEvidence(results));
  log(`results: ${JSON_PATH}`);
  log(`markdown: ${MD_PATH}`);
  log(`evidence: ${EVIDENCE_PATH}`);
  console.log(`\n${markdown}`);
  return 0;
}

// ---------------------------------------------------------------------------
// Video fixture (generated once, git-ignored tmp directory)
// ---------------------------------------------------------------------------
const BENCH_PORT = Number(process.env.RAST_BENCH_PORT || 8799);
const VIDEO_FIXTURE = join(REPO, "tests/e2e/tmp/fixture.webm");

function ensureVideoFixture() {
  if (statExists(VIDEO_FIXTURE)) return;
  mkdirSync(dirname(VIDEO_FIXTURE), { recursive: true });
  for (const candidate of ["/usr/bin/ffmpeg", "/home/lufria/.cache/ms-playwright/ffmpeg-1013/ffmpeg-linux"]) {
    if (!statExists(candidate)) continue;
    try {
      execFileSync(candidate, ["-y", "-f", "lavfi", "-i", "testsrc2=size=960x540:rate=30", "-t", "6", "-c:v", "libvpx", "-b:v", "1200k", "-an", VIDEO_FIXTURE], { stdio: "ignore", timeout: 120000 });
      log(`generated video fixture ${VIDEO_FIXTURE}`);
      return;
    } catch { /* try next candidate */ }
  }
  warn("could not generate a video fixture (ffmpeg missing)");
}

// ---------------------------------------------------------------------------
// Markdown rendering
// ---------------------------------------------------------------------------
function renderMarkdown(results) {
  const m = results.measurements;
  const res = (name) => (m.resources ?? []).find((sample) => sample.phase === name) ?? null;
  const lines = [];
  const row = (label, stat, unit = "ms", extra = "") => {
    if (!stat || !stat.n) { lines.push(`| ${label} | n/a | n/a | n/a | n/a | ${unit} | ${extra} |`); return; }
    lines.push(`| ${label} | ${fmt(stat.p50, 1)} | ${fmt(stat.p95, 1)} | ${fmt(stat.min, 1)}–${fmt(stat.max, 1)} | ${stat.n} | ${unit} | ${extra} |`);
  };
  const scalar = (label, value, unit, extra = "") => lines.push(`| ${label} | ${fmt(value, 1)} | — | — | 1 | ${unit} | ${extra} |`);

  lines.push(`# RAST native runtime performance — ${results.label}`);
  lines.push("");
  lines.push(`Generated ${results.generatedAt} against \`${results.target.baseUrl}\` as \`${results.target.user.username}\` (${results.target.app.name}, \`${results.target.app.id}\`, backend \`${results.target.app.backend ?? "?"}\`).`);
  lines.push(`Raw JSON: \`${OUT_PREFIX}.json\`; evidence: \`${OUT_PREFIX}-evidence.txt\`.`);
  lines.push("");
  lines.push("## Environment");
  lines.push("");
  lines.push("| Fact | Value |");
  lines.push("|---|---|");
  lines.push(`| RAST version | ${results.environment.hostStart.rastVersion ?? "n/a"} |`);
  lines.push(`| Xpra | ${results.environment.hostStart.xpraVersion ?? "n/a"} |`);
  lines.push(`| Chrome | ${results.environment.hostStart.chromeVersion ?? "n/a"} (deb ${results.environment.hostStart.chromeDebVersion ?? "?"}) |`);
  lines.push(`| Display | Xvfb software (:${results.target.display ?? "?"}), ${results.environment.hostStart.cpus} vCPU ${results.environment.hostStart.cpuModel}, load ${fmt(results.environment.hostStart.load1, 2)} start → ${fmt(results.environment.hostEnd.load1, 2)} end |`);
  lines.push(`| Session unit | \`${results.target.sessionUnit}\` (uid ${results.target.identity.uid}) |`);
  lines.push(`| rast.service | ${results.environment.hostStart.rastService?.ActiveState ?? "?"} since ${results.environment.hostStart.rastService?.ExecMainStartTimestamp ?? "?"} |`);
  lines.push(`| Negotiated encoder | \`${m.session.xpraInfoBefore?.encoder ?? "n/a"}\`${m.session.xpraInfoBefore?.window ? ` (${m.session.xpraInfoBefore.window.width}x${m.session.xpraInfoBefore.window.height}, quality ${fmt(m.session.xpraInfoBefore.window.qualityCurrent)}, speed ${fmt(m.session.xpraInfoBefore.window.speedCurrent)})` : ""} |`);
  lines.push(`| Xpra batch config | min-delay ${fmt(m.session.xpraInfoMidInteraction?.batchMinDelayMs ?? m.session.xpraInfoBefore?.batchMinDelayMs)} ms, max-delay ${fmt(m.session.xpraInfoMidInteraction?.batchMaxDelayMs ?? m.session.xpraInfoBefore?.batchMaxDelayMs)} ms, expire ${fmt(m.session.xpraInfoMidInteraction?.batchExpireMs ?? m.session.xpraInfoBefore?.batchExpireMs)} ms |`);
  lines.push(`| Encodings offered by client | ${m.session.xpraInfoBefore?.encodingsAvailable ?? "n/a"} |`);
  lines.push(`| Failure injection / profile | see the AFTER section and the deploy note in docs/BENCHMARKS.md |`);
  lines.push("");
  lines.push("## Negotiated Xpra encoding / latency evidence");
  lines.push("");
  lines.push("`xpra info` of the benchmark user's session, captured before the interaction phases, while the person was typing (mid-interaction), and after the measured phases. `batch delay` is Xpra's adaptive batching delay: the larger it is, the longer a keypress can wait before its frame is encoded (this is the main suspect for typing/scroll lag).");
  lines.push("");
  lines.push("| Field | before | mid-interaction | after |");
  lines.push("|---|---|---|---|");
  const ev = (get) => (info) => {
    if (!info) return "n/a";
    const value = get(info);
    if (value === undefined || value === null || (typeof value === "number" && !Number.isFinite(value))) return "n/a";
    return Number.isFinite(value) ? fmt(value, 1) : value;
  };
  const rowEv = (label, get) => lines.push(`| ${label} | ${ev(get)(m.session.xpraInfoBefore)} | ${ev(get)(m.session.xpraInfoMidInteraction)} | ${ev(get)(m.session.xpraInfoAfter)} |`);
  rowEv("encoder (dominant by painted pixels)", (info) => info?.encoder);
  rowEv("window size", (info) => info?.window ? `${info.window.width}x${info.window.height}` : "n/a");
  rowEv("quality.cur / avg", (info) => info?.window ? `${fmt(info.window.qualityCurrent)} / ${fmt(info.window.qualityAverage)}` : "n/a");
  rowEv("speed.cur / avg", (info) => info?.window ? `${fmt(info.window.speedCurrent)} / ${fmt(info.window.speedAverage)}` : "n/a");
  rowEv("batch delay avg (ms)", (info) => info?.batchDelayAvgMs);
  rowEv("batch delay max (ms)", (info) => info?.batchDelayMaxMs);
  rowEv("batch min-delay / max-delay (ms)", (info) => info ? `${fmt(info.batchMinDelayMs)} / ${fmt(info.batchMaxDelayMs)}` : "n/a");
  rowEv("damage target-latency (ms)", (info) => info?.window?.targetLatencyMs);
  rowEv("damage frame-total latency (ms)", (info) => info?.damageFrameTotalLatencyMs);
  rowEv("ping latency p50 / p90 / max (ms)", (info) => info ? `${fmt(info.clientPingLatencyMs?.p50)} / ${fmt(info.clientPingLatencyMs?.p90)} / ${fmt(info.clientPingLatencyMs?.max)}` : "n/a");
  rowEv("compressor", (info) => info?.compressor);
  lines.push("");
  lines.push("Server-side totals at the end of the run: " + (`${fmt(m.session.xpraInfoAfter?.damageFps)} damage fps, output packetcount ${fmt(m.session.xpraInfoAfter?.outputPacketCount)}, input packetcount ${fmt(m.session.xpraInfoAfter?.inputPacketCount)}` ) + ".");
  lines.push("");
  lines.push("## Baseline table");
  lines.push("");
  lines.push("| Measurement | p50 | p95 | min–max | n | unit | notes |");
  lines.push("|---|---|---|---|---|---|---|");
  row("Cold launch — click → adopted window", m.launch.cold.adoptedMs, "ms", "session stopped first");
  row("Cold launch — click → placeholder window", m.launch.cold.startingMs, "ms", "shell responsiveness, not app");
  row("Cold launch — adopted window → first painted", m.launch.cold.firstPaintedMs, "ms", "additional paint wait");
  row("Warm launch — click → adopted window", m.launch.warm.adoptedMs, "ms", "shared session kept");
  row("Warm launch — click → placeholder window", m.launch.warm.startingMs, "ms", "");
  row("Warm launch — adopted window → first painted", m.launch.warm.firstPaintedMs, "ms", "");
  row("Input latency — keydown → changed pixel", m.inputLatency, "ms", `screenshot polling; poll granularity ${fmt(m.inputLatency.pollGranularityMs?.p50, 1)} ms`);
  row("Typing burst 40 chars — total (first key → final frame)", m.typingBurst.totalMs, "ms", "upper bound");
  row("Typing burst 40 chars — dispatch (first → last key)", m.typingBurst.dispatchMs, "ms", "");
  row("Typing burst 40 chars — backlog drain (last key → final frame)", m.typingBurst.drainMs, "ms", "upper bound");
  if (m.idle) {
    scalar("Idle CPU (session + app units)", res("idle")?.cpuPercent, "% of one core", `${m.idle.idleSeconds} s quiet`);
    scalar("Idle memory — memory.current", (res("idle")?.memoryEndBytes ?? NaN) / 1024 / 1024, "MiB", "");
    scalar("Idle memory — memory.peak", (res("idle")?.memoryPeakBytes ?? NaN) / 1024 / 1024, "MiB", "");
    scalar("Idle painted frames (screencast)", m.idle.cast?.distinctFps, "fps", "baseline noise on a static page");
  }
  if (m.scroll) {
    scalar("Scroll — server-encoded frames", res("scroll")?.xpraFramesPerSecond, "fps", "xpra info total_frames delta / duration");
    scalar("Scroll — painted frames (screencast)", m.scroll.cast?.distinctFps, "fps", `${m.scroll.cast?.distinctFrames ?? "?"} distinct frames in ${fmt(m.scroll.cast?.durationMs)} ms`);
    scalar("Scroll — painted frames (screenshot sampling)", m.scroll.sampled?.distinctFps, "fps", `lower bound, ${m.scroll.sampled?.samples ?? "?"} samples`);
    scalar("Scroll — frame interval p50 / p95", m.scroll.cast?.frameDeltasMs?.p50, "ms", `p95 ${fmt(m.scroll.cast?.frameDeltasMs?.p95)} ms`);
    scalar("Scroll — CPU during phase", res("scroll")?.cpuPercent, "% of one core", "");
  }
  if (m.animation) {
    scalar("Animation (60 Hz canvas) — painted frames (screencast)", m.animation.cast?.distinctFps, "fps", "");
    scalar("Animation (60 Hz canvas) — server-encoded frames", res("animation")?.xpraFramesPerSecond, "fps", "");
    scalar("Animation — CPU during phase", res("animation")?.cpuPercent, "% of one core", "");
  }
  if (m.video) {
    scalar("Video (local VP8) — painted frames (screencast)", m.video.cast?.distinctFps, "fps", "");
    scalar("Video (local VP8) — server-encoded frames", res("video")?.xpraFramesPerSecond, "fps", "");
    scalar("Video — CPU during phase", res("video")?.cpuPercent, "% of one core", "");
  }
  scalar("Direct Xpra session page — time to first paint", m.direct?.firstPaintMs, "ms", "desktop shell excluded");
  scalar("bandwidth — session total received", m.bandwidth.receivedBytes / 1024 / 1024, "MiB", `${m.bandwidth.receivedFrames} WS frames`);
  scalar("bandwidth — session total sent", m.bandwidth.sentBytes / 1024 / 1024, "MiB", `${m.bandwidth.sentFrames} WS frames`);
  lines.push("");
  lines.push("## Per-phase resources and bandwidth");
  lines.push("");
  lines.push("| Phase | duration s | CPU % of one core | memory start → end (MiB) | memory peak (MiB) | server-encoded frames | WS received KiB | WS sent KiB |");
  lines.push("|---|---|---|---|---|---|---|---|");
  for (const sample of m.resources ?? []) {
    lines.push(`| ${sample.phase} | ${fmt(sample.durationMs / 1000, 1)} | ${fmt(sample.cpuPercent, 1)} | ${mib(sample.memoryStartBytes)} → ${mib(sample.memoryEndBytes)} | ${mib(sample.memoryPeakBytes)} | ${sample.xpraFrames ?? "n/a"} | ${fmt((sample.traffic?.receivedBytes ?? 0) / 1024, 0)} | ${fmt((sample.traffic?.sentBytes ?? 0) / 1024, 0)} |`);
  }
  lines.push("");
  lines.push("## Method");
  lines.push("");
  lines.push("* Headless Playwright Chromium (same build as `tests/e2e/lib/browser.mjs`) logs in through the real login form, then drives the real desktop: start menu, adopted native windows, taskbar.");
  lines.push("* The remote Chrome is navigated (through its own address bar) to a benchmark page served by the harness on `127.0.0.1`; the page paints a large swatch and changes the whole page background on every key, so one changed window pixel proves the key reached the display. A pure-black strip lets the harness locate the remote page origin inside the adopted window.");
  lines.push("* Input latency: 20 samples of keydown → first changed screenshot pixel, reported p50/p95. Screenshot polling makes this an upper bound; the measured poll granularity is included.");
  lines.push("* Typing burst: 40 chars with `delay: 0`; dispatch time, backlog drain (last key → first stable frame) and total are reported.");
  lines.push("* Launch latency: cold = the user's native session is stopped first (`POST /api/linux-apps/session/close`), warm = the shared Xpra session stays alive and only the app is restarted; the click target is the start-menu tile with the menu already open, the end point is the first adopted RAST window (`linux:<app>:<wid>`).");
  lines.push("* FPS: (a) distinct CDP screencast frames (painted compositor frames, hash of each JPEG frame), (b) distinct fixed-interval screenshots (lower bound), (c) server-encoded frames from consecutive `xpra info` `total_frames.*` deltas.");
  lines.push("* CPU/memory: cgroup v2 `cpu.stat`/`memory.current`/`memory.peak` of `rast-xpra-u<id>` plus every active `rast-app-*` unit of that identity, sampled before/after each phase.");
  lines.push("* Bandwidth: WebSocket payload bytes of the Xpra HTML5 client, observed by Playwright `websocket` events (application-level, after Xpra compression).");
  lines.push("* Evidence: `xpra info` of the benchmark user's session (`sudo -u rast-u<id> xpra info --socket-dir=/run/rast-linux/<uid>/sockets :<display>`), `systemctl show` of the session unit, and the encoding lines of the session journal.");
  lines.push("");
  lines.push("## Limitations");
  lines.push("");
  lines.push("* The browser is headless Chromium; software Xvfb on the server; no GPU and no hardware video decode anywhere in the path.");
  lines.push("* `--base-url` points at the public HTTPS URL by default, so timings include Cloudflare/nginx and the WAN round trip (the actual user path). Use the loopback URL for a LAN-free reference, but keep the same URL for BEFORE/AFTER.");
  lines.push("* Input latency, typing drain and screenshot-based FPS are upper bounds limited by the display-sampling interval; they are comparable across runs on the same host.");
  lines.push("* The CDP screencast used for 'painted compositor frames' is throttled by headless Chromium (~20 fps ceiling on this host), so it is a lower bound and not a 60 fps-capable measurement; the server-side encoded-frame count from `xpra info` is the authoritative frame-rate number.");
  lines.push("* `xpra info` ping/latency fields measure the server↔proxy/browser path as seen by the Xpra server and are corroboration, not an independent measurement.");
  lines.push("* One benchmark user at a time; the host may run other workloads (load average recorded in the JSON).");
  lines.push("");
  lines.push("## Comparison with the other conditions");
  lines.push("");
  lines.push("The three-condition comparison (BEFORE / AFTER-balanced / AFTER-responsive), the");
  lines.push("deployment/environment delta, the final profile decision and the interpretation of");
  lines.push("the encoder-selection difference live in `docs/BENCHMARKS.md` §7. Re-run this");
  lines.push("benchmark with the same command and a new `--label` to extend the comparison:");
  lines.push("");
  lines.push("```bash");
  lines.push("node tests/e2e/perf/bench.mjs --label=<condition> --base-url=" + (results.target.baseUrl || ""));
  lines.push("```");
  lines.push("");
  return `${lines.join("\n")}\n`;
}

function renderEvidence(results) {
  const session = results.measurements.session;
  const lines = [];
  lines.push(`# Benchmark evidence — ${results.label} (${results.generatedAt})`);
  lines.push("");
  lines.push(`Session unit: ${results.target.sessionUnit}; display :${results.target.display}; uid ${results.target.identity.uid}`);
  lines.push("");
  lines.push("## Xpra encoder modules on the host (xpra encoding)");
  lines.push("");
  lines.push("```");
  for (const line of results.environment.hostStart.xpraEncoders ?? []) lines.push(line);
  lines.push("```");
  lines.push("");
  lines.push("## Session unit (systemctl show, ExecStart)");
  lines.push("");
  lines.push("```");
  lines.push(session.evidence?.execStart?.trim?.() ?? session.evidence?.execStart ?? "n/a");
  lines.push("```");
  lines.push("");
  lines.push("## Session unit environment (Xpra batching / profile flags)");
  lines.push("");
  lines.push("```");
  lines.push(session.evidence?.environment?.trim?.() ?? session.evidence?.environment ?? "n/a");
  lines.push("```");
  lines.push("");
  lines.push("## Session unit properties");
  lines.push("");
  lines.push("```");
  lines.push(JSON.stringify(session.evidence?.show ?? session.evidence?.sessionUnitAfter ?? {}, null, 1));
  lines.push("```");
  lines.push("");
  lines.push("## xpra info (selected fields, before the interaction phases)");
  lines.push("");
  lines.push("```");
  for (const line of session.xpraInfoBefore?.selectedLines ?? []) lines.push(line);
  lines.push("```");
  lines.push("");
  lines.push("## xpra info (selected fields, MID-INTERACTION — captured while typing)");
  lines.push("");
  lines.push("```");
  for (const line of session.xpraInfoMidInteraction?.selectedLines ?? []) lines.push(line);
  lines.push("```");
  lines.push("");
  lines.push("## xpra info (selected fields, after the interaction phases)");
  lines.push("");
  lines.push("```");
  for (const line of session.xpraInfoAfter?.selectedLines ?? []) lines.push(line);
  lines.push("```");
  lines.push("");
  lines.push("## Session journal — encoding/quality lines");
  lines.push("");
  lines.push("```");
  for (const line of session.evidence?.journalEncodingLines ?? []) lines.push(line);
  lines.push("```");
  lines.push("");
  return `${lines.join("\n")}\n`;
}

// `--render-only=<file.json>` re-renders the markdown report for an existing
// result JSON without re-running the benchmark (used to regenerate reports after
// renderer fixes; no measurement data is touched).
const RENDER_ONLY = arg("render-only", null);
if (RENDER_ONLY) {
  const source = String(RENDER_ONLY);
  const results = JSON.parse(readFileSync(source, "utf8"));
  const mdPath = source.replace(/\.json$/, ".md");
  writeFileSync(mdPath, renderMarkdown(results));
  console.log(`rendered ${mdPath}`);
  process.exit(0);
}

ensureVideoFixture();
main()
  .then((code) => process.exit(code ?? 0))
  .catch((error) => {
    console.error("bench failed:", error);
    process.exit(2);
  });
