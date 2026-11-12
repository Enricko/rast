// (i) Idle cleanup: the server stops a graphical session/copy after it is no
// longer used.
//
// Native: config `linux_idle_minutes = 2` — after the last app exits the
// shared session stops once it has been idle that long.
// Docker baseline: the copy stops EMPTY_GRACE (60 s) after its last window
// closed, and the 30-minute input limit applies while a window stays open.

import * as browser from "../lib/browser.mjs";
import {
  allAppWindows,
  assert,
  closeAppWindows,
  ensurePortableApp,
  openAppViaUi,
  waitSessionGone,
} from "./_helpers.mjs";
import { sleep } from "../lib/rast-api.mjs";

export default {
  id: "i",
  title: "Idle cleanup stops the session after use",
  modes: ["docker", "native"],
  timeoutMs: 12 * 60_000,
  async run(ctx) {
    const { app } = await ensurePortableApp(ctx);
    const alice = ctx.user(0);
    const desktop = await browser.openDesktop(ctx.browser, alice);
    await openAppViaUi(ctx, desktop.page, app, { timeoutMs: 120_000, client: alice });
    const windows = await allAppWindows(desktop.page, app);
    assert(windows.length > 0, "the fixture app did not open");

    const backendSession = ctx.mode === "native" ? await alice.nativeSessionState() : null;
    ctx.record(`session before cleanup: ${JSON.stringify(backendSession?.session ?? null).slice(0, 300)}`);
    // Other apps may share the same native session (e.g. an XTerm from another
    // scenario). In that case the session is expected to STAY alive; only this
    // app's run has to end. The grace period only applies to an empty session.
    const otherActiveRuns = (backendSession?.session?.apps ?? []).filter(
      (run) => run.appId !== app.id && ["launching", "running", "starting"].includes(String(run.status)),
    );
    ctx.record(`other active runs in the session: ${JSON.stringify(otherActiveRuns.map((run) => run.appId))}`);

    const closedAt = Date.now();
    await closeAppWindows(desktop.page, app);
    ctx.record("windows closed; waiting for the backend to clean up");
    // Docker: EMPTY_GRACE is 60 s; native: linux_idle_minutes = 2 min.
    const grace = ctx.mode === "native" ? 2 * 60_000 : 60_000;
    const result = await waitSessionGone(ctx, alice, app, { timeoutMs: grace + 3 * 60_000 });
    const elapsed = Date.now() - closedAt;
    ctx.metric("cleanupMs", elapsed, "ms");
    ctx.record(`session cleaned up after ${(elapsed / 1000).toFixed(1)} s (grace ${grace / 1000}s)`);
    if (otherActiveRuns.length === 0) {
      assert(elapsed >= grace - 5_000, `session stopped after ${elapsed} ms, before the ${grace} ms grace`);
    } else {
      // A shared session with other running apps must stay up; the target run
      // ending is the correct observable outcome.
      const state = await ctx.mode === "native" ? alice.nativeSessionState() : null;
      const stillActive = (state?.session?.apps ?? []).some(
        (run) => run.appId !== app.id && ["launching", "running", "starting"].includes(String(run.status)),
      );
      ctx.record(`shared session still has other apps running: ${stillActive}`);
    }
    if (ctx.mode === "native") {
      const unit = await ctx.host.unitStatus(`rast-xpra-u${alice.user.id}`);
      ctx.record(`session unit after cleanup: ${JSON.stringify(unit)}`);
      const apps = await ctx.host.appUnits();
      ctx.record(`app units after cleanup: ${apps.length}`);
      assert(apps.length === 0, "native app units are still running after cleanup");
    } else {
      const copies = await ctx.host.dockerPs(["--filter", "label=rast.managed=linux-app"]);
      assert(copies.length === 0, "Docker copies are still running after cleanup");
    }
    await ctx.screenshot(desktop.page, "after-idle-cleanup").catch(() => {});
    await desktop.context.close();
    await sleep(500);
  },
};
