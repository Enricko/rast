// Full screen: the keyboard shortcut and its hint.
import assert from "node:assert/strict";
import test from "node:test";
import { fullscreenShortcutLabel, isFullscreenShortcut } from "../src/core/fullscreen";

test("Ctrl+Shift+F (⌘⇧F on a Mac) toggles full screen", () => {
  assert.equal(isFullscreenShortcut({ key: "F", code: "KeyF", ctrlKey: true, shiftKey: true }, "windows"), true);
  assert.equal(isFullscreenShortcut({ key: "F", code: "KeyF", ctrlKey: true, shiftKey: true }, "other"), true);
  assert.equal(isFullscreenShortcut({ key: "F", code: "KeyF", metaKey: true, shiftKey: true }, "mac"), true);
  // Other layouts: the key, whatever its position.
  assert.equal(isFullscreenShortcut({ key: "f", code: "KeyU", ctrlKey: true, shiftKey: true }, "other"), true);
  assert.equal(isFullscreenShortcut({ key: "f", code: "KeyF", ctrlKey: true }, "windows"), false);
  assert.equal(isFullscreenShortcut({ key: "F", code: "KeyF", ctrlKey: true, shiftKey: true, altKey: true }, "windows"), false);
  assert.equal(isFullscreenShortcut({ key: "F", code: "KeyF", ctrlKey: true, shiftKey: true, repeat: true }, "windows"), false);
  assert.equal(isFullscreenShortcut({ key: "F", code: "KeyF", metaKey: true, shiftKey: true }, "windows"), false);
  assert.equal(fullscreenShortcutLabel("mac"), "⌘⇧F");
  assert.equal(fullscreenShortcutLabel("windows"), "Ctrl+Shift+F");
});
