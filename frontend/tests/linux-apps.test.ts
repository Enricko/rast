import assert from "node:assert/strict";
import test from "node:test";

import {
  discoveredAsLinuxApp,
  discoveredIsShadowed,
  isDiscoveredDesktopId,
  reconciledDiscoveredApps,
  setDiscoveredOverview,
  setLinuxOverview,
  linuxAppsInstallAvailable,
  linuxAppsOffered,
  matchWindowApp,
  splitDiscoveredImportArgs,
  validateDiscoveredImport,
  type DiscoveredApp,
  type LinuxApp,
  type LinuxOverview,
} from "../src/apps/linuxapp/catalog";

function overview(patch: Partial<LinuxOverview>): LinuxOverview {
  return {
    apps: [],
    sessions: [],
    limits: { minFreeMemoryBytes: 0, idleMinutes: 15, maxCpus: 4 },
    dockerEnabled: false,
    ...patch,
  } as LinuxOverview;
}

/** A managed Linux app for inventory/reconciliation tests. */
function managedLinuxApp(patch: Partial<LinuxApp>): LinuxApp {
  return {
    id: "app",
    name: "App",
    desktopId: "app",
    format: "deb",
    fileName: "app.deb",
    version: "1",
    package: null,
    description: null,
    command: ["/usr/bin/app"],
    launchGuess: false,
    iconUrl: null,
    status: "ready",
    error: null,
    jobId: null,
    update: { source: null, latestVersion: null, available: false, checkedAt: null, error: null },
    memoryMb: 0,
    cpus: 0,
    sizeBytes: null,
    visibility: "users",
    running: 0,
    createdAt: 1,
    updatedAt: 1,
    backend: "native",
    ...patch,
  } as LinuxApp;
}

// The operator can quarantine the whole Linux apps feature with
// `linux_apps_enabled = false`; the App Center section and the
// "Install Linux App" button must disappear even when Docker is available,
// and the install endpoints reject new installs server-side.
test("a quarantined feature hides the install workflow", () => {
  const state = overview({ linuxAppsEnabled: false, dockerEnabled: true, nativeEnabled: true });
  assert.equal(linuxAppsOffered(state), false);
  assert.equal(linuxAppsInstallAvailable(state), false);
});

test("an offered feature still needs an execution backend", () => {
  assert.equal(linuxAppsInstallAvailable(overview({ linuxAppsEnabled: true, dockerEnabled: false, nativeEnabled: false })), false);
  assert.equal(linuxAppsInstallAvailable(overview({ linuxAppsEnabled: true, dockerEnabled: true })), true);
  assert.equal(linuxAppsInstallAvailable(overview({ linuxAppsEnabled: true, nativeEnabled: true })), true);
});

test("the button does not flicker before the overview arrives", () => {
  assert.equal(linuxAppsOffered(null), true);
  assert.equal(linuxAppsInstallAvailable(null), true);
  // Servers older than the flag do not send it: they keep the old behaviour.
  assert.equal(linuxAppsOffered(overview({ dockerEnabled: true })), true);
});

// ---------------------------------------------------------------------------
// Discovered applications (installed outside App Center)
// ---------------------------------------------------------------------------

function discoveredApp(patch: Partial<DiscoveredApp>): DiscoveredApp {
  return {
    id: "d-0123456789abcdef",
    name: "WordCraft",
    genericName: "Word Processor",
    comment: "Write and design documents",
    exec: ["/var/lib/rast/linux-apps/profiles/artcraft-launcher/1/home/Applications/wordcraft-0.18.1-linux-x86_64.AppImage"],
    tryExec: null,
    icon: "ai.storyteller.wordcraft",
    iconUrl: "/api/linux-apps/discovered/d-0123456789abcdef/icon",
    categories: ["Office", "WordProcessor"],
    terminal: false,
    startupWmClass: "ai.storyteller.wordcraft",
    desktopPath: "/var/lib/rast/linux-apps/profiles/artcraft-launcher/1/data/applications/ai.storyteller.wordcraft.desktop",
    desktopId: "ai.storyteller.wordcraft.desktop",
    ownerUserId: 1,
    scope: "managed-profile",
    source: "external-installer",
    installDir: "/var/lib/rast/linux-apps/profiles/artcraft-launcher/1/home/Applications",
    version: "0.18.1",
    workingDir: null,
    status: "ready",
    approved: true,
    hidden: false,
    lastSeenMs: 1,
    lastLaunchedMs: null,
    createdAt: 1,
    updatedAt: 1,
    canLaunch: true,
    ...patch,
  };
}

