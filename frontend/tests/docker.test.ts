import assert from "node:assert/strict";
import test from "node:test";

import {
  containerStateLabel,
  dockerTime,
  filterGroups,
  filterImages,
  filterVolumes,
  groupContainers,
  groupState,
  imageRefError,
  indexStats,
  maskEnv,
  portLinks,
  pushSample,
  shortId,
  usageFor,
  type ContainerStat,
  type EngineContainer,
  type EngineImage,
  type EngineVolume,
  type GroupApp,
} from "../src/apps/applications/model";

function container(name: string, extra: Partial<EngineContainer> = {}): EngineContainer {
  return {
    id: `${name.replace(/[^a-f0-9]/g, "a").padEnd(12, "0")}${"0".repeat(52)}`.slice(0, 64),
    name,
    image: "nginx:alpine",
    state: "running",
    status: "Up 2 hours",
    ports: [],
    mounts: [],
    labels: {},
    project: "",
    service: "",
    managed: false,
    slug: null,
    ...extra,
  };
}

const shopApp: GroupApp = { id: "app-1", name: "Shop", slug: "shop", status: "running" };

test("containers group into apps, compose projects and standalone containers", () => {
  const containers = [
    container("zeta"),
    container("drive-db", { project: "drive" }),
    container("drive-app", { project: "drive" }),
    container("rast-shop-web-1", { project: "rast-shop", slug: "shop", managed: true }),
    container("alpha"),
  ];
  const groups = groupContainers(containers, [shopApp]);
  assert.deepEqual(
    groups.map((g) => [g.kind, g.name, g.containers.map((c) => c.name)]),
    [
      ["app", "Shop", ["rast-shop-web-1"]],
      ["project", "drive", ["drive-app", "drive-db"]],
      ["single", "alpha", ["alpha"]],
      ["single", "zeta", ["zeta"]],
    ],
  );
});

test("apps without containers still show, and read-only accounts use the app's containers", () => {
  const stopped: GroupApp = { id: "app-2", name: "Blog", slug: "blog", status: "stopped" };
  const groups = groupContainers([], [stopped]);
  assert.equal(groups.length, 1);
  assert.equal(groupState(groups[0]!).label, "Stopped");

  const withContainers: GroupApp = { ...shopApp, containers: [{ name: "shop-web-1", state: "running", image: "nginx" }] };
  const readOnly = groupContainers(null, [withContainers]);
  assert.equal(readOnly[0]!.containers[0]!.name, "shop-web-1");
  assert.equal(groupState(readOnly[0]!).label, "Running");
});

test("containers of an unknown app fall back to a project group", () => {
  const groups = groupContainers([container("rast-new-web-1", { project: "rast-new", slug: "new", managed: true })], []);
  assert.equal(groups[0]!.kind, "project");
  assert.equal(groups[0]!.name, "rast-new");
});

test("group state summarises running containers", () => {
  const [group] = groupContainers(
    [container("a", { project: "p" }), container("b", { project: "p", state: "exited", status: "Exited (0) 1 hour ago" })],
    [],
  );
  assert.deepEqual(groupState(group!), { running: 1, total: 2, label: "Running (1/2)", tone: "warn" });
  const deploying = groupContainers([], [{ ...shopApp, status: "deploying" }]);
  assert.equal(groupState(deploying[0]!).label, "Working…");
});

test("container state labels include the exit code", () => {
  assert.equal(containerStateLabel({ state: "exited", status: "Exited (137) 2 days ago" }), "Exited (137)");
  assert.equal(containerStateLabel({ state: "exited", status: "" }), "Exited");
  assert.equal(containerStateLabel({ state: "running" }), "Running");
});

test("filtering keeps whole groups on a name match and narrows otherwise", () => {
  const groups = groupContainers(
    [
      container("drive-app", { project: "drive", image: "nextcloud:34" }),
      container("drive-db", { project: "drive", image: "postgres:17", state: "exited" }),
      container("web"),
    ],
    [{ id: "app-2", name: "Blog", slug: "blog", status: "stopped" }],
  );
  assert.deepEqual(filterGroups(groups, "drive", false).map((g) => g.containers.length), [2]);
  assert.deepEqual(filterGroups(groups, "postgres", false).map((g) => g.containers.map((c) => c.name)), [["drive-db"]]);
  assert.deepEqual(filterGroups(groups, "", true).map((g) => g.name), ["drive", "web"]);
  assert.deepEqual(filterGroups(groups, "blog", false).map((g) => g.name), ["Blog"]);
});

