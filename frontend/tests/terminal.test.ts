import assert from "node:assert/strict";
import test from "node:test";

import { exitNotice, parseTerminalMessage, terminalUrl } from "../src/apps/applications/terminal";

test("terminal address follows the page's scheme and host", () => {
  const id = "0123456789ab";
  assert.equal(terminalUrl({ protocol: "http:", host: "192.168.1.20:8720" }, id), "ws://192.168.1.20:8720/api/docker/containers/0123456789ab/terminal");
  assert.equal(terminalUrl({ protocol: "https:", host: "rast.example.com" }, id), "wss://rast.example.com/api/docker/containers/0123456789ab/terminal");
  assert.equal(terminalUrl({ protocol: "http:", host: "nas.local" }, "a/b?c"), "ws://nas.local/api/docker/containers/a%2Fb%3Fc/terminal");
});

test("server messages are parsed strictly", () => {
  assert.deepEqual(parseTerminalMessage('{"type":"ready","container":"drive-app"}'), { type: "ready", container: "drive-app" });
  assert.deepEqual(parseTerminalMessage('{"type":"exit","code":130}'), { type: "exit", code: 130 });
  assert.deepEqual(parseTerminalMessage('{"type":"exit","code":null}'), { type: "exit", code: null });
  assert.deepEqual(parseTerminalMessage('{"type":"error","message":"The container is not running."}'), { type: "error", message: "The container is not running." });
  assert.deepEqual(parseTerminalMessage('{"type":"error"}'), { type: "error", message: "The terminal is not available." });
  for (const bad of ["", "not json", "42", "null", '{"type":"run","cmd":"ls"}', "[]"]) {
    assert.equal(parseTerminalMessage(bad), null, bad);
  }
});

test("exit notices are plain sentences", () => {
  assert.equal(exitNotice(0), "The shell exited.");
  assert.equal(exitNotice(137), "The shell exited with code 137.");
  assert.equal(exitNotice(null), "The terminal session ended.");
  assert.equal(exitNotice(127, true), "This container has no shell (sh), so a terminal cannot run in it.");
  assert.equal(exitNotice(127, false), "The shell exited with code 127.");
});
