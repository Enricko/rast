// Host-side helpers: process/session introspection and safe command execution.
//
// The harness runs on the same host as the dev RAST instance, so scenarios
// can verify what actually happens outside the browser: Xpra sessions, native
// systemd units, per-user cgroups, Docker copies and installed packages.
// Every privileged command goes through sudo (passwordless in this
// environment); no command ever targets the production instance.

import { execFile } from "node:child_process";
import { existsSync, readFileSync, readdirSync, statSync } from "node:fs";
import { promisify } from "node:util";

const execFileAsync = promisify(execFile);

export async function run(command, args = [], { timeoutMs = 30_000, env, input } = {}) {
  try {
    const { stdout, stderr } = await execFileAsync(command, args, {
      timeout: timeoutMs,
      maxBuffer: 16 * 1024 * 1024,
      env: env ? { ...process.env, ...env } : process.env,
      input,
    });
    return { ok: true, stdout, stderr, code: 0 };
  } catch (error) {
    return {
      ok: false,
      stdout: error.stdout ?? "",
      stderr: error.stderr ?? String(error),
      code: error.code ?? -1,
    };
  }
}

export function sudo(command, args = [], options = {}) {
  return run("sudo", [command, ...args], options);
}

/** Run a shell command as root without going through a login shell. */
export function sudoSh(script, options = {}) {
  return run("sudo", ["bash", "-c", script], options);
}

export async function mustRun(command, args = [], options = {}) {
  const result = await run(command, args, options);
  if (!result.ok) {
    throw new Error(`${command} ${args.join(" ")} failed (${result.code}): ${result.stderr || result.stdout}`);
  }
  return result.stdout;
}

export async function mustSudo(command, args = [], options = {}) {
  const result = await sudo(command, args, options);
  if (!result.ok) {
    throw new Error(`sudo ${command} ${args.join(" ")} failed (${result.code}): ${result.stderr || result.stdout}`);
  }
  return result.stdout;
}

export function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

export function readMaybe(path) {
  try {
    return readFileSync(path, "utf8");
  } catch {
    return null;
  }
}

export async function waitFor(fn, { timeoutMs = 30_000, intervalMs = 500, what = "condition" } = {}) {
  const started = Date.now();
  let last;
  for (;;) {
    try {
      last = await fn();
      if (last) return last;
    } catch (error) {
      last = error;
    }
    if (Date.now() - started > timeoutMs) {
      throw new Error(`timed out after ${timeoutMs} ms waiting for ${what} (last: ${last})`);
    }
    await sleep(intervalMs);
  }
}

// ---------------------------------------------------------------------------
// Dev instance
// ---------------------------------------------------------------------------

/** Environment of a process, as a Map, read from /proc (requires root). */
export async function processEnv(pid) {
  const text = readMaybe(`/proc/${pid}/environ`);
  if (text) {
    const map = new Map();
    for (const pair of text.split("\0")) {
      const index = pair.indexOf("=");
      if (index > 0) map.set(pair.slice(0, index), pair.slice(index + 1));
    }
    return map;
  }
  const result = await sudoSh(`cat /proc/${pid}/environ 2>/dev/null | tr '\\0' '\\n'`);
  const map = new Map();
  for (const pair of result.stdout.split("\n")) {
    const index = pair.indexOf("=");
    if (index > 0) map.set(pair.slice(0, index), pair.slice(index + 1));
  }
  return map;
}

export async function instancePids() {
  const result = await sudo("pgrep", ["-af", "^/tmp/rast-e2e/bin/rast"]).catch(() => null);
  const out = result?.stdout ?? (await sudoSh("pgrep -af '^/tmp/rast-e2e/bin/rast' || true")).stdout;
  const pids = {};
  for (const line of out.split("\n")) {
    const match = line.match(/^(\d+)\s+(.*)$/);
    if (!match) continue;
    if (match[2].includes(" worker")) pids.worker = Number(match[1]);
    else if (match[2].includes("native-helper")) pids.helper = Number(match[1]);
    else if (match[2].includes(" serve")) pids.server = Number(match[1]);
  }
  return pids;
}

export async function healthz(baseUrl) {
  const response = await fetch(`${baseUrl}/healthz`).catch(() => null);
  return response?.status === 200;
}

