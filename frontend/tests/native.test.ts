import assert from "node:assert/strict";
import test from "node:test";

import {
  isNativeApp,
  linuxNativeSession,
  matchWindowApp,
  nativeAppsEnabled,
  nativeWindowAppId,
  setLinuxNativeSession,
  setLinuxOverview,
  type LinuxApp,
  type NativeSession,
} from "../src/apps/linuxapp/catalog";
import {
  previewArchitecture,
  previewCandidates,
  previewCompatibility,
  previewDependencies,
  previewIdentity,
  previewLauncher,
  previewLauncherOwner,
  previewName,
  previewNeedsChoice,
  previewPackageName,
  previewSummary,
  previewTransaction,
  previewVersion,
  type LinuxInstallPreview,
} from "../src/apps/packages/linux-preview";

function app(id: string, extra: Partial<LinuxApp> = {}): LinuxApp {
  return {
    id,
    desktopId: `linux:${id}`,
    name: id.charAt(0).toUpperCase() + id.slice(1),
    format: "deb",
    fileName: `${id}.deb`,
    version: "1.0.0",
    package: null,
    description: null,
    command: [`/usr/bin/${id}`],
    launchGuess: false,
    iconUrl: null,
    status: "ready",
    error: null,
    jobId: null,
    update: { source: null, latestVersion: null, available: false, checkedAt: null, error: null },
    memoryMb: 2048,
    cpus: 2,
    sizeBytes: 1024,
    visibility: "users",
    running: 0,
    createdAt: 0,
    updatedAt: 0,
    backend: "native",
    native: null,
    ...extra,
  };
}

function session(extra: Partial<NativeSession> = {}): NativeSession {
  return {
    id: "native",
    status: "ready",
    url: "/linux-apps/session/0123456789abcdef0123456789abcdef/",
    startedAt: 0,
    viewers: 1,
    separateWindows: true,
    reason: null,
    error: null,
    apps: [],
    ...extra,
  };
}

function preview(extra: Partial<LinuxInstallPreview> = {}): LinuxInstallPreview {
  return {
    format: "deb",
    fileName: "example_2.0_amd64.deb",
    sizeBytes: 12_000_000,
    architecture: "amd64",
    source: { kind: "path", path: "/Downloads/example_2.0_amd64.deb" },
    app: {
      name: "Example",
      version: "2.0",
      package: "example",
      launcher: { argv: ["/opt/example/example"] },
    },
    candidates: [{ name: "Example", argv: ["/opt/example/example"], desktopEntry: "example.desktop" }],
    needsChoice: false,
    compatibility: { ok: true, reasons: [], warnings: [] },
    deb: {
      package: { name: "example", version: "2.0", preexisting: false },
      transaction: { installs: [], removals: [], upgrades: [], downloadBytes: 0 },
      dependencies: [],
    },
    ...extra,
  };
}

test("isNativeApp only accepts backend native", () => {
  assert.equal(isNativeApp(app("chrome")), true);
  assert.equal(isNativeApp(app("chrome", { backend: "docker" })), false);
  assert.equal(isNativeApp(undefined), false);
  assert.equal(isNativeApp(null), false);
});

test("nativeWindowAppId keeps the linux:<app>:<window> shape", () => {
  assert.equal(nativeWindowAppId("chrome", 42), "linux:chrome:42");
  assert.equal(nativeWindowAppId("chrome", "8388620"), "linux:chrome:8388620");
  assert.equal(nativeWindowAppId("chrome", 42).split(":").length, 3);
});

test("matchWindowApp matches X11 class-instance to installed apps", () => {
  const chrome = app("chrome", { package: "google-chrome-stable", command: ["/usr/bin/google-chrome"] });
  const files = app("files", { package: "nautilus", command: ["/usr/bin/nautilus"] });
  assert.equal(matchWindowApp([chrome, files], ["Google-chrome", "google-chrome"])?.id, "chrome");
  assert.equal(matchWindowApp([chrome, files], ["Org.gnome.Nautilus", "Nautilus"])?.id, "files");
  assert.equal(matchWindowApp([chrome], ["xterm", "XTerm"]), null);
});

test("matchWindowApp prefers exact matches over prefixes", () => {
  const short = app("term", { command: ["/usr/bin/xterm"], package: "xterm" });
  const other = app("xterm256", { command: ["/usr/bin/xterm-256color"], package: "xterm-256color" });
  const matched = matchWindowApp([short, other], ["XTerm", "xterm"]);
  assert.equal(matched?.id, "term");
});

test("nativeAppsEnabled reflects the server overview flag", () => {
  setLinuxOverview({
    apps: [],
    sessions: [],
    limits: { minFreeMemoryBytes: 0, idleMinutes: 30, maxCpus: 4 },
    dockerEnabled: true,
    nativeEnabled: true,
    session: null,
  });
  assert.equal(nativeAppsEnabled(), true);
  setLinuxOverview({
    apps: [],
    sessions: [],
    limits: { minFreeMemoryBytes: 0, idleMinutes: 30, maxCpus: 4 },
    dockerEnabled: true,
  });
  assert.equal(nativeAppsEnabled(), false);
});

test("the shared native session is stored and cleared", () => {
  const state = session({ status: "starting", apps: [{ runId: "run-1", appId: "chrome", name: "Chrome", status: "starting" }] });
  setLinuxOverview({
    apps: [],
    sessions: [],
    limits: { minFreeMemoryBytes: 0, idleMinutes: 30, maxCpus: 4 },
    dockerEnabled: true,
    nativeEnabled: true,
    session: state,
  });
  assert.equal(linuxNativeSession()?.status, "starting");
  setLinuxNativeSession(null);
  assert.equal(linuxNativeSession(), null);
});

