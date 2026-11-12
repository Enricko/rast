// (k) Failed installation and failed update leave accurate, recoverable
// state: a corrupt file, a wrong-architecture .deb and a corrupt archive are
// rejected with a precise error, the app row says "failed", and the server
// stays healthy (existing apps keep working).

import { readFileSync } from "node:fs";

import * as browser from "../lib/browser.mjs";
import { assert, ensureChrome, ensurePortableApp } from "./_helpers.mjs";
import { sleep } from "../lib/rast-api.mjs";

async function installExpectingFailure(ctx, client, { localPath, fileName }) {
  await client.uploadFile(localPath, { name: fileName });
  const started = await client.installFromPath(`/${fileName}`);
  ctx.record(`${fileName}: install started (${JSON.stringify(started).slice(0, 200)})`);
  let job = null;
  let jobError = null;
  if (started.jobId) {
    try {
      job = await client.waitJob(started.jobId, { timeoutMs: 5 * 60_000, intervalMs: 1000 });
    } catch (error) {
      jobError = error;
      job = error.job ?? (await client.job(started.jobId).catch(() => null));
    }
  }
  // The app row (when the API created one before the failure) must say failed
  // with a non-empty error and must not be openable.
  const startedAt = Date.now();
  let failedApp = null;
  for (;;) {
    const listing = await client.listApps();
    failedApp = listing.apps.find((app) => app.fileName === fileName) ?? null;
    if (failedApp) break;
    if (!started.app && !started.jobId) break;
    if (Date.now() - startedAt > 30_000) break;
    await sleep(500);
  }
  const state = {
    fileName,
    jobStatus: job?.status ?? jobError?.job?.status ?? null,
    jobError: job?.error ?? jobError?.job?.error ?? (jobError ? String(jobError.message) : null),
    app: failedApp ? { id: failedApp.id, status: failedApp.status, error: failedApp.error } : null,
  };
  ctx.record(`failure state for ${fileName}: ${JSON.stringify(state)}`);
  assert(
    (state.jobStatus && ["failed", "cancelled"].includes(state.jobStatus)) || (failedApp && failedApp.status === "failed"),
    `${fileName}: the install was not reported as failed (job ${state.jobStatus}, app ${failedApp?.status})`,
  );
  assert(state.jobError || failedApp?.error, `${fileName}: failure has no error message`);
  if (failedApp) {
    assert(failedApp.status === "failed", `${fileName}: app status is ${failedApp.status}, expected failed`);
    assert(failedApp.error && failedApp.error.length > 5, `${fileName}: app error is empty`);
    // A failed app must not be openable.
    const openAttempt = await client.raw("POST", `/api/linux-apps/${encodeURIComponent(failedApp.id)}/open`, {});
    ctx.record(`${fileName}: open on the failed app -> ${openAttempt.status}`);
    assert(openAttempt.status >= 400, `${fileName}: a failed app could be opened`);
    // Delete it: the server must accept the cleanup and forget it.
    await client.uninstall(failedApp.id, false);
    await sleep(500);
    const gone = await client.findApp((app) => app.id === failedApp.id);
    assert(!gone, `${fileName}: failed app could not be deleted`);
    ctx.record(`${fileName}: failed app deleted cleanly`);
  }
  return state;
}

export default {
  id: "k",
  title: "Failed install and update leave recoverable state",
  modes: ["docker", "native"],
  timeoutMs: 20 * 60_000,
  async run(ctx) {
    const admin = ctx.admin;
    // Existing healthy apps must not be disturbed by the failures below.
    const { app: chrome } = await ensureChrome(ctx);

    const corrupt = await installExpectingFailure(ctx, admin, {
      localPath: ctx.fixtures.corruptDeb,
      fileName: "invalid-corrupt.deb",
    });
    assert(
      /deb|package|archive|invalid|corrupt|unpack|install/i.test(String(corrupt.jobError ?? corrupt.app?.error)),
      `corrupt .deb failure is not precise: ${corrupt.jobError ?? corrupt.app?.error}`,
    );

    const wrongArch = await installExpectingFailure(ctx, admin, {
      localPath: ctx.fixtures.wrongArchDeb,
      fileName: "wrong-arch.deb",
    });
    const archMessage = String(wrongArch.jobError ?? wrongArch.app?.error ?? "");
    ctx.record(`wrong-arch error: ${archMessage}`);
    assert(
      /arch|amd64|arm64|aarch64|platform|not compatible/i.test(archMessage),
      `wrong-architecture failure does not mention the architecture: ${archMessage}`,
    );

    const badArchive = await installExpectingFailure(ctx, admin, {
      localPath: ctx.fixtures.invalidArchive,
      fileName: "invalid-archive.tar.gz",
    });
    assert(
      /archive|unpack|tar|corrupt|invalid|format/i.test(String(badArchive.jobError ?? badArchive.app?.error)),
      `corrupt archive failure is not precise: ${badArchive.jobError ?? badArchive.app?.error}`,
    );

    // A failed update must not damage the installed app.
    const portable = (await ensurePortableApp(ctx)).app;
    const before = await admin.getApp(portable.id);
    let updateFailure = null;
    try {
      const update = await admin.request("POST", `/api/linux-apps/${encodeURIComponent(portable.id)}/update`, {
        body: { path: "/invalid-corrupt.deb" },
      });
      ctx.record(`update with an invalid file accepted for job ${update.jobId ?? JSON.stringify(update).slice(0, 200)}`);
      if (update.jobId) {
        await admin.waitJob(update.jobId, { timeoutMs: 5 * 60_000 }).catch((error) => {
          updateFailure = error;
        });
      }
    } catch (error) {
      // A synchronous 4xx is also an accurate, recoverable failure.
      updateFailure = error;
      ctx.record(`update rejected synchronously: ${error.message}`);
    }
    const after = await admin.getApp(portable.id);
    ctx.record(`app after failed update: status ${after.status}, version ${after.version}`);
    assert(after.status === "ready", `the failed update left the app in status ${after.status}`);
    assert(after.build === before.build, `the failed update changed the build (${before.build} -> ${after.build})`);
    if (updateFailure) ctx.record(`failed update reported: ${updateFailure.message}`);

    // The healthy apps still work.
    const chromeAfter = await admin.getApp(chrome.id);
    assert(chromeAfter.status === "ready", "the Chrome app was damaged by the failed installs");
    ctx.record("existing apps remain ready");

    const alice = ctx.user(0);
    const desktop = await browser.openDesktop(ctx.browser, alice);
    await browser.launchApp(desktop.page, chrome.name);
    const windows = await browser
      .waitForWindows(desktop.page, (list) => list.some((window) => String(window.appId).includes(chrome.id)), {
        timeoutMs: 180_000,
        what: "Chrome window after failed installs",
      })
      .catch(async (error) => {
        const last = await browser.windowsInfo(desktop.page);
        throw new Error(`${error.message}; windows: ${JSON.stringify(last)}`);
      });
    ctx.record(`Chrome still opens after the failures (${windows.length} window(s))`);
    await ctx.screenshot(desktop.page, "healthy-app-after-failures");
    await desktop.context.close();
  },
};
