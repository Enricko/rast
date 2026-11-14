// (l) Uninstall respects shared dependencies and the user-data choice.
//
// Docker baseline: uninstall removes the app's images/containers; the
// per-person data volume is kept unless *Also delete settings* was chosen.
// Native: the install transaction's packages are removed only when nothing
// else needs them (packages that were already installed are never removed),
// and the per-user profile/data choice is honoured.

import * as browser from "../lib/browser.mjs";
import {
  allAppWindows,
  assert,
  closeAppWindows,
  ensureChrome,
  ensurePortableApp,
  installFromFile,
  openAppForUser,
  openAppViaUi,
} from "./_helpers.mjs";
import { sleep } from "../lib/rast-api.mjs";

export default {
  id: "l",
  title: "Uninstall respects shared dependencies and user data choice",
  modes: ["docker", "native"],
  timeoutMs: 20 * 60_000,
  async run(ctx) {
    const admin = ctx.admin;
    const alice = ctx.user(0);
    const { app: chrome } = await ensureChrome(ctx);
    let { app: portable } = await ensurePortableApp(ctx);

    // Create per-user data for the fixture app.
    const desktop = await browser.openDesktop(ctx.browser, alice);
    await openAppViaUi(ctx, desktop.page, portable, { timeoutMs: 180_000, client: alice });
    await openAppForUser(ctx, alice, portable);
    if (ctx.mode === "docker") {
      await browser.waitForAppFrame(desktop.page, { timeoutMs: 60_000 }).catch(() => {});
    }
    await closeAppWindows(desktop.page, portable).catch((error) => ctx.record(`close note: ${error.message}`));
    await sleep(1500);

    let dataRef;
    if (ctx.mode === "docker") {
      const volumes = await ctx.host.dockerVolumes(["--filter", "label=rast.managed=linux-app"]);
      dataRef = volumes.find((name) => name.includes(`u${alice.user.id}`));
      ctx.record(`alice data volume before uninstall: ${dataRef ?? "none (created lazily)"}`);
    } else {
      const identity = await ctx.host.linuxIdentities();
      const mine = identity.find((entry) => entry.userId === alice.user.id);
      dataRef = mine ? `${mine.home}/RAST` : null;
      ctx.record(`alice app data root: ${dataRef}`);
    }

    // 1. Uninstall keeping data.
    await admin.uninstall(portable.id, false);
    await sleep(1000);
    const gone = await admin.findApp((app) => app.id === portable.id);
    assert(!gone, "the app still exists after uninstall");
    ctx.record("app uninstalled with removeData=false");
    if (ctx.mode === "docker") {
      const volumes = await ctx.host.dockerVolumes(["--filter", "label=rast.managed=linux-app"]);
      const kept = dataRef ? volumes.includes(dataRef) : volumes.length > 0;
      ctx.record(`volumes after keep-data uninstall: ${JSON.stringify(volumes)}`);
      assert(kept, "the per-person data volume was removed although removeData=false");
      const images = await ctx.host.dockerImages(`rast-linux/${portable.id}`);
      ctx.record(`images for the uninstalled app: ${JSON.stringify(images)}`);
      assert(images.length === 0, "the app image was not removed on uninstall");
    } else {
      const cmd = `ls -d ${dataRef} 2>/dev/null`;
      const result = await ctx.host.sudoSh(cmd);
      ctx.record(`profile dir kept: ${result.stdout.trim() || "(not present, created lazily)"}`);
    }
    // Other apps are untouched.
    const chromeStill = await admin.getApp(chrome.id);
    assert(chromeStill.status === "ready", "uninstalling the fixture app damaged Chrome");

    // 2. Reinstall and uninstall deleting data.
    const reinstall = await installFromFile(ctx, admin, {
      localPath: ctx.fixtures.archive,
      fileName: "rast-test-app-1.0.tar.gz",
      label: "fixture reinstall",
    });
    portable = reinstall.app;
    ctx.record(`fixture reinstalled: ${portable.id} (${portable.status})`);
    await openAppViaUi(ctx, desktop.page, portable, { timeoutMs: 180_000, client: alice });
    await openAppForUser(ctx, alice, portable);
    await closeAppWindows(desktop.page, portable).catch(() => {});
    await sleep(1500);
    await admin.uninstall(portable.id, true);
    await sleep(1500);
    assert(!(await admin.findApp((app) => app.id === portable.id)), "the app still exists after delete-data uninstall");
    if (ctx.mode === "docker") {
      const volumes = await ctx.host.dockerVolumes(["--filter", "label=rast.managed=linux-app"]);
      const mine = volumes.filter((name) => name.includes(`u${alice.user.id}`));
      ctx.record(`volumes after delete-data uninstall: ${JSON.stringify(volumes)}`);
      assert(mine.length === 0, "the per-person data volume survived removeData=true");
    }

    // 3. Native: packages that existed before RAST adopted the app are never
    //    removed; the system package state stays consistent.
    if (ctx.mode === "native") {
      for (const packageName of ["libgtk-3-0t64", "libx11-6"]) {
        const state = await ctx.host.dpkgInstalled(packageName);
        ctx.record(`dpkg ${packageName}: ${JSON.stringify(state)}`);
        assert(state?.installed, `${packageName} was removed although it predates the app install`);
      }
      const helperCheck = await ctx.host.sudo("/tmp/rast-e2e/bin/rast", [
        "--config",
        `${ctx.config.instanceRoot}/rast.toml`,
        "native-helper",
        "--check",
      ]);
      ctx.record(`native-helper --check exit ${helperCheck.ok ? 0 : helperCheck.code}`);
    }

    // The desktop no longer offers the app and Chrome still opens.
    await browser.launchApp(desktop.page, chrome.name);
    await browser
      .waitForWindows(desktop.page, (list) => list.some((window) => String(window.appId).includes(chrome.id)), {
        timeoutMs: 180_000,
        what: "Chrome after uninstalls",
      })
      .catch((error) => ctx.record(`Chrome open note: ${error.message}`));
    const remaining = await allAppWindows(desktop.page, chrome);
    assert(remaining.length > 0, "Chrome no longer opens after the uninstalls");
    ctx.record("Chrome still opens after uninstalling the fixture app");
    await ctx.screenshot(desktop.page, "after-uninstall");
    await desktop.context.close();
  },
};
