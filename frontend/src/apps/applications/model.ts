// Docker engine model: payload shapes of /api/docker/* and the pure helpers the
// Docker app uses to group, filter and label containers, images and volumes.
// No DOM here, so everything is unit-tested in tests/docker.test.ts.

export interface PortMap {
  hostIp?: string;
  hostPort?: number;
  containerPort: number;
  protocol: string;
}

export interface EngineContainer {
  id: string;
  name: string;
  image: string;
  command?: string;
  state: string;
  status: string;
  runningFor?: string;
  ports: PortMap[];
  mounts: string[];
  labels: Record<string, string>;
  project: string;
  service: string;
  managed: boolean;
  slug: string | null;
}

export interface ContainerStat {
  id: string;
  name: string;
  cpuPercent: number;
  memoryBytes: number;
  memoryLimitBytes: number;
  memoryPercent: number;
  netRxBytes: number;
  netTxBytes: number;
  blockReadBytes: number;
  blockWriteBytes: number;
  pids: number;
}

export interface EngineImage {
  id: string;
  repository: string;
  tag: string;
  reference: string;
  digest: string;
  sizeBytes: number | null;
  size: string;
  createdAt: string;
  createdSince: string;
  containers: number | null;
  usedBy: string[];
  inUse: boolean;
}

export interface EngineVolume {
  name: string;
  driver: string;
  scope: string;
  mountpoint: string;
  labels: Record<string, string>;
  project: string;
  anonymous: boolean;
  inUse: boolean;
  usedBy: string[];
  app: string | null;
}

export interface InspectMount {
  type: string;
  source: string;
  target: string;
  name: string;
  readOnly: boolean;
}

export interface InspectInfo {
  id: string;
  name: string;
  image: string;
  state: string;
  ports: PortMap[];
  mounts: InspectMount[];
  env: string[];
  labels: Record<string, string>;
  restartPolicy: string;
  privileged: boolean;
  networkMode: string;
  networks: string[];
  command: string;
  entrypoint: string;
  workingDir: string;
  user: string;
  createdAt: string;
  startedAt: string;
  finishedAt: string;
  exitCode: number | null;
  health: string;
}

export interface ContainerDetail {
  container: EngineContainer;
  inspect: InspectInfo;
  managed: boolean;
  slug: string | null;
  appId: string | null;
  terminal: { allowed: boolean; reason: string };
}

export interface LogLine {
  ts: number;
  stream: string;
  service?: string;
  message: string;
}

/** A managed app as the grouping needs it. */
export interface GroupApp {
  id: string;
  name: string;
  slug: string;
  status: string;
  containers?: Array<{ name: string; state: string; image: string }>;
}

export type GroupKind = "app" | "project" | "single";

export interface ContainerGroup {
  key: string;
  kind: GroupKind;
  name: string;
  app: GroupApp | null;
  containers: EngineContainer[];
}

export type UsageFilter = "all" | "used" | "unused";

export function isRunning(container: { state: string }): boolean {
  return container.state === "running";
}

export function shortId(id: string): string {
  return id.replace(/^sha256:/, "").slice(0, 12);
}

function byName(a: { name: string }, b: { name: string }): number {
  return a.name.localeCompare(b.name);
}

/**
 * Containers grouped the way Docker Desktop shows them: RAST apps first, then
 * other Compose projects, then standalone containers. `containers` is null
 * when this account cannot list the engine (read-only access): app groups then
 * use the containers the app API reports.
 */
export function groupContainers(containers: EngineContainer[] | null, apps: GroupApp[]): ContainerGroup[] {
  const appGroups: ContainerGroup[] = [];
  const projects = new Map<string, EngineContainer[]>();
  const singles: ContainerGroup[] = [];
  const bySlug = new Map<string, EngineContainer[]>();
  for (const container of containers ?? []) {
    if (container.slug) {
      const list = bySlug.get(container.slug) ?? [];
      list.push(container);
      bySlug.set(container.slug, list);
    } else if (container.project) {
      const list = projects.get(container.project) ?? [];
      list.push(container);
      projects.set(container.project, list);
    } else {
      singles.push({ key: `container:${container.id}`, kind: "single", name: container.name, app: null, containers: [container] });
    }
  }
  for (const app of apps) {
    const members = containers
      ? bySlug.get(app.slug) ?? []
      : (app.containers ?? []).map((c, index) => fromAppContainer(app, c, index));
    bySlug.delete(app.slug);
    appGroups.push({ key: `app:${app.id}`, kind: "app", name: app.name || app.slug, app, containers: members.sort(byName) });
  }
  // Containers of an app this list does not know yet (deployed elsewhere a
  // moment ago) still show, as a plain project.
  for (const [slug, members] of bySlug) {
    const project = members[0]?.project || `rast-${slug}`;
    projects.set(project, [...(projects.get(project) ?? []), ...members]);
  }
  const projectGroups: ContainerGroup[] = [...projects].map(([name, members]) => ({
    key: `project:${name}`,
    kind: "project",
    name,
    app: null,
    containers: members.sort(byName),
  }));
  return [...appGroups.sort(byName), ...projectGroups.sort(byName), ...singles.sort(byName)];
}

