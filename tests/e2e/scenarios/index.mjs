// Scenario registry. Order matters: later scenarios reuse apps installed by
// earlier ones (or install them themselves when run alone).

import smoke from "./s0-smoke.mjs";
import installDeb from "./a-install-deb.mjs";
import portableArchive from "./b-portable-archive.mjs";
import sharedSession from "./c-shared-session.mjs";
import chromeWindows from "./d-chrome-windows.mjs";
import twoUsers from "./e-two-users.mjs";
import concurrentLaunch from "./f-concurrent-launch.mjs";
import refreshReconnect from "./g-refresh-reconnect.mjs";
import closePreserves from "./h-close-preserves.mjs";
import idleCleanup from "./i-idle-cleanup.mjs";
import restartRecovery from "./j-restart-recovery.mjs";
import failedInstalls from "./k-failed-installs.mjs";
import uninstall from "./l-uninstall.mjs";
import migration from "./m-migration.mjs";

export const scenarios = [
  smoke,
  installDeb,
  portableArchive,
  sharedSession,
  chromeWindows,
  twoUsers,
  concurrentLaunch,
  refreshReconnect,
  closePreserves,
  idleCleanup,
  restartRecovery,
  failedInstalls,
  uninstall,
  migration,
];
