import assert from "node:assert/strict";
import test from "node:test";

import {
  discoveredAsLinuxApp,
  linuxAppsInstallAvailable,
  linuxAppsOffered,
  matchWindowApp,
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

test("a discovered entry maps to a native LinuxApp with its parsed argv", () => {
  const app = discoveredAsLinuxApp(discoveredApp({}));
  assert.equal(app.id, "d-0123456789abcdef");
  assert.equal(app.backend, "native");
  assert.deepEqual(app.command, discoveredApp({}).exec);
  assert.equal(app.desktopId, "ai.storyteller.wordcraft.desktop");
  assert.equal(app.status, "ready");
});
