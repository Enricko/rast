// (b) An administrator installs a portable archive and launches it.
//
// The fixture archive (tests/e2e/fixtures/rast-test-app-1.0.tar.gz) contains
// a GTK3 app with a .desktop entry, a deterministic "New Window" button and
// an entry whose text is mirrored into a label — the scenarios use those to
// prove that windows open, input reaches the app and results are visible.

import * as browser from "../lib/browser.mjs";
import { ensurePortableApp, openAppForUser, waitForAppWindows } from "./_helpers.mjs";

export default {
  id: "b",
  title: "Admin installs a portable archive and launches it",
  modes: ["docker", "native"],
  timeoutMs: 25 * 60_000,
  async run(ctx) {
    const app = (await ensurePortableApp(ctx)).app;
    const alice = ctx.user(0);

    const session = await browser.openDesktop(ctx.browser, alice);
    const started = Date.now();
    const windows = await browser.launchApp(session.page, app.name).then(() =>
      waitForAppWindows(ctx, session.page, app, { timeoutMs: 180_000 }),
    );
    ctx.metric("firstWindowMs", Date.now() - started, "ms");
    const titled = windows.filter((window) => (window.title ?? "").includes("RAST Test App"));
    if (titled.length === 0) {
      ctx.record(`windows: ${JSON.stringify(windows)}`);
      throw new Error("the fixture window title 'RAST Test App' did not appear");
    }
    ctx.record(`window title(s): ${windows.map((window) => window.title).join(", ")}`);
    await ctx.screenshot(session.page, "portable-app-window");

    // The app copy/session exists for the backend.
    const backendSession = await openAppForUser(ctx, alice, app);
    ctx.record(`session: ${JSON.stringify(backendSession).slice(0, 500)}`);

    // Second window from inside the app: click the deterministic button.
    // The GTK layout puts "New Window" at roughly (100, 95) inside the
    // window content (18 px border + heading + hint).
    const firstWindow = windows.find((window) => (window.title ?? "").includes("RAST Test App")) ?? windows[0];
    const before = (await browser.windowsInfo(session.page)).length;
    if (ctx.mode === "docker") {
      await browser.waitForAppFrame(session.page, { timeoutMs: 60_000 });
      await browser.clickInApp(session.page, { position: { x: 100, y: 96 } });
    } else {
      // Native: the Xpra window is mirrored as a RAST window; click the same
      // relative position inside that window.
      const box = await session.page.locator(`.window[data-window-id="${firstWindow.id}"]`).boundingBox();
      await session.page.mouse.click(box.x + 100, box.y + 96);
    }
    await ctx.screenshot(session.page, "portable-app-second-window");
    const after = await browser.waitForWindows(
      session.page,
      (list) => list.length > before,
      { timeoutMs: 60_000, what: "a second window from the app" },
    ).catch(async (error) => {
      const last = await browser.windowsInfo(session.page);
      throw new Error(`${error.message}; windows: ${JSON.stringify(last)}`);
    });
    const secondTitles = after.map((window) => window.title);
    ctx.record(`windows after New Window: ${JSON.stringify(secondTitles)}`);
    const taskbar = await browser.taskbarInfo(session.page);
    ctx.record(`taskbar entries: ${JSON.stringify(taskbar)}`);
    if (!secondTitles.some((title) => title.includes("#2"))) {
      ctx.record("note: multi-window mirroring did not produce a '#2' title (backend still opened a window)");
    }
    await ctx.screenshot(session.page, "portable-app-two-windows");
    await session.context.close();
  },
};
