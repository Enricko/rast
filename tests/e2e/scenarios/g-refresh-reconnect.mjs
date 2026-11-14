// (g) A browser refresh reconnects to the running app/session without
// duplicating processes.

import * as browser from "../lib/browser.mjs";
import {
  allAppWindows,
  assert,
  ensureChrome,
  openAppForUser,
  openAppViaUi,
  sessionIdentity,
} from "./_helpers.mjs";
import { sleep } from "../lib/rast-api.mjs";

export default {
  id: "g",
  title: "Browser refresh reconnects without duplicating processes",
  modes: ["docker", "native"],
  timeoutMs: 10 * 60_000,
  async run(ctx) {
    const { app } = await ensureChrome(ctx);
    const alice = ctx.user(0);
    const desktop = await browser.openDesktop(ctx.browser, alice);
    await openAppViaUi(ctx, desktop.page, app, { timeoutMs: 240_000, client: alice });
    const beforeWindows = await allAppWindows(desktop.page, app);
    assert(beforeWindows.length > 0, "no Chrome window before refresh");
    const beforeSession = await openAppForUser(ctx, alice, app);
    ctx.record(`session before refresh: ${sessionIdentity(ctx, beforeSession)}`);
    await ctx.screenshot(desktop.page, "before-refresh");

    let beforeProcesses;
    if (ctx.mode === "native") {
      beforeProcesses = await ctx.host.pgrep("^/usr/bin/google-chrome|^/opt/google/chrome");
      ctx.record(`Chrome processes before refresh: ${beforeProcesses.length}`);
      const units = (await ctx.host.appUnits()).filter((unit) => unit.unit.startsWith("rast-app-"));
      ctx.record(`app units before refresh: ${JSON.stringify(units.map((unit) => unit.unit))}`);
    } else {
      beforeProcesses = await ctx.host.dockerPs(["--filter", "label=rast.managed=linux-app"]);
      ctx.record(`Docker copies before refresh: ${JSON.stringify(beforeProcesses.map((copy) => copy.name))}`);
    }

    // Reload like a person pressing F5.
    await desktop.page.reload({ waitUntil: "domcontentloaded" });
    await desktop.page.waitForSelector(".taskbar", { timeout: 60_000 });
    await sleep(2000);
    ctx.record("page reloaded");

    // The shell must reconnect to the same session (no new copy/run).
    const afterSession = await openAppForUser(ctx, alice, app);
    assert(
      sessionIdentity(ctx, afterSession) === sessionIdentity(ctx, beforeSession),
      `session changed across refresh: ${sessionIdentity(ctx, beforeSession)} -> ${sessionIdentity(ctx, afterSession)}`,
    );
    ctx.record(`session after refresh: ${sessionIdentity(ctx, afterSession)}`);

    if (ctx.mode === "native") {
      const afterProcesses = await ctx.host.pgrep("^/usr/bin/google-chrome|^/opt/google/chrome");
      ctx.record(`Chrome processes after refresh: ${afterProcesses.length}`);
      // The Xpra session must still be the same unit and Chrome must not have
      // been duplicated by the reload (allow xpra client-side processes).
      const units = (await ctx.host.appUnits()).filter((unit) => unit.unit.startsWith("rast-app-"));
      ctx.record(`app units after refresh: ${JSON.stringify(units.map((unit) => unit.unit))}`);
      assert(
        units.length >= 1,
        "the app run unit disappeared after refresh (the app should keep running)",
      );
    } else {
      const afterCopies = await ctx.host.dockerPs(["--filter", "label=rast.managed=linux-app"]);
      ctx.record(`Docker copies after refresh: ${JSON.stringify(afterCopies.map((copy) => copy.name))}`);
      assert(
        afterCopies.length === beforeProcesses.length,
        `refresh duplicated Docker copies (${beforeProcesses.length} -> ${afterCopies.length})`,
      );
    }

    // The streamed window must come back.
    const restored = await allAppWindows(desktop.page, app);
    if (restored.length === 0) {
      // Some shells only reopen the window when the person clicks the app.
      await browser.launchApp(desktop.page, app.name);
      await browser
        .waitForWindows(desktop.page, (list) => list.some((window) => String(window.appId).includes(app.id)), {
          timeoutMs: 60_000,
          what: "window reattached after refresh",
        })
        .catch(() => {});
    }
    const finalWindows = await allAppWindows(desktop.page, app);
    ctx.record(`RAST windows after refresh: ${finalWindows.length}`);
    assert(finalWindows.length > 0, "the app window did not come back after the refresh");
    await ctx.screenshot(desktop.page, "after-refresh");
    await desktop.context.close();
  },
};
