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
    createIframe(url: string) { loaded.push(url); this.iframe = {}; },
    removeIframe() { this.iframe = null; },
  };
  (NativeLinuxHost.prototype as any).applySession.call(host, next);
  assert.deepEqual(loaded, [next.url]);
  assert.equal(host.shim, null);
  setLinuxNativeSession(null);
});
