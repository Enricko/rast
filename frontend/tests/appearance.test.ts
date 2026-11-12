// Regressions for per-account appearance persistence.
//
// A failed GET /api/appearance used to leave the service on the default look;
// the tray theme toggle then saved a *full* payload built from those defaults
// and replaced the account's accent and wallpaper. Responses that arrived after
// an account switch could also land in the next account's state and cache.
import { test } from "node:test";
import assert from "node:assert/strict";
import {
  AppearanceService,
  AppearanceSync,
  DEFAULT_APPEARANCE,
  StaleAppearanceError,
  appearancePatch,
  hasAppearanceChanges,
  normalizeAppearance,
  normalizeDesktopIcons,
  normalizeTaskbarPins,
  type Appearance,
} from "../src/core/appearance";
import type { ApiClient } from "../src/core/api";
import type { EventStream } from "../src/core/sse";

interface Call {
  method: string;
  path: string;
  body?: unknown;
  resolve: (value: unknown) => void;
  reject: (error: unknown) => void;
}

/** An ApiClient whose requests stay pending until the test settles them. */
function fakeApi() {
  const calls: Call[] = [];
  const request = (method: string) => (path: string, body?: unknown) =>
    new Promise((resolve, reject) => calls.push({ method, path, body, resolve, reject }));
  const api = {
    get: request("GET"),
    put: request("PUT"),
    post: request("POST"),
    del: request("DELETE"),
    getCsrfToken: () => "csrf",
  } as unknown as ApiClient;
  return { api, calls };
}

const tick = () => new Promise((resolve) => setTimeout(resolve, 0));

function saved(overrides: Partial<Appearance> & { overlay?: number; builtinId?: string } = {}): Appearance {
  return normalizeAppearance({
    ...DEFAULT_APPEARANCE,
    theme: overrides.theme ?? "dark",
    accent: overrides.accent ?? "plum",
    wallpaper: {
      ...DEFAULT_APPEARANCE.wallpaper,
      builtinId: overrides.builtinId ?? "desert-dusk",
      overlay: overrides.overlay ?? 35,
    },
  });
}

test("a failed load is not a known state and the theme toggle saves only the theme", async () => {
  const { api, calls } = fakeApi();
  const service = new AppearanceService(api);
  const loading = service.load(7);
  calls.shift()!.reject(new Error("502 Bad Gateway"));
  await assert.rejects(loading);
  assert.equal(service.status, "error");
  assert.equal(service.isKnown, false);

  const saving = service.saveTheme("light");
  await tick();
  const put = calls.shift()!;
  assert.equal(put.method, "PUT");
  // Only the theme: accent and wallpaper keep what the server has.
  assert.deepEqual(put.body, { theme: "light" });
  put.resolve(saved({ theme: "light" }));
  assert.equal(await saving, true);
  assert.equal(service.status, "ready");
  assert.equal(service.value.accent, "plum");
  assert.equal(service.value.wallpaper.builtinId, "desert-dusk");
  assert.equal(service.value.wallpaper.overlay, 35);
});

test("a response for the previous account never reaches the next one", async () => {
  const { api, calls } = fakeApi();
  const service = new AppearanceService(api);
  const seen: string[] = [];
  service.subscribe((value) => seen.push(value.accent));

  const loadA = service.load(1);
  const requestA = calls.shift()!;
  const loadB = service.load(2);
  const requestB = calls.shift()!;
  requestB.resolve(saved({ accent: "forest" }));
  assert.equal((await loadB).accent, "forest");

  // Account 1's response arrives late (slow network) and must be dropped.
  requestA.resolve(saved({ accent: "rose" }));
  await assert.rejects(loadA, StaleAppearanceError);
  assert.equal(service.currentUserId, 2);
  assert.equal(service.value.accent, "forest");
  assert.ok(!seen.includes("rose"), `listeners saw ${seen.join(",")}`);
});

test("sign-out drops pending saves", async () => {
  const { api, calls } = fakeApi();
  const service = new AppearanceService(api);
  const loading = service.load(3);
  calls.shift()!.resolve(saved());
  await loading;

  const saving = service.save({ accent: "teal" });
  const put = calls.shift()!;
  service.clear();
  put.resolve(saved({ accent: "teal" }));
  await assert.rejects(saving, StaleAppearanceError);
  assert.equal(service.currentUserId, null);
  assert.equal(service.status, "idle");
  assert.equal(service.value.accent, DEFAULT_APPEARANCE.accent);
});

