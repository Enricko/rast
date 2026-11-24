// Measure the pre-warmed launch path on production: after sign-in the desktop
// asks the server to start the shared native session; the first application
// launch should then only pay the application start (cf. the ~12 s cold start
// and ~0.8 s warm launch documented in BENCHMARKS.md).
//
// Usage: cd tests/e2e && node prewarm/measure.mjs [--user=t-bob]
import { chromium } from "playwright";
import { readFileSync } from "node:fs";

const arg = (name, fallback) => {
  const found = process.argv.find((item) => item.startsWith(`--${name}=`));
  return found ? found.split("=").slice(1).join("=") : fallback;
};
const baseUrl = (arg("base-url", "https://desktop.lufria.tech")).replace(/\/$/, "");
const user = arg("user", "t-bob");
const creds = Object.fromEntries(
  readFileSync("/home/lufria/.config/rast-test/creds", "utf8")
    .split("\n").map((line) => line.trim()).filter((line) => line.includes("="))
    .map((line) => [line.slice(0, line.indexOf("=")), line.slice(line.indexOf("=") + 1)]),
);
const executable = process.env.RAST_E2E_CHROMIUM || "/home/lufria/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome";
const browser = await chromium.launch({ headless: true, executablePath: executable, args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"] });
const context = await browser.newContext({ viewport: { width: 1480, height: 920 }, ignoreHTTPSErrors: true });
const page = await context.newPage();
page.setDefaultTimeout(60_000);
const api = async (path, method = "GET", body = null, csrf = null) =>
  page.evaluate(async ({ path, method, body, csrf }) => {
    const response = await fetch(path, {
      method,
      headers: { ...(body ? { "content-type": "application/json" } : {}), ...(csrf ? { "X-CSRF-Token": csrf } : {}) },
      body: body ? JSON.stringify(body) : undefined,
    });
    return { status: response.status, data: await response.json().catch(() => null) };
  }, { path, method, body, csrf });

const result = { user, baseUrl };
try {
  await page.goto(`${baseUrl}/`, { waitUntil: "domcontentloaded" });
  await page.waitForSelector('.auth-form, input[name="username"], .taskbar');
  if (await page.locator('input[name="username"]').count()) {
    await page.fill('input[name="username"]', user);
    await page.fill('input[type="password"]', creds[user]);
    await page.click('.auth-form button[type="submit"], button:has-text("Sign in")');
  }
  await page.waitForSelector(".taskbar", { timeout: 60_000 });
  const me = await api("/api/auth/me");
  const csrf = me.data?.csrfToken;
  // Start from a stopped session so pre-warming is what we actually measure.
  await api("/api/linux-apps/session/close", "POST", {}, csrf);
  // Let the display/socket teardown settle: starting a new session on the same
  // display 1 s after close makes the readiness check race the old lock (and
  // the server retries once after its 30 s timeout, which would distort the
  // measurement rather than the user experience).
  for (let i = 0; i < 20; i += 1) {
    const overview = await api("/api/linux-apps");
    if (!overview.data?.session) break;
    await page.waitForTimeout(250);
  }
  await page.waitForTimeout(3000);
  await page.reload({ waitUntil: "domcontentloaded" });
  const t0 = Date.now();
  await page.waitForSelector(".taskbar", { timeout: 60_000 });
  result.taskbarMs = Date.now() - t0;
  // Poll until the background session is ready (the prewarm request is fired
  // by the desktop after the app list loads).
  let session = null;
  const pollStart = Date.now();
  while (Date.now() - pollStart < 90_000) {
    const overview = await api("/api/linux-apps");
    session = overview.data?.session ?? null;
    if (session && session.status === "ready") break;
    await page.waitForTimeout(250);
  }
  result.prewarmReadyMs = session && session.status === "ready" ? Date.now() - pollStart : null;
  result.session = session ? { display: session.display, profile: session.profile, backend: session.displayBackend } : null;
  // Launch Chrome from the start menu and time the adopted RAST window.
  await page.locator(".taskbar-start").click();
  await page.waitForSelector(".start-menu");
  const tile = page.locator(".menu-app", { hasText: "Google Chrome" }).first();
  await tile.waitFor();
  const clickAt = Date.now();
  await tile.click();
  await page.waitForSelector(".linux-native-window", { timeout: 180_000 });
  result.launchToWindowMs = Date.now() - clickAt;
  await page.screenshot({ path: `prewarm/prewarmed-window-${Date.now()}.png` }).catch(() => {});
  // Close the window and the session again.
  const close = page.locator(".linux-native-window").first().locator("xpath=..").locator(".win-control-close");
  if (await close.count()) await close.first().click().catch(() => {});
  await page.waitForTimeout(1000);
  await api("/api/linux-apps/session/close", "POST", {}, csrf);
} catch (error) {
  result.error = String(error);
} finally {
  console.log(JSON.stringify(result, null, 2));
  await context.close();
  await browser.close();
}
