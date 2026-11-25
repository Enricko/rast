import assert from "node:assert/strict";
import test from "node:test";

import {
  filterApps,
  filterPackages,
  groupByChannel,
  installedLabel,
  jobCancellable,
  jobFraction,
  linkLabel,
  monogram,
  sortForTiles,
  statusLine,
  suggestedVersion,
  systemSourceLabel,
  systemWideText,
  tileState,
  updateSummary,
  upToDateText,
  versionOptionLabel,
} from "../src/apps/packages/format";
import type { AppPackage, PackageView, VersionChoice } from "../src/apps/packages/types";

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
    updates: [{ channel: "1.26", from: "1.26.10", to: "1.26.11", revision: 1, replacesDefault: true, fromSystem: false }],
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

test("a runtime the system already has counts as installed and can be updated", () => {
  const system = {
    command: "php",
    path: "/usr/bin/php",
    realPath: "/usr/bin/php8.5",
    version: "8.5.4",
    versionCommand: "php -n --version",
    output: "PHP 8.5.4 (cli)",
    source: "apt",
    sourcePackage: "php8.5-cli",
    onPath: true,
    detectedAt: 0,
  };
  const php = pkg("php", {
    system,
    updateState: "ready",
    updates: [{ channel: "8.5", from: "8.5.4", to: "8.5.12", revision: 1, replacesDefault: true, fromSystem: true }],
  });
  const go = pkg("go");
  assert.deepEqual(filterPackages([php, go], "installed").map((p) => p.id), ["php"]);
  assert.deepEqual(updateSummary(php), { kind: "ready", text: "Update ready: 8.5.4 (system) → 8.5.12" });
  assert.equal(systemSourceLabel(system), "apt · php8.5-cli");
  assert.equal(systemSourceLabel({ source: "manual", sourcePackage: null }), "manual install");
  assert.equal(systemSourceLabel({ source: "snap" }), "snap");
});

test("each tile offers one action with one short line", () => {
  const system = {
    command: "php",
    path: "/usr/bin/php",
    realPath: "/usr/bin/php8.5",
    version: "8.5.4",
    versionCommand: "php -n --version",
    output: "",
    source: "apt",
    onPath: true,
    detectedAt: 0,
  };
  const update = pkg("php", {
    system,
    versions: [choice("8.5.12")],
    updates: [{ channel: "8.5", from: "8.5.4", to: "8.5.12", revision: 1, replacesDefault: true, fromSystem: true }],
  });
  assert.deepEqual(tileState(update), { action: "update", subtitle: "8.5.4 → 8.5.12", version: "8.5.12", makeDefault: true });
  assert.deepEqual(statusLine(update), { kind: "update", text: "Update available: 8.5.4 (system) → 8.5.12" });

  const fresh = pkg("go", { versions: [choice("1.27.4"), choice("1.26.11")] });
  assert.deepEqual(tileState(fresh), { action: "install", subtitle: "1.27.4", version: "1.27.4", makeDefault: true });
  assert.deepEqual(statusLine(fresh), { kind: "available", text: "Not installed" });

  const current = pkg("nodejs", {
    installed: [{ version: "22.23.5", isDefault: true, channel: "22" } as never, { version: "24.22.3", isDefault: false } as never],
    versions: [choice("26.13.1", { channel: "26" }), choice("22.23.5", { channel: "22", installedRevision: 1 })],
    latest: choice("26.13.1", { channel: "26" }),
  });
  assert.deepEqual(tileState(current), { action: "installed", subtitle: "22.23.5 +1" });
  assert.equal(upToDateText(current), "Up to date in 22");
  assert.deepEqual(statusLine(current), { kind: "installed", text: "Installed · up to date in 22" });
  assert.equal(statusLine(pkg("n", { ...current, versions: current.versions.map((v) => ({ ...v, channelLabel: `Node.js ${v.channel}` })) })).text, "Installed · up to date in Node.js 22");

  const systemOnly = pkg("python", { system: { ...system, command: "python3", version: "3.14.4" }, versions: [choice("3.14.4", { channel: "3.14" })], latest: choice("3.14.4", { channel: "3.14" }) });
  assert.equal(installedLabel(systemOnly), "3.14.4 · System");
  assert.deepEqual(statusLine(systemOnly), { kind: "installed", text: "Installed (system) · up to date" });

  const empty = pkg("lua", { unavailableReason: "No verified version is published in the catalog yet." });
  assert.deepEqual(tileState(empty), { action: "unavailable", subtitle: "Not available" });
  assert.equal(statusLine(empty).kind, "unavailable");
  // Third-party packages are installed release by release, never from the tile.
  assert.equal(tileState(pkg("tool", { official: false, versions: [choice("1.0.0")] })).action, "none");
});