// ---------------------------------------------------------------------------
// Discovery reconciliation (Runtime 2.2: no duplicate applications)
// ---------------------------------------------------------------------------

function resetInventories(): void {
  setLinuxOverview({ ...overview({}), apps: [] });
  setDiscoveredOverview({ apps: [], enabled: true });
}

test("a discovered entry shadowed by a managed app is not a launcher duplicate", () => {
  resetInventories();
  setLinuxOverview({
    ...overview({}),
    apps: [
      managedLinuxApp({ id: "google-chrome", name: "Google Chrome", desktopId: "google-chrome", command: ["/usr/bin/dbus-run-session", "--", "/usr/bin/google-chrome-stable"] }),
    ],
  });
  const chromeEntry = discoveredApp({
    id: "d-chrome-desktop",
    name: "Google Chrome",
    exec: ["/usr/bin/google-chrome-stable"],
    desktopId: "google-chrome.desktop",
    scope: "system",
    source: "xdg",
    status: "pending_approval",
    canLaunch: false,
  });
  assert.equal(discoveredIsShadowed(chromeEntry), true);
  assert.deepEqual(reconciledDiscoveredApps(), []);
  setLinuxOverview({ ...overview({}), apps: [] });
  setDiscoveredOverview({ apps: [], enabled: true });
});

test("one visible entry per executable: a manual import does not double a desktop entry", () => {
  resetInventories();
  const xdg = discoveredApp({
    id: "d-vlc-xdg",
    name: "VLC media player",
    exec: ["/usr/bin/vlc"],
    desktopId: "vlc.desktop",
    scope: "system",
    source: "xdg",
    status: "pending_approval",
    canLaunch: false,
  });
  const manual = discoveredApp({
    id: "d-vlc-import",
    name: "VLC media player",
    exec: ["/usr/bin/vlc"],
    scope: "system",
    source: "manual",
    status: "ready",
    approved: true,
    canLaunch: true,
  });
  setDiscoveredOverview({ apps: [xdg, manual], enabled: true });
  const seen = reconciledDiscoveredApps();
  assert.equal(seen.length, 1);
  assert.equal(seen[0]?.id, "d-vlc-xdg");
  setDiscoveredOverview({ apps: [], enabled: true });
});

test("isDiscoveredDesktopId recognises only discovered launcher identities", () => {
  setDiscoveredOverview({ apps: [discoveredApp({})], enabled: true });
  assert.equal(isDiscoveredDesktopId("linux:d-0123456789abcdef"), true);
  assert.equal(isDiscoveredDesktopId("linux:google-chrome"), false);
  assert.equal(isDiscoveredDesktopId("terminal"), false);
  setDiscoveredOverview({ apps: [], enabled: true });
});

test("a discovered window matches through its StartupWMClass", () => {
  const wordcraft = discoveredApp({});
  const app = matchWindowApp([], ["ai.storyteller.wordcraft", "Ai.storyteller.wordcraft"], [wordcraft]);
  assert.equal(app?.id, wordcraft.id);
  assert.equal(app?.backend, "native");
  assert.equal(app?.name, "WordCraft");
});

test("a discovered window can also match through its exec basename", () => {
  const app = matchWindowApp([], ["wordcraft-0.18.1-linux-x86_64"], [
    discoveredApp({ startupWmClass: null, desktopId: null }),
  ]);
  assert.equal(app?.id, "d-0123456789abcdef");
});

test("managed apps still win when both match, and unknown classes stay unmatched", () => {
  const managed: LinuxApp = discoveredAsLinuxApp(
    discoveredApp({ id: "managed", name: "Managed", exec: ["/opt/ai.storyteller.wordcraft"] }),
  );
  const matched = matchWindowApp([managed], ["ai.storyteller.wordcraft"], [discoveredApp({})]);
  assert.equal(matched?.id, "managed");
  assert.equal(matchWindowApp([], ["totally-unknown-class"], [discoveredApp({})]), null);
});

test("a managed app matches a window through any command token and a linux: desktop id", () => {
  const managed = managedLinuxApp({
    id: "google-chrome",
    name: "Google Chrome",
    desktopId: "linux:google-chrome",
    command: ["/usr/bin/dbus-run-session", "--", "/usr/bin/google-chrome-stable"],
  });
  const matched = matchWindowApp([managed], ["google-chrome", "Google-chrome"], []);
  assert.equal(matched?.id, "google-chrome");
  const byBinary = matchWindowApp([managed], ["google-chrome-stable"], []);
  assert.equal(byBinary?.id, "google-chrome");
});

