// (h) Closing one app preserves the other apps of the same user.
//
// Also proves that typing in the remaining app still reaches it: the fixture
// app mirrors the typed text into a label, so the streamed pixels change.

import * as browser from "../lib/browser.mjs";
import {
  allAppWindows,
  assert,
  closeAppWindows,
  ensureChrome,
  ensurePortableApp,
  openAppViaUi,
  openAppForUser,
  waitSessionGone,
} from "./_helpers.mjs";

export default {
  id: "h",
  title: "Closing one app preserves the other apps",
  modes: ["docker", "native"],
  timeoutMs: 12 * 60_000,
  async run(ctx) {
    const [{ app: chrome }, { app: portable }] = await Promise.all([
      ensureChrome(ctx),
      ensurePortableApp(ctx),
    ]);
    const alice = ctx.user(0);
    const desktop = await browser.openDesktop(ctx.browser, alice);
    await openAppViaUi(ctx, desktop.page, chrome, { timeoutMs: 240_000, client: alice });
    await openAppViaUi(ctx, desktop.page, portable, { timeoutMs: 120_000, client: alice });

    const portableWindow = (await allAppWindows(desktop.page, portable))[0];
    assert(portableWindow, "the fixture app has no window");
    const chromeWindows = await allAppWindows(desktop.page, chrome);
    assert(chromeWindows.length > 0, "Chrome has no window");
    const chromeSession = await openAppForUser(ctx, alice, chrome);
    ctx.record(`chrome session: ${JSON.stringify(chromeSession).slice(0, 300)}`);

    // Close Chrome from the desktop.
    await closeAppWindows(desktop.page, chrome);
    ctx.record("Chrome windows closed");
    const chromeGone = await waitSessionGone(ctx, alice, chrome, { timeoutMs: 180_000 }).catch((error) => {
      throw new Error(`Chrome copy/session did not stop: ${error.message}`);
    });
    ctx.record(`chrome session stopped: ${JSON.stringify(chromeGone).slice(0, 300)}`);

    // The fixture app window and its backend are untouched.
    const stillOpen = await allAppWindows(desktop.page, portable);
    assert(stillOpen.length > 0, "the fixture app window disappeared when Chrome closed");
    await openAppForUser(ctx, alice, portable);
    ctx.record("fixture app session still running");

    // Input still reaches the remaining app: its pixels change while typing.
    const beforeShot = await ctx.screenshot(desktop.page, "remaining-app-before-typing");
    let input;
    if (ctx.mode === "docker") {
      await browser.waitForAppFrame(desktop.page, { timeoutMs: 60_000 });
      const box = await browser.frameBox(desktop.page);
      // The GTK entry is roughly in the middle of the fixture window.
      await desktop.page.mouse.click(box.x + box.width / 2, box.y + box.height * 0.62);
      await desktop.page.waitForTimeout(300);
      const baseline = await browser.frameBuffer(desktop.page);
      await desktop.page.keyboard.type("still works", { delay: 40 });
      const changed = await browser
        .waitForFrameChange(desktop.page, baseline, { timeoutMs: 10_000 })
        .catch(() => ({ changed: false }));
      input = changed;
    } else {
      const locator = desktop.page.locator(`.window[data-window-id="${stillOpen[0].id}"]`);
      const box = await locator.boundingBox();
      const baseline = await desktop.page.screenshot({ clip: box });
      await desktop.page.mouse.click(box.x + box.width / 2, box.y + box.height * 0.62);
      await desktop.page.keyboard.type("still works", { delay: 40 });
      await desktop.page.waitForTimeout(1500);
      const afterBuffer = await desktop.page.screenshot({ clip: box });
      input = { changed: !afterBuffer.equals(baseline) };
    }
    ctx.record(`remaining app pixels changed while typing: ${input.changed} (${input.elapsedMs ?? "n/a"} ms)`);
    assert(input.changed, "input did not reach the remaining app (no pixel change)");
    await ctx.screenshot(desktop.page, "remaining-app-after-typing");
    ctx.record(`before screenshot: ${beforeShot}`);
    await desktop.context.close();
  },
};
