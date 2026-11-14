import assert from "node:assert/strict";
import test from "node:test";
import { readFileSync } from "node:fs";
import vm from "node:vm";

// Execute the actual injected bridge, not a second implementation of its math.
const source = readFileSync("../src/linux_apps/xpra_shim.js", "utf8");
function bridge() {
  const content = {
    getBoundingClientRect: () => ({ left: 140, top: 90, width: 500, height: 300 }),
  };
  const adopted = { getAttribute: () => "7" };
  const target = { closest: () => adopted };
  const win = { wid: 7, x: 400, y: 200, w: 1000, h: 600, __rastContentEl: content };
  const client = {
    container: {}, id_to_window: { 7: win }, connected: true,
    getMouse: () => ({ x: -1, y: -1, button: 0 }),
  };
  const window = {
    client, parent: null, addEventListener() {}, setInterval() { return 1; },
    clearInterval() {}, setTimeout() { return 1; }, clearTimeout() {},
    location: { pathname: "/linux-apps/session/0123456789abcdef0123456789abcdef/" },
    RASTNative: undefined as any,
  };
  vm.runInNewContext(source, { window, console: { info() {}, warn() {} }, document: {} });
  return { client, win, target, api: window.RASTNative };
}

test("adopted pointer motion and buttons use identical remote coordinates", () => {
  const { client, win, target } = bridge();
  const event = { clientX: 240, clientY: 150, button: 0, target };
  // Xpra on_mousemove calls getMouse(e) without win; buttons pass win.
  const motion = client.getMouse(event);
  const click = client.getMouse(event, win);
  assert.deepEqual(motion, click);
  assert.equal(click.x, 600);
  assert.equal(click.y, 320);
  assert.equal(click.button, 1);
  assert.equal(click.x - win.x, 200);
  assert.equal(click.y - win.y, 120);
});

test("jQuery wrapped buttons use the native event target and button", () => {
  const { client, target } = bridge();
  const result = client.getMouse({ originalEvent: { clientX: 140, clientY: 90, which: 3, target } });
  assert.equal(result.x, 400);
  assert.equal(result.y, 200);
  assert.equal(result.button, 3);
});

test("non-adopted events retain upstream pointer behavior", () => {
  const { client } = bridge();
  assert.equal(client.getMouse({ clientX: 1 }).x, -1);
});

test("trusted audio unlock resumes only output and retries a blocked player", async () => {
  const { client, api } = bridge();
  let resumed = 0, played = 0;
  Object.assign(client, {
    audio_context: { state: "suspended", resume: async () => { resumed++; } },
    audio: { paused: true, play: async () => { played++; } },
  });
  assert.equal(api.unlockAudio(), true);
  assert.equal(resumed, 1);
  assert.equal(played, 1);
});
