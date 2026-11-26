// Terminal app (apps/terminal): messages, shell lists, tab names, reconnects.
import assert from "node:assert/strict";
import test from "node:test";

import { isBusy, MAX_RECONNECTS, parseShellList, parseShellMessage, reconnectDelay, sandboxSummary, shellSocketUrl, tabLabel } from "../src/apps/terminal/shell";

const shell = { id: "0123456789abcdef", number: 2, createdAt: 1, attached: false, program: "htop" };

test("the socket is on the page's own host", () => {
  assert.equal(shellSocketUrl({ protocol: "https:", host: "desktop.example.com" }), "wss://desktop.example.com/api/terminal/ws");
  assert.equal(shellSocketUrl({ protocol: "http:", host: "127.0.0.1:8720" }), "ws://127.0.0.1:8720/api/terminal/ws");
});

test("server messages are parsed strictly", () => {
  assert.deepEqual(parseShellMessage(JSON.stringify({ type: "ready", session: shell, created: true })), { type: "ready", session: shell, created: true });
  assert.deepEqual(parseShellMessage('{"type":"exit","code":3}'), { type: "exit", code: 3 });
  assert.deepEqual(parseShellMessage('{"type":"exit"}'), { type: "exit", code: null });
  assert.deepEqual(parseShellMessage('{"type":"detached"}'), { type: "detached" });
  assert.deepEqual(parseShellMessage('{"type":"error"}'), { type: "error", message: "The terminal is not available." });
  for (const bad of ["", "nope", "[]", '{"type":"exec"}', JSON.stringify({ type: "ready", session: { ...shell, id: "../x" } })]) {
    assert.equal(parseShellMessage(bad), null, bad);
  }
});

test("only valid shells are listed", () => {
  assert.deepEqual(parseShellList([shell, { id: "short" }, null, "x"]), [shell]);
  assert.deepEqual(parseShellList(undefined), []);
});

test("tabs name the program that is running", () => {
  assert.equal(tabLabel({ number: 1, program: "bash" }), "1");
  assert.equal(tabLabel({ number: 2, program: "htop" }), "2 · htop");
  assert.equal(tabLabel({ number: 3, program: "" }), "3");
  assert.equal(isBusy("vim"), true);
  assert.equal(isBusy("zsh"), false);
});

test("reconnects back off and give up after about two minutes", () => {
  assert.deepEqual([0, 1, 2, 3, 4, 9].map(reconnectDelay), [1000, 2000, 4000, 8000, 15000, 15000]);
  let total = 0;
  for (let attempt = 0; attempt < MAX_RECONNECTS; attempt += 1) total += reconnectDelay(attempt);
  assert.ok(total >= 90_000 && total <= 150_000, String(total));
});

test("a sandbox badge says what the sandbox allows", () => {
  assert.equal(sandboxSummary({ memoryMb: 1024, cpus: "2", network: false }), "Your own sandbox: only your home folder, no network, 1 GB memory, 2 CPUs");
  assert.equal(sandboxSummary({ memoryMb: 512, cpus: "1" }), "Your own sandbox: only your home folder, 512 MB memory, 1 CPU");
  assert.equal(sandboxSummary(undefined), "Your own sandbox: only your home folder");
});
