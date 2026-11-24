// After the compat harness launches several native apps into one t-alice
// session, verify the RAST desktop adopted each of their X11 windows.
import { chromium } from "playwright";
import { readFileSync, writeFileSync } from "node:fs";
const base = "https://desktop.lufria.tech";
const creds = Object.fromEntries(readFileSync("/home/lufria/.config/rast-test/creds","utf8").split("\n").map(l=>l.trim()).filter(l=>l.includes("=")).map(l=>[l.slice(0,l.indexOf("=")),l.slice(l.indexOf("=")+1)]));
const user = process.argv[2] || "t-alice";
const out = process.argv[3] || "/tmp/rast-adopt-count.json";
const browser = await chromium.launch({ headless: true, executablePath: "/home/lufria/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome", args: ["--no-sandbox","--disable-gpu","--disable-dev-shm-usage"] });
const context = await browser.newContext({ viewport: { width: 1500, height: 950 }, ignoreHTTPSErrors: true });
const page = await context.newPage();
page.setDefaultTimeout(90000);
const result = {};
try {
  await page.goto(base + "/", { waitUntil: "domcontentloaded" });
  await page.waitForSelector('.auth-form, input[name="username"], .taskbar');
  if (await page.locator('input[name="username"]').count()) {
    await page.fill('input[name="username"]', user);
    await page.fill('input[type="password"]', creds[user]);
    await page.click('.auth-form button[type="submit"], button:has-text("Sign in")');
  }
  await page.waitForSelector(".taskbar");
  const session = await page.evaluate(() => fetch('/api/linux-apps').then(r=>r.json()).then(d=>d.session ?? null));
  result.sessionBackend = session?.displayBackend;
  // Wait for the adopted native windows to appear (bounded).
  let titles = [];
  for (let i = 0; i < 60; i++) {
    titles = await page.evaluate(() =>
      Array.from(document.querySelectorAll(".linux-native-window")).map((el) => {
        const win = el.closest(".window");
        const title = win?.querySelector(".win-title, .win-tab-title")?.textContent?.trim() ?? "";
        return title;
      }),
    );
    if (titles.length > 0 && await page.evaluate(() => !document.querySelector(".win-body .starting, .window .starting"))) break;
    await page.waitForTimeout(500);
  }
  result.adoptedWindows = titles.length;
  result.titles = titles;
  await page.screenshot({ path: "tests/e2e/results/compat-adopted-windows.png", fullPage: false }).catch(()=>{});
} catch (error) { result.error = String(error?.stack ?? error).slice(0, 400); }
writeFileSync(out, JSON.stringify(result, null, 2));
console.log(JSON.stringify(result, null, 2));
await context.close(); await browser.close();