function fromAppContainer(app: GroupApp, container: { name: string; state: string; image: string }, index: number): EngineContainer {
  return {
    id: `${app.slug}-${index}`,
    name: container.name,
    image: container.image,
    state: container.state,
    status: container.state,
    ports: [],
    mounts: [],
    labels: {},
    project: `rast-${app.slug}`,
    service: "",
    managed: true,
    slug: app.slug,
  };
}

export interface GroupState {
  running: number;
  total: number;
  label: string;
  tone: "ok" | "warn" | "error" | "idle";
}

export function groupState(group: ContainerGroup): GroupState {
  const total = group.containers.length;
  const running = group.containers.filter(isRunning).length;
  if (group.app && (group.app.status === "deploying" || group.app.status === "error") ) {
    return {
      running,
      total,
      label: group.app.status === "deploying" ? "Working…" : "Error",
      tone: group.app.status === "deploying" ? "warn" : "error",
    };
  }
  if (total === 0) return { running, total, label: "Stopped", tone: "idle" };
  if (running === total) return { running, total, label: total > 1 ? `Running (${running}/${total})` : "Running", tone: "ok" };
  if (running === 0) {
    const restarting = group.containers.some((c) => c.state === "restarting");
    return { running, total, label: restarting ? "Restarting" : "Stopped", tone: restarting ? "warn" : "idle" };
  }
  return { running, total, label: `Running (${running}/${total})`, tone: "warn" };
}

export function containerTone(container: { state: string }): GroupState["tone"] {
  switch (container.state) {
    case "running":
      return "ok";
    case "restarting":
    case "paused":
    case "removing":
      return "warn";
    case "dead":
      return "error";
    default:
      return "idle";
  }
}

/** "Running", "Exited (137)", "Paused"… from `docker ps` state + status. */
export function containerStateLabel(container: { state: string; status?: string }): string {
  const state = container.state || "unknown";
  if (state === "exited") {
    const code = /Exited \((-?\d+)\)/.exec(container.status ?? "")?.[1];
    return code !== undefined ? `Exited (${code})` : "Exited";
  }
  return state.charAt(0).toUpperCase() + state.slice(1);
}

export function matchesContainer(container: EngineContainer, query: string): boolean {
  const needle = query.trim().toLowerCase();
  if (!needle) return true;
  return [container.name, container.image, container.project, container.service, container.id]
    .some((value) => value?.toLowerCase().includes(needle));
}

/**
 * Groups narrowed to the search and the "running only" switch. A group whose
 * own name matches keeps all its containers.
 */
export function filterGroups(groups: ContainerGroup[], query: string, runningOnly: boolean): ContainerGroup[] {
  const needle = query.trim().toLowerCase();
  const out: ContainerGroup[] = [];
  for (const group of groups) {
    const nameMatch = !needle || group.name.toLowerCase().includes(needle) || (group.app?.slug ?? "").includes(needle);
    let members = nameMatch ? group.containers : group.containers.filter((c) => matchesContainer(c, needle));
    if (runningOnly) members = members.filter(isRunning);
    const keepEmptyApp = group.kind === "app" && nameMatch && !runningOnly;
    if (members.length === 0 && !keepEmptyApp) continue;
    out.push({ ...group, containers: members });
  }
  return out;
}

export interface PortLink {
  label: string;
  href: string | null;
}

function isDirectHost(hostname: string): boolean {
  if (!hostname) return false;
  if (hostname === "localhost" || hostname.endsWith(".local") || hostname.endsWith(".lan")) return true;
  if (/^\d{1,3}(\.\d{1,3}){3}$/.test(hostname)) return true;
  if (hostname.startsWith("[") || hostname.includes(":")) return true;
  return !hostname.includes(".");
}

