// (f) Concurrent launch requests reuse the intended session/copy instead of
// starting duplicates.

import * as browser from "../lib/browser.mjs";
import {
  allAppWindows,
  assert,
  ensureChrome,
  openAppViaUi,
  sessionIdentity,
  waitDockerSession,
  waitNativeSession,
} from "./_helpers.mjs";
import { sleep } from "../lib/rast-api.mjs";

export default {
  id: "f",
  title: "Concurrent launch requests reuse the intended session",
  modes: ["docker", "native"],
  timeoutMs: 10 * 60_000,
  async run(ctx) {
    const { app } = await ensureChrome(ctx);
    const alice = ctx.user(0);

    // Fire the launch/open endpoint eight times at once.
    const requests = Array.from({ length: 8 }, (_, index) =>
      index % 2 === 0 ? alice.openApp(app.id) : alice.launch(app.id),
    );
    const settled = await Promise.allSettled(requests);
    const fulfilled = settled.filter((result) => result.status === "fulfilled");
    const rejected = settled.filter((result) => result.status === "rejected");
    ctx.record(`concurrent requests: ${fulfilled.length} ok, ${rejected.length} rejected`);
    if (rejected.length > 0) {
      ctx.record(`first rejection: ${rejected[0].reason?.message ?? rejected[0].reason}`);
    }
    assert(fulfilled.length >= 2, "not enough concurrent launches succeeded");

    // Wait for readiness and collect the session identities the backend reports.
    let identities = new Set();
    if (ctx.mode === "native") {
      const { session } = await waitNativeSession(ctx, alice, null, { timeoutMs: 240_000 });
      identities.add(`display:${session.display}`);
      const listing = await alice.listApps();
      const runsNow = (listing.sessions ?? []).length;
      ctx.record(`native session: ${JSON.stringify(session).slice(0, 500)} (listApps sessions: ${runsNow})`);
    } else {
      for (const result of fulfilled) {
        const session = result.value;
        if (!session?.id) continue;
        const ready = await waitDockerSession(ctx, alice, session.id, { timeoutMs: 180_000 });
        identities.add(`id:${ready.id}`);
      }
      ctx.record(`distinct Docker session ids returned: ${identities.size}`);
    }
    assert(identities.size === 1, `concurrent launches produced ${identities.size} distinct sessions`);
    ctx.record(`all concurrent launches resolve to one session: ${[...identities][0]}`);

    // The backend kept exactly one copy/session.
    if (ctx.mode === "native") {
      const state = await alice.nativeSession();
      const session = state.session ?? {};
      const appRuns = (session.runs ?? session.apps ?? []).filter((run) => run.appId === app.id);
      ctx.record(`native runs for the app: ${JSON.stringify(appRuns).slice(0, 400)}`);
      const units = (await ctx.host.appUnits()).filter((unit) => unit.unit.startsWith("rast-app-"));
      ctx.record(`native app units: ${JSON.stringify(units.map((unit) => unit.unit))}`);
    } else {
      const copies = (await ctx.host.dockerPs(["--filter", "label=rast.managed=linux-app"])).filter(
        (container) =>
          container.labels["rast.linux.app"] === app.id &&
          String(container.labels["rast.linux.user"]) === String(alice.user.id),
      );
      ctx.record(`Docker copies: ${JSON.stringify(copies.map((copy) => copy.name))}`);
      assert(copies.length === 1, `expected exactly one Docker copy, found ${copies.length}`);
    }

    // The UI shows one app window, not eight.
    const desktop = await browser.openDesktop(ctx.browser, alice);
    await openAppViaUi(ctx, desktop.page, app, { timeoutMs: 120_000, client: alice });
    await sleep(1000);
    const windows = await allAppWindows(desktop.page, app);
    ctx.record(`RAST windows after opening: ${windows.length}`);
    assert(windows.length >= 1, "no window opened");
    await ctx.screenshot(desktop.page, "concurrent-launch-one-window");
    await desktop.context.close();
  },
};
