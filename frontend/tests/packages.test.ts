import assert from "node:assert/strict";
import test from "node:test";

import {
  filterPackages,
  groupByChannel,
  jobFraction,
  monogram,
  suggestedVersion,
  updateSummary,
  versionOptionLabel,
} from "../src/apps/packages/format";
import type { PackageView, VersionChoice } from "../src/apps/packages/types";

function choice(version: string, extra: Partial<VersionChoice> = {}): VersionChoice {
  return {
    version,
    revision: 1,
    channel: version.split(".").slice(0, 2).join("."),
    prerelease: false,
    compatible: true,
    targets: ["linux-x86_64"],
    bundled: [],
    extensions: [],
    executables: [],
    ...extra,
  };
}

function pkg(id: string, extra: Partial<PackageView> = {}): PackageView {
  return {
    id,
    packageType: "runtime",
    name: id,
    description: "",
    publisher: { id: "rast", name: "RAST" },
    source: "official",
    official: true,
    verification: { kind: "signed-catalog", label: "", detail: "" },
    inCatalog: true,
    installed: [],
    defaultExecutables: [],
    versions: [],
    unsupported: [],
    updates: [],
    detected: [],
    updateState: "none",
    consumers: [],
    ...extra,
  };
}

test("views filter installed packages and packages with updates", () => {
  const installed = pkg("go", { installed: [{ version: "1.26.11" } as never], updateState: "ready" });
  const detectedOnly = pkg("python", { installed: [{ version: "3.14.10" } as never], updateState: "detected" });
  const fresh = pkg("php", { description: "PHP CLI" });
  const all = [installed, detectedOnly, fresh];
  assert.deepEqual(filterPackages(all, "all").map((p) => p.id), ["go", "python", "php"]);
  assert.deepEqual(filterPackages(all, "installed").map((p) => p.id), ["go", "python"]);
  assert.deepEqual(filterPackages(all, "updates").map((p) => p.id), ["go", "python"]);
  assert.deepEqual(filterPackages(all, "all", "cli").map((p) => p.id), ["php"]);
});

test("ready updates and detected upstream versions are described differently", () => {
  const ready = pkg("go", {
    updateState: "ready",
    updates: [{ channel: "1.26", from: "1.26.10", to: "1.26.11", revision: 1, replacesDefault: true }],
  });
  assert.deepEqual(updateSummary(ready), { kind: "ready", text: "Update ready: 1.26.10 → 1.26.11" });
  const detected = pkg("go", {
    updateState: "detected",
    detected: [{ version: "1.26.12", channel: "1.26", state: "verifying", detail: "", detectedAt: "", source: "catalog" }],
  });
  assert.equal(updateSummary(detected).kind, "detected");
  assert.match(updateSummary(detected).text, /New upstream 1\.26\.12 detected — being verified/);
  assert.equal(updateSummary(pkg("x")).kind, "none");
});

test("version picker groups by channel and suggests the newest stable compatible version", () => {
  const versions = [
    choice("3.15.0rc1", { channel: "3.15", prerelease: true }),
    choice("3.14.10", { installedRevision: 1 }),
    choice("3.13.18"),
    choice("3.12.1", { compatible: false, reason: "Requires glibc 2.40" }),
  ];
  assert.deepEqual(groupByChannel(versions).map((g) => g.channel), ["3.15", "3.14", "3.13", "3.12"]);
  assert.equal(suggestedVersion(pkg("python", { versions }))?.version, "3.13.18");
  assert.equal(versionOptionLabel(versions[0]!), "3.15.0rc1 · pre-release");
  assert.equal(versionOptionLabel(versions[1]!), "3.14.10 · installed");
  assert.equal(versionOptionLabel(versions[3]!), "3.12.1 · unavailable");
  assert.equal(versionOptionLabel(choice("3.14.10", { revision: 2, installedRevision: 1 })), "3.14.10 · rev 2 · installed (older revision)");
});

test("progress fraction is bounded and monograms are short", () => {
  assert.equal(jobFraction(5, 10), 0.5);
  assert.equal(jobFraction(20, 10), 1);
  assert.equal(jobFraction(3, 0), 0);
  assert.equal(monogram("Node.js", "nodejs"), "JS");
  assert.equal(monogram("Example Tool"), "Ex");
});
