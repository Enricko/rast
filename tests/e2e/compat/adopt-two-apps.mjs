// Evidence for multi-window adoption: two native apps are launched through the
// API before the browser connects; then the RAST desktop must adopt both.
import { chromium } from "playwright";
import { readFileSync, writeFileSync } from "node:fs";
const base = "https://desktop.lufria.tech";
const creds = Object.fromEntries(readFileSync("/home/lufria/.config/rast-test/creds","utf8").split("\n").map(l=>l.trim()).filter(l=>l.includes("=")).map(l=>[l.slice(0,l.indexOf("=")),l.slice(l.indexOf("=")+1)]));
const user = "t-alice";
const browser = await chromium.launch({ headless: true, executablePath: "/home/lufria/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome", args: ["--no-sandbox","--disable-gpu","--disable-dev-shm-usage"] });
const context = await browser.newContext({ viewport:{width:1500,height:950}, ignoreHTTPSErrors:true });
const page = await context.newPage();
page.setDefaultTimeout(90000);
const out = { checks: {} };
const api = (path, method="GET", body=null, csrf=null) => page.evaluate(async ({path,method,body,csrf}) => {
  const r = await fetch(path,{method,headers:{...(body?{"content-type":"application/json"}:{}),...(csrf?{"X-CSRF-Token":csrf}:{})},body:body?JSON.stringify(body):undefined});
  return {status:r.status,data:await r.json().catch(()=>null)};
},{path,method,body,csrf});
try {
  await page.goto(base+"/", { waitUntil:"domcontentloaded" });
  await page.waitForSelector('.auth-form, input[name="username"], .taskbar');
  if (await page.locator('input[name="username"]').count()) { await page.fill('input[name="username"]',user); await page.fill('input[type="password"]',creds[user]); await page.click('.auth-form button[type="submit"], button:has-text("Sign in")'); }
  await page.waitForSelector(".taskbar");
  const csrf = (await api("/api/auth/me")).data?.csrfToken;
  await api("/api/linux-apps/session/close","POST",{},csrf).catch(()=>{});
  await page.waitForTimeout(3000);
  // Launch two known native apps through the API before any viewer exists.
  for (const appId of ["google-chrome", "artcraft-launcher"]) {
    const r = await api(`/api/linux-apps/${appId}/launch`, "POST", {}, csrf);
    out.checks[`launch_${appId}`] = r.status === 200;
  }
  let session = null;
  for (let i=0;i<120;i++) { session = (await api("/api/linux-apps")).data?.session ?? null; if (session?.status==="ready" && (session.apps??[]).length>=2) break; await page.waitForTimeout(500); }
  out.session = session ? { status: session.status, backend: session.displayBackend, apps: (session.apps??[]).map(a=>a.appId), display: session.display } : null;
  await page.reload({ waitUntil: "domcontentloaded" });
  await page.waitForSelector(".taskbar");
  let titles = [];
  for (let i=0;i<40;i++) {
    titles = await page.evaluate(() => Array.from(document.querySelectorAll(".linux-native-window")).map((el) => el.closest(".window")?.querySelector(".win-title, .win-tab-title")?.textContent?.trim() ?? ""));
    if (titles.length >= 2) break;
    await page.waitForTimeout(500);
  }
  out.checks.both_adopted = titles.length >= 2;
  out.adoptedTitles = titles;
  await page.screenshot({ path: "results/multi-window-adoption.png" }).catch(()=>{});
  await api("/api/linux-apps/session/close","POST",{},csrf).catch(()=>{});
} catch (error) { out.error = String(error?.stack ?? error).slice(0,400); }
writeFileSync("results/multi-window-adoption.json", JSON.stringify(out,null,2));
console.log(JSON.stringify(out,null,2));
await context.close(); await browser.close();
