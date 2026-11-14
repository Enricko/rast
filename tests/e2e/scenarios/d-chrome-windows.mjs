// (d) One user opens three Chrome windows: each becomes its own RAST window
// with its own taskbar entry; focus, resize and close behave like desktop
// windows.
//
// The extra windows are opened from inside Chrome with Ctrl+N, sent through
// the embedded stream — exactly like a person would. The scenario starts from
// a clean app state (windows restored from an earlier browser session are
// closed first) so the assertions are deterministic.

import * as browser from "../lib/browser.mjs";
import {
  allAppWindows,
  assert,
  closeAppWindows,
  ensureChrome,
  nativeRemoteWindows,
  openAppViaUi,
  openAppForUser,
} from "./_helpers.mjs";

async function chromeWindowsOf(page, app) {
  const windows = await allAppWindows(page, app);
  return windows.filter((window) => /Chrome/i.test(String(window.title)));
}

async function openChromeWindowFromInside(page, targetId, expectedCount) {
  // Focus the window through its RAST title bar, then send the Chrome
  // shortcut. `force` bypasses Playwright's actionability wait for covered
  // windows; the click point is the window's own top strip. Retry a couple of
  // times: the keystroke is forwarded only after the shell has registered the
  // focus change, which can lag one snapshot.
  for (let attempt = 0; attempt < 3; attempt += 1) {
    await page
      .locator(`.window[data-window-id="${targetId}"]`)
      .click({ position: { x: 40, y: 8 }, force: true })
      .catch(() => {});
    await page.waitForTimeout(400);
    await page.keyboard.press("Control+n");
    await page.waitForTimeout(900);
    const info = await browser.windowsInfo(page);
    const chrome = info.filter((window) => /Chrome/i.test(String(window.title)) && !/^Starting/i.test(String(window.title)));
    if (chrome.length >= expectedCount) return chrome.length;
  }
  const info = await browser.windowsInfo(page);
  return info.filter((window) => /Chrome/i.test(String(window.title)) && !/^Starting/i.test(String(window.title))).length;
}

