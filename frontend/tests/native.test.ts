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
  APPIMAGE_REJECTED_MESSAGE,
  INSTALL_STAGES,
  installProgressFraction,
  installStageFromFraction,
  installStageFromMessage,
  installStageIndex,
  installStageLabel,
  isSupportedPackageFileName,
  PACKAGE_FILE_ACCEPT,
  packageFileProblem,
  previewArchitecture,
  previewAvailability,
  previewCandidates,
  previewCompatibility,
  previewDependencies,
  previewDuplicate,
  previewHost,
  previewIdentity,
  previewInstallActionLabel,
  previewInstallPlan,
  previewLauncher,
  previewLauncherOwner,
  previewName,
  previewNeedsChoice,
  previewPackageName,
  previewProfile,
  previewProfileSummary,
  previewSource,
  previewSummary,
  previewTransaction,
  previewVersion,
  UNSUPPORTED_FILE_MESSAGE,
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

// ------------------------------------------------- Install Linux App workflow

test("install stages map from the backend job messages", () => {
  assert.equal(installStageFromMessage("Validating the package"), "validating");
  assert.equal(installStageFromMessage("Installing the system package"), "installing");
  assert.equal(installStageFromMessage("Registering the application"), "registering");
  assert.equal(installStageFromMessage("Verifying the installation"), "registering");
  assert.equal(installStageFromMessage("Ready"), "ready");
  assert.equal(installStageFromMessage("installing the system package"), "installing");
  assert.equal(installStageFromMessage("Downloading the package"), "uploading");
  assert.equal(installStageFromMessage(""), "validating");
  assert.equal(installStageFromMessage("Something new", 3, 5), "registering");
  assert.equal(installStageFromMessage(undefined as unknown as string), "validating");
});

test("install stage fraction fallback covers the whole five-step job", () => {
  assert.deepEqual(INSTALL_STAGES, ["uploading", "validating", "installing", "registering", "ready"]);
  assert.equal(installStageIndex("uploading"), 0);
  assert.equal(installStageIndex("ready"), 4);
  assert.equal(installStageLabel("uploading"), "Uploading");
  assert.equal(installStageLabel("ready"), "Ready");
  assert.equal(installStageFromFraction(0, 0), "validating");
  assert.equal(installStageFromFraction(0, 5), "validating");
  assert.equal(installStageFromFraction(1, 5), "installing");
  assert.equal(installStageFromFraction(2, 5), "installing");
  assert.equal(installStageFromFraction(3, 5), "registering");
  assert.equal(installStageFromFraction(4, 5), "registering");
  assert.equal(installStageFromFraction(5, 5), "ready");
  assert.equal(installStageFromFraction(7, 5), "ready");
  assert.equal(installProgressFraction(1, 4), 0.25);
  assert.equal(installProgressFraction(9, 4), 1);
  assert.equal(installProgressFraction(1, 0), 0);
});

test("previewInstallPlan prefers the new install field and falls back to deb.preexisting", () => {
  const modern = previewInstallPlan(
    preview({
      install: {
        mode: "register",
        hostPackageInstalled: true,
        existingApp: { id: "example", name: "Example", version: "2.0", status: "ready", backend: "native" },
      },
    }),
  );
  assert.equal(modern.mode, "register");
  assert.equal(modern.hostPackageInstalled, true);
  assert.equal(modern.existingApp?.id, "example");
  assert.equal(modern.existingApp?.status, "ready");

  const legacyRegister = previewInstallPlan(
    preview({
      duplicate: null,
      deb: { package: { name: "example", version: "2.0", preexisting: true } },
    }),
  );
  assert.equal(legacyRegister.mode, "register");
  assert.equal(legacyRegister.hostPackageInstalled, true);
  assert.equal(legacyRegister.existingApp, null);

  const legacyDuplicate = previewInstallPlan(
    preview({
      deb: { package: { name: "example", version: "2.0", preexisting: true } },
      duplicate: { appId: "example", name: "Example", version: "1.0", status: "ready" },
    }),
  );
  assert.equal(legacyDuplicate.mode, "duplicate");
  assert.equal(legacyDuplicate.existingApp?.id, "example");

  const fresh = previewInstallPlan(preview());
  assert.equal(fresh.mode, "fresh");
  assert.equal(fresh.hostPackageInstalled, false);
  assert.equal(fresh.existingApp, null);
});

