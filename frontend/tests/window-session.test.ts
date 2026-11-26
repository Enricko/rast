// Desktop session saving: what is sent, when, and what is never sent.
import { test } from "node:test";
import assert from "node:assert/strict";
import type { ApiClient } from "../src/core/api";
import {
  createSessionSaver,
  newWindowKey,
  placementOf,
  storableParams,
  type SavedSession,
} from "../src/shell/window-session";

type Listener = () => void;

/** Just enough of `window` and `document` for the saver. */
function fakePage() {
  const listeners = new Map<string, Set<Listener>>();
  const on = (type: string, fn: Listener) => {
    if (!listeners.has(type)) listeners.set(type, new Set());
    listeners.get(type)?.add(fn);
  };
  const off = (type: string, fn: Listener) => listeners.get(type)?.delete(fn);
  const page = { visibilityState: "visible" };
  Object.assign(globalThis, {
    window: { setTimeout, clearTimeout, addEventListener: on, removeEventListener: off },
    document: Object.assign(page, { addEventListener: on, removeEventListener: off }),
  });
  return {
    fire(type: string) {
      for (const fn of listeners.get(type) ?? []) fn();
    },
    count: () => Array.from(listeners.values()).reduce((n, set) => n + set.size, 0),
    hide() {
      page.visibilityState = "hidden";
      this.fire("visibilitychange");
    },
  };
}

function session(apps: string[]): SavedSession {
  return {
    version: 1,
    windows: apps.map((appId, i) => ({
      key: `w-${i}`,
      appId,
      rect: { x: 10 * i, y: 10, w: 800, h: 500 },
      state: "normal",
      minimized: false,
    })),
    focused: apps.length ? `w-${apps.length - 1}` : null,
    stage: { w: 1920, h: 1022 },
  };
}

function fakeApi() {
  const sent: Array<{ body: SavedSession; keepalive: boolean }> = [];
  let fail = false;
  let hold: Promise<void> | null = null;
  const api = {
    async request(method: string, path: string, body: SavedSession, options: { keepalive?: boolean }) {
      assert.equal(method, "PUT");
      assert.equal(path, "/api/desktop/session");
      sent.push({ body: JSON.parse(JSON.stringify(body)) as SavedSession, keepalive: Boolean(options.keepalive) });
      if (hold) await hold;
      if (fail) throw new Error("offline");
    },
  } as unknown as ApiClient;
  return {
    api,
    sent,
    failNext(value: boolean) {
      fail = value;
    },
    holdRequests(promise: Promise<void> | null) {
      hold = promise;
    },
  };
}

test("saves each change once and never repeats an unchanged session", async () => {
  fakePage();
  const { api, sent } = fakeApi();
  let current = session(["files"]);
  const saver = createSessionSaver(api, () => current);
  await saver.flush();
  await saver.flush();
  assert.equal(sent.length, 1);
  current = session(["files", "monitor"]);
  await saver.flush();
  assert.equal(sent.length, 2);
  assert.deepEqual(
    sent[1]?.body.windows.map((w) => w.appId),
    ["files", "monitor"],
  );
  saver.stop();
});

test("a failed save is tried again with the next change", async () => {
  fakePage();
  const fake = fakeApi();
  const current = session(["files"]);
  const saver = createSessionSaver(fake.api, () => current);
  fake.failNext(true);
  await saver.flush();
  fake.failNext(false);
  await saver.flush();
  assert.equal(fake.sent.length, 2);
  await saver.flush();
  assert.equal(fake.sent.length, 2);
  saver.stop();
});

test("hiding or closing the page sends the latest session at once, kept alive", async () => {
  const page = fakePage();
  const fake = fakeApi();
  let current = session(["files"]);
  const saver = createSessionSaver(fake.api, () => current);
  // An ordinary save is still on its way when the page goes away.
  let release: () => void = () => undefined;
  fake.holdRequests(new Promise<void>((resolve) => (release = resolve)));
  const first = saver.flush();
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.equal(fake.sent.length, 1);
  current = session(["files", "logs"]);
  page.fire("pagehide");
  assert.equal(fake.sent.length, 2);
  assert.equal(fake.sent[1]?.keepalive, true);
  assert.deepEqual(
    fake.sent[1]?.body.windows.map((w) => w.appId),
    ["files", "logs"],
  );
  release();
  fake.holdRequests(null);
  await first;
  // Hidden with nothing new: nothing more is sent.
  page.hide();
  await Promise.resolve();
  assert.equal(fake.sent.length, 2);
  saver.stop();
});

test("a stopped saver sends nothing (the desktop closing its windows is not a change)", async () => {
  const page = fakePage();
  const fake = fakeApi();
  let current = session(["files"]);
  const saver = createSessionSaver(fake.api, () => current);
  saver.stop();
  current = session([]);
  saver.schedule();
  await saver.flush();
  page.fire("pagehide");
  assert.equal(fake.sent.length, 0);
  assert.equal(page.count(), 0);
});

test("scheduled saves are folded into one", async () => {
  fakePage();
  const fake = fakeApi();
  let current = session(["files"]);
  const saver = createSessionSaver(fake.api, () => current);
  saver.schedule();
  current = session(["files", "monitor"]);
  saver.schedule();
  await new Promise((resolve) => setTimeout(resolve, 900));
  assert.equal(fake.sent.length, 1);
  assert.equal(fake.sent[0]?.body.windows.length, 2);
  saver.stop();
});

test("app state is stored only when it is plain, small JSON", () => {
  assert.deepEqual(storableParams({ path: "/Documents" }), { path: "/Documents" });
  assert.equal(storableParams(undefined), undefined);
  assert.equal(storableParams(null), undefined);
  assert.equal(storableParams({ big: "x".repeat(5000) }), undefined);
  const cyclic: Record<string, unknown> = {};
  cyclic.self = cyclic;
  assert.equal(storableParams(cyclic), undefined);
  // Functions and other non-JSON values are dropped, not kept.
  assert.deepEqual(storableParams({ tab: "users", fn: () => 1 }), { tab: "users" });
});

test("window keys and placements", () => {
  const a = newWindowKey();
  assert.match(a, /^w-[A-Za-z0-9]{1,16}$/);
  assert.notEqual(a, newWindowKey());
  const saved = session(["files"]).windows[0];
  assert.ok(saved);
  assert.deepEqual(placementOf({ ...saved, state: "maximized", minimized: true }), {
    rect: saved.rect,
    state: "maximized",
    minimized: true,
  });
});
