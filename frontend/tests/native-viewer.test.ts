import assert from "node:assert/strict";
import test from "node:test";
import { NativeLinuxHost } from "../src/shell/xpra-native";
import { setLinuxNativeSession, type NativeSession } from "../src/apps/linuxapp/catalog";

// The host uses window timers for its bounded waits; these unit tests never
// touch a real DOM, so Node's own timers are enough (and must not keep the test
// process alive after a launch test).
(globalThis as any).window = {
  setTimeout: (callback: () => void, ms: number) => {
    const timer = setTimeout(callback, ms);
    (timer as { unref?: () => void }).unref?.();
    return timer as unknown as number;
  },
  clearTimeout: (timer: unknown) => clearTimeout(timer as NodeJS.Timeout),
  setInterval: () => 0,
  clearInterval: () => undefined,
};

const session = (status: string, url = "/linux-apps/session/0123456789abcdef0123456789abcdef/"): NativeSession => ({
  id: "native", status, url, startedAt: 1, viewers: 0, separateWindows: true,
  reason: null, error: null, apps: [],
});

const run = () => ({ runId: "run-1", appId: "chrome", name: "Google Chrome", status: "running" });

const noToast = { error: () => undefined, warning: () => undefined, info: () => undefined, success: () => undefined };

/**
 * Minimal transport host: real prototype methods (applySession, the launch
 * gate, ...) on a plain object, no DOM. `createIframe` records the URL and
 * behaves like a viewer whose shim is attached; `attachShim` must never be
 * needed for the assertions (it throws so a wrong path fails loudly).
 */
function transportHost() {
  const loaded: string[] = [];
  const host: any = Object.assign(Object.create(NativeLinuxHost.prototype), {
    disposed: false,
    session: null,
    iframe: null,
    shim: null,
    shimConnected: false,
    viewerDeferred: false,
    viewerRequested: false,
    launchesInFlight: 0,
    connecting: null,
    connectWaitTimer: 0,
    connectWaitDone: null,
    nonSeamlessNotice: "",
    lastSnapshot: null,
    frames: new Map(),
    starting: new Map(),
    launchRecoveryTimer: 0,
    lastLaunchSlug: null,
    lastLaunchAt: 0,
    deps: { toast: noToast },
    clipboard: { attach: () => undefined, detach: () => undefined, setConnected: () => undefined },
    resetInputState: () => undefined,
    createIframe(url: string) {
      loaded.push(url);
      this.iframe = { url };
    },
    removeIframe() {
      this.iframe = null;
      this.shim = null;
    },
    attachShim() {
      throw new Error("the transport shim must not be attached from applySession in these tests");
    },
  });
  return { host, loaded };
}

const applySession = (host: any, value: NativeSession | null): void =>
  (NativeLinuxHost.prototype as any).applySession.call(host, value);

test("a starting session never loads the viewer before Xpra is ready", () => {
  setLinuxNativeSession(null);
  const { host, loaded } = transportHost();
  // The session is already in use by this desktop (the existing behaviour this
  // test pins: a starting session must not load the viewer early).
  host.viewerRequested = true;
  applySession(host, session("starting"));
  assert.equal(loaded.length, 0);
  applySession(host, session("ready"));
  assert.equal(loaded.length, 1);
  assert.equal(host.viewerDeferred, false);
  setLinuxNativeSession(null);
});

test("a replacement session uses its new viewer URL", () => {
  const old = session("ready");
  const next = session("ready", "/linux-apps/session/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/");
  const { host, loaded } = transportHost();
  host.session = old;
  host.iframe = { url: old.url };
  host.shim = { attached: true };
  host.viewerRequested = true;
  applySession(host, next);
  assert.deepEqual(loaded, [next.url]);
  assert.equal(host.shim, null);
  setLinuxNativeSession(null);
});