test("previewName/version/architecture read the current contract", () => {
  const value = preview();
  assert.equal(previewName(value), "Example");
  assert.equal(previewVersion(value), "2.0");
  assert.equal(previewArchitecture(value), "amd64");
  assert.equal(previewPackageName(value), "example");
  assert.equal(previewLauncherOwner(previewLauncher(value)), "/opt/example/example");
});

test("preview readers fall back to the older flat shape", () => {
  const legacy: LinuxInstallPreview = {
    name: "Legacy",
    version: "1.0",
    fileName: "legacy.deb",
    package: { name: "legacy", preexisting: true },
    launcher: { argv: ["/usr/bin/legacy"] },
    dependencies: ["libfoo"],
    compatibility: { ok: false, reasons: ["needs glibc 9"] },
  };
  assert.equal(previewName(legacy), "Legacy");
  assert.equal(previewVersion(legacy), "1.0");
  assert.equal(previewPackageName(legacy), "legacy");
  assert.equal(previewLauncherOwner(previewLauncher(legacy)), "/usr/bin/legacy");
  assert.deepEqual(previewDependencies(legacy), ["libfoo"]);
  assert.equal(previewCompatibility(legacy).ok, false);
});

test("previewIdentity lists file, format, version and source", () => {
  const rows = previewIdentity(
    preview({
      source: { kind: "url", url: "https://example.test/example.deb" },
    }),
  );
  const map = new Map(rows.map((row) => [row.label, row.value]));
  assert.equal(map.get("File"), "example_2.0_amd64.deb");
  assert.equal(map.get("Format"), "DEB");
  assert.equal(map.get("Version"), "2.0");
  assert.equal(map.get("Architecture"), "amd64");
  assert.equal(map.get("Downloaded from"), "https://example.test/example.deb");
  assert.equal(map.get("Starts"), "/opt/example/example");
  assert.equal(map.get("Package"), "example 2.0");
});

test("previewTransaction groups removals/upgrades/installs and the download size", () => {
  const summary = previewTransaction(
    preview({
      deb: {
        package: { name: "example", version: "3.0", preexisting: true },
        transaction: {
          removals: [{ name: "old-lib", version: "1.0" }],
          upgrades: [{ name: "libfoo", version: "2.0", oldVersion: "1.5" }],
          installs: [{ name: "libbar", version: "1.0" }],
          downloadBytes: 5_000_000,
        },
      },
    }),
  );
  assert.equal(summary.removals, 1);
  assert.equal(summary.upgrades, 1);
  assert.equal(summary.installs, 1);
  assert.equal(summary.downloadBytes, 5_000_000);
  assert.equal(summary.preexisting, true);
  assert.equal(summary.hasChanges, true);
  assert.deepEqual(
    summary.rows.map((row) => [row.kind, row.name, row.oldVersion]),
    [
      ["remove", "old-lib", null],
      ["upgrade", "libfoo", "1.5"],
      ["install", "libbar", null],
    ],
  );
});

test("previewTransaction without a deb transaction has no changes", () => {
  const summary = previewTransaction(preview({ deb: null }));
  assert.equal(summary.hasChanges, false);
  assert.equal(summary.downloadBytes, 0);
  assert.equal(summary.preexisting, false);
});

test("previewCompatibility fails on explicit incompatibility or reasons, keeping warnings", () => {
  assert.deepEqual(previewCompatibility(preview({ compatibility: { ok: true, reasons: [], warnings: [] } })), {
    ok: true,
    reasons: [],
    warnings: [],
  });
  const blocked = previewCompatibility(preview({ compatibility: { ok: false, reasons: ["needs glibc 2.39"], warnings: ["unsigned"] } }));
  assert.equal(blocked.ok, false);
  assert.deepEqual(blocked.reasons, ["needs glibc 2.39"]);
  assert.deepEqual(blocked.warnings, ["unsigned"]);
  const warned = previewCompatibility(preview({ compatibility: { ok: true, reasons: [], warnings: ["unsigned"] } }));
  assert.equal(warned.ok, true);
  assert.deepEqual(warned.warnings, ["unsigned"]);
  assert.equal(previewCompatibility(preview({ compatibility: {} })).ok, true);
});

test("previewDependencies filters empty entries", () => {
  assert.deepEqual(previewDependencies(preview({ deb: { dependencies: ["libgtk-3-0", "", "  "] } })), ["libgtk-3-0"]);
  assert.deepEqual(previewDependencies(preview({ deb: null })), []);
});

test("preview candidates: the admin must choose when several programs are found", () => {
  const multi = preview({
    candidates: [
      { name: "One", argv: ["/usr/bin/one"] },
      { name: "Two", argv: ["/usr/bin/two"] },
    ],
    app: { name: "Example", launcher: null },
    needsChoice: true,
  });
  assert.equal(previewNeedsChoice(multi), true);
  assert.equal(previewCandidates(multi).length, 2);
  assert.equal(previewNeedsChoice(preview()), false);
  const single = preview({ candidates: [{ argv: ["/usr/bin/only"] }], app: { name: "Example", launcher: null } });
  assert.equal(previewNeedsChoice(single), false);
  assert.equal(previewCandidates(single).length, 1);
});

test("previewSummary mentions the number of package changes", () => {
  const withChanges = previewSummary(
    preview({
      app: { name: "Chrome", version: "130" },
      deb: {
        transaction: {
          installs: [
            { name: "a", version: "1" },
            { name: "b", version: "1" },
          ],
          removals: [],
          upgrades: [],
        },
      },
    }),
  );
  assert.match(withChanges, /Install Chrome 130\?/);
  assert.match(withChanges, /changes 2 packages/);
  const plain = previewSummary(preview({ app: { name: "Chrome", version: "130" }, deb: null }));
  assert.equal(plain, "Install Chrome 130?");
});
