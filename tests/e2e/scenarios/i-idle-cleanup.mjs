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
    // A native session stops after it has been empty (no running app AND no
    // connected viewer) for EMPTY_GRACE plus `linux_idle_minutes`. Close the
    // browser first so the viewer leaves, then observe the backend through the
    // API exactly like a person who closed the tab.
    await desktop.context.close();
    ctx.record("windows closed and browser tab closed; waiting for the backend to clean up");
    // Docker: EMPTY_GRACE is 60 s. Native: EMPTY_GRACE (60 s) plus
    // linux_idle_minutes (2 min) — the server only reclaims an empty,
    // viewer-less session after both.
    const grace = ctx.mode === "native" ? 2 * 60_000 : 60_000;
    const deadline = grace + 4 * 60_000;
    let sessionEnded = false;
    let shared = otherActiveRuns.length > 0;
    let stateAfter = null;
    for (;;) {
      if (ctx.mode === "native") {
        stateAfter = (await alice.nativeSessionState().catch(() => null))?.session ?? null;
        const activeRuns = (stateAfter?.apps ?? []).filter((run) =>
          ["launching", "running", "starting"].includes(String(run.status)),
        );
        const otherActive = activeRuns.filter((run) => run.appId !== app.id);
        if (!stateAfter || stateAfter.status === "ended") {
          sessionEnded = true;
          break;
        }
        if (otherActive.length > 0) {
          shared = true;
          break;
        }
        if (activeRuns.some((run) => run.appId === app.id)) {
          // The run is still reported as running; keep waiting.
        }
      } else {
        const listing = await alice.listApps();
        const mine = (listing.sessions ?? []).filter(
          (session) => session.appId === app.id && session.status !== "ended",
        );
        if (mine.length === 0) {
          sessionEnded = true;
          break;
        }
      }
      if (Date.now() - closedAt > deadline) {
        throw new Error(
          `backend did not stop the empty session within ${deadline / 1000}s (last: ${JSON.stringify(stateAfter)?.slice(0, 500)})`,
        );
      }
      await sleep(1000);
    }
    const elapsed = Date.now() - closedAt;
    ctx.metric("cleanupMs", elapsed, "ms");
    ctx.record(
      `backend cleanup after ${(elapsed / 1000).toFixed(1)} s (grace ${grace / 1000}s, ended=${sessionEnded}, shared=${shared})`,
    );
    if (!shared) {
      assert(sessionEnded, "the empty session/copy was not stopped");
      assert(elapsed >= grace - 5_000, `session stopped after ${elapsed} ms, before the ${grace} ms grace`);
      assert(elapsed < grace + 3 * 60_000, `session stopped after ${elapsed} ms, far past the ${grace} ms grace`);
    } else {
      // A shared session with other running apps must stay up; only this app's
      // run ends (the idle timer starts once the session is truly empty).
      const stillActive = (stateAfter?.apps ?? []).some(
        (run) => run.appId !== app.id && ["launching", "running", "starting"].includes(String(run.status)),
      );
      ctx.record(`shared session still has other apps running: ${stillActive}`);
    }
    if (ctx.mode === "native") {
      const unit = await ctx.host.unitStatus(`rast-xpra-u${alice.user.id}`);
      ctx.record(`session unit after cleanup: ${JSON.stringify(unit)}`);
      const apps = await ctx.host.appUnits();
      const activeApps = apps.filter((entry) => entry.active === "active");
      // Other RAST accounts may have their own apps running on this host; only
      // this person's units count for the idle-cleanup assertion.
      const identity = (await ctx.host.linuxIdentities()).find((entry) => entry.userId === alice.user.id);
      const uid = identity?.uid ?? 210000 + alice.user.id;
      const ownUnit = `rast-app-u${uid}-`;
      const ownApps = activeApps.filter((entry) => entry.unit.startsWith(ownUnit));
      ctx.record(
        `app units after cleanup: ${ownApps.length} active for uid ${uid} / ${activeApps.length} active host-wide / ${apps.length} listed`,
      );
      if (!shared) {
        assert(ownApps.length === 0, "native app units are still running after cleanup");
      } else {
        assert(
          !activeApps.some((entry) => entry.unit.includes(app.id)),
          "this app's unit is still running in the shared session",
        );
      }
    } else {
      const copies = await ctx.host.dockerPs(["--filter", "label=rast.managed=linux-app"]);
      assert(copies.length === 0, "Docker copies are still running after cleanup");
    }
    await sleep(500);
  },
};
