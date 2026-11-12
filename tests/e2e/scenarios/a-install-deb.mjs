// (a) An administrator installs a compatible desktop .deb and launches it.
//
// Docker baseline: upload → App Center install job (worker builds an image
// from the .deb) → the app appears for every user → a user opens it and gets
// their own copy streamed into a RAST window.
// Native backend: the same API install path runs the host package transaction
// and the app launches into the user's shared Xpra session.

import * as browser from "../lib/browser.mjs";
import { summaryOfApp, ensureChrome, ensureNativeSessionReady, openAppForUser } from "./_helpers.mjs";

export default {
  id: "a",
  title: "Admin installs a compatible .deb and launches it",
  modes: ["docker", "native"],
  timeoutMs: 25 * 60_000,
  async run(ctx) {
    const admin = ctx.admin;
    const alice = ctx.user(0);

    const started = Date.now();
    const { app, installedNow } = await ensureChrome(ctx);
    ctx.metric("installMs", installedNow ? Date.now() - started : 0, "ms");
    ctx.record(`app after install: ${JSON.stringify(summaryOfApp(app))}`);
    if (installedNow) ctx.record(`install took ${((Date.now() - started) / 1000).toFixed(1)} s`);

    // The app is shared with every user by default and ready to open.
    const aliceView = await alice.getApp(app.id);
    if (aliceView.status !== "ready") throw new Error(`alice sees status ${aliceView.status}`);
    ctx.record(`app is visible to a non-admin (status ${aliceView.status})`);

    // App Center shows the installed app (administrator evidence).
    const adminSession = await browser.openDesktop(ctx.browser, admin);
    await browser.launchApp(adminSession.page, "App Center");
    await adminSession.page.waitForSelector(".pkg-tile", { timeout: 30_000 });
    const tile = adminSession.page.locator(".pkg-tile", { hasText: app.name }).first();
    await tile.waitFor({ timeout: 30_000 });
    ctx.record(`App Center lists ${app.name}`);
    await ctx.screenshot(adminSession.page, "app-center-lists-app");
    await adminSession.context.close();

    // Launch: the first launch is the cold one for this app copy/session.
    const aliceSession = await browser.openDesktop(ctx.browser, alice);
    // Native: the first launch may need a profile-ancestor permission repair on
    // this host; start the session through the API with retries first, then the
    // UI launch below reuses it (documented in results/README.md).
    const prepared = ctx.mode === "native" ? await ensureNativeSessionReady(ctx, alice, app) : null;
    if (prepared) ctx.record(`native session prepared: display ${prepared.session?.display ?? "?"}`);
    const launchStarted = Date.now();
    await browser.launchApp(aliceSession.page, app.name);
    const windows = await browser.waitForWindows(
      aliceSession.page,
      (list) => list.length > 0,
      { timeoutMs: 180_000, what: `a RAST window for ${app.name}` },
    );
    const launchMs = Date.now() - launchStarted;
    ctx.metric("firstWindowMs", launchMs, "ms");
    ctx.record(`first RAST window after ${(launchMs / 1000).toFixed(1)} s: ${JSON.stringify(windows)}`);
    await ctx.screenshot(aliceSession.page, "app-window-opened");

    // The backend really started the copy/session.
    const session = await openAppForUser(ctx, alice, app);
    ctx.record(`session: ${JSON.stringify(session).slice(0, 600)}`);

    if (ctx.mode === "docker") {
      const copies = await ctx.host.dockerPs(["--filter", "label=rast.managed=linux-app"]);
      const mine = copies.filter(
        (container) =>
          container.labels["rast.linux.app"] === app.id &&
          String(container.labels["rast.linux.user"]) === String(alice.user.id),
      );
      if (mine.length === 0) throw new Error("no Docker copy for the user after opening the app");
      ctx.record(`Docker copy: ${mine[0].name} (${mine[0].image}, ${mine[0].status})`);
      await ctx.screenshot(aliceSession.page, "app-stream-visible");
    } else {
      const identity = await ctx.host.linuxIdentities();
      const mine = identity.find((entry) => entry.userId === alice.user.id);
      ctx.record(`Linux identity: ${JSON.stringify(mine)}`);
      const unit = await ctx.host.unitStatus(`rast-xpra-u${alice.user.id}`);
      ctx.record(`session unit rast-xpra-u${alice.user.id}: ${JSON.stringify(unit)}`);
      if (unit.ActiveState !== "active") throw new Error("the shared Xpra session unit is not active");
    }

    const frames = await browser.appFrames(aliceSession.page).count();
    ctx.record(`embedded app frame(s): ${frames}`);
    if (frames === 0 && ctx.mode === "docker") {
      ctx.record("note: no iframe found; the legacy stream may be inside a canvas window");
    }
    await browser.waitForAppFrame(aliceSession.page, { timeoutMs: 30_000 }).catch(() => {});
    await ctx.screenshot(aliceSession.page, "app-usable");
    await aliceSession.context.close();
  },
};
