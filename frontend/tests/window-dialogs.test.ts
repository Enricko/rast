// Dialogs kept with their window: reopening, matching, closing, updating.
import { test } from "node:test";
import assert from "node:assert/strict";
import { WindowDialogs } from "../src/shell/window-dialogs";

/** Enough of an element for a dialog's view tracker. */
const fakeDialog = () =>
  ({
    addEventListener() {},
    removeEventListener() {},
    querySelectorAll: () => [],
    children: [],
  }) as unknown as HTMLElement;

test("a saved dialog opens again once its app runs, and is kept until it does", () => {
  let changes = 0;
  const dialogs = new WindowDialogs(() => (changes += 1));
  const reopened: unknown[] = [];
  dialogs.apply([{ name: "new-folder", params: { folder: "/Docs" } }], false);
  dialogs.register("new-folder", (params) => reopened.push(params));
  assert.equal(reopened.length, 0, "not before the app started");
  // A save meanwhile keeps the dialog in the session.
  assert.deepEqual(dialogs.capture(), [{ name: "new-folder", params: { folder: "/Docs" } }]);
  dialogs.mounted();
  assert.deepEqual(reopened, [{ folder: "/Docs" }]);
  dialogs.mounted();
  assert.equal(reopened.length, 1, "asked once");
  // The app opens it with the same name and params: it is the saved one.
  const session = dialogs.session("new-folder", { folder: "/Docs" });
  let closed = 0;
  session.opened(fakeDialog(), () => {
    closed += 1;
    session.closed();
  });
  assert.deepEqual(dialogs.capture(), [{ name: "new-folder", params: { folder: "/Docs" } }]);
  assert.ok(changes > 0);
  session.closed();
  assert.equal(dialogs.capture(), undefined);
  assert.equal(closed, 0);
});

test("another page closing a dialog closes it here; a new one is opened", () => {
  const dialogs = new WindowDialogs(() => undefined);
  const reopened: string[] = [];
  dialogs.register("rename", (params) => reopened.push(JSON.stringify(params)));
  dialogs.mounted();
  const session = dialogs.session("rename", { path: "/a", name: "a" });
  let closed = 0;
  session.opened(fakeDialog(), () => {
    closed += 1;
    session.closed();
  });
  // Still open on the other page: nothing happens.
  dialogs.apply([{ name: "rename", params: { path: "/a", name: "a" } }], true);
  assert.equal(closed, 0);
  assert.equal(reopened.length, 0);
  // The other page renamed something else instead.
  dialogs.apply([{ name: "rename", params: { path: "/b", name: "b" } }], true);
  assert.equal(closed, 1);
  assert.deepEqual(reopened, ['{"path":"/b","name":"b"}']);
  // And then closed it: what was waiting to open is dropped.
  dialogs.apply([], true);
  assert.equal(dialogs.capture(), undefined);
});

test("a dialog that changes (rows added) is saved as it is now", () => {
  const dialogs = new WindowDialogs(() => undefined);
  const session = dialogs.session("app-deploy", { envRows: 0 });
  session.update({ envRows: 1 });
  session.opened(fakeDialog(), () => session.closed());
  session.update({ envRows: 3 });
  assert.deepEqual(dialogs.capture(), [{ name: "app-deploy", params: { envRows: 3 } }]);
});

test("closing the window closes its dialogs and keeps nothing", () => {
  const dialogs = new WindowDialogs(() => undefined);
  let closed = 0;
  const session = dialogs.session("backup-create");
  session.opened(fakeDialog(), () => {
    closed += 1;
    session.closed();
  });
  dialogs.apply([{ name: "user-create" }], false);
  dialogs.dispose();
  assert.equal(closed, 1);
  assert.equal(dialogs.capture(), undefined);
});

test("saved dialogs with bad names are ignored", () => {
  const dialogs = new WindowDialogs(() => undefined);
  dialogs.apply([{ name: "../x" }, { name: "" }, null, "x", { name: "ok" }], false);
  assert.deepEqual(dialogs.capture(), [{ name: "ok" }]);
});

test("a dialog opened over another opens again after it", async () => {
  const dialogs = new WindowDialogs(() => undefined);
  const calls: string[] = [];
  let listOpen: () => void = () => undefined;
  dialogs.register("user-sessions", () => {
    calls.push("list");
    return new Promise<void>((resolve) => (listOpen = resolve));
  });
  dialogs.register("user-sessions-revoke", () => void calls.push("confirm"));
  dialogs.apply([{ name: "user-sessions", params: { userId: 2 } }, { name: "user-sessions-revoke", params: { userId: 2 } }], false);
  dialogs.mounted();
  assert.deepEqual(calls, ["list"], "the confirmation waits for the list");
  listOpen();
  await new Promise((resolve) => setTimeout(resolve, 0));
  assert.deepEqual(calls, ["list", "confirm"]);
});

test("a dialog another page shows is kept even when this page does not open it", () => {
  const realNow = Date.now;
  try {
    const dialogs = new WindowDialogs(() => undefined);
    dialogs.register("session-revoke", () => undefined); // e.g. this device's own session
    dialogs.mounted();
    dialogs.apply([{ name: "session-revoke", params: { sessionId: "s1" } }], true);
    dialogs.apply([{ name: "new-folder", params: { folder: "/" } }], false);
    const later = realNow() + 60_000;
    Date.now = () => later;
    // Still open on the other page: saving must not close it there. The
    // restored one nobody opened is dropped.
    assert.deepEqual(dialogs.capture(), [{ name: "session-revoke", params: { sessionId: "s1" } }]);
    // The other page closed it.
    dialogs.apply([], true);
    assert.equal(dialogs.capture(), undefined);
  } finally {
    Date.now = realNow;
  }
});

test("a window's dialogs open over that window; the desktop's over the desktop", () => {
  const content = { tag: "win-body" } as unknown as HTMLElement;
  const windowDialogs = new WindowDialogs(() => undefined, content);
  assert.equal(windowDialogs.session("rename", { path: "/a" }).host, content);
  const desktopDialogs = new WindowDialogs(() => undefined);
  assert.equal(desktopDialogs.session("sign-out").host, undefined);
});
