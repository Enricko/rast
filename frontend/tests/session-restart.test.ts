import assert from "node:assert/strict";
import test from "node:test";

import { restartMessage, restartToastTimeout } from "../src/shell/session-restart";

test("a revocation warns with the seconds left", () => {
  const now = 1_000_000;
  const forced = { reason: "access_revoked", forced: true, deadlineMs: now + 59_500 };
  assert.match(restartMessage(forced, now), /restart in 60 s: save your work/);
  assert.match(restartMessage(forced, now + 70_000), /restart now/);
  assert.equal(restartToastTimeout(forced, now), 59_500);
  assert.equal(restartToastTimeout(forced, now + 70_000), 1000);
});

test("a grant only offers the restart and stays until dismissed", () => {
  const offer = { reason: "access_granted", forced: false, deadlineMs: null };
  assert.match(restartMessage(offer, 0), /Restart your session/);
  assert.equal(restartToastTimeout(offer, 0), 0);
});