// ---------------------------------------------------------------------------
// Native identities, Xpra sessions and systemd units
// ---------------------------------------------------------------------------

export async function linuxIdentities(uidBase = 210000) {
  const out = await mustRun("getent", ["passwd"]).catch(() => "");
  const identities = [];
  for (const line of out.split("\n")) {
    const parts = line.split(":");
    if (parts.length < 7) continue;
    const username = parts[0];
    const match = username.match(/^rast-u(\d+)$/);
    if (!match) continue;
    identities.push({
      username,
      userId: Number(match[1]),
      uid: Number(parts[2]),
      gid: Number(parts[3]),
      home: parts[5],
    });
  }
  return identities;
}

/** Running `rast-app-*` systemd transient units. */
export async function appUnits() {
  const result = await sudo("systemctl", ["list-units", "--type=service", "--all", "--no-legend", "--plain", "rast-app-*"]).catch(
    () => null,
  );
  const out = result?.stdout ?? "";
  return out
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => {
      const parts = line.split(/\s+/);
      return { unit: parts[0], load: parts[1], active: parts[2], sub: parts[3], description: parts.slice(4).join(" ") };
    });
}

/** cgroup v2 directory of a unit/slice, e.g. /sys/fs/cgroup/rast.slice/rast-linux.slice/rast-xpra-u2.service */
export async function unitCgroupDir(unit) {
  const show = await run("systemctl", ["show", unit, "--property=ControlGroup", "--value"]);
  const control = (show.stdout || "").trim();
  if (show.ok && control && control !== "/" ) return `/sys/fs/cgroup${control}`;
  // Fall back to searching the usual locations.
  for (const prefix of [
    `/sys/fs/cgroup/system.slice/${unit}`,
    `/sys/fs/cgroup/rast.slice/rast-linux.slice/${unit}`,
    `/sys/fs/cgroup/${unit}`,
  ]) {
    if (existsSync(prefix)) return prefix;
  }
  return null;
}

/** Read a cgroup file, via sudo when the current account cannot read it. */
export async function cgroupFile(path) {
  if (!path) return null;
  const direct = readMaybe(path);
  if (direct !== null) return direct;
  const result = await sudo("cat", [path]);
  return result.ok ? result.stdout : null;
}

export async function unitStatus(unit) {
  const show = await run("systemctl", [
    "show",
    unit,
    "--property=ActiveState,SubState,MainPID,MemoryCurrent,CPUUsageNSec,TasksCurrent,Result",
  ]);
  const status = {};
  for (const line of show.stdout.split("\n")) {
    const index = line.indexOf("=");
    if (index > 0) status[line.slice(0, index)] = line.slice(index + 1);
  }
  return status;
}

/** Session units `rast-xpra-u<id>` that exist on the host. */
export async function sessionUnits() {
  const result = await sudo("systemctl", [
    "list-units",
    "--type=service",
    "--all",
    "--no-legend",
    "--plain",
    "rast-xpra-*",
  ]).catch(() => null);
  const out = result?.stdout ?? "";
  return out
    .split("\n")
    .map((line) => line.trim())
    .filter(Boolean)
    .map((line) => {
      const parts = line.split(/\s+/);
      return { unit: parts[0], load: parts[1], active: parts[2], sub: parts[3] };
    });
}

/** Memory and CPU of one systemd unit from its cgroup. */
/**
 * Memory/CPU of one systemd unit, from `systemctl show` and its real cgroup
 * (custom slices live under /sys/fs/cgroup/rast.slice/..., never system.slice).
 */
export async function unitResources(unit) {
  const status = await unitStatus(unit);
  const cgroup = await unitCgroupDir(unit);
  let memoryCurrent = Number(status.MemoryCurrent);
  if (!Number.isFinite(memoryCurrent) || memoryCurrent <= 0) {
    const text = await cgroupFile(cgroup ? `${cgroup}/memory.current` : null);
    memoryCurrent = text ? Number(text.trim()) : NaN;
  }
  const cpuText = await cgroupFile(cgroup ? `${cgroup}/cpu.stat` : null);
  let cpuUsageUsec = NaN;
  if (cpuText) {
    const match = cpuText.match(/^usage_usec (\d+)$/m);
    if (match) cpuUsageUsec = Number(match[1]);
  }
  const peakText = await cgroupFile(cgroup ? `${cgroup}/memory.peak` : null);
  const memoryPeak = Number((peakText ?? "").trim());
  return {
    unit,
    cgroup,
    active: status.ActiveState,
    sub: status.SubState,
    mainPid: Number(status.MainPID),
    tasks: Number(status.TasksCurrent),
    memoryBytes: memoryCurrent,
    memoryPeakBytes: memoryPeak,
    cpuUsageUsec,
  };
}