test("a pre-warmed session keeps the hidden transport disconnected until an app runs", () => {
  setLinuxNativeSession(null);
  const { host, loaded } = transportHost();

  // The server started the shared session in the background: no app run yet.
  applySession(host, session("starting"));
  assert.equal(loaded.length, 0, "a starting session never loads the viewer");
  assert.equal(host.viewerDeferred, true, "a session with no app run is deferred");

  applySession(host, session("ready"));
  assert.equal(loaded.length, 0, "the pre-warmed session must not create the hidden transport");
  assert.equal(host.viewerDeferred, true);

  // Another RAST tab started an app in the same shared session: its windows
  // must be shown here too, so the transport is connected.
  const inUse = session("ready");
  inUse.apps.push(run());
  applySession(host, inUse);
  assert.deepEqual(loaded, [inUse.url]);
  assert.equal(host.viewerDeferred, false);

  // The session ends and the replacement is pre-warmed: defer it again, so a
  // later sign-in cycle cannot inherit this generation's connected viewer.
  applySession(host, { ...session("ended"), reason: "idle" });
  assert.equal(host.iframe, null);
  assert.equal(host.viewerDeferred, false);
  assert.equal(host.viewerRequested, false);
  assert.equal(host.session?.status, "ended");

  applySession(host, session("ready"));
  assert.equal(loaded.length, 1, "the new pre-warmed session is deferred again");
  assert.equal(host.viewerDeferred, true);
  setLinuxNativeSession(null);
});

test("a reaped pre-warm the person never used is not announced", () => {
  setLinuxNativeSession(null);
  const { host } = transportHost();
  const infos: string[] = [];
  host.deps.toast = { ...noToast, info: (message: string) => infos.push(message) };
  applySession(host, session("starting"));
  applySession(host, session("ready"));
  applySession(host, { ...session("ended"), reason: "idle" });
  assert.deepEqual(infos, [], "no interruption for a background session nobody used");
  setLinuxNativeSession(null);
});

test("a session is not deferred once a launch asked for the viewer", () => {
  setLinuxNativeSession(null);
  const { host, loaded } = transportHost();
  // The launch API may report the session before the app run is listed; the
  // launch gate has already declared the transport needed.
  host.viewerRequested = true;
  const ready = session("ready");
  applySession(host, ready);
  assert.deepEqual(loaded, [ready.url]);
  assert.equal(host.viewerDeferred, false);

  const inFlight = transportHost();
  inFlight.host.launchesInFlight = 1;
  applySession(inFlight.host, session("ready"));
  assert.equal(inFlight.loaded.length, 1, "a launch in flight is never deferred around");
  setLinuxNativeSession(null);
});

test("the first native launch connects the hidden transport before asking the server", async () => {
  setLinuxNativeSession(null);
  const { host, loaded } = transportHost();
  host.session = session("ready");
  host.viewerDeferred = true;
  let waitedMs = 0;
  host.waitForShimConnection = async (timeoutMs: number) => {
    waitedMs = timeoutMs;
    return true;
  };
  const prepareForLaunch = (NativeLinuxHost.prototype as any).prepareForLaunch;
  assert.equal(await prepareForLaunch.call(host), true);
  assert.deepEqual(loaded, [host.session.url], "the viewer is created before the launch");
  assert.equal(host.viewerDeferred, false);
  assert.equal(host.viewerRequested, true);
  assert.ok(waitedMs > 0 && waitedMs <= 60_000, "the shim wait has a bounded timeout");
  setLinuxNativeSession(null);
});

test("there is nothing to connect before a session exists or while it is starting", async () => {
  setLinuxNativeSession(null);
  const ensureConnected = (NativeLinuxHost.prototype as any).ensureConnected;
  const { host, loaded } = transportHost();

  assert.equal(await ensureConnected.call(host), true, "a cold launch creates the session itself");
  assert.equal(host.iframe, null);

  host.session = session("starting");
  assert.equal(await ensureConnected.call(host), true, "a starting session has no viewer URL to load");
  assert.equal(loaded.length, 0);

  host.session = session("ready");
  host.shimConnected = true;
  assert.equal(await ensureConnected.call(host), true, "an already connected transport is reused");
  assert.equal(loaded.length, 0);
  setLinuxNativeSession(null);
});

