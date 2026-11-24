// Real Terminal smoke test: signs in through the form, opens the built-in
// Terminal, runs a command and reads its output from the xterm DOM.
// Usage: node tests/e2e/terminal/smoke.mjs [--user=t-admin] [--base-url=https://desktop.lufria.tech]
import { chromium } from "playwright";
import { readFileSync } from "node:fs";

const arg = (name, fallback) => {
  const found = process.argv.find((item) => item.startsWith(`--${name}=`));
  return found ? found.split("=").slice(1).join("=") : fallback;
};
const baseUrl = (arg("base-url", process.env.RAST_BASE_URL || "https://desktop.lufria.tech")).replace(/\/$/, "");
const user = arg("user", "t-admin");
const credsPath = process.env.RAST_CREDS || "/home/lufria/.config/rast-test/creds";
const creds = Object.fromEntries(
  readFileSync(credsPath, "utf8")
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line && line.includes("="))
    .map((line) => [line.slice(0, line.indexOf("=")), line.slice(line.indexOf("=") + 1)]),
);
const password = creds[user];
if (!password) {
  console.error(`no credentials for ${user} in ${credsPath}`);
  process.exit(2);
}
const marker = `RAST_TERMINAL_OK_${Date.now().toString(36)}`;
const executable =
  process.env.RAST_E2E_CHROMIUM ||
  "/home/lufria/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome";
// Screenshots must be written next to this file whatever the working directory.
const shotPath = (name) =>
  new URL(`./${name}`, import.meta.url).pathname;
const browser = await chromium.launch({
  headless: true,
  executablePath: executable,
  args: ["--no-sandbox", "--disable-gpu", "--disable-dev-shm-usage"],
});
let ok = false;
const context = await browser.newContext({ viewport: { width: 1480, height: 920 }, ignoreHTTPSErrors: true });
const page = await context.newPage();
page.setDefaultTimeout(30_000);
try {
  await page.goto(`${baseUrl}/`, { waitUntil: "domcontentloaded" });
  await page.waitForSelector('.auth-form, input[name="username"], .taskbar', { timeout: 30_000 });
  if (await page.locator('input[name="username"]').count()) {
    await page.fill('input[name="username"]', user);
    await page.fill('input[type="password"]', password);
    await page.click('.auth-form button[type="submit"], button:has-text("Sign in")');
  }
  await page.waitForSelector(".taskbar", { timeout: 30_000 });
  // Open Terminal from the start menu (Applications).
  await page.locator(".taskbar-start").click();
  await page.waitForSelector(".start-menu", { timeout: 15_000 });
  const menu = page.locator(".start-menu");
  const search = menu.locator(".menu-search");
  if (await search.count()) {
    await search.fill("Terminal");
    await page.waitForTimeout(300);
  }
  const tile = menu.locator(".menu-app", { hasText: "Terminal" }).first();
  await tile.waitFor({ timeout: 20_000 });
  await tile.click();
  await page.waitForSelector(".xterm, .terminal, .win-body canvas", { timeout: 45_000 });
  const terminal = page.locator(".xterm").first();
  if (await terminal.count()) {
    await terminal.click();
    await page.waitForTimeout(1500);
    // The Terminal app owns a real xterm.js textarea; make sure keyboard input
    // reaches it (the desktop must not leave focus in the hidden Xpra frame).
    const focused = await page.evaluate(() =>
      document.activeElement?.classList?.contains("xterm-helper-textarea") ?? false,
    );
    if (!focused) {
      await page.locator(".xterm-helper-textarea").first().click({ force: true }).catch(() => {});
      const active = await page.evaluate(() => document.activeElement?.className ?? "");
      if (!String(active).includes("xterm-helper-textarea")) {
        console.error(`Terminal input is not focused (active=${active})`);
      }
    }
    await page.keyboard.type(`echo ${marker}`);
    await page.keyboard.press("Enter");
    await page.waitForTimeout(2500);
    const text = await page.locator("body").innerText();
    ok = text.includes(marker);
    if (!ok) {
      const shot = shotPath(`smoke-failure-${Date.now()}.png`);
      await page.screenshot({ path: shot, fullPage: false }).catch(() => {});
      const text = await page.locator("body").innerText().catch(() => "");
      console.error(`Terminal output did not contain ${marker}; screenshot: ${shot}`);
      console.error(`page text (last 800 chars): ${text.slice(-800)}`);
    }
  } else {
    console.error("no .xterm element found after opening Terminal");
  }
} catch (error) {
  console.error(`terminal smoke failed: ${error?.stack ?? error}`);
  await page.screenshot({ path: shotPath(`smoke-failure-${Date.now()}.png`) }).catch(() => {});
} finally {
  await context.close();
  await browser.close();
}
console.log(ok ? "TERMINAL SMOKE PASS" : "TERMINAL SMOKE FAIL");
process.exit(ok ? 0 : 1);