test("previewInstallActionLabel registers an existing package instead of installing it again", () => {
  const fresh = previewInstallPlan(preview());
  assert.equal(previewInstallActionLabel(fresh), "Install");
  const register = previewInstallPlan(preview({ install: { mode: "register", hostPackageInstalled: true } }));
  assert.equal(previewInstallActionLabel(register), "Add existing installation to RAST");
  const duplicate = previewInstallPlan(preview({ install: { mode: "duplicate" } }));
  assert.equal(previewInstallActionLabel(duplicate), "Install");
});

test("previewProfile reads saved user data and degrades to none", () => {
  assert.deepEqual(previewProfile(preview({ profile: { exists: true, users: 2 } })), { exists: true, users: 2 });
  assert.equal(previewProfileSummary({ exists: true, users: 2 }), "2 user profiles will be preserved");
  assert.equal(previewProfileSummary({ exists: true, users: 1 }), "1 user profile will be preserved");
  assert.deepEqual(previewProfile(preview({ profile: { exists: true } })), { exists: true, users: 0 });
  assert.equal(previewProfileSummary({ exists: true, users: 0 }), "Saved app data will be preserved");
  assert.deepEqual(previewProfile(preview()), { exists: false, users: 0 });
  assert.equal(previewProfileSummary({ exists: false, users: 0 }), "None yet");
  assert.deepEqual(previewProfile(preview({ profile: { users: -3 } })), { exists: false, users: 0 });
});

test("previewAvailability reads server choices and defaults to all users", () => {
  assert.deepEqual(previewAvailability(preview()), {
    defaultVisibility: "users",
    options: ["users", "admins"],
  });
  assert.deepEqual(
    previewAvailability(preview({ availability: { default: "admins", options: ["admins", "users"] } })),
    { defaultVisibility: "admins", options: ["admins", "users"] },
  );
  assert.deepEqual(
    previewAvailability(preview({ availability: { default: "admins", options: ["admins"] } })),
    { defaultVisibility: "admins", options: ["admins"] },
  );
  // Unknown or repeated values are ignored instead of being shown.
  assert.deepEqual(previewAvailability(preview({ availability: { options: ["admins", "root", "admins"] } })), {
    defaultVisibility: "users",
    options: ["users", "admins"],
  });
});

test("previewDuplicate normalizes duplicate and install.existingApp", () => {
  const duplicate = previewDuplicate(
    preview({ duplicate: { appId: "chrome", name: "Chrome", version: "130", status: "ready", canUpdate: true } }),
  );
  assert.equal(duplicate?.id, "chrome");
  assert.equal(duplicate?.name, "Chrome");
  assert.equal(duplicate?.version, "130");
  assert.equal(duplicate?.canUpdate, true);
  const fromInstall = previewDuplicate(
    preview({ duplicate: null, install: { mode: "duplicate", existingApp: { id: "files", name: "Files" } } }),
  );
  assert.equal(fromInstall?.id, "files");
  assert.equal(previewDuplicate(preview()), null);
});

test("previewHost and previewSource read the host and where the file comes from", () => {
  assert.deepEqual(previewHost(preview()), { distribution: null, architecture: null });
  assert.deepEqual(
    previewHost(preview({ hostDistribution: "Debian GNU/Linux 12", hostArchitecture: "amd64" })),
    { distribution: "Debian GNU/Linux 12", architecture: "amd64" },
  );
  assert.deepEqual(previewSource(preview()), { kind: "path", label: "File Station path", value: "/Downloads/example_2.0_amd64.deb" });
  assert.deepEqual(
    previewSource(preview({ source: { kind: "url", url: "https://example.test/example.deb" } })),
    { kind: "url", label: "Downloaded from", value: "https://example.test/example.deb" },
  );
  assert.deepEqual(previewSource(preview({ source: null })), {
    kind: "file",
    label: "File",
    value: "example_2.0_amd64.deb",
  });
});

