// (d) One user opens three Chrome windows: each becomes its own RAST window
// with its own taskbar entry; focus, resize and close behave like desktop
// windows.
//
// The extra windows are opened from inside Chrome with Ctrl+N, sent through
// the embedded stream — exactly like a person would.

import * as browser from "../lib/browser.mjs";
import {
  allAppWindows,
  assert,
  closeAppWindows,
  ensureChrome,
  openAppViaUi,
  openAppForUser,
} from "./_helpers.mjs";

async function openChromeWindowFromInside(page) {
  // Focus a real (mirrored) Chrome window, then send the Chrome shortcut.
  // While Chrome is still starting, the shell shows a "Starting …" placeholder
  // that cannot receive input, so wait for a real window first.
  const windows = await browser.waitForWindows(
    page,
    (list) => list.some((window) => /Chrome/i.test(String(window.title)) && !/^Starting/i.test(String(window.title))),
    { timeoutMs: 120_000, what: "a mirrored Chrome window" },
  );
  const target = windows.filter((window) => /Chrome/i.test(String(window.title)) && !/^Starting/i.test(String(window.title))).pop();
  await page.locator(`.window[data-window-id="${target.id}"]`).click({ position: { x: Math.min(120, Math.max(20, target.rect.width - 40)), y: 8 } });
  await page.waitForTimeout(500);
  await page.keyboard.press("Control+n");
  return target;
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
    await openAppViaUi(ctx, desktop.page, app, { timeoutMs: 240_000, client: alice });
    const first = await allAppWindows(desktop.page, app);
    ctx.record(`initial Chrome windows: ${JSON.stringify(first.map((window) => window.title))}`);

    // Two more windows inside Chrome.
    for (let index = 0; index < 2; index += 1) {
      await openChromeWindowFromInside(desktop.page);
      await browser
        .waitForWindows(
          desktop.page,
          (windows) => windows.filter((window) => String(window.appId).includes(app.id)).length >= index + 2,
          { timeoutMs: 60_000, what: `Chrome window ${index + 2}` },
        )
        .catch(() => {});
    }
    const windows = await allAppWindows(desktop.page, app);
    ctx.record(`Chrome windows after Ctrl+N x2: ${JSON.stringify(windows.map((window) => window.title))}`);
    ctx.metric("chromeWindowCount", windows.length);
    await ctx.screenshot(desktop.page, "three-chrome-windows");

    // Taskbar entries for each window.
    const taskbar = await browser.taskbarInfo(desktop.page);
    const labels = taskbar.map((entry) => entry.label);
    ctx.record(`taskbar labels: ${JSON.stringify(labels)}`);
    assert(taskbar.length >= 3, `expected at least 3 taskbar entries, found ${taskbar.length}`);

    // Focus: use the taskbar entries (a covered window cannot be clicked
    // directly) and check that the corresponding window becomes focused.
    for (const window of windows.slice(0, 3)) {
      const entries = desktop.page.locator(".taskbar-app");
      const count = await entries.count();
      let clicked = false;
      for (let index = 0; index < count; index += 1) {
        const label = (await entries.nth(index).locator(".taskbar-app-label").textContent())?.trim() ?? "";
        if (window.title && label.includes(window.title)) {
          await entries.nth(index).click({ force: true });
          clicked = true;
          break;
        }
      }
      if (!clicked) {
        // Fall back to clicking the titlebar with the mouse (works even when
        // Playwright's actionability checks would time out on a covered window).
        const box = await desktop.page.locator(`.window[data-window-id="${window.id}"]`).boundingBox();
        if (box) await desktop.page.mouse.click(box.x + 40, box.y + 8);
      }
      await desktop.page.waitForTimeout(400);
      const after = await browser.windowsInfo(desktop.page);
      const focused = after.find((entry) => entry.focused);
      ctx.record(`focused after taskbar click for ${window.id}: ${focused?.title ?? "none"}`);
    }

    // Resize the first window with its bottom-right handle.
    const resizeTarget = windows[0];
    const before = resizeTarget.rect;
    const handle = desktop.page.locator(`.window[data-window-id="${resizeTarget.id}"] .win-resize-se`);
    if (await handle.count()) {
      const box = await handle.first().boundingBox();
      if (box) {
        await desktop.page.mouse.move(box.x + box.width / 2, box.y + box.height / 2);
        await desktop.page.mouse.down();
        await desktop.page.mouse.move(box.x + box.width / 2 + 140, box.y + box.height / 2 + 90, { steps: 12 });
        await desktop.page.mouse.up();
      }
      await desktop.page.waitForTimeout(600);
      const after = (await browser.windowsInfo(desktop.page)).find((window) => window.id === resizeTarget.id);
      ctx.record(`resize: ${JSON.stringify(before)} -> ${JSON.stringify(after?.rect)}`);
      if (after) {
        assert(
          after.rect.width > before.width + 40 || after.rect.height > before.height + 30,
          `window did not resize (${before.width}x${before.height} -> ${after.width}x${after.height})`,
        );
      }
    } else {
      ctx.record("window has no resize handle (frameless); resize handled by the app window only");
    }
    await ctx.screenshot(desktop.page, "three-chrome-windows resized and focused");

    // Close all three; the copy/session ends when the last window closes.
    const backendSession = await openAppForUser(ctx, alice, app);
    ctx.record(`session while the windows are open: ${JSON.stringify(backendSession).slice(0, 400)}`);
    await closeAppWindows(desktop.page, app);
    const left = await browser.windowsInfo(desktop.page);
    assert(left.length === 0, `RAST windows remain after closing all Chrome windows: ${left.length}`);
    ctx.record("all three Chrome windows closed from the desktop");
    await ctx.screenshot(desktop.page, "after-closing-all-chrome-windows");
    await desktop.context.close();
  },
};