/** Process ids below a cgroup (reads of /sys/fs/cgroup need root here). */
export async function cgroupPids(unit) {
  const dir = await unitCgroupDir(unit);
  if (!dir) return [];
  const text = await cgroupFile(`${dir}/cgroup.procs`);
  return (text ?? "")
    .split("\n")
    .map((line) => Number(line.trim()))
    .filter((pid) => Number.isInteger(pid) && pid > 0);
}

export async function unitPids(unit) {
  const result = await sudoSh(
    `systemctl show ${JSON.stringify(unit)} --property=MainPID --value; ` +
      `cat /sys/fs/cgroup/system.slice/${unit}/cgroup.procs 2>/dev/null || true`,
  );
  return result.stdout
    .split("\n")
    .map((line) => Number(line.trim()))
    .filter((pid) => Number.isInteger(pid) && pid > 0);
}

/** PIDs below one systemd unit, with titles, for X11 windows of the session. */
export async function x11WindowsOfUnit(unit) {
  const pids = await unitPids(unit);
  if (pids.length === 0) return [];
  return pids.map((pid) => ({ pid, comm: readMaybe(`/proc/${pid}/comm`)?.trim() ?? "" }));
}

// ---------------------------------------------------------------------------
// Processes
// ---------------------------------------------------------------------------

/** All processes whose command line matches a regex (as root). */
export async function pgrep(regex) {
  const result = await sudo("pgrep", ["-af", regex]);
  if (!result.ok && !result.stdout.trim()) return [];
  return result.stdout
    .split("\n")
    .filter(Boolean)
    .map((line) => {
      const match = line.match(/^(\d+)\s+(.*)$/);
      return match ? { pid: Number(match[1]), cmd: match[2] } : { pid: 0, cmd: line };
    })
    .filter((entry) => entry.pid > 0);
}

export function readCgroupFile(path) {
  const text = readMaybe(path);
  return text ? text.trim() : null;
}

/** Sum RSS (bytes) of a set of pids from /proc/<pid>/status (root). */
export async function sumRss(pids) {
  const script = pids.map((pid) => `awk '/VmRSS/ {print $2}' /proc/${pid}/status 2>/dev/null`).join("; ");
  const result = await sudoSh(script || "true");
  const kb = result.stdout
    .split("\n")
    .map((line) => Number(line.trim()))
    .filter((value) => Number.isFinite(value))
    .reduce((total, value) => total + value, 0);
  return kb * 1024;
}

export async function systemdCgls(pattern = "rast-xpra") {
  const result = await sudo("systemd-cgls", ["--no-pager", "-u", pattern]).catch(() => null);
  return result?.stdout ?? "";
}

// ---------------------------------------------------------------------------
// Docker (via sudo; the operator account is not in the docker group)
// ---------------------------------------------------------------------------

export async function docker(args, options = {}) {
  return sudo("docker", args, options);
}

export async function dockerPs(filterArgs = []) {
  const result = await docker([
    "ps",
    "-a",
    "--format",
    "{{.Names}}\t{{.Image}}\t{{.Status}}\t{{.Labels}}",
    ...filterArgs,
  ]);
  if (!result.ok) return [];
  return result.stdout
    .split("\n")
    .filter(Boolean)
    .map((line) => {
      const [name, image, status, labels] = line.split("\t");
      const labelMap = {};
      for (const pair of (labels ?? "").split(",")) {
        const index = pair.indexOf("=");
        if (index > 0) labelMap[pair.slice(0, index)] = pair.slice(index + 1);
      }
      return { name, image, status, labels: labelMap };
    });
}

