// Clean pre-warm measurement: the caller must close the test account's session
// first; this script then signs in, waits for the background pre-warm, and
// measures click -> adopted window WITHOUT any close/reload race.
import { chromium } from "playwright";
import { readFileSync } from "node:fs";
const arg = (name, fallback) => {
  const found = process.argv.find((item) => item.startsWith(`--${name}=`));
  return found ? found.split("=").slice(1).join("=") : fallback;
};
const baseUrl = (arg("base-url", "https://desktop.lufria.tech")).replace(/\/$/, "");
const user = arg("user", "t-alice");
const creds = Object.fromEntries(readFileSync("/home/lufria/.config/rast-test/creds", "utf8").split("\n").map((l) => l.trim()).filter((l) => l.includes("=")).map((l) => [l.slice(0, l.indexOf("=")), l.slice(l.indexOf("=") + 1)]));
const browser = await chromium.launch({ headless: true, executablePath: process.env.RAST_E2E_CHROMIUM || "/home/lufria/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome", args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"] });
const context = await browser.newContext({ viewport: { width: 1480, height: 920 }, ignoreHTTPSErrors: true });
const page = await context.newPage();
page.setDefaultTimeout(90_000);
const result = { user, checks: {}, console: [], launchRequests: [] };
page.on("console", (m) => { if (m.type() !== "log") result.console.push(`${m.type()}: ${m.text().slice(0, 200)}`); });
page.on("request", (r) => { if (r.url().includes("/api/linux-apps/") && r.url().includes("/launch")) result.launchRequests.push({ at: Date.now(), url: r.url() }); });
const api = (path, method = "GET", body = null, csrf = null) => page.evaluate(async ({ path, method, body, csrf }) => {
  const r = await fetch(path, { method, headers: { ...(body ? { "content-type": "application/json" } : {}), ...(csrf ? { "X-CSRF-Token": csrf } : {}) }, body: body ? JSON.stringify(body) : undefined });
  return { status: r.status, data: await r.json().catch(() => null) };
}, { path, method, body, csrf });
try {
  await page.goto(`${baseUrl}/`, { waitUntil: "domcontentloaded" });
  await page.waitForSelector('.auth-form, input[name="username"], .taskbar');
  if (await page.locator('input[name="username"]').count()) {
    await page.fill('input[name="username"]', user);
    await page.fill('input[type="password"]', creds[user]);
    await page.click('.auth-form button[type="submit"], button:has-text("Sign in")');
  }
  const t0 = Date.now();
  await page.waitForSelector(".taskbar");
  result.taskbarMs = Date.now() - t0;
  let session = null;
  for (let i = 0; i < 240; i += 1) {
    session = await page.evaluate(() => fetch('/api/linux-apps').then((r) => r.json()).then((d) => d.session ?? null));
    if (session?.status === "ready") break;
    await page.waitForTimeout(250);
  }
  result.prewarmReadyMs = session?.status === "ready" ? Date.now() - t0 : null;
  result.session = session ? { status: session.status, backend: session.displayBackend, apps: (session.apps ?? []).map((a) => a.appId), viewers: session.viewers } : null;
  result.checks.prewarm_ready = session?.status === "ready";
  result.checks.deferred_no_iframe = await page.evaluate(() => document.querySelectorAll("iframe.linux-native-session-frame").length === 0);
  await page.locator(".taskbar-start").click();
  await page.waitForSelector(".start-menu");
  const search = page.locator(".start-menu .menu-search");
  if (await search.count()) { await search.fill("Chrome"); await page.waitForTimeout(300); }
  const clickAt = Date.now();
  await page.locator(".menu-app", { hasText: "Google Chrome" }).first().click();
  await page.waitForSelector(".linux-native-window", { timeout: 120_000 });
  result.clickToWindowMs = Date.now() - clickAt;
  result.checks.launch_requested = result.launchRequests.length > 0;
  result.launchRequestDelayMs = result.launchRequests[0] ? result.launchRequests[0].at - clickAt : null;
  await page.waitForTimeout(1500);
  result.sessionAfter = await page.evaluate(() => fetch('/api/linux-apps').then((r) => r.json()).then((d) => { const s = d.session ?? null; return s ? { status: s.status, apps: (s.apps ?? []).map((a) => a.appId), viewers: s.viewers, backend: s.displayBackend } : null; }));
  await page.screenshot({ path: "results/prewarm-clean-window.png" }).catch(() => {});
  const csrf = (await api("/api/auth/me")).data?.csrfToken;
  await api("/api/linux-apps/session/close", "POST", {}, csrf).catch(() => {});
} catch (error) { result.error = String(error?.stack ?? error).slice(0, 500); }
console.log(JSON.stringify(result, null, 2));
await context.close(); await browser.close();
