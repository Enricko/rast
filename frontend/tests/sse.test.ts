// Regression: after a server restart the event sequence starts again at 1. A
// tab that kept the previous process's cursor dropped every new event as a
// "duplicate" (shared-folder uploads never reached an open File Station).
import { test } from "node:test";
import assert from "node:assert/strict";
import { acceptSequence } from "../src/core/sse";

test("hello starts a fresh cursor even when it is lower (server restarted)", () => {
  const cursor = acceptSequence(19, "hello", "1", { seq: 1 });
  assert.equal(cursor, 1);
  // The upload event of the new process is then delivered.
  assert.equal(acceptSequence(cursor!, "file.changed", "7", {}), 7);
});

test("hello without a payload falls back to the frame id", () => {
  assert.equal(acceptSequence(500, "hello", "42", null), 42);
  assert.equal(acceptSequence(500, "hello", "", null), 0);
});

test("id jumps are not gaps: events filtered for this account are skipped server-side", () => {
  assert.equal(acceptSequence(10, "file.changed", "25", {}), 25);
});

test("older frames on the same connection are dropped", () => {
  assert.equal(acceptSequence(30, "file.changed", "29", {}), null);
});

test("the initial monitor snapshot shares the hello id and is kept", () => {
  assert.equal(acceptSequence(12, "monitor", "12", {}), 12);
});

test("frames without an id keep the cursor", () => {
  assert.equal(acceptSequence(12, "resync", "", {}), 12);
  assert.equal(acceptSequence(12, "message", "not-a-number", {}), 12);
});
