import { chromium } from "/home/lufria/RAST/tests/e2e/node_modules/playwright/index.mjs";
import { readFileSync } from "node:fs";
const base = "https://desktop.lufria.tech";
const args = process.argv.slice(2);
const user = (args.find((a)=>a.startsWith("--user="))?.split("=")[1]) || "t-bob";
const creds = Object.fromEntries(readFileSync("/home/lufria/.config/rast-test/creds","utf8").split("\n").map(l=>l.trim()).filter(l=>l.includes("=")).map(l=>[l.slice(0,l.indexOf("=")),l.slice(l.indexOf("=")+1)]));
const browser = await chromium.launch({ headless: true, executablePath: "/home/lufria/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome", args: ["--no-sandbox","--disable-gpu","--disable-dev-shm-usage"] });
const context = await browser.newContext({ viewport:{width:1400,height:900}, ignoreHTTPSErrors:true });
const page = await context.newPage();
page.setDefaultTimeout(90_000);
const result = { checks: {}, details: {} };
const marker = `PREWARM_TERM_${Date.now().toString(36)}`;
try {
  // start from no session so pre-warm is what creates it
  await page.goto(`${base}/`, { waitUntil: "domcontentloaded" });
  await page.waitForSelector('.auth-form, input[name="username"], .taskbar');
  if (await page.locator('input[name="username"]').count()) {
    await page.fill('input[name="username"]', user);
    await page.fill('input[type="password"]', creds[user]);
    await page.click('.auth-form button[type="submit"], button:has-text("Sign in")');
  }
  await page.waitForSelector(".taskbar");
  const csrf = (await page.evaluate(() => fetch('/api/auth/me').then(r=>r.json()))).csrfToken;
  const t0 = Date.now();
  await page.waitForSelector(".taskbar");
  let session = null;
  for (let i=0;i<360;i++) {
    session = await page.evaluate(() => fetch('/api/linux-apps').then(r=>r.json()).then(d=>d.session ?? null));
    if (session?.status === "ready") break;
    await page.waitForTimeout(250);
  }
  result.prewarmReadyMs = session?.status === "ready" ? Date.now() - t0 : null;
  result.user = user;
  result.details.session = session ? { status: session.status, backend: session.displayBackend, apps: (session.apps??[]).map(a=>a.appId), viewers: session.viewers } : null;
  result.checks.prewarm_session_ready = session?.status === "ready";
  result.checks.deferred_viewer_no_iframe = await page.evaluate(() => document.querySelectorAll("iframe.linux-native-session-frame").length === 0);
  result.checks.focus_outside_viewer = await page.evaluate(() => {
    const a = document.activeElement;
    return a && a.tagName !== "IFRAME" && !a.classList?.contains("linux-native-session-frame");
  });
  // Open the Terminal and type IMMEDIATELY (no click into the terminal body):
  // with a deferred viewer this must not be stolen by the hidden Xpra frame.
  await page.locator(".taskbar-start").click();
  await page.waitForSelector(".start-menu");
  const search = page.locator(".start-menu .menu-search");
  if (await search.count()) { await search.fill("Terminal"); await page.waitForTimeout(300); }
  await page.locator(".menu-app", { hasText: "Terminal" }).first().click();
  await page.waitForSelector(".xterm", { timeout: 45000 });
  await page.waitForTimeout(800);
  result.checks.no_iframe_after_terminal_open = await page.evaluate(() => document.querySelectorAll("iframe.linux-native-session-frame").length === 0);
  result.checks.active_not_viewer = await page.evaluate(() => {
    const active = document.activeElement;
    const insideViewer = document.querySelector("iframe.linux-native-session-frame")?.contains(active) ?? false;
    return !insideViewer;
  });
  await page.keyboard.type(`echo ${marker}`);
  await page.keyboard.press("Enter");
  let typed = false;
  for (let i=0;i<20;i++) {
    const text = await page.locator("body").innerText();
    if (text.includes(marker)) { typed = true; break; }
    await page.waitForTimeout(300);
  }
  result.checks.terminal_typing_without_click = typed;
  await page.screenshot({ path: "/home/lufria/RAST/tests/e2e/results/prewarm-terminal-5fd93b1.png" }).catch(()=>{});
  await page.evaluate(async (csrf) => { await fetch('/api/linux-apps/session/close',{method:'POST',headers:{'X-CSRF-Token':csrf}}); }, csrf).catch(()=>{});
} catch (error) { result.error = String(error?.stack ?? error).slice(0, 400); }
console.log(JSON.stringify(result, null, 2));
await context.close(); await browser.close();
