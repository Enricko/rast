// Tapping the OS key (⌘ Cmd on a Mac, the Windows key on Windows) opens the
// start menu; combinations such as ⌘C must not.
import assert from "node:assert/strict";
import test from "node:test";

import { detectPlatform, OS_KEY_TAP_MS, OsKeyTap, osKeyLabel, startMenuHint } from "../src/core/os-key";

const key = (k: string, timeStamp: number, extra: Record<string, unknown> = {}) => ({ key: k, timeStamp, ...extra });

test("a lone tap of the OS key is a tap", () => {
  const tap = new OsKeyTap();
  tap.keydown(key("Meta", 0));
  assert.equal(tap.keyup(key("Meta", 120)), true);
  // Old Firefox names it "OS".
  tap.keydown(key("OS", 200));
  assert.equal(tap.keyup(key("OS", 300)), true);
});

test("combinations, clicks, focus changes and long holds are not taps", () => {
  const tap = new OsKeyTap();
  tap.keydown(key("Meta", 0));
  tap.keydown(key("c", 50, { metaKey: true }));
  assert.equal(tap.keyup(key("Meta", 100)), false, "⌘C");

  tap.keydown(key("Shift", 0));
  tap.keydown(key("Meta", 10, { shiftKey: true }));
  assert.equal(tap.keyup(key("Meta", 60, { shiftKey: true })), false, "Shift+Win");

  tap.keydown(key("Meta", 0));
  tap.keydown(key("Shift", 20, { metaKey: true }));
  assert.equal(tap.keyup(key("Meta", 80, { shiftKey: true })), false, "⌘⇧4");

  tap.keydown(key("Meta", 0));
  tap.cancel(); // ⌘-click, scroll, ⌘Tab (blur)
  assert.equal(tap.keyup(key("Meta", 90)), false, "⌘-click");

  tap.keydown(key("Meta", 0));
  tap.keydown(key("Meta", 500, { repeat: true }));
  assert.equal(tap.keyup(key("Meta", OS_KEY_TAP_MS + 1)), false, "held too long");

  assert.equal(tap.keyup(key("Meta", 10)), false, "a release without a press");
  // After all that, a clean tap still works.
  tap.keydown(key("Meta", 2000));
  assert.equal(tap.keyup(key("Meta", 2100)), true);
});

test("the platform decides the key's name and the hints", () => {
  assert.equal(detectPlatform({ userAgentData: { platform: "macOS" } }), "mac");
  assert.equal(detectPlatform({ platform: "MacIntel" }), "mac");
  assert.equal(detectPlatform({ platform: "Win32" }), "windows");
  assert.equal(detectPlatform({ platform: "Linux x86_64", userAgent: "X11; Linux x86_64" }), "other");
  assert.equal(detectPlatform(undefined), "other");
  assert.equal(osKeyLabel("mac"), "⌘ Cmd");
  assert.equal(osKeyLabel("windows", true), "Win");
  assert.deepEqual(startMenuHint("mac"), { short: "⌘", tip: "⌘ Cmd or Ctrl+Space" });
  assert.deepEqual(startMenuHint("windows"), { short: "Win", tip: "Windows key or Ctrl+Space" });
  assert.deepEqual(startMenuHint("other"), { short: "Ctrl+Space", tip: "Ctrl+Space" });
});