export default {
  id: "d",
  title: "Three Chrome windows: taskbar entries, focus, resize, close",
  modes: ["docker", "native"],
  timeoutMs: 15 * 60_000,
  async run(ctx) {
    const { app } = await ensureChrome(ctx);
    const alice = ctx.user(0);
    const desktop = await browser.openDesktop(ctx.browser, alice);
    // A previous scenario/run may have left Chrome windows open; reconnect
    // restores them by design, so close them before counting.
    await closeAppWindows(desktop.page, app).catch((error) => ctx.record(`cleanup note: ${error.message}`));
    await openAppViaUi(ctx, desktop.page, app, { timeoutMs: 240_000, client: alice });
    await browser.waitForWindows(
      desktop.page,
      (list) => list.filter((window) => /Chrome/i.test(String(window.title)) && !/^Starting/i.test(String(window.title))).length >= 1,
      { timeoutMs: 120_000, what: "the first mirrored Chrome window" },
    );
    const first = await chromeWindowsOf(desktop.page, app);
    ctx.record(`initial Chrome windows: ${JSON.stringify(first.map((window) => window.title))}`);

    // Two more windows inside Chrome.
    for (let index = 0; index < 2; index += 1) {
      const windows = await chromeWindowsOf(desktop.page, app);
      const target = windows.find((window) => window.focused) ?? windows[windows.length - 1];
      await openChromeWindowFromInside(desktop.page, target.id, index + 2);
      await browser
        .waitForWindows(
          desktop.page,
          (list) =>
            list.filter((window) => /Chrome/i.test(String(window.title)) && !/^Starting/i.test(String(window.title))).length >=
            index + 2,
          { timeoutMs: 90_000, what: `Chrome window ${index + 2}` },
        )
        .catch((error) => ctx.record(`waiting for window ${index + 2}: ${error.message}`));
    }
    const windows = await chromeWindowsOf(desktop.page, app);
    ctx.record(`Chrome windows after Ctrl+N x2: ${JSON.stringify(windows.map((window) => window.title))}`);
    ctx.metric("chromeWindowCount", windows.length);
    assert(windows.length >= 3, `expected three Chrome windows, found ${windows.length}`);
    await ctx.screenshot(desktop.page, "three-chrome-windows");

    // Taskbar entries for each window (normal top-level windows only).
    const taskbar = (await browser.taskbarInfo(desktop.page)).filter((entry) =>
      String(entry.label).includes("Google Chrome"),
    );
    ctx.record(`Chrome taskbar labels: ${JSON.stringify(taskbar.map((entry) => entry.label))}`);
    assert(taskbar.length >= 3, `expected at least 3 Chrome taskbar entries, found ${taskbar.length}`);

    // Focus: clicking a Chrome taskbar entry focuses its window (clicking the
    // entry of the focused window minimizes it, a second click restores it).
    for (let index = 0; index < 3; index += 1) {
      const entries = desktop.page.locator(".taskbar-app");
      const labels = await entries.locator(".taskbar-app-label").allTextContents();
      let entryIndex = -1;
      let seen = 0;
      for (let position = 0; position < labels.length; position += 1) {
        if (!String(labels[position]).includes("Google Chrome")) continue;
        if (seen === index) {
          entryIndex = position;
          break;
        }
        seen += 1;
      }
      if (entryIndex < 0) continue;
      await entries.nth(entryIndex).click({ force: true });
      await desktop.page.waitForTimeout(350);
      let state = await browser.windowsInfo(desktop.page);
      let focused = state.find((window) => window.focused);
      if (!focused || !/Chrome/i.test(String(focused.title))) {
        // The entry we clicked was the focused window: the click minimized it.
        await entries.nth(entryIndex).click({ force: true });
        await desktop.page.waitForTimeout(350);
        state = await browser.windowsInfo(desktop.page);
        focused = state.find((window) => window.focused);
      }
      ctx.record(`focused after Chrome taskbar entry #${index + 1}: ${focused?.title ?? "none"}`);
      assert(focused && /Chrome/i.test(String(focused.title)), `taskbar entry #${index + 1} did not focus a Chrome window`);
    }

    // Resize the currently focused (topmost) window with its bottom-right
    // handle: a covered window's handle would receive the pointer instead.
    let current = await browser.windowsInfo(desktop.page);
    let resizeTarget =
      current.find((window) => window.focused && /Chrome/i.test(String(window.title))) ?? windows[windows.length - 1];
    const targetEl = desktop.page.locator(`.window[data-window-id="${resizeTarget.id}"]`);
    if (resizeTarget.maximized) {
      // Resize handles are inert while maximized (like any desktop); restore
      // first, then resize the restored window.
      await targetEl.locator(".win-control-maximize").click();
      await desktop.page.waitForTimeout(500);
      ctx.record(`restored ${resizeTarget.id} from maximized before resizing`);
    }
    resizeTarget =
      (await browser.windowsInfo(desktop.page)).find((window) => window.id === resizeTarget.id) ?? resizeTarget;
    const before = resizeTarget.rect;
    const remoteBefore = ctx.mode === "native" ? await nativeRemoteWindows(desktop.page) : null;
    const handle = targetEl.locator(".win-resize-se");
    if (await handle.count()) {
      const box = await handle.first().boundingBox();
      if (box) {
        // Shrink horizontally (the window is near the right edge of the
        // viewport) and grow vertically; both must reach the remote window.
        await desktop.page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
        await desktop.page.mouse.down();
        await desktop.page.mouse.move(box.x + box.width / 2 - 140, box.y + box.height / 2 + 50, { steps: 12 });
        await desktop.page.mouse.up();
      }
      await desktop.page.waitForTimeout(1000);
      const after = (await browser.windowsInfo(desktop.page)).find((window) => window.id === resizeTarget.id);
      ctx.record(`resize: ${JSON.stringify(before)} -> ${JSON.stringify(after?.rect)}`);
      assert(after, `window ${resizeTarget.id} disappeared during the resize`);
      assert(
        after.rect.width < before.width - 40 || after.rect.height > before.height + 30,
        `window did not resize (${before.width}x${before.height} -> ${after.rect.width}x${after.rect.height})`,
      );
      if (ctx.mode === "native") {
        const remoteAfter = await nativeRemoteWindows(desktop.page);
        const wid = String(resizeTarget.appId).split(":").pop();
        const remote = (remoteAfter ?? []).find((window) => String(window.wid) === wid);
        const remoteWas = (remoteBefore ?? []).find((window) => String(window.wid) === wid);
        ctx.record(
          `remote window geometry: before ${remoteWas ? `${remoteWas.width}x${remoteWas.height}` : "?"} -> after ${
            remote ? `${remote.width}x${remote.height}` : "?"
          } (RAST rect ${after.rect.width}x${after.rect.height})`,
        );
        if (remote && remoteWas) {
          assert(
            remote.width !== remoteWas.width || remote.height !== remoteWas.height,
            "the remote Xpra window did not follow the RAST resize",
          );
        }
      }
    } else {
      ctx.record("window has no resize handle (frameless); resize handled by the app window only");
    }
    await ctx.screenshot(desktop.page, "three-chrome-windows resized and focused");

    // Close every Chrome window from the desktop; the app's run ends with the
    // last window and no window remains.
    const backendSession = await openAppForUser(ctx, alice, app);
    ctx.record(`session while the windows are open: ${JSON.stringify(backendSession).slice(0, 400)}`);
    await closeAppWindows(desktop.page, app).catch((error) => ctx.record(`cleanup note: ${error.message}`));
    const left = await allAppWindows(desktop.page, app).catch(() => []);
    assert(left.length === 0, `RAST windows remain after closing all Chrome windows: ${left.length}`);
    ctx.record("all Chrome windows closed from the desktop");
    await ctx.screenshot(desktop.page, "after-closing-all-chrome-windows");
    await desktop.context.close();
  },
};
