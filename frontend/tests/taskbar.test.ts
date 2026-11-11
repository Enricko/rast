// Taskbar ordering: pinned applications keep their slots when opened.
import { test } from "node:test";
import assert from "node:assert/strict";
import { taskbarSlots, type TaskbarSlot, type TaskbarWindow } from "../src/shell/taskbar-order";

const PINS = ["files", "monitor", "controlpanel"];
const allowed = () => true;
const label = (slots: Array<TaskbarSlot<TaskbarWindow>>) =>
  slots.map((slot) => (slot.kind === "launcher" ? `pin:${slot.appId}` : `win:${slot.win.id}`));

test("opening a pinned application keeps its button in place", () => {
  assert.deepEqual(label(taskbarSlots(PINS, [], allowed)), ["pin:files", "pin:monitor", "pin:controlpanel"]);
  // Control Panel opened: it stays third (it used to jump behind the others).
  assert.deepEqual(label(taskbarSlots(PINS, [{ id: "w1", appId: "controlpanel" }], allowed)), [
    "pin:files",
    "pin:monitor",
    "win:w1",
  ]);
  // File Station opened: it stays first.
  assert.deepEqual(label(taskbarSlots(PINS, [{ id: "w2", appId: "files" }], allowed)), [
    "win:w2",
    "pin:monitor",
    "pin:controlpanel",
  ]);
});

test("a pinned application's windows share its slot; unpinned windows follow in opening order", () => {
  const windows = [
    { id: "a", appId: "logs" },
    { id: "b", appId: "files" },
    { id: "c", appId: "applications" },
    { id: "d", appId: "files" },
  ];
  const slots = taskbarSlots(["files", "monitor"], windows, allowed);
  assert.deepEqual(label(slots), ["win:b", "win:d", "pin:monitor", "win:a", "win:c"]);
  assert.deepEqual(
    slots.flatMap((slot) => (slot.kind === "window" ? [slot.pinned] : [])),
    [true, true, false, false],
  );
});

test("pins the account cannot open show no launcher; nothing pinned lists windows only", () => {
  const noMonitor = (appId: string) => appId !== "monitor";
  assert.deepEqual(label(taskbarSlots(PINS, [], noMonitor)), ["pin:files", "pin:controlpanel"]);
  assert.deepEqual(label(taskbarSlots([], [{ id: "x", appId: "files" }, { id: "y", appId: "logs" }], allowed)), [
    "win:x",
    "win:y",
  ]);
  // A repeated pin is placed once.
  assert.deepEqual(label(taskbarSlots(["files", "files"], [], allowed)), ["pin:files"]);
});