/**
 * Published ports as labels ("8080:80") with a browser link when the port is
 * reachable from where RAST is opened: only on a direct address (an IP,
 * localhost, a .local/.lan name). Through a tunnel domain the port is not
 * reachable, so no link is offered. Ports bound to 127.0.0.1 are only linked
 * from the server itself.
 */
export function portLinks(ports: PortMap[], location: { hostname: string; protocol?: string }): PortLink[] {
  const seen = new Set<string>();
  const out: PortLink[] = [];
  const direct = isDirectHost(location.hostname);
  const local = location.hostname === "localhost" || location.hostname === "127.0.0.1";
  for (const port of ports) {
    if (!port.hostPort) continue;
    const label = `${port.hostPort}:${port.containerPort}${port.protocol && port.protocol !== "tcp" ? `/${port.protocol}` : ""}`;
    if (seen.has(label)) continue;
    seen.add(label);
    const loopback = port.hostIp === "127.0.0.1" || port.hostIp === "::1";
    const linkable = port.protocol !== "udp" && direct && (!loopback || local);
    const host = location.hostname.includes(":") && !location.hostname.startsWith("[") ? `[${location.hostname}]` : location.hostname;
    out.push({ label, href: linkable ? `http://${host}:${port.hostPort}` : null });
  }
  return out;
}

export interface Usage {
  cpuPercent: number;
  memoryBytes: number;
  sampled: number;
}

export function usageFor(containers: EngineContainer[], stats: Map<string, ContainerStat>): Usage {
  let cpu = 0;
  let memory = 0;
  let sampled = 0;
  for (const container of containers) {
    const stat = stats.get(container.id) ?? stats.get(container.name);
    if (!stat) continue;
    cpu += stat.cpuPercent;
    memory += stat.memoryBytes;
    sampled += 1;
  }
  return { cpuPercent: cpu, memoryBytes: memory, sampled };
}

/** Stats keyed by full id, short id and name, so any of them finds a sample. */
export function indexStats(items: ContainerStat[]): Map<string, ContainerStat> {
  const map = new Map<string, ContainerStat>();
  for (const item of items) {
    if (item.id) {
      map.set(item.id, item);
      map.set(item.id.slice(0, 12), item);
    }
    if (item.name) map.set(item.name, item);
  }
  return map;
}

export function statFor(container: EngineContainer, stats: Map<string, ContainerStat>): ContainerStat | undefined {
  return stats.get(container.id) ?? stats.get(container.id.slice(0, 12)) ?? stats.get(container.name);
}

export function imageLabel(image: EngineImage): string {
  return image.reference || "<untagged>";
}

export function filterImages(images: EngineImage[], query: string, filter: UsageFilter): EngineImage[] {
  const needle = query.trim().toLowerCase();
  return images
    .filter((image) => (filter === "used" ? image.inUse : filter === "unused" ? !image.inUse : true))
    .filter((image) => !needle || [image.reference, image.id, ...image.usedBy].some((v) => v.toLowerCase().includes(needle)))
    .sort((a, b) => {
      if (!a.reference !== !b.reference) return a.reference ? -1 : 1;
      return imageLabel(a).localeCompare(imageLabel(b)) || a.id.localeCompare(b.id);
    });
}

export function filterVolumes(volumes: EngineVolume[], query: string, filter: UsageFilter): EngineVolume[] {
  const needle = query.trim().toLowerCase();
  return volumes
    .filter((volume) => (filter === "used" ? volume.inUse : filter === "unused" ? !volume.inUse : true))
    .filter((volume) => !needle || [volume.name, volume.project, volume.app ?? "", ...volume.usedBy].some((v) => v.toLowerCase().includes(needle)))
    .sort((a, b) => {
      if (a.anonymous !== b.anonymous) return a.anonymous ? 1 : -1;
      return a.name.localeCompare(b.name);
    });
}

/** Client copy of the server's image reference check, for instant feedback. */
export function imageRefError(value: string): string | null {
  const image = value.trim();
  if (!image) return "Enter an image name, like nginx:latest.";
  if (image.length > 255) return "That name is too long.";
  if (!/^[A-Za-z0-9]/.test(image)) return "The name must start with a letter or a digit.";
  if (!/^[A-Za-z0-9._\-/:@]+$/.test(image)) return "Use only letters, digits and . _ - / : @";
  if (image.includes("//") || image.includes("..")) return "That name has an empty part.";
  if ((image.match(/@/g) ?? []).length > 1) return "That name has more than one digest.";
  if (/[/:@.-]$/.test(image)) return "That name looks incomplete.";
  return null;
}

