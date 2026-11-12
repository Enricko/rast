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