test("a viewer that cannot connect is reported clearly and the launch still proceeds", async () => {
  setLinuxNativeSession(null);
  const { host, loaded } = transportHost();
  const apiCalls: string[] = [];
  const warnings: string[] = [];
  host.session = session("ready");
  host.viewerDeferred = true;
  host.deps = {
    api: {
      post: async (path: string) => {
        apiCalls.push(path);
        return { run: { id: "run-1", appId: "chrome", appName: "Google Chrome", status: "running" }, session: null };
      },
    },
    toast: { ...noToast, warning: (message: string) => warnings.push(message) },
  };
  host.showStarting = () => ({ slug: "chrome", detailEl: { textContent: "" } });
  host.framesFor = () => [];
  host.applySession = () => undefined;
  host.closeStarting = () => undefined;
  host.waitForShimConnection = async () => false;

  const launch = (NativeLinuxHost.prototype as any).launch;
  await launch.call(host, { id: "chrome", name: "Google Chrome", backend: "native" });

  assert.deepEqual(loaded, [host.session.url], "connecting was still attempted before the launch");
  assert.equal(warnings.length, 1, "the person is told the viewer could not connect");
  assert.match(warnings[0], /could not connect/i);
  assert.deepEqual(apiCalls, ["/api/linux-apps/chrome/launch"], "the launch goes ahead anyway");
  assert.equal(host.lastLaunchSlug, "chrome");
  assert.equal(host.launchesInFlight, 0, "the in-flight launch counter is balanced");
  setLinuxNativeSession(null);
});

test("a non-seamless session never renders the remote desktop in a RAST window", () => {
  const errors: string[] = [];
  const opened: unknown[] = [];
  const host: any = {
    disposed: false,
    frames: new Map(),
    starting: new Map(),
    shimConnected: false,
    clipboard: { attach() {}, detach() {}, setConnected() {} },
    resetInputState() {},
    session: { url: "/linux-apps/session/0123456789abcdef0123456789abcdef/" },
    nonSeamlessNotice: "",
    deps: {
      toast: { error: (message: string) => errors.push(message) },
      wm: {
        open: (...args: unknown[]) => {
          opened.push(args);
          return null;
        },
      },
    },
  };
  const snapshot = {
    ready: true,
    separate: false,
    windows: [],
    size: [1280, 720],
    connected: true,
    version: 1,
    key: null,
    error: null,
    disconnectReason: null,
    desktopWidth: 1280,
    desktopHeight: 720,
    clipboardEnabled: false,
    captureKeyboard: false,
    audio: { enabled: false, state: "", codec: null },
  };
  host.onNonSeamlessSnapshot = (NativeLinuxHost.prototype as any).onNonSeamlessSnapshot;
  const applySnapshot = (NativeLinuxHost.prototype as any).applySnapshot;
  applySnapshot.call(host, snapshot);
  assert.equal(opened.length, 0, "a non-seamless session must not open a window");
  assert.equal(errors.length, 1, "the anomaly is reported once");
  assert.match(errors[0], /desktop mode/);
  // The transport viewer may keep publishing snapshots: never spam the person.
  applySnapshot.call(host, snapshot);
  assert.equal(errors.length, 1);
  assert.equal(opened.length, 0);
});

test("a native app is never downgraded to the legacy whole-session viewer", () => {
  const errors: string[] = [];
  const legacy: unknown[] = [];
  const host: any = {
    deps: {
      findApp: () => ({ id: "linux:chrome" }),
      openLegacy: (definition: unknown) => legacy.push(definition),
      toast: { error: (message: string) => errors.push(message) },
    },
  };
  const nativeApp = { id: "chrome", name: "Google Chrome", backend: "native" };
  (NativeLinuxHost.prototype as any).fallbackToLegacy.call(host, nativeApp);
  assert.equal(legacy.length, 0, "the legacy viewer must never receive a native app");
  assert.equal(errors.length, 1, "the person is told why it did not open");
});

test("a docker-backed app keeps its documented legacy window", () => {
  const legacy: unknown[] = [];
  const host: any = {
    deps: {
      findApp: () => ({ id: "linux:legacy" }),
      openLegacy: (definition: unknown) => legacy.push(definition),
      toast: { error: () => undefined },
    },
  };
  const dockerApp = { id: "legacy", name: "Legacy app", backend: "docker" };
  (NativeLinuxHost.prototype as any).fallbackToLegacy.call(host, dockerApp);
  assert.equal(legacy.length, 1);
});
