// s0 — smoke: the dev instance is reachable, the browser login works, the
// desktop shell renders and App Center opens. Fast sanity check before the
// longer scenarios.

import * as browser from "../lib/browser.mjs";
import { config } from "../lib/config.mjs";

export default {
  id: "s0",
  title: "Smoke: API health, browser login and desktop shell",
  modes: ["docker", "native"],
  timeoutMs: 2 * 60_000,
  async run(ctx) {
    const health = await fetch(`${config.baseUrl}/healthz`);
    ctx.assert = null;
    if (health.status !== 200) throw new Error(`/healthz returned ${health.status}`);

    const adminSession = await browser.newDesktopSession(ctx.browser);
    await browser.loginViaForm(
      adminSession.page,
      config.admin.username,
      config.admin.password,
    );
    const windows = await browser.windowsInfo(adminSession.page);
    const taskbar = await browser.taskbarInfo(adminSession.page);
    if (taskbar.length === 0) throw new Error("taskbar rendered no entries");
    ctx.record(`logged in through the form as ${config.admin.username} (windows: ${windows.length})`);
    await ctx.screenshot(adminSession.page, "desktop-after-login");

    await browser.launchApp(adminSession.page, "App Center");
    await adminSession.page.waitForSelector(".pkg-grid, .pkg-heading, .pkg-nav", { timeout: 30_000 });
    ctx.record("App Center opened");
    await ctx.screenshot(adminSession.page, "app-center");

    const bootstrap = await (await fetch(`${config.baseUrl}/api/bootstrap`)).json();
    ctx.record(`bootstrap: ${bootstrap.product} ${bootstrap.version} (authenticated=${bootstrap.authenticated})`);
    await adminSession.context.close();
  },
};