export async function dockerStats(names) {
  const result = await docker(["stats", "--no-stream", "--format", "{{.Name}}\t{{.MemUsage}}\t{{.CPUPerc}}", ...names]);
  if (!result.ok) return [];
  return result.stdout
    .split("\n")
    .filter(Boolean)
    .map((line) => {
      const [name, memory, cpu] = line.split("\t");
      return { name, memory, cpu };
    });
}

export async function dockerVolumes(filterArgs = []) {
  const result = await docker(["volume", "ls", "--format", "{{.Name}}", ...filterArgs]);
  return result.ok ? result.stdout.split("\n").filter(Boolean) : [];
}

export async function dockerImages(repository) {
  const result = await docker(["images", "--format", "{{.Repository}}:{{.Tag}}", repository]);
  return result.ok ? result.stdout.split("\n").filter(Boolean) : [];
}

/** cgroup v2 directory of a running Docker container (system.slice docker scope). */
export async function dockerContainerCgroup(name) {
  const inspect = await docker(["inspect", "-f", "{{.Id}}", name]);
  if (!inspect.ok) return null;
  const id = inspect.stdout.trim();
  if (!id) return null;
  for (const dir of [
    `/sys/fs/cgroup/system.slice/docker-${id}.scope`,
    `/sys/fs/cgroup/docker/${id}`,
  ]) {
    if (existsSync(dir)) return dir;
  }
  return null;
}

/** Memory and CPU of a set of containers from their cgroups (comparable with native units). */
export async function dockerCgroupResources(names) {
  let memoryBytes = 0;
  let cpuUsec = 0;
  let found = false;
  const dirs = [];
  for (const name of names) {
    const dir = await dockerContainerCgroup(name);
    if (!dir) continue;
    dirs.push({ name, dir });
    const memText = await cgroupFile(`${dir}/memory.current`);
    const cpuText = await cgroupFile(`${dir}/cpu.stat`);
    const memory = memText ? Number(memText.trim()) : NaN;
    const match = cpuText?.match(/^usage_usec (\d+)$/m);
    if (Number.isFinite(memory)) { memoryBytes += memory; found = true; }
    if (match) { cpuUsec += Number(match[1]); found = true; }
  }
  return { names, dirs, memoryBytes: found ? memoryBytes : NaN, cpuUsec: found ? cpuUsec : NaN };
}

export async function dockerTop(name) {
  const result = await docker(["top", name, "-eo", "pid,user,comm,args"]);
  return result.ok ? result.stdout.trim().split("\n").slice(1) : [];
}

// ---------------------------------------------------------------------------
// dpkg (native installs)
// ---------------------------------------------------------------------------

export async function dpkgInstalled(packageName) {
  const result = await run("dpkg-query", ["-W", "-f=${Status}|${Version}", packageName]);
  if (!result.ok) return null;
  const [status, version] = result.stdout.split("|");
  return { status, version, installed: status?.endsWith("installed") ?? false };
}

export async function aptPackageExists(packageName) {
  const result = await run("apt-cache", ["show", packageName]);
  return result.ok && result.stdout.length > 0;
}

// ---------------------------------------------------------------------------
// Metrics helpers
// ---------------------------------------------------------------------------

export function formatBytes(bytes) {
  if (!Number.isFinite(bytes)) return "n/a";
  const units = ["B", "KiB", "MiB", "GiB"];
  let value = bytes;
  let unit = 0;
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024;
    unit += 1;
  }
  return `${value.toFixed(value >= 10 || unit === 0 ? 0 : 1)} ${units[unit]}`;
}

/** CPU percent between two cgroup cpu.stat samples (usage_usec). */
export function cpuPercentBetween(beforeUsec, afterUsec, elapsedMs, cpuCount = 1) {
  if (!Number.isFinite(beforeUsec) || !Number.isFinite(afterUsec) || elapsedMs <= 0) return NaN;
  // 1e6 usec = 1 CPU-second.
  return ((afterUsec - beforeUsec) / 1000 / elapsedMs) * 100;
}

export function fileExists(path) {
  try {
    statSync(path);
    return true;
  } catch {
    return false;
  }
}

export function listDir(path) {
  try {
    return readdirSync(path);
  } catch {
    return [];
  }
}

export function readText(path) {
  return readMaybe(path);
}

export async function readTextSudo(path) {
  const result = await sudo("cat", [path]);
  return result.ok ? result.stdout : null;
}
