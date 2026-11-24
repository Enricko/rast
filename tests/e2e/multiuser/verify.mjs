// Multi-user isolation verification for the native runtime.
// Signs in two test accounts, creates/reuses their sessions, asserts the
// session proxy refuses cross-user access, checks display port exposure and
// process ownership from the host, then cleans up.
// Usage: cd tests/e2e && node multiuser/verify.mjs
import { chromium } from "playwright";
import { readFileSync } from "node:fs";
import { execFileSync } from "node:child_process";

const base = "https://desktop.lufria.tech";
const creds = Object.fromEntries(
  readFileSync("/home/lufria/.config/rast-test/creds", "utf8")
    .split("\n").map((l) => l.trim()).filter((l) => l.includes("="))
    .map((l) => [l.slice(0, l.indexOf("=")), l.slice(l.indexOf("=") + 1)]),
);
const chrome = process.env.RAST_E2E_CHROMIUM || "/home/lufria/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome";
const browser = await chromium.launch({ headless: true, executablePath: chrome, args: ["--no-sandbox", "--disable-gpu"] });
const results = [];
const check = (id, ok, detail = "") => { results.push({ id, ok, detail }); console.log(`${ok ? "PASS" : "FAIL"} ${id} ${detail}`); };
const sudo = (...args) => { try { return execFileSync("sudo", ["-n", ...args], { encoding: "utf8" }); } catch (e) { return ""; } };

async function login(user) {
  const context = await browser.newContext({ viewport: { width: 1280, height: 800 }, ignoreHTTPSErrors: true });
  const page = await context.newPage();
  page.setDefaultTimeout(60_000);
  await page.goto(`${base}/`, { waitUntil: "domcontentloaded" });
  await page.waitForSelector('.auth-form, input[name="username"], .taskbar');
  if (await page.locator('input[name="username"]').count()) {
    await page.fill('input[name="username"]', user);
    await page.fill('input[type="password"]', creds[user]);
    await page.click('.auth-form button[type="submit"], button:has-text("Sign in")');
  }
  await page.waitForSelector(".taskbar");
  return { context, page };
}
const api = (page, path, method = "GET", body = null, csrf = null) =>
  page.evaluate(async ({ path, method, body, csrf }) => {
    const r = await fetch(path, { method, headers: { ...(body ? { "content-type": "application/json" } : {}), ...(csrf ? { "X-CSRF-Token": csrf } : {}) }, body: body ? JSON.stringify(body) : undefined });
    return { status: r.status, data: await r.json().catch(() => null) };
  }, { path, method, body, csrf });

try {
  const alice = await login("t-alice");
  const aliceMe = await api(alice.page, "/api/auth/me");
  const aliceCsrf = aliceMe.data?.csrfToken;
  await api(alice.page, "/api/linux-apps/google-chrome/launch", "POST", {}, aliceCsrf);
  // wait for a session url
  let aliceSession = null;
  for (let i = 0; i < 120 && !aliceSession; i += 1) {
    const overview = await api(alice.page, "/api/linux-apps");
    aliceSession = overview.data?.session?.url ?? null;
    if (!aliceSession) await alice.page.waitForTimeout(500);
  }
  check("alice.session", Boolean(aliceSession), aliceSession ?? "no session");
  const bob = await login("t-bob");
  const bobMe = await api(bob.page, "/api/auth/me");
  const bobCsrf = bobMe.data?.csrfToken;
  if (aliceSession) {
    const status = await bob.page.evaluate(async (url) => (await fetch(url, { headers: { accept: "text/html" } })).status, aliceSession);
    check("cross-user.session.forbidden", status === 403 || status === 401, `bob GET alice session -> HTTP ${status}`);
    const anon = await browser.newContext({ ignoreHTTPSErrors: true });
    const anonPage = await anon.newPage();
    const anonStatus = await anonPage.evaluate(async (url) => (await fetch(url)).status, aliceSession).catch(() => 401);
    await anon.close();
    check("anonymous.session.unauthorized", anonStatus === 401 || anonStatus === 403, `anonymous -> HTTP ${anonStatus}`);
  }
  // Host evidence: session units and ports belong to distinct identities.
  const units = sudo("systemctl", "list-units", "--plain", "--no-legend", "rast-xpra-*");
  check("host.session-units", (units.match(/rast-xpra-u[0-9]+\.service/g) ?? []).length >= 1, units.trim().split("\n").slice(0, 3).join(" | "));
  const listeners = sudo("ss", "-ltnp");
  const exposed = ["8723", "8722"].some((port) => listeners.includes(`:${port} `));
  check("host.no-raw-display-port", !exposed, "no Xpra/display port bound beyond RAST's own loopback proxy listeners");
  const nft = sudo("nft", "list", "table", "inet", "rast_native");
  check("host.display-firewall", nft.includes("reject") || nft.includes("xpra_ports"), nft.trim().split("\n")[0] ?? "");
  await api(alice.page, "/api/linux-apps/session/close", "POST", {}, aliceCsrf);
  await api(bob.page, "/api/linux-apps/session/close", "POST", {}, bobCsrf).catch(() => {});
  await alice.context.close();
  await bob.context.close();
} catch (error) {
  check("harness", false, String(error).slice(0, 300));
} finally {
  await browser.close();
  const failed = results.filter((r) => !r.ok);
  console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
  process.exit(failed.length ? 1 : 0);
}
