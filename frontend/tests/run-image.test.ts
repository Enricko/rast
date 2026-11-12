import assert from "node:assert/strict";
import test from "node:test";

import {
  composeForRun,
  initialRunPlan,
  runPlanErrors,
  slugWords,
  suggestAppName,
  suggestHostPort,
  usedHostPorts,
  volumesFor,
  type EngineContainer,
  type ImageDetails,
  type RunPlan,
} from "../src/apps/applications/model";

function details(extra: Partial<ImageDetails> = {}): ImageDetails {
  return { id: "sha256:aa", tags: [], exposedPorts: [], volumes: [], env: [], cmd: "", entrypoint: "", workingDir: "", user: "", os: "linux", architecture: "amd64", sizeBytes: null, created: "", ...extra };
}

function withPorts(ports: Array<[number, string]>): EngineContainer {
  return { ports: ports.map(([hostPort, protocol]) => ({ hostPort, containerPort: hostPort, protocol })) } as unknown as EngineContainer;
}

test("app names come from the repository", () => {
  assert.equal(suggestAppName("louislam/uptime-kuma:2"), "uptime-kuma");
  assert.equal(suggestAppName("nginx"), "nginx");
  assert.equal(suggestAppName("ghcr.io/home-assistant/home-assistant:stable"), "home-assistant");
  assert.equal(suggestAppName("registry.local:5000/team/My_App@sha256:abc"), "my-app");
  assert.equal(slugWords("  ---  "), "app");
});

test("host ports avoid ports already in use", () => {
  const used = usedHostPorts([withPorts([[80, "tcp"], [3001, "tcp"], [53, "udp"]])]);
  assert.equal(suggestHostPort(8080, "tcp", used), 8080);
  assert.equal(suggestHostPort(80, "tcp", used), 8080);
  assert.equal(suggestHostPort(3001, "tcp", used), 3002);
  assert.equal(suggestHostPort(53, "tcp", used), 53);
  assert.equal(suggestHostPort(53, "udp", used), 8053);
});

test("volume paths get short, unique names", () => {
  assert.deepEqual(volumesFor(["/app/data"]), [{ name: "data", path: "/app/data" }]);
  assert.deepEqual(
    volumesFor(["/var/lib/postgresql/data", "/srv/data", "/config"]).map((v) => v.name),
    ["data", "srv-data", "config"],
  );
  assert.deepEqual(volumesFor(["/data", "/x/data", "/y/x/data"]).map((v) => v.name), ["data", "x-data", "data-2"]);
});

test("the first plan publishes exposed ports and keeps volumes", () => {
  const plan = initialRunPlan(
    "louislam/uptime-kuma:2",
    details({ exposedPorts: [{ port: 3001, protocol: "tcp" }, { port: 3001, protocol: "udp" }], volumes: ["/app/data"] }),
    [withPorts([[3001, "tcp"]])],
  );
  assert.equal(plan.name, "uptime-kuma");
  assert.deepEqual(plan.ports, [
    { container: 3001, protocol: "tcp", host: 3002 },
    { container: 3001, protocol: "udp", host: 3001 },
  ]);
  assert.deepEqual(plan.volumes, [{ name: "data", path: "/app/data" }]);
});

test("plans are checked before they run", () => {
  const good: RunPlan = { name: "shop", image: "nginx:alpine", ports: [{ container: 80, protocol: "tcp", host: 8090 }], volumes: [{ name: "data", path: "/data" }], env: [{ key: "TZ", value: "Asia/Jakarta", secret: false }] };
  assert.deepEqual(runPlanErrors(good), []);
  const bad: RunPlan = {
    name: " ",
    image: "-x",
    ports: [
      { container: 80, protocol: "tcp", host: 70000 },
      { container: 81, protocol: "tcp", host: 8090 },
      { container: 82, protocol: "tcp", host: 8090 },
      { container: 83, protocol: "tcp", host: 3001 },
      { container: 84, protocol: "tcp", host: null },
    ],
    volumes: [
      { name: "data", path: "relative" },
      { name: "data", path: "/a:b" },
      { name: "-bad", path: "/ok" },
    ],
    env: [
      { key: "1BAD", value: "", secret: false },
      { key: "A", value: "", secret: false },
      { key: "A", value: "", secret: false },
    ],
  };
  const errors = runPlanErrors(bad, new Set(["3001/tcp"]));
  for (const expected of ["Enter a name", "start with a letter", "between 1 and 65535", "8090 is used twice", "3001 is already used", "must be absolute", "listed twice", "Invalid variable name", "A is listed twice"]) {
    assert.ok(errors.some((error) => error.includes(expected)), `${expected} in ${errors.join(" | ")}`);
  }
});

test("compose files keep environment values out", () => {
  const plan: RunPlan = {
    name: "Uptime Kuma",
    image: "louislam/uptime-kuma:2",
    ports: [
      { container: 3001, protocol: "tcp", host: 3001 },
      { container: 5353, protocol: "udp", host: 5353 },
      { container: 9000, protocol: "tcp", host: null },
    ],
    volumes: [{ name: "data", path: "/app/data" }],
    env: [{ key: "ADMIN_PASSWORD", value: "fictional-secret", secret: true }],
  };
  const compose = composeForRun(plan);
  assert.equal(
    compose,
    [
      "services:",
      "  uptime-kuma:",
      '    image: "louislam/uptime-kuma:2"',
      "    restart: unless-stopped",
      "    ports:",
      '      - "3001:3001"',
      '      - "5353:5353/udp"',
      "    volumes:",
      '      - "data:/app/data"',
      "    environment:",
      '      ADMIN_PASSWORD: "${ADMIN_PASSWORD}"',
      "volumes:",
      "  data: {}",
      "",
    ].join("\n"),
  );
  assert.ok(!compose.includes("fictional-secret"));
  assert.equal(composeForRun({ name: "x", image: "alpine", ports: [], volumes: [], env: [] }), 'services:\n  x:\n    image: "alpine"\n    restart: unless-stopped\n');
});
