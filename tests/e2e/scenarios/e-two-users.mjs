// (e) Two users use Chrome concurrently with separate profiles, cookies,
// files and displays.
//
// Native: two Linux identities, two `rast-xpra-u<id>` sessions, separate uids
// and DISPLAYs. Docker baseline: two containers, one per person, with
// separate named volumes.

import * as browser from "../lib/browser.mjs";
import {
  allAppWindows,
  assert,
  ensureChrome,
  identityFor,
  openAppForUser,
  openAppViaUi,
  sessionIdentity,
  sessionUnitFor,
} from "./_helpers.mjs";
import { sleep } from "../lib/rast-api.mjs";

export default {
  id: "e",
  title: "Two users use Chrome concurrently with separate data",
  modes: ["docker", "native"],
  timeoutMs: 15 * 60_000,
  async run(ctx) {
    const { app } = await ensureChrome(ctx);
    const [alice, bob] = [ctx.user(0), ctx.user(1)];

    // Both users open Chrome at (nearly) the same time.
    const [aliceDesktop, bobDesktop] = await Promise.all([
      browser.openDesktop(ctx.browser, alice),
      browser.openDesktop(ctx.browser, bob),
    ]);
    const startedAt = Date.now();
    await Promise.all([
      openAppViaUi(ctx, aliceDesktop.page, app, { timeoutMs: 240_000, client: alice }),
      openAppViaUi(ctx, bobDesktop.page, app, { timeoutMs: 240_000, client: bob }),
    ]);
    ctx.metric("twoUserOpenMs", Date.now() - startedAt, "ms");
    const aliceWindows = await allAppWindows(aliceDesktop.page, app);
    const bobWindows = await allAppWindows(bobDesktop.page, app);
    assert(aliceWindows.length > 0, "alice has no Chrome window");
    assert(bobWindows.length > 0, "bob has no Chrome window");
    ctx.record(`alice windows: ${aliceWindows.map((window) => window.title).join(", ")}`);
    ctx.record(`bob windows: ${bobWindows.map((window) => window.title).join(", ")}`);
    await ctx.screenshot(aliceDesktop.page, "alice-chrome");
    await ctx.screenshot(bobDesktop.page, "bob-chrome");

    const aliceSession = await openAppForUser(ctx, alice, app);
    const bobSession = await openAppForUser(ctx, bob, app);
    ctx.record(`alice session: ${JSON.stringify(aliceSession).slice(0, 400)}`);
    ctx.record(`bob session: ${JSON.stringify(bobSession).slice(0, 400)}`);
    assert(
      sessionIdentity(ctx, aliceSession) !== sessionIdentity(ctx, bobSession),
      "both users share the same session/copy",
    );

    if (ctx.mode === "native") {
      const aliceIdentity = await identityFor(ctx, alice);
      const bobIdentity = await identityFor(ctx, bob);
      assert(aliceIdentity && bobIdentity, "both users need a Linux identity");
      assert(aliceIdentity.uid !== bobIdentity.uid, "Linux identities share a uid");
      assert(aliceIdentity.home !== bobIdentity.home, "Linux identities share a home");
      ctx.record(`alice identity: ${JSON.stringify(aliceIdentity)}`);
      ctx.record(`bob identity: ${JSON.stringify(bobIdentity)}`);
      const aliceUnit = await sessionUnitFor(ctx, alice);
      const bobUnit = await sessionUnitFor(ctx, bob);
      assert(aliceUnit?.status.ActiveState === "active", "alice session unit is not active");
      assert(bobUnit?.status.ActiveState === "active", "bob session unit is not active");
      ctx.record(`alice unit ${aliceUnit.unit}: ${aliceUnit.status.SubState}`);
      ctx.record(`bob unit ${bobUnit.unit}: ${bobUnit.status.SubState}`);
      const aliceState = (await alice.nativeSession()).session ?? {};
      const bobState = (await bob.nativeSession()).session ?? {};
      assert(aliceState.display !== bobState.display, "both users are on the same DISPLAY");
      ctx.record(`displays: alice :${aliceState.display}, bob :${bobState.display}`);
      // Separate profile/data trees under the identity homes / app data dir.
      for (const [who, identity] of [
        ["alice", aliceIdentity],
        ["bob", bobIdentity],
      ]) {
        const home = await ctx.host.sudo("ls", ["-la", identity.home]).catch(() => null);
        ctx.record(`${who} home ${identity.home} entries: ${(home?.stdout ?? "").split("\n").length - 1}`);
      }
    } else {
      const copies = await ctx.host.dockerPs(["--filter", "label=rast.managed=linux-app"]);
      const aliceCopy = copies.find(
        (container) =>
          container.labels["rast.linux.app"] === app.id &&
          String(container.labels["rast.linux.user"]) === String(alice.user.id),
      );
      const bobCopy = copies.find(
        (container) =>
          container.labels["rast.linux.app"] === app.id &&
          String(container.labels["rast.linux.user"]) === String(bob.user.id),
      );
      assert(aliceCopy, "no Docker copy for alice");
      assert(bobCopy, "no Docker copy for bob");
      assert(aliceCopy.name !== bobCopy.name, "both users share a Docker copy");
      ctx.record(`copies: ${aliceCopy.name}, ${bobCopy.name}`);
      const volumes = await ctx.host.dockerVolumes(["--filter", "label=rast.managed=linux-app"]);
      const aliceVolume = volumes.find((name) => name.includes(`u${alice.user.id}`));
      const bobVolume = volumes.find((name) => name.includes(`u${bob.user.id}`));
      ctx.record(`alice volume: ${aliceVolume ?? "none"}; bob volume: ${bobVolume ?? "none"}`);
      assert(aliceVolume && bobVolume, "each user must have their own app data volume");
      assert(aliceVolume !== bobVolume, "both users share one app data volume");
    }

    // Closing one user's window leaves the other user untouched.
    const aliceWindowId = aliceWindows[0].id;
    const close = aliceDesktop.page.locator(`.window[data-window-id="${aliceWindowId}"] .win-control-close`);
    if (await close.count()) await close.first().click();
    await aliceDesktop.page.waitForTimeout(1000);
    const bobStill = await allAppWindows(bobDesktop.page, app);
    assert(bobStill.length > 0, "closing alice's window affected bob's window");
    ctx.record("alice closing her window did not affect bob");
    await sleep(500);
    await aliceDesktop.context.close();
    await bobDesktop.context.close();
  },
};