test("rapid theme changes are coalesced and the last choice wins", async () => {
  const { api, calls } = fakeApi();
  const service = new AppearanceService(api);
  const loading = service.load(4);
  calls.shift()!.resolve(saved({ theme: "light" }));
  await loading;
  const emitted: string[] = [];
  service.subscribe((value) => emitted.push(value.theme));

  const first = service.saveTheme("dark");
  service.saveTheme("light");
  const last = service.saveTheme("dark");
  await tick();
  assert.equal(calls.length, 1, "one request at a time");
  calls.shift()!.resolve(saved({ theme: "dark" }));
  assert.equal(await first, true);
  assert.equal(await last, true);
  // "light" was superseded before it was sent, and the state already matched
  // the final choice: no second request, one announcement.
  assert.equal(calls.length, 0);
  assert.deepEqual(emitted, ["dark"]);
  assert.equal(service.savingTheme, false);
});

test("an account switch during a theme save does not block the next account", async () => {
  const { api, calls } = fakeApi();
  const service = new AppearanceService(api);
  const loadA = service.load(5);
  calls.shift()!.resolve(saved());
  await loadA;
  const pending = service.saveTheme("light");
  await tick();
  const staleRequest = calls.shift()!;

  const loadB = service.load(6);
  calls.shift()!.resolve(saved({ theme: "dark", accent: "amber" }));
  await loadB;
  const next = service.saveTheme("system");
  await tick();
  const request = calls.shift()!;
  assert.deepEqual(request.body, { theme: "system" });
  request.resolve(saved({ theme: "system", accent: "amber" }));
  assert.equal(await next, true);

  staleRequest.resolve(saved({ theme: "light" }));
  assert.equal(await pending, true);
  assert.equal(service.value.theme, "system");
  assert.equal(service.value.accent, "amber");
});

test("editor saves send only the changed fields", () => {
  const baseline = saved();
  assert.deepEqual(appearancePatch(baseline, baseline), {});
  assert.equal(hasAppearanceChanges(baseline, baseline), false);
  // Upload list changes are not settings.
  assert.equal(hasAppearanceChanges(baseline, { ...baseline, uploads: [] }), false);

  const draft = normalizeAppearance({ ...baseline, accent: "teal", wallpaper: { ...baseline.wallpaper, overlay: 10 } });
  assert.deepEqual(appearancePatch(baseline, draft), { accent: "teal", wallpaper: { overlay: 10 } });

  const upload = normalizeAppearance({
    ...baseline,
    wallpaper: { ...baseline.wallpaper, kind: "upload", uploadId: "u1" },
  });
  assert.deepEqual(appearancePatch(baseline, upload), { wallpaper: { kind: "upload", uploadId: "u1" } });
});

test("a resync with an unchanged snapshot raises no conflict", async () => {
  const { api, calls } = fakeApi();
  const service = new AppearanceService(api);
  const loading = service.load(8);
  calls.shift()!.resolve(saved());
  await loading;

  let resync: ((reason: string) => void) | null = null;
  const events = {
    on: () => () => {},
    onResync: (listener: (reason: string) => void) => {
      resync = listener;
      return () => {};
    },
  } as unknown as EventStream;
  const sync = new AppearanceSync(service, events);
  sync.registerDirtySource(() => true);
  let conflicts = 0;
  sync.onConflict(() => {
    conflicts += 1;
  });

  resync!("reconnect");
  calls.shift()!.resolve(saved());
  await tick();
  assert.equal(conflicts, 0, "identical state is not a conflict");

  resync!("reconnect");
  calls.shift()!.resolve(saved({ accent: "indigo" }));
  await tick();
  assert.equal(conflicts, 1, "a real change still reports a conflict to a dirty editor");
  sync.dispose();
});

test("desktop defaults: missing fields read as customised widgets and the default pins", () => {
  const legacy = normalizeAppearance({ theme: "dark" } as Partial<Appearance>);
  assert.equal(legacy.widgetsCustomized, true, "no default widgets from an old response or cache");
  assert.deepEqual(legacy.taskbarPins, ["files", "monitor", "controlpanel"]);
  assert.equal(normalizeAppearance({ widgetsCustomized: false } as Partial<Appearance>).widgetsCustomized, false);
  assert.deepEqual(normalizeTaskbarPins([]), [], "nothing pinned is a choice");
  assert.deepEqual(normalizeTaskbarPins(["logs", "logs", 3, "Bad Id", "files"]), ["logs", "files"]);
});