test("packageFileProblem accepts .deb and portable archives and explains AppImage", () => {
  for (const name of ["app.deb", "app.tar.gz", "app.tgz", "app.tar.xz", "app.txz", "app.tar.bz2", "app.tbz2", "app.tar"]) {
    assert.equal(packageFileProblem(name), null, name);
    assert.equal(isSupportedPackageFileName(name), true, name);
  }
  assert.equal(packageFileProblem("app.AppImage"), APPIMAGE_REJECTED_MESSAGE);
  assert.match(APPIMAGE_REJECTED_MESSAGE, /not validated on this server/);
  assert.equal(packageFileProblem("app.appimage"), APPIMAGE_REJECTED_MESSAGE);
  assert.equal(isSupportedPackageFileName("app.AppImage"), false);
  assert.equal(packageFileProblem("setup.exe"), UNSUPPORTED_FILE_MESSAGE);
  assert.equal(packageFileProblem("archive.gz"), UNSUPPORTED_FILE_MESSAGE);
  assert.equal(packageFileProblem(""), UNSUPPORTED_FILE_MESSAGE);
  assert.match(PACKAGE_FILE_ACCEPT, /\.deb/);
  assert.doesNotMatch(PACKAGE_FILE_ACCEPT, /appimage/i);
});

test("previewTransaction reads the newer deb.preexisting flag", () => {
  const summary = previewTransaction(
    preview({
      deb: { preexisting: true, transaction: { installs: [], removals: [], upgrades: [] } },
    }),
  );
  assert.equal(summary.preexisting, true);
  const plan = previewInstallPlan(
    preview({ install: null, duplicate: null, deb: { preexisting: true } }),
  );
  assert.equal(plan.mode, "register");
  assert.equal(plan.hostPackageInstalled, true);
});

test("the current server contract preview normalizes for the install dialog", () => {
  // Shape produced by POST /api/linux-apps/preview on the native backend: the
  // flat `source` is the string "file", `deb.preexisting` is on `deb`, and the
  // additive install/profile/availability fields describe the install plan.
  const live: LinuxInstallPreview = {
    format: "deb",
    fileName: "google-chrome-stable_130.0_amd64.deb",
    sizeBytes: 118_000_000,
    sha256: "abc",
    source: "file" as unknown as LinuxInstallPreview["source"],
    architecture: "amd64",
    app: {
      name: "Google Chrome",
      version: "130.0",
      package: "google-chrome-stable",
      launcher: { argv: ["/usr/bin/google-chrome-stable"] },
    },
    candidates: [],
    needsChoice: false,
    compatibility: { ok: true, reasons: [], warnings: ["Chromium apps use the basic password store"] },
    deb: {
      preexisting: true,
      package: "google-chrome-stable",
      transaction: { installs: [], upgrades: [], removals: [], downloadBytes: 0 },
      dependencies: [],
    },
    update: null,
    warnings: ["Chromium apps use the basic password store"],
    duplicate: null,
    profile: { exists: true, users: 2 },
    install: { mode: "register", hostPackageInstalled: true, existingApp: null },
    availability: { default: "users", options: ["users", "admins"] },
    hostArchitecture: "amd64",
    hostDistribution: "Debian GNU/Linux 12",
  };
  assert.equal(previewName(live), "Google Chrome");
  assert.equal(previewVersion(live), "130.0");
  assert.deepEqual(previewSource(live), {
    kind: "file",
    label: "File",
    value: "google-chrome-stable_130.0_amd64.deb",
  });
  assert.deepEqual(previewHost(live), { distribution: "Debian GNU/Linux 12", architecture: "amd64" });
  assert.equal(previewTransaction(live).preexisting, true);
  const plan = previewInstallPlan(live);
  assert.equal(plan.mode, "register");
  assert.equal(previewInstallActionLabel(plan), "Add existing installation to RAST");
  assert.deepEqual(previewProfile(live), { exists: true, users: 2 });
  assert.deepEqual(previewAvailability(live).options, ["users", "admins"]);
  assert.equal(previewCompatibility(live).ok, true);
  assert.match(previewCompatibility(live).warnings[0] ?? "", /password store/);
});
