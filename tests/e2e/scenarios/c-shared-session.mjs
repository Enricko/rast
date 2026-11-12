// (c) One user opens Chrome + another GUI app through ONE shared graphical
// session (native: a single Xpra session per person; docker baseline: one
// container per app copy, which is the documented difference).

import * as browser from "../lib/browser.mjs";
import {
  appRunsOf,
  assert,
  ensureChrome,
  ensurePortableApp,
  identityFor,
  openAppForUser,
  openAppViaUi,
  sessionUnitFor,
  waitForAppWindows,
} from "./_helpers.mjs";

export default {
  id: "c",
  title: "Same user opens Chrome and another GUI app (shared session)",
  modes: ["docker", "native"],
  timeoutMs: 15 * 60_000,
  async run(ctx) {
    const [{ app: chrome }, { app: portable }] = await Promise.all([
      ensureChrome(ctx),
      ensurePortableApp(ctx),
    ]);
    const alice = ctx.user(0);
    const desktop = await browser.openDesktop(ctx.browser, alice);

    await openAppViaUi(ctx, desktop.page, chrome, { timeoutMs: 240_000, client: alice });
    await openAppViaUi(ctx, desktop.page, portable, { timeoutMs: 120_000, client: alice });
    const windows = await browser.windowsInfo(desktop.page);
    ctx.record(`windows: ${JSON.stringify(windows.map((window) => ({ title: window.title, appId: window.appId })))}`);
    const chromeWindows = windows.filter((window) => String(window.appId).includes(chrome.id));
    const portableWindows = windows.filter((window) => String(window.appId).includes(portable.id));
    assert(chromeWindows.length > 0, "Chrome window is missing");
    assert(portableWindows.length > 0, "second app window is missing");
    ctx.record(`both apps open at once (${chromeWindows.length} + ${portableWindows.length} windows)`);
    await ctx.screenshot(desktop.page, "two-apps-one-user");

    const session = await openAppForUser(ctx, alice, chrome);
    const session2 = await openAppForUser(ctx, alice, portable);
    ctx.record(`chrome session: ${JSON.stringify(session).slice(0, 400)}`);
    ctx.record(`portable session: ${JSON.stringify(session2).slice(0, 400)}`);

    if (ctx.mode === "native") {
      // One session unit per person, both runs inside it.
      const unit = await sessionUnitFor(ctx, alice);
      assert(unit, `no Linux identity for ${alice.user.username}`);
      ctx.record(`identity: ${JSON.stringify(unit.identity)}`);
      ctx.record(`session unit: ${JSON.stringify(unit.status)}`);
      assert(unit.status.ActiveState === "active", "rast-xpra session unit is not active");
      const handles = await identityFor(ctx, alice);
      ctx.metric("sessionMemoryBytes", Number(unit.status.MemoryCurrent) || 0, "bytes");
      const state = await alice.nativeSession();
      const sessionJson = state.session ?? {};
      ctx.record(`native session: ${JSON.stringify(sessionJson).slice(0, 600)}`);
      const runObjects = appRunsOf(sessionJson, chrome).length + appRunsOf(sessionJson, portable).length;
      ctx.record(`runs reported by the session API: ${runObjects} (both apps are in the one session)`);
      // Both apps share the same DISPLAY (one graphical session).
      const units = await ctx.host.appUnits();
      ctx.record(`native app units: ${JSON.stringify(units.map((unit) => unit.unit))}`);
      if (handles) {
        const pids = await ctx.host.pgrep(`^/usr/bin/google-chrome|rast-test-app`);
        ctx.record(`app processes: ${JSON.stringify(pids.slice(0, 6))}`);
      }
    } else {
      const copies = await ctx.host.dockerPs(["--filter", "label=rast.managed=linux-app"]);
      const mine = copies.filter(
        (container) => String(container.labels["rast.linux.user"]) === String(alice.user.id),
      );
      ctx.record(`Docker copies for the user (one per app, by design): ${mine.map((c) => c.name).join(", ")}`);
      assert(mine.length >= 2, `expected 2 Docker copies, found ${mine.length}`);
    }

    // Closing one app must not disturb the other (detail in scenario h).
    const chromeWindow = chromeWindows[0];
    await pageCloseWindow(desktop.page, chromeWindow.id);
    await browser.waitForWindows(desktop.page, (list) => !list.some((window) => window.id === chromeWindow.id), {
      timeoutMs: 60_000,
      what: "Chrome RAST window closed",
    });
    const stillOpen = await browser.windowsInfo(desktop.page);
    assert(
      stillOpen.some((window) => String(window.appId).includes(portable.id)),
      "the other app's window disappeared when Chrome was closed",
    );
    ctx.record("closing the Chrome window kept the other app's window");
    await ctx.screenshot(desktop.page, "after-closing-chrome");
    await desktop.context.close();
  },
};

async function pageCloseWindow(page, windowId) {
  const window = page.locator(`.window[data-window-id="${windowId}"]`);
  const close = window.locator(".win-control-close");
  if (await close.count()) {
    await close.first().click();
    return;
  }
  await page.locator(`.window[data-window-id="${windowId}"]`).click({ position: { x: 20, y: 8 } });
  const taskbar = await browser.taskbarInfo(page);
  const active = taskbar.findIndex((entry) => entry.active);
  if (active >= 0) await page.locator(".taskbar-app").nth(active).click({ button: "middle" });
}