test("port links are offered only where the port is reachable", () => {
  const ports = [
    { containerPort: 80, hostPort: 8080, protocol: "tcp" },
    { containerPort: 80, hostPort: 8080, protocol: "tcp" },
    { containerPort: 5432, hostPort: 5432, hostIp: "127.0.0.1", protocol: "tcp" },
    { containerPort: 53, hostPort: 5353, protocol: "udp" },
    { containerPort: 9000, protocol: "tcp" },
  ];
  assert.deepEqual(portLinks(ports, { hostname: "192.168.1.20" }), [
    { label: "8080:80", href: "http://192.168.1.20:8080" },
    { label: "5432:5432", href: null },
    { label: "5353:53/udp", href: null },
  ]);
  assert.equal(portLinks(ports, { hostname: "localhost" })[1]!.href, "http://localhost:5432");
  assert.equal(portLinks(ports, { hostname: "desktop.example.com" })[0]!.href, null);
  assert.equal(portLinks(ports, { hostname: "nas.local" })[0]!.href, "http://nas.local:8080");
});

test("usage sums samples found by id, short id or name", () => {
  const stat = (id: string, name: string, cpu: number, mem: number): ContainerStat => ({
    id,
    name,
    cpuPercent: cpu,
    memoryBytes: mem,
    memoryLimitBytes: 0,
    memoryPercent: 0,
    netRxBytes: 0,
    netTxBytes: 0,
    blockReadBytes: 0,
    blockWriteBytes: 0,
    pids: 1,
  });
  const a = container("a");
  const b = container("b");
  const stats = indexStats([stat(a.id, "a", 1.5, 100), stat("ffffffffffff", "b", 2, 50)]);
  assert.deepEqual(usageFor([a, b, container("c")], stats), { cpuPercent: 3.5, memoryBytes: 150, sampled: 2 });
  assert.equal(stats.get(a.id.slice(0, 12))?.name, "a");
});

function image(reference: string, inUse: boolean, extra: Partial<EngineImage> = {}): EngineImage {
  return {
    id: `sha256:${reference.length}${"a".repeat(63)}`,
    repository: reference.split(":")[0] ?? "",
    tag: reference.split(":")[1] ?? "",
    reference,
    digest: "",
    sizeBytes: 1000,
    size: "1kB",
    createdAt: "",
    createdSince: "",
    containers: inUse ? 1 : 0,
    usedBy: inUse ? ["web"] : [],
    inUse,
    ...extra,
  };
}

test("images filter by usage and search, untagged last", () => {
  const images = [image("", false, { id: "sha256:ffff" }), image("redis:8", false), image("nginx:alpine", true)];
  assert.deepEqual(filterImages(images, "", "all").map((i) => i.reference), ["nginx:alpine", "redis:8", ""]);
  assert.deepEqual(filterImages(images, "", "used").map((i) => i.reference), ["nginx:alpine"]);
  assert.deepEqual(filterImages(images, "", "unused").map((i) => i.reference), ["redis:8", ""]);
  assert.deepEqual(filterImages(images, "web", "all").map((i) => i.reference), ["nginx:alpine"]);
  assert.equal(shortId("sha256:be2656061b536c18"), "be2656061b53");
});

test("volumes filter by usage and search, anonymous last", () => {
  const volume = (name: string, inUse: boolean, extra: Partial<EngineVolume> = {}): EngineVolume => ({
    name,
    driver: "local",
    scope: "local",
    mountpoint: "",
    labels: {},
    project: "",
    anonymous: false,
    inUse,
    usedBy: inUse ? ["db"] : [],
    app: null,
    ...extra,
  });
  const volumes = [volume("f".repeat(64), false, { anonymous: true }), volume("pgdata", true), volume("cache", false, { app: "shop" })];
  assert.deepEqual(filterVolumes(volumes, "", "all").map((v) => v.name.slice(0, 6)), ["cache", "pgdata", "ffffff"]);
  assert.deepEqual(filterVolumes(volumes, "", "used").map((v) => v.name), ["pgdata"]);
  assert.deepEqual(filterVolumes(volumes, "shop", "all").map((v) => v.name), ["cache"]);
});

test("image references are checked like the server does", () => {
  for (const ok of ["nginx", "nginx:1.27-alpine", "ghcr.io/org/app:v1", "localhost:5000/a/b", "alpine@sha256:abc"]) {
    assert.equal(imageRefError(ok), null, ok);
  }
  for (const bad of ["", "-v", "nginx latest", "a//b", "a/../b", "nginx:", "a@b@c", "nginx;id"]) {
    assert.notEqual(imageRefError(bad), null, bad);
  }
});

test("small helpers", () => {
  assert.deepEqual(maskEnv("PASSWORD=a=b"), { key: "PASSWORD", value: "a=b" });
  assert.deepEqual(maskEnv("FLAG"), { key: "FLAG", value: "" });
  assert.equal(dockerTime("0001-01-01T00:00:00Z"), null);
  assert.equal(dockerTime("2026-11-10T13:24:02Z"), Date.parse("2026-11-10T13:24:02Z"));
  assert.deepEqual(pushSample([1, 2, 3], 4, 3), [2, 3, 4]);
  assert.deepEqual(pushSample([], Number.NaN), [0]);
});