test("removing every default widget is saved; an unchanged customised list is not", async () => {
  const { api, calls } = fakeApi();
  const service = new AppearanceService(api);
  const loading = service.load(21);
  calls.shift()!.resolve({ ...saved(), widgets: [], widgetsCustomized: false });
  await loading;

  // The desktop shows defaults; removing both leaves an empty list, which must
  // still be stored or the defaults would return on the next visit.
  const removing = service.saveWidgets([]);
  await tick();
  const put = calls.shift()!;
  assert.equal(put.method, "PUT");
  assert.deepEqual(put.body, { widgets: [] });
  put.resolve({ ...saved(), widgets: [], widgetsCustomized: true });
  assert.equal(await removing, true);
  assert.equal(service.value.widgetsCustomized, true);

  // Now customised and unchanged: no request.
  assert.equal(await service.saveWidgets([]), true);
  assert.equal(calls.length, 0);
});

test("taskbar pin changes send only the pin list and the newest list wins", async () => {
  const { api, calls } = fakeApi();
  const service = new AppearanceService(api);
  const loading = service.load(22);
  calls.shift()!.resolve(saved());
  await loading;

  const first = service.saveTaskbarPins(["files"]);
  const second = service.saveTaskbarPins(["files", "logs"]);
  assert.deepEqual(service.pendingTaskbarPins, ["files", "logs"]);
  await tick();
  // Both changes arrived before the request left: one PUT with the newest list.
  const put = calls.shift()!;
  assert.deepEqual(put.body, { taskbarPins: ["files", "logs"] });
  assert.equal(calls.length, 0);
  // A change while that request is in flight is sent after it.
  const third = service.saveTaskbarPins(["logs"]);
  put.resolve({ ...saved(), taskbarPins: ["files", "logs"] });
  await tick();
  const next = calls.shift()!;
  assert.deepEqual(next.body, { taskbarPins: ["logs"] });
  next.resolve({ ...saved(), taskbarPins: ["logs"] });
  assert.equal(await third, true);
  assert.equal(await first, true);
  assert.equal(await second, true);
  assert.deepEqual(service.value.taskbarPins, ["logs"]);
  assert.equal(service.pendingTaskbarPins, null);
});

test("desktop icons: null is the default layout, saves send only the layout, reset sends null", async () => {
  assert.equal(normalizeAppearance({} as Partial<Appearance>).desktopIcons, null);
  assert.equal(normalizeDesktopIcons(null), null);
  assert.deepEqual(normalizeDesktopIcons([]), [], "an empty desktop is a choice");
  assert.deepEqual(
    normalizeDesktopIcons([
      { id: "files", col: 0, row: 0 },
      { id: "files", col: 1, row: 0 },
      { id: "logs", col: 0, row: 0 },
      { id: "Bad Id", col: 2, row: 0 },
      { id: "monitor", col: 64, row: 0 },
      { id: "backup", col: 1.5, row: 0 },
      { id: "monitor", col: 3, row: 2 },
    ]),
    [
      { id: "files", col: 0, row: 0 },
      { id: "monitor", col: 3, row: 2 },
    ],
    "duplicates, bad ids, shared cells and out-of-range cells are dropped",
  );

  const { api, calls } = fakeApi();
  const service = new AppearanceService(api);
  const loading = service.load(23);
  calls.shift()!.resolve(saved());
  await loading;
  assert.equal(service.value.desktopIcons, null);

  const saving = service.saveDesktopIcons([{ id: "files", col: 2, row: 1 }]);
  assert.deepEqual(service.pendingDesktopIcons, [{ id: "files", col: 2, row: 1 }]);
  await tick();
  const put = calls.shift()!;
  assert.deepEqual(put.body, { desktopIcons: [{ id: "files", col: 2, row: 1 }] });
  put.resolve({ ...saved(), desktopIcons: [{ id: "files", col: 2, row: 1 }] });
  assert.equal(await saving, true);
  assert.deepEqual(service.value.desktopIcons, [{ id: "files", col: 2, row: 1 }]);
  assert.equal(service.pendingDesktopIcons, null);

  const resetting = service.resetDesktopIcons();
  await tick();
  const reset = calls.shift()!;
  assert.deepEqual(reset.body, { desktopIcons: null });
  reset.resolve({ ...saved(), desktopIcons: null });
  assert.equal(await resetting, true);
  assert.equal(service.value.desktopIcons, null);
});
