import { test } from "node:test";
import assert from "node:assert/strict";
import { SDK_MODULES } from "../src/core/sdk";
import pinned from "../src/core/sdk-v1.json";
import { allApps, findApp, permissionsSignature, setPackageApps } from "../src/apps/registry";

test("SDK v1 keeps every export package apps were built against", () => {
  for (const [name, exports] of Object.entries(pinned as Record<string, string[]>)) {
    const module = (SDK_MODULES as Record<string, Record<string, unknown>>)[name];
    assert.ok(module, `SDK module ${name}`);
    for (const key of exports) assert.ok(key in module, `${name}.${key} is part of SDK v1`);
  }
});

test("a Package Center app takes the place of a built-in app with its id", () => {
  const builtIn = findApp("agent");
  assert.ok(builtIn && !builtIn.packageVersion);
  const packaged = { id: "agent", name: "AI Agent", icon: "package", description: "", packageVersion: "1.0.0", mount() {} };
  const before = permissionsSignature(null, true);
  assert.equal(setPackageApps([packaged]), true);
  assert.equal(setPackageApps([{ ...packaged }]), false, "same versions: unchanged");
  assert.equal(findApp("agent")?.packageVersion, "1.0.0");
  assert.equal(allApps().filter((app) => app.id === "agent").length, 1);
  assert.notEqual(permissionsSignature(null, true), before, "an update shows in the signature");
  assert.equal(setPackageApps([]), true);
  assert.equal(findApp("agent"), builtIn);
});
