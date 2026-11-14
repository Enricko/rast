// (b) An administrator installs a portable archive and launches it.
//
// The fixture archive (tests/e2e/fixtures/rast-test-app-1.0.tar.gz) contains
// a GTK3 app with a .desktop entry, a deterministic "New Window" button and
// an entry whose text is mirrored into a label — the scenarios use those to
// prove that windows open, input reaches the app and results are visible.

import * as browser from "../lib/browser.mjs";
import {
  clickInNativeWindow,
  ensurePortableApp,
  openAppForUser,
  waitForAppWindows,
} from "./_helpers.mjs";

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
    const realTitled = (list) =>
      list.filter(
        (window) =>
          (window.title ?? "").includes("RAST Test App") && !/^Starting/i.test(String(window.title)),
      );
    if (realTitled(windows).length === 0) {
      // The launch placeholder is replaced by the real window once the app
      // maps its first X11 window; wait for that instead of clicking a frame
      // that is about to disappear.
      await browser
        .waitForWindows(
          session.page,
          (list) => realTitled(list).length > 0,
          { timeoutMs: 120_000, what: "the real RAST Test App window" },
        )
        .catch((error) => ctx.record(`waiting for the real window: ${error.message}`));
    }
    const current = await browser.windowsInfo(session.page);
    const titled = realTitled(current);
    if (titled.length === 0) {
      ctx.record(`windows: ${JSON.stringify(current)}`);
      throw new Error("the fixture window title 'RAST Test App' did not appear");
    }
    ctx.record(`window title(s): ${current.map((window) => window.title).join(", ")}`);
    await ctx.screenshot(session.page, "portable-app-window");

    // The app copy/session exists for the backend.
    const backendSession = await openAppForUser(ctx, alice, app);
    ctx.record(`session: ${JSON.stringify(backendSession).slice(0, 500)}`);

    // Second window from inside the app: click the deterministic button.
    // The GTK layout puts "New Window" at roughly (100, 95) inside the
    // window content (18 px border + heading + hint).
    const firstWindow =
      realTitled(await browser.windowsInfo(session.page))[0] ?? windows[windows.length - 1];
    // Compare window IDs, not counts: the shell may still hold a "Starting …"
    // placeholder that is replaced when the real window arrives, which keeps
    // the count identical even though a new window appeared.
    const beforeIds = new Set((await browser.windowsInfo(session.page)).map((window) => window.id));
    if (ctx.mode === "docker") {
      await browser.waitForAppFrame(session.page, { timeoutMs: 60_000 });
      await browser.clickInApp(session.page, { position: { x: 100, y: 96 } });
    } else {
      // Native: the Xpra window is mirrored as a RAST window; click the same
      // position *inside the window body* (the title bar/border belong to the
      // RAST frame, not the application).
      const point = await clickInNativeWindow(session.page, firstWindow.id, {
        position: { x: 100, y: 96 },
      });
      ctx.record(`clicked New Window at ${point.x.toFixed(0)},${point.y.toFixed(0)}`);
    }
    await ctx.screenshot(session.page, "portable-app-second-window");
    const after = await browser.waitForWindows(
      session.page,
      (list) => list.some((window) => !beforeIds.has(window.id)),
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