/** Hide the value of `KEY=value` environment entries. */
export function maskEnv(entry: string): { key: string; value: string } {
  const index = entry.indexOf("=");
  if (index < 0) return { key: entry, value: "" };
  return { key: entry.slice(0, index), value: entry.slice(index + 1) };
}

/** Docker's zero time ("0001-01-01T00:00:00Z") means "never". */
export function dockerTime(value: string): number | null {
  if (!value || value.startsWith("0001-")) return null;
  const ms = Date.parse(value);
  return Number.isFinite(ms) ? ms : null;
}

/** Rolling window of samples for the stats charts. */
export function pushSample(series: number[], value: number, max = 60): number[] {
  const next = [...series, Number.isFinite(value) ? value : 0];
  return next.length > max ? next.slice(next.length - max) : next;
}

// ------------------------------------------------------------------ run image

/** What `GET /api/docker/images/inspect` reports about one image. */
export interface ImageDetails {
  id: string;
  tags: string[];
  exposedPorts: Array<{ port: number; protocol: string }>;
  volumes: string[];
  env: Array<{ key: string; value: string }>;
  cmd: string;
  entrypoint: string;
  workingDir: string;
  user: string;
  os: string;
  architecture: string;
  sizeBytes: number | null;
  created: string;
}

export interface RunPort {
  container: number;
  protocol: string;
  /** Host port to publish on; `null` keeps the port private. */
  host: number | null;
}

export interface RunVolume {
  /** Named volume owned by the app. */
  name: string;
  /** Absolute path inside the container. */
  path: string;
}

export interface RunEnv {
  key: string;
  value: string;
  secret: boolean;
}

/** Everything "Run image" turns into a managed app. */
export interface RunPlan {
  name: string;
  image: string;
  ports: RunPort[];
  volumes: RunVolume[];
  env: RunEnv[];
}

const ENV_KEY = /^[A-Za-z_][A-Za-z0-9_]*$/;
const VOLUME_NAME = /^[A-Za-z0-9][A-Za-z0-9_.-]*$/;

/** Lowercase words joined by dashes, like the server's app slugs. */
export function slugWords(value: string, fallback = "app"): string {
  const slug = value
    .toLowerCase()
    .replace(/[^a-z0-9]+/g, "-")
    .replace(/^-+|-+$/g, "")
    .slice(0, 40)
    .replace(/-+$/, "");
  return slug || fallback;
}

/** A friendly default app name from an image reference:
 * `louislam/uptime-kuma:2` → `uptime-kuma`. */
export function suggestAppName(reference: string): string {
  const withoutDigest = reference.split("@")[0] ?? reference;
  const lastSegment = withoutDigest.split("/").pop() ?? withoutDigest;
  const repository = lastSegment.split(":")[0] ?? lastSegment;
  return slugWords(repository);
}

/** Host ports already published by containers, as `port/protocol`. */
export function usedHostPorts(containers: EngineContainer[]): Set<string> {
  const used = new Set<string>();
  for (const container of containers) {
    for (const port of container.ports ?? []) {
      if (port.hostPort) used.add(`${port.hostPort}/${(port.protocol || "tcp").toLowerCase()}`);
    }
  }
  return used;
}

/** The container's own port when it is free on the host; otherwise the next
 * free one (ports below 1024 move to 8000+, like 80 → 8080). */
export function suggestHostPort(port: number, protocol: string, used: Set<string>): number {
  const free = (candidate: number): boolean => !used.has(`${candidate}/${protocol}`);
  if (free(port)) return port;
  const start = port < 1024 ? 8000 + port : port + 1;
  for (let candidate = start; candidate <= 65535 && candidate < start + 1000; candidate += 1) {
    if (free(candidate)) return candidate;
  }
  return port;
}

/** Volume names for the image's volume paths: `/app/data` → `data`,
 * `/var/lib/postgresql/data` → `postgresql-data` when `data` is taken. */
export function volumesFor(paths: string[]): RunVolume[] {
  const taken = new Set<string>();
  const out: RunVolume[] = [];
  for (const path of paths) {
    const parts = path.split("/").filter(Boolean).map((part) => slugWords(part, ""));
    const last = parts[parts.length - 1] || "data";
    const candidates = [last, parts.slice(-2).filter(Boolean).join("-"), "data"];
    let name = candidates.find((candidate) => candidate && !taken.has(candidate)) ?? "data";
    for (let n = 2; taken.has(name); n += 1) name = `${last}-${n}`;
    taken.add(name);
    out.push({ name, path });
  }
  return out;
}