test("tiles list updates first, then installed, then the rest", () => {
  const a = pkg("a", { name: "Alpha" });
  const b = pkg("b", { name: "Beta", installed: [{ version: "1" } as never] });
  const c = pkg("c", { name: "Gamma", updates: [{ channel: "1", from: "1", to: "2", revision: 1, replacesDefault: true, fromSystem: false }] });
  const d = pkg("d", { name: "Delta" });
  assert.deepEqual(sortForTiles([a, b, c, d]).map((p) => p.id), ["c", "b", "a", "d"]);
  assert.equal(linkLabel("https://www.python.org/downloads/"), "python.org");
  assert.equal(linkLabel("not a url"), "not a url");
});

test("an install can be cancelled until it moves into place", () => {
  for (const m of ["Queued: install 8.5.12", "Downloading 3.0 MB of 30.0 MB", "Extracting (120 files)", "Checking executables", "Verified SHA-256 and size"]) {
    assert.equal(jobCancellable(m), true, m);
  }
  for (const m of ["Moving into place", "Recording installation", "Making it the RAST default"]) {
    assert.equal(jobCancellable(m), false, m);
  }
});

test("sudo and services follow the system-wide links", () => {
  const terminal = { command: "php", source: "package-center", version: "8.5.12", path: "/var/lib/rast/packages/bin/php" };
  const system = {
    command: "php", path: "/usr/bin/php", realPath: "/usr/bin/php8.5", version: "8.5.4", versionCommand: "php -n --version",
    output: "PHP 8.5.4", source: "apt", sourcePackage: "php8.5-cli", onPath: true, detectedAt: 0,
  };
  const p = pkg("php", { defaultVersion: "8.5.12", terminal, system });
  const links = { enabled: true, linked: ["php"], pending: [], displaced: [], directory: "/usr/local/bin", watcher: true };
  assert.deepEqual(systemWideText(p, links), { text: "run 8.5.12", sub: "Linked in /usr/local/bin" });
  assert.equal(systemWideText(p, { ...links, enabled: false })?.text, "run the system 8.5.4");
  assert.equal(systemWideText(p, { ...links, linked: [], watcher: false })?.text, "Not linked yet");
  assert.equal(systemWideText(p, { ...links, linked: [] })?.text, "Linking…");
  assert.equal(systemWideText(pkg("php", { defaultVersion: null, terminal, system }), links), null, "no App Center default");
  assert.equal(systemSourceLabel({ source: "manual", sourcePackage: null, displaced: true }), "manual install · moved aside");
});

test("RAST apps: Installed hides uninstalled ones, Updates has none, search matches name and description", () => {
  const apps: AppPackage[] = [
    { id: "files", name: "File Station", description: "Browse files.", installed: true, removable: false },
    { id: "applications", name: "Docker", description: "Docker containers.", installed: false, removable: true },
    { id: "backup", name: "Backup Center", description: "Back up data.", installed: true, removable: true },
  ];
  assert.deepEqual(filterApps(apps, "installed", "").map((a) => a.id), ["files", "backup"]);
  assert.deepEqual(filterApps(apps, "all", "").map((a) => a.id), ["files", "applications", "backup"]);
  assert.deepEqual(filterApps(apps, "updates", ""), []);
  assert.deepEqual(filterApps(apps, "all", "docker").map((a) => a.id), ["applications"]);
  assert.deepEqual(filterApps(apps, "installed", "docker"), []);
});
