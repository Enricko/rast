// Desktop widget data states come from the Resource Monitor data itself:
// loading -> live, unavailable (monitor says so), denied (no access / 403),
// error with retry, and stale when samples stop arriving.
import { test } from "node:test";
import assert from "node:assert/strict";
import { ApiError, type ApiClient } from "../src/core/api";
import type { RealtimeStore } from "../src/core/realtime";
import type { MonitorSnapshot } from "../src/core/types";
import { DEFAULT_WIDGETS, MonitorFeed, effectiveWidgets, sampleAge, staleAfterMs } from "../src/shell/widgets";

// The feed only needs window timers.
(globalThis as unknown as { window: typeof globalThis }).window ??= globalThis;

const snapshot = (overrides: Partial<MonitorSnapshot> = {}): MonitorSnapshot => ({
  ts: 1,
  intervalSecs: 2,
  host: {
    cpuPercent: 12,
    cpuCores: 4,
    loadAvg: [0.5, 0.4, 0.3],
    memoryUsed: 2,
    memoryTotal: 8,
    swapUsed: 0,
    swapTotal: 0,
    uptimeSecs: 3600,
  },
  network: { rxBytesPerSec: 100, txBytesPerSec: 50, rxTotal: 1, txTotal: 1 },
  disks: [],
  components: [],
  ...overrides,
});

function harness(handler: (path: string) => Promise<unknown>, allowed = true) {
  const monitorListeners: Array<(data: MonitorSnapshot) => void> = [];
  const api = { get: (path: string) => handler(path) } as unknown as ApiClient;
  const realtime = {
    on: (_kind: string, listener: (data: MonitorSnapshot) => void) => {
      monitorListeners.push(listener);
      return () => {};
    },
    onResync: () => () => {},
  } as unknown as RealtimeStore;
  const feed = new MonitorFeed(api, realtime, () => allowed);
  return { feed, push: (data: MonitorSnapshot) => monitorListeners.forEach((listener) => listener(data)) };
}

const settle = () => new Promise((resolve) => setTimeout(resolve, 0));

test("loading, then live with history and live samples", async () => {
  const { feed, push } = harness(async (path) =>
    path === "/api/monitor/snapshot" ? snapshot() : { available: true, points: [{ value: 10 }, { value: 11 }] },
  );
  feed.start();
  assert.equal(feed.state.status, "loading");
  await settle();
  assert.equal(feed.state.status, "live");
  assert.deepEqual(feed.state.cpu, [10, 11]);
  push(snapshot({ host: { ...snapshot().host!, cpuPercent: 42 } }));
  assert.equal(feed.state.cpu.at(-1), 42);
  feed.stop();
});

test("the monitor reporting it cannot sample is 'unavailable' with its reason", async () => {
  const { feed } = harness(async () => snapshot({ unavailable: true, reason: "procfs is not mounted" }));
  feed.start();
  await settle();
  assert.equal(feed.state.status, "unavailable");
  assert.equal(feed.state.reason, "procfs is not mounted");
  feed.stop();
});

test("no Resource Monitor access is 'denied' without requesting data", async () => {
  let requests = 0;
  const { feed } = harness(async () => {
    requests += 1;
    return snapshot();
  }, false);
  feed.start();
  await settle();
  assert.equal(feed.state.status, "denied");
  assert.equal(requests, 0);
  feed.stop();
});

test("a 403 from the monitor API is 'denied'; other failures are retryable errors", async () => {
  const denied = harness(async () => {
    throw new ApiError(403, "forbidden", "Resource Monitor is not available");
  });
  denied.feed.start();
  await settle();
  assert.equal(denied.feed.state.status, "denied");
  denied.feed.stop();

  let fail = true;
  const flaky = harness(async (path) => {
    if (fail) throw new Error("Request failed (502)");
    return path === "/api/monitor/snapshot" ? snapshot() : { available: true, points: [] };
  });
  flaky.feed.start();
  await settle();
  assert.equal(flaky.feed.state.status, "error");
  fail = false;
  await flaky.feed.refresh(true);
  assert.equal(flaky.feed.state.status, "live");
  flaky.feed.stop();
});

test("staleness follows the sampling interval and shows the real age", () => {
  assert.equal(staleAfterMs(snapshot({ intervalSecs: 2 })), 15_000);
  assert.equal(staleAfterMs(snapshot({ intervalSecs: 10 })), 30_000);
  assert.equal(sampleAge(1_000, 22_000), "21 s");
  assert.equal(sampleAge(0, 5 * 60_000), "5 min");
});

test("accounts that never chose widgets get the default set, only with Resource Monitor access", () => {
  const fresh = { widgets: [], widgetsCustomized: false };
  assert.deepEqual(effectiveWidgets(fresh, true), DEFAULT_WIDGETS);
  assert.deepEqual(effectiveWidgets(fresh, false), [], "no 'No access' cards for a default nobody chose");
  const own = { widgets: [{ id: "network" as const, x: 0.5, y: 0.5 }], widgetsCustomized: true };
  assert.deepEqual(effectiveWidgets(own, true), own.widgets);
  assert.deepEqual(effectiveWidgets({ widgets: [], widgetsCustomized: true }, true), [], "an emptied desktop stays empty");
});