/** A first plan for an image: its name, every exposed port on a free host
 * port, and a named volume for every volume path. */
export function initialRunPlan(reference: string, details: ImageDetails, containers: EngineContainer[]): RunPlan {
  const used = usedHostPorts(containers);
  const ports: RunPort[] = [];
  for (const exposed of details.exposedPorts) {
    const protocol = exposed.protocol.toLowerCase();
    const host = suggestHostPort(exposed.port, protocol, used);
    used.add(`${host}/${protocol}`);
    ports.push({ container: exposed.port, protocol, host });
  }
  return { name: suggestAppName(reference), image: reference, ports, volumes: volumesFor(details.volumes), env: [] };
}

/** Problems that stop the plan from running, in reading order. */
export function runPlanErrors(plan: RunPlan, used: Set<string> = new Set()): string[] {
  const errors: string[] = [];
  if (!plan.name.trim()) errors.push("Enter a name for the app.");
  else if (plan.name.trim().length > 80) errors.push("The name is too long (80 characters at most).");
  const imageError = imageRefError(plan.image);
  if (imageError) errors.push(imageError);
  const hostPorts = new Set<string>();
  for (const port of plan.ports) {
    if (port.host === null) continue;
    if (!Number.isInteger(port.host) || port.host < 1 || port.host > 65535) {
      errors.push(`Port ${port.container}: the host port must be between 1 and 65535.`);
      continue;
    }
    const key = `${port.host}/${port.protocol}`;
    if (hostPorts.has(key)) errors.push(`Host port ${port.host} is used twice.`);
    else if (used.has(key)) errors.push(`Host port ${port.host} is already used by another container.`);
    hostPorts.add(key);
  }
  const volumeNames = new Set<string>();
  const volumePaths = new Set<string>();
  for (const volume of plan.volumes) {
    if (!VOLUME_NAME.test(volume.name)) errors.push(`Volume “${volume.name || "(empty)"}”: use letters, digits, . _ - and start with a letter or digit.`);
    else if (volumeNames.has(volume.name)) errors.push(`Volume “${volume.name}” is listed twice.`);
    if (!volume.path.startsWith("/") || /["\n\r]/.test(volume.path) || volume.path.includes(":")) errors.push(`Volume “${volume.name}”: the container path must be absolute, like /data.`);
    else if (volumePaths.has(volume.path)) errors.push(`The container path ${volume.path} is used twice.`);
    volumeNames.add(volume.name);
    volumePaths.add(volume.path);
  }
  const keys = new Set<string>();
  for (const variable of plan.env) {
    if (!ENV_KEY.test(variable.key)) errors.push(`Invalid variable name “${variable.key || "(empty)"}”.`);
    else if (keys.has(variable.key)) errors.push(`The variable ${variable.key} is listed twice.`);
    keys.add(variable.key);
  }
  return errors;
}

/** The Compose file for a plan. Values of environment variables stay out of
 * it: they are passed as `${KEY}` and kept in the app's environment, where
 * secrets stay hidden. */
export function composeForRun(plan: RunPlan): string {
  const service = slugWords(plan.name);
  const q = (value: string): string => JSON.stringify(value);
  const lines = ["services:", `  ${service}:`, `    image: ${q(plan.image.trim())}`, "    restart: unless-stopped"];
  const published = plan.ports.filter((port) => port.host !== null);
  if (published.length) {
    lines.push("    ports:");
    for (const port of published) {
      const suffix = port.protocol === "tcp" ? "" : `/${port.protocol}`;
      lines.push(`      - ${q(`${port.host}:${port.container}${suffix}`)}`);
    }
  }
  if (plan.volumes.length) {
    lines.push("    volumes:");
    for (const volume of plan.volumes) lines.push(`      - ${q(`${volume.name}:${volume.path}`)}`);
  }
  if (plan.env.length) {
    lines.push("    environment:");
    for (const variable of plan.env) lines.push(`      ${variable.key}: ${q(`\${${variable.key}}`)}`);
  }
  if (plan.volumes.length) {
    lines.push("volumes:");
    for (const volume of plan.volumes) lines.push(`  ${volume.name}: {}`);
  }
  return `${lines.join("\n")}\n`;
}
