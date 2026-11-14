import assert from "node:assert/strict";
import test from "node:test";

import {
  linuxAppsInstallAvailable,
  linuxAppsOffered,
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
