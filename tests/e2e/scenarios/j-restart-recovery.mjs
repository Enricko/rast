// (j) Service restart recovery.
//
// Docker baseline: a restart forgets copies (their access keys live in
// memory) and RAST stops the leftovers on start-up.
// Native: the session is persisted in the database (display/port/key/unit)
// and adopted after the restart; the Xpra session keeps running, a later
// launch returns the same run instead of duplicating it.

import { execFile } from "node:child_process";
import { promisify } from "node:util";

import * as browser from "../lib/browser.mjs";
import {
  allAppWindows,
  assert,
  closeAppWindows,
  ensurePortableApp,
  openAppViaUi,
  openAppForUser,
  sessionIdentity,
  waitNativeSession,
} from "./_helpers.mjs";
import { RastApi, sleep } from "../lib/rast-api.mjs";

const execFileAsync = promisify(execFile);

async function restartDevServer(ctx) {
  const script = `${ctx.config.instanceRoot === "/tmp/rast-e2e" ? "/home/lufria/RAST" : "/home/lufria/RAST"}/scripts/e2e/dev-instance.sh`;
  ctx.record(`restarting the dev server: ${script} restart-server`);
  const { stdout, stderr } = await execFileAsync("bash", [script, "restart-server"], {
    timeout: 180_000,
    maxBuffer: 4 * 1024 * 1024,
  });
  ctx.record(`restart output: ${(stdout + stderr).trim().split("\n").slice(-3).join(" | ")}`);
  const started = Date.now();
  for (;;) {
    const response = await fetch(`${ctx.config.baseUrl}/healthz`).catch(() => null);
    if (response?.status === 200) break;
    if (Date.now() - started > 120_000) throw new Error("the server did not come back after the restart");
    await sleep(500);
  }
  ctx.record(`server healthy again after ${((Date.now() - started) / 1000).toFixed(1)} s`);
}

export default {
  id: "j",
  title: "Service restart recovery",
  modes: ["docker", "native"],
  timeoutMs: 12 * 60_000,
  async run(ctx) {
    const { app } = await ensurePortableApp(ctx);
    const alice = ctx.user(0);
    const desktop = await browser.openDesktop(ctx.browser, alice);
    await openAppViaUi(ctx, desktop.page, app, { timeoutMs: 120_000, client: alice });
    const windows = await allAppWindows(desktop.page, app);
    assert(windows.length > 0, "the fixture app did not open");
    const before = await openAppForUser(ctx, alice, app);
    const beforeIdentity = sessionIdentity(ctx, before);
    ctx.record(`session before restart: ${beforeIdentity}`);
    if (ctx.mode === "native") {
      const unit = await ctx.host.unitStatus(`rast-xpra-u${alice.user.id}`);
      ctx.record(`session unit before restart: ${unit.ActiveState}/${unit.SubState}`);
    } else {
      const copies = await ctx.host.dockerPs(["--filter", "label=rast.managed=linux-app"]);
      ctx.record(`Docker copies before restart: ${JSON.stringify(copies.map((copy) => copy.name))}`);
    }

    await restartDevServer(ctx);

    // The session cookie lives in the database: the old client keeps working.
    const refreshed = new RastApi(ctx.config.baseUrl);
    refreshed.cookies = alice.cookies;
    refreshed.csrf = alice.csrf;
    refreshed.user = alice.user;
    const me = await refreshed.get("/api/auth/me").catch((error) => {
      throw new Error(`session did not survive the restart: ${error.message}`);
    });
    ctx.record(`session survived the restart (user ${me.username ?? me.user?.username ?? "?"})`);

    if (ctx.mode === "docker") {
      // Restart drops in-memory copies; leftovers are stopped.
      const listing = await refreshed.listApps();
      const running = (listing.sessions ?? []).filter((session) => session.status !== "ended");
      ctx.record(`copies reported after restart: ${JSON.stringify(running)}`);
      const started = Date.now();
      for (;;) {
        const copies = await ctx.host.dockerPs(["--filter", "label=rast.managed=linux-app"]);
        if (copies.length === 0) break;
        if (Date.now() - started > 120_000) {
          throw new Error(`leftover Docker copies were not stopped: ${JSON.stringify(copies.map((copy) => copy.name))}`);
        }
        await sleep(1000);
      }
      ctx.record("leftover Docker copies were stopped on start-up");
    } else {
      // Native session adoption: same display/key, unit still running, and a
      // new launch returns the same run.
      const state = await waitNativeSession(ctx, refreshed, null, { timeoutMs: 60_000 });
      const session = state.session ?? {};
      ctx.record(`native session after restart: ${JSON.stringify(session).slice(0, 500)}`);
      assert(session.display !== undefined, "adopted native session has no display");
      const unit = await ctx.host.unitStatus(`rast-xpra-u${alice.user.id}`);
      ctx.record(`session unit after restart: ${unit.ActiveState}/${unit.SubState}`);
      assert(unit.ActiveState === "active", "the adopted Xpra session unit is not active");
      const relaunched = await refreshed.launch(app.id);
      const runId = relaunched.run?.id ?? null;
      ctx.record(`launch after restart returned run ${runId}`);
      const runs = (session.apps ?? []).filter((entry) => entry.appId === app.id);
      if (runId && runs.length > 0) {
        assert(
          runs.some((entry) => entry.runId === runId),
          `restart duplicated the run (returned ${runId}, session has ${JSON.stringify(runs.map((r) => r.runId))})`,
        );
      }
    }

    // The desktop can reconnect after the restart.
    await desktop.page.reload({ waitUntil: "domcontentloaded" });
    await desktop.page.waitForSelector(".taskbar", { timeout: 60_000 });
    await sleep(1500);
    ctx.record("browser reconnected after the restart");

    // Cleaning up: close the app windows; the session idles out.
    await closeAppWindows(desktop.page, app).catch((error) => ctx.record(`cleanup note: ${error.message}`));
    await ctx.screenshot(desktop.page, "after-service-restart");
    await desktop.context.close();
  },
};
