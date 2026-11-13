import assert from "node:assert/strict";
import test from "node:test";
import { WindowManager } from "../src/core/wm";

test("focusing the same window is idempotent even inside a subscriber", () => {
  let emissions = 0;
  const record = { el: { style: { zIndex: "0" }, classList: { toggle() {} } }, id: "chrome" };
  const manager: any = {
    records: new Map([["chrome", record]]), focusedId: null, zCounter: 1,
    emitChange() {
      emissions++;
      // Native window snapshots may synchronously reflect focus back to WM.
      if (emissions < 5) WindowManager.prototype.focus.call(manager, "chrome");
    },
  };
  WindowManager.prototype.focus.call(manager, "chrome");
  WindowManager.prototype.focus.call(manager, "chrome");
  assert.equal(emissions, 1);
  assert.equal(manager.zCounter, 2);
  assert.equal(record.el.style.zIndex, "2");
});
