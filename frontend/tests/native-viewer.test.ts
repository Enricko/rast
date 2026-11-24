import assert from "node:assert/strict";
import test from "node:test";
import { NativeLinuxHost } from "../src/shell/xpra-native";
import { setLinuxNativeSession, type NativeSession } from "../src/apps/linuxapp/catalog";

const session = (status: string, url = "/linux-apps/session/0123456789abcdef0123456789abcdef/"): NativeSession => ({
  id: "native", status, url, startedAt: 1, viewers: 0, separateWindows: true,
  reason: null, error: null, apps: [],
});

test("a starting session never loads the viewer before Xpra is ready", () => {
  setLinuxNativeSession(null);
  const loaded: string[] = [];
  const host: any = {
    disposed: false, session: null, iframe: null, shim: null,
    shimConnected: false,
    clipboard: { attach() {}, detach() {}, setConnected() {} },
    resetInputState() {},
    createIframe(url: string) { loaded.push(url); this.iframe = {}; },
    attachShim() { throw new Error("should not attach to a failed starting page"); },
    removeIframe() { this.iframe = null; },
  };
  const apply = (value: NativeSession) => (NativeLinuxHost.prototype as any).applySession.call(host, value);
  apply(session("starting"));
  assert.equal(loaded.length, 0);
  apply(session("ready"));
  assert.equal(loaded.length, 1);
  host.shim = {};
  apply(session("ready"));
  assert.equal(loaded.length, 1);
  setLinuxNativeSession(null);
});

test("a replacement session uses its new viewer URL", () => {
  const old = session("ready");
  const next = session("ready", "/linux-apps/session/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa/");
  const loaded: string[] = [];
  const host: any = {
    disposed: false, session: old, iframe: {}, shim: {},
    shimConnected: false,
    clipboard: { attach() {}, detach() {}, setConnected() {} },
    resetInputState() {},
    createIframe(url: string) { loaded.push(url); this.iframe = {}; },
    removeIframe() { this.iframe = null; },
  };
  (NativeLinuxHost.prototype as any).applySession.call(host, next);
  assert.deepEqual(loaded, [next.url]);
  assert.equal(host.shim, null);
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