test("a discovered entry maps to a native LinuxApp with its parsed argv", () => {
  const app = discoveredAsLinuxApp(discoveredApp({}));
  assert.equal(app.id, "d-0123456789abcdef");
  assert.equal(app.backend, "native");
  assert.deepEqual(app.command, discoveredApp({}).exec);
  assert.equal(app.desktopId, "ai.storyteller.wordcraft.desktop");
  assert.equal(app.status, "ready");
  // The icon URL is passed through; the generic App Center icon is the fallback.
  assert.equal(app.iconUrl, "/api/linux-apps/discovered/d-0123456789abcdef/icon");
  assert.equal(discoveredAsLinuxApp(discoveredApp({ iconUrl: null })).iconUrl, null);
});

// ---------------------------------------------------------------------------
// "Import existing application" form (pure client-side validation)
// ---------------------------------------------------------------------------

test("import arguments are split like an argv array", () => {
  assert.deepEqual(splitDiscoveredImportArgs("").args, []);
  assert.deepEqual(splitDiscoveredImportArgs("  ").args, []);
  assert.deepEqual(splitDiscoveredImportArgs("--flag value").args, ["--flag", "value"]);
  assert.deepEqual(splitDiscoveredImportArgs('--title "Hello world" plain').args, [
    "--title",
    "Hello world",
    "plain",
  ]);
  assert.deepEqual(splitDiscoveredImportArgs("--title 'single quoted'").args, [
    "--title",
    "single quoted",
  ]);
  assert.deepEqual(splitDiscoveredImportArgs("a \"b c\" d").args, ["a", "b c", "d"]);
  assert.deepEqual(splitDiscoveredImportArgs("").error, null);
  assert.notEqual(splitDiscoveredImportArgs('--title "unterminated').error, null);
  assert.notEqual(splitDiscoveredImportArgs("a\0b").error, null);
});

test("an import draft becomes a request only when every value is valid", () => {
  const valid = validateDiscoveredImport({
    name: "WordCraft",
    path: "/opt/wordcraft/wordcraft",
    args: "--flag",
    workingDir: "/opt/wordcraft",
    icon: "ai.storyteller.wordcraft",
  });
  assert.deepEqual(valid.problems, {});
  assert.deepEqual(valid.body, {
    name: "WordCraft",
    path: "/opt/wordcraft/wordcraft",
    args: ["--flag"],
    workingDir: "/opt/wordcraft",
    icon: "ai.storyteller.wordcraft",
  });

  // Optional fields stay absent when empty.
  const minimal = validateDiscoveredImport({
    name: "Tool",
    path: "/usr/bin/tool",
    args: "",
    workingDir: "",
    icon: "",
  });
  assert.deepEqual(minimal.body, { name: "Tool", path: "/usr/bin/tool", args: [] });

  // A relative executable is refused client-side (the server refuses it too).
  const relative = validateDiscoveredImport({
    name: "Tool",
    path: "bin/tool",
    args: "",
    workingDir: "",
    icon: "",
  });
  assert.equal(relative.body, null);
  assert.ok(relative.problems.path);

  // Path traversal in the working directory / icon is refused.
  const traversal = validateDiscoveredImport({
    name: "Tool",
    path: "/usr/bin/tool",
    args: "",
    workingDir: "/tmp/../etc",
    icon: "../outside.png",
  });
  assert.equal(traversal.body, null);
  assert.ok(traversal.problems.workingDir);
  assert.ok(traversal.problems.icon);

  // An icon file path must be PNG or SVG; a theme name is fine.
  assert.equal(
    validateDiscoveredImport({
      name: "Tool",
      path: "/usr/bin/tool",
      args: "",
      workingDir: "",
      icon: "/usr/share/pixmaps/tool.png",
    }).body?.icon,
    "/usr/share/pixmaps/tool.png",
  );
  const gif = validateDiscoveredImport({
    name: "Tool",
    path: "/usr/bin/tool",
    args: "",
    workingDir: "",
    icon: "/usr/share/pixmaps/tool.gif",
  });
  assert.equal(gif.body, null);
  assert.ok(gif.problems.icon);
});
