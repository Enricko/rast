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
  const sent: Array<{ body: SavedSession; keepalive: boolean; origin: string | undefined }> = [];
  let fail = false;
  let hold: Promise<void> | null = null;
  /** The server's session version (each save makes a newer one). */
  const server = { version: 100 };
  const api = {
    async request(
      method: string,
      path: string,
      body: SavedSession,
      options: { keepalive?: boolean; headers?: Record<string, string> },
    ) {
      assert.equal(method, "PUT");
      assert.equal(path, "/api/desktop/session");
      sent.push({
        body: JSON.parse(JSON.stringify(body)) as SavedSession,
        keepalive: Boolean(options.keepalive),
        origin: options.headers?.["X-RAST-Desktop"],
      });
      if (hold) await hold;
      if (fail) throw new Error("offline");
      server.version += 1;
      return { updatedAt: server.version };
    },
  } as unknown as ApiClient;
  return {
    api,
    sent,
    server,
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

/** A desktop: what it shows, and the sessions it was asked to show. */
function fakeDesktop(apps: string[]) {
  const desktop = { current: session(apps), shown: [] as string[][], busy: false };
  return Object.assign(desktop, {
    capture: () => desktop.current,
    options: {
      apply: (next: SavedSession) => {
        desktop.shown.push(next.windows.map((w) => w.appId));
        desktop.current = next;
      },
      busy: () => desktop.busy,
    },
  });
}

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

test("another page's save is shown at once and not saved back", async () => {
  fakePage();
  const fake = fakeApi();
  const desk = fakeDesktop(["files"]);
  const saver = createSessionSaver(fake.api, desk.capture, desk.options);
  saver.started(100, true);
  saver.receive({ session: session(["files", "logs"]), updatedAt: 105, origin: "p-other" });
  assert.deepEqual(desk.shown, [["files", "logs"]]);
  assert.equal(saver.version, 105);
  // Showing it changed the windows; that is not this page's change.
  saver.schedule();
  await saver.flush();
  assert.equal(fake.sent.length, 0);
  // The same or an older session again: nothing.
  saver.receive({ session: session(["monitor"]), updatedAt: 105, origin: "p-other" });
  saver.receive({ session: session(["monitor"]), updatedAt: 103, origin: "p-other" });
  assert.equal(desk.shown.length, 1);
  saver.stop();
});

test("a page's own save coming back is not shown again", async () => {
  fakePage();
  const fake = fakeApi();
  const desk = fakeDesktop(["files"]);
  const saver = createSessionSaver(fake.api, desk.capture, desk.options);
  saver.started(100, false);
  await saver.flush();
  assert.equal(fake.sent.length, 1);
  assert.equal(fake.sent[0]?.origin, saver.pageId);
  assert.match(saver.pageId, /^p-[A-Za-z0-9]+$/);
  assert.equal(saver.version, 101);
  saver.receive({ session: session(["files"]), updatedAt: 101, origin: saver.pageId });
  // Even when the event comes first, an own save never moves windows.
  saver.receive({ session: session(["logs"]), updatedAt: 150, origin: saver.pageId });
  assert.equal(desk.shown.length, 0);
  assert.equal(saver.version, 150);
  saver.stop();
});

test("a change of this page's own wins over a session that arrived while it was on its way", async () => {
  fakePage();
  const fake = fakeApi();
  const desk = fakeDesktop(["files"]);
  const saver = createSessionSaver(fake.api, desk.capture, desk.options);
  saver.started(100, true);
  // This page changed something; another page's older save arrives first.
  desk.current = session(["files", "monitor"]);
  saver.schedule();
  saver.receive({ session: session(["logs"]), updatedAt: 100.5, origin: "p-other" });
  assert.equal(desk.shown.length, 0, "not while this page has a change to save");
  await saver.flush();
  assert.equal(fake.sent.length, 1);
  // The server has this page's save (newer): the other session is dropped.
  assert.equal(desk.shown.length, 0);
  assert.deepEqual(
    desk.current.windows.map((w) => w.appId),
    ["files", "monitor"],
  );
  saver.stop();
});

test("a session saved after this page's save is shown once that save is answered", async () => {
  fakePage();
  const fake = fakeApi();
  const desk = fakeDesktop(["files"]);
  const saver = createSessionSaver(fake.api, desk.capture, desk.options);
  saver.started(100, true);
  let release: () => void = () => undefined;
  fake.holdRequests(new Promise<void>((resolve) => (release = resolve)));
  desk.current = session(["files", "monitor"]);
  const saving = saver.flush();
  await tick();
  // Saved on the server after this page's save (101): newer.
  saver.receive({ session: session(["logs"]), updatedAt: 102, origin: "p-other" });
  assert.equal(desk.shown.length, 0);
  release();
  fake.holdRequests(null);
  await saving;
  assert.deepEqual(desk.shown, [["logs"]]);
  saver.stop();
});

test("sessions from other pages wait while a window is dragged, and until the desktop started", async () => {
  fakePage();
  const fake = fakeApi();
  const desk = fakeDesktop(["files"]);
  const saver = createSessionSaver(fake.api, desk.capture, desk.options);
  // Before the desktop opened its saved windows: kept for later.
  saver.receive({ session: session(["logs"]), updatedAt: 99, origin: "p-other" });
  saver.receive({ session: session(["monitor"]), updatedAt: 104, origin: "p-other" });
  assert.equal(desk.shown.length, 0);
  desk.busy = true;
  saver.started(100, true);
  assert.equal(desk.shown.length, 0, "not during a drag");
  desk.busy = false;
  await new Promise((resolve) => setTimeout(resolve, 1_100));
  assert.deepEqual(desk.shown, [["monitor"]], "only the newest, after the drag");
  assert.equal(fake.sent.length, 0);
  saver.stop();
});

test("a stopped saver shows nothing more", () => {
  fakePage();
  const fake = fakeApi();
  const desk = fakeDesktop(["files"]);
  const saver = createSessionSaver(fake.api, desk.capture, desk.options);
  saver.started(100, true);
  saver.stop();
  saver.receive({ session: session(["logs"]), updatedAt: 200, origin: "p-other" });
  assert.equal(desk.shown.length, 0);
});
