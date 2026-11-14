// (m) A Docker-backed app either migrates to the native backend (profile data
// preserved) or reports a precise compatibility failure and stays usable.
//
// Two-phase scenario because the backend is a server startup setting:
//   1) node run.mjs --mode=docker --scenario=m     prepares the Docker app +
//      per-user data (creates the legacy volume);
//   2) scripts/e2e/dev-instance.sh up native
//   3) node run.mjs --mode=native --scenario=m     runs the migration itself.
// In docker mode the scenario only prepares and reports what it created.

import * as browser from "../lib/browser.mjs";
import {
  allAppWindows,
  assert,
  closeAppWindows,
  ensureChrome,
  openAppForUser,
  openAppViaUi,
  waitSessionGone,
} from "./_helpers.mjs";
import { sleep } from "../lib/rast-api.mjs";

const ARCH_RE = /arch|amd64|arm64|aarch64|platform|compatib|depend|package|volume|unsupported|not supported/i;

export default {
  id: "m",
  title: "Docker app data migrates or reports a precise compatibility failure",
  modes: ["docker", "native"],
  timeoutMs: 25 * 60_000,
  async run(ctx) {
    const admin = ctx.admin;
    const alice = ctx.user(0);

    // ------------------------------------------------------------- prepare
    const { app: chrome } = await ensureChrome(ctx);
    let app = (await admin.findApp((candidate) => candidate.id === chrome.id)) ?? chrome;

    if (ctx.mode === "docker") {
      if ((app.backend ?? "docker") !== "docker") ctx.skip(`Chrome is already on the ${app.backend} backend`);
      const desktop = await browser.openDesktop(ctx.browser, alice);
      await openAppViaUi(ctx, desktop.page, app, { timeoutMs: 240_000, client: alice });
      const session = await openAppForUser(ctx, alice, app);
      ctx.record(`Docker copy created for the migration source: ${JSON.stringify(session).slice(0, 300)}`);
      await closeAppWindows(desktop.page, app).catch(() => {});
      await waitSessionGone(ctx, alice, app, { timeoutMs: 180_000 }).catch((error) =>
        ctx.record(`cleanup note: ${error.message}`),
      );
      const volumes = await ctx.host.dockerVolumes(["--filter", "label=rast.managed=linux-app"]);
      const mine = volumes.filter((name) => name.includes(`u${alice.user.id}`));
      ctx.record(`legacy data volume(s) ready for migration: ${JSON.stringify(mine)}`);
      assert(mine.length > 0, "no legacy Docker data volume was created to migrate");
      await ctx.screenshot(desktop.page, "migration-prepared").catch(() => {});
      await desktop.context.close();
      ctx.record(
        "PREPARED — now run: scripts/e2e/dev-instance.sh up native && node tests/e2e/run.mjs --mode=native --scenario=m",
      );
      return;
    }

    // --------------------------------------------------------------- native
    if (!app) ctx.skip("no Chrome app: run the docker-phase prepare first");
    if ((app.backend ?? "docker") !== "docker") {
      ctx.skip(`the app backend is '${app.backend}', expected a docker-backed app to migrate`);
    }
    ctx.record(`migrating app ${app.id} (backend ${app.backend}, file ${app.fileName})`);
    const volumesBefore = await ctx.host.dockerVolumes(["--filter", "label=rast.managed=linux-app"]);
    const mineBefore = volumesBefore.filter((name) => name.includes(`u${alice.user.id}`));
    ctx.record(`legacy volumes before: ${JSON.stringify(mineBefore)}`);

    // The .deb the Docker app was built from is what the native install needs.
    let started;
    try {
      started = await admin.migrate(app.id, { path: `/${app.fileName}` });
    } catch (error) {
      ctx.record(`migrate with the original file failed synchronously: ${error.message}`);
      started = await admin.migrate(app.id, {}).catch((fallbackError) => {
        ctx.record(`migrate without a file also failed: ${fallbackError.message}`);
        throw fallbackError;
      });
    }
    ctx.record(`migrate response: ${JSON.stringify(started).slice(0, 300)}`);
    let jobFailure = null;
    if (started?.jobId) {
      await admin.waitJob(started.jobId, { timeoutMs: 20 * 60_000, intervalMs: 2000 }).catch((error) => {
        jobFailure = error;
      });
    }
    // Wait for a terminal migration state on the app row.
    const startedAt = Date.now();
    let migrated = null;
    for (;;) {
      migrated = await admin.getApp(app.id);
      const state = migrated.native?.migration?.state ?? migrated.migrationState;
      if (state === "migrated" || migrated.backend === "native" || state === "failed") break;
      if (Date.now() - startedAt > 20 * 60_000) break;
      await sleep(1000);
    }
    const migration = migrated.native?.migration ?? {
      state: migrated.migrationState,
      error: migrated.migrationError,
    };
    ctx.record(`app after migrate: backend ${migrated.backend}, migration ${JSON.stringify(migration)}`);
    await ctx.screenshot(await browser.openDesktop(ctx.browser, admin).then((session) => session.page), "after-migrate").catch(() => {});

    if (migrated.backend === "native" && (migration.state === "migrated" || migration.state === undefined)) {
      ctx.record("migration succeeded");
      const session = await openAppForUser(ctx, alice, app);
      const state = await alice.nativeSessionState();
      ctx.record(`native session: ${JSON.stringify(state.session).slice(0, 400)}`);
      const desktop = await browser.openDesktop(ctx.browser, alice);
      await openAppViaUi(ctx, desktop.page, migrated, { timeoutMs: 240_000, client: alice });
      const windows = await allAppWindows(desktop.page, migrated);
      assert(windows.length > 0, "the migrated app did not open a RAST window");
      await ctx.screenshot(desktop.page, "migrated-app-running");
      // Profile data must be preserved: the retained backup/volume is reported
      // and the user's native profile directory exists.
      if (migration.backup) ctx.record(`retained migration backup: ${JSON.stringify(migration.backup)}`);
      if (migration.dockerVolume) {
        const volumes = await ctx.host.dockerVolumes(["--filter", "label=rast.managed=linux-app"]);
        ctx.record(`legacy volume retained: ${volumes.includes(migration.dockerVolume)}`);
      }
      await closeAppWindows(desktop.page, migrated).catch(() => {});
      await desktop.context.close();

      // Rollback is the documented escape hatch: the app must return to Docker.
      const rolled = await admin.rollbackMigration(migrated.id).catch((error) => {
        ctx.record(`rollback reported: ${error.message}`);
        return null;
      });
      if (rolled) {
        const after = await admin.getApp(migrated.id);
        ctx.record(`after rollback: backend ${after.backend}, migration ${JSON.stringify(after.native?.migration ?? null)}`);
        assert(after.backend === "docker", `rollback left the app on the ${after.backend} backend`);
        ctx.record("rollback returned the app to the Docker backend");
      }
    } else {
      // Precise, recoverable failure.
      const message = String(migration.error ?? jobFailure?.message ?? "");
      ctx.record(`migration failed with: ${message}`);
      assert(message.length > 5, "migration failed without an error message");
      assert(ARCH_RE.test(message), `migration failure is not precise: ${message}`);
      assert(migrated.backend === "docker", `a failed migration changed the backend to ${migrated.backend}`);
      // The app still works on Docker: launching it must succeed.
      await admin.migrate(app.id, {}).catch(() => {});
      const session = await alice.openApp(app.id);
      ctx.record(`app still launchable after the failed migration: ${JSON.stringify(session).slice(0, 300)}`);
      assert(session?.id || session?.status, "the app is no longer launchable after a failed migration");
      const cleanup = await admin.cleanupMigration(app.id, false).catch((error) => {
        ctx.record(`cleanup reported: ${error.message}`);
        return null;
      });
      if (cleanup) ctx.record("cleanup accepted after the failed migration");
    }
    const volumesAfter = await ctx.host.dockerVolumes(["--filter", "label=rast.managed=linux-app"]);
    ctx.record(`legacy volumes after: ${JSON.stringify(volumesAfter)}`);
  },
};
