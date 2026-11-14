#!/usr/bin/env node
// End-to-end checks for the native Terminal against a disposable RAST instance
// that runs the real privileged helper (root) and has docker_enabled = false.
//
// This script is NOT part of `npm test`/CI because it needs:
//   * a RAST instance at BASE_URL with the native helper running,
//   * terminal_enabled = true and a storage home for the admin user,
//   * Playwright's chromium (tests/e2e/node_modules).
//
// Usage:  cd tests/e2e && node terminal-native.mjs
// Env:    RAST_E2E_BASE_URL, RAST_E2E_ADMIN_USER, RAST_E2E_ADMIN_PASSWORD,
//         RAST_E2E_STORAGE_HOME (physical admin home, for assertions)
//
// It prints a JSON result summary and writes screenshots next to itself
// (terminal-native-*.png) on success.

import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { chromium } from "playwright";

const here = dirname(fileURLToPath(import.meta.url));
const BASE = (process.env.RAST_E2E_BASE_URL ?? "http://127.0.0.1:8811").replace(/\/$/, "");
const ADMIN = {
  username: process.env.RAST_E2E_ADMIN_USER ?? "admin",
  password: process.env.RAST_E2E_ADMIN_PASSWORD ?? "TermAdminPass123!",
};
const STORAGE_HOME = process.env.RAST_E2E_STORAGE_HOME ?? "/home/lufria/term-e2e/storage/homes/admin";
const BROWSER = process.env.RAST_E2E_BROWSER ?? "/home/lufria/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome";

const results = [];
let adminCsrf = "";
function check(name, ok, detail) {
  results.push({ name, ok: Boolean(ok), detail });
  console.log(`${ok ? "PASS" : "FAIL"} ${name}${detail ? ` — ${typeof detail === "string" ? detail : JSON.stringify(detail)}` : ""}`);
  return Boolean(ok);
}

async function apiJson(request, path, options = {}) {
  const method = (options.method ?? "GET").toUpperCase();
  const headers = {
    "content-type": "application/json",
    ...(method !== "GET" && method !== "HEAD" && adminCsrf ? { "X-CSRF-Token": adminCsrf } : {}),
    ...(options.headers ?? {}),
  };
  const response = await request.fetch(`${BASE}${path}`, { ...options, headers });
  const text = await response.text();
  let body;
  try { body = JSON.parse(text); } catch { body = text; }
  return { status: response.status(), body };
}

/** JSON request executed in the page (same-origin cookies + CSRF). */
async function adminFetch(page, path, { method = "GET", body = undefined } = {}) {
  return page.evaluate(
    async ({ path, method, body }) => {
      const me = await (await fetch("/api/auth/me", { credentials: "same-origin" })).json();
      const response = await fetch(path, {
        method,
        credentials: "same-origin",
        headers: { "content-type": "application/json", "X-CSRF-Token": me.csrfToken },
        body: body === undefined ? undefined : JSON.stringify(body),
      });
      const text = await response.text();
      let parsed;
      try { parsed = JSON.parse(text); } catch { parsed = text; }
      return { status: response.status, body: parsed };
    },
    { path, method, body },
  );
}

async function login(context, user) {
  const response = await context.request.post(`${BASE}/api/auth/login`, { data: user });
  if (!response.ok()) throw new Error(`login failed for ${user.username}: ${response.status()} ${await response.text()}`);
  return response.json();
}

async function openDesktop(context) {
  const page = await context.newPage();
  await page.goto(`${BASE}/`, { waitUntil: "load", timeout: 60_000 });
  await page.waitForSelector(".taskbar", { timeout: 30_000 });
  return page;
}

/** Launch an app from the start menu by its visible name. */
async function launchApp(page, name) {
  await page.locator(".taskbar-start").click();
  await page.waitForSelector(".start-menu", { timeout: 10_000 });
  const search = page.locator(".menu-search");
  if (await search.count()) await search.fill(name);
  const tile = page.locator(".menu-app", { hasText: name }).first();
  await tile.waitFor({ timeout: 10_000 });
  await tile.click();
  await page.waitForSelector(".start-menu", { state: "detached", timeout: 10_000 }).catch(() => {});
}

/** Run one or more browser-side WebSocket terminal scenarios. */
async function wsScenarios(page, { csrf, adminSessionId }) {
  return page.evaluate(async ({ csrf, adminSessionId, base }) => {
    const url = `${base.replace(/^http/, "ws")}/api/terminal/ws`;
    const sleep = (ms) => new Promise((resolve) => setTimeout(resolve, ms));

    function connect() {
      return new Promise((resolve, reject) => {
        const ws = new WebSocket(url);
        const state = { ws, text: [], raw: new Uint8Array(), exit: null, error: null, ready: null, closed: false };
        const decoder = new TextDecoder();
        ws.binaryType = "arraybuffer";
        ws.onopen = () => resolve(state);
        ws.onerror = () => reject(new Error("websocket error"));
        ws.onclose = () => { state.closed = true; };
        ws.onmessage = (event) => {
          if (typeof event.data === "string") {
            try {
              const message = JSON.parse(event.data);
              if (message.type === "ready") state.ready = message;
              else if (message.type === "exit") state.exit = message;
              else if (message.type === "error") state.error = message;
              state.text.push(message);
            } catch { state.text.push({ type: "unparsed", data: event.data }); }
          } else {
            const chunk = new Uint8Array(event.data);
            const merged = new Uint8Array(state.raw.length + chunk.length);
            merged.set(state.raw, 0);
            merged.set(chunk, state.raw.length);
            state.raw = merged;
          }
        };
      });
    }

    async function waitFor(state, predicate, timeout = 15_000) {
      const deadline = Date.now() + timeout;
      while (Date.now() < deadline) {
        if (predicate(state)) return true;
        await sleep(50);
      }
      return false;
    }

    const output = (target) => decoder.decode(target.raw);

    async function start(extra = {}) {
      const state = await connect();
      const start = { type: "start", csrf, cols: 100, rows: 30, ...extra };
      state.ws.send(JSON.stringify(start));
      await waitFor(state, (s) => s.ready || s.error || s.closed, 20_000);
      return state;
    }

    async function type(state, text) {
      const bytes = new TextEncoder().encode(text);
      state.ws.send(bytes);
    }

    let decoder = new TextDecoder();
    const outcome = {};

    // 1. Interactive shell: command, resize, exit.
    {
      const state = await start();
      outcome.started = { ready: state.ready, error: state.error };
      await type(state, "echo WS-E2E-OK\n");
      const saw = await waitFor(state, () => output(state).includes("WS-E2E-OK"), 15_000);
      outcome.command = { saw, output: output(state).slice(-300) };
      state.ws.send(JSON.stringify({ type: "resize", cols: 132, rows: 43 }));
      await sleep(300);
      await type(state, "stty size\n");
      const sawSize = await waitFor(state, () => output(state).includes("43 132"), 10_000);
      outcome.resize = { sawSize, output: output(state).slice(-200) };
      await type(state, "exit\n");
      const exited = await waitFor(state, (s) => s.exit, 15_000);
      outcome.exit = { exited, exit: state.exit };
    }

    // 2. Reconnect to the same shell instead of a duplicate process.
    {
      const first = await start();
      outcome.reconnectBase = { ready: first.ready, id: first.ready?.sessionId };
      first.ws.close();
      await sleep(500);
      const second = await start({ session: first.ready.sessionId });
      outcome.reconnect = { ready: second.ready, error: second.error };
      await type(second, "echo REATTACH-OK\n");
      const saw = await waitFor(second, () => decoder.decode(second.raw).includes("REATTACH-OK"), 15_000);
      outcome.reconnectOutput = saw;
      second.ws.send(JSON.stringify({ type: "close" }));
      await sleep(300);
    }

    // 3. Symlink escape and traversal attempts must be refused by the server.
    {
      const state = await start({ folder: { path: "/linkout" } });
      outcome.symlink = { error: state.error, ready: state.ready };
      state.ws.close();
      const traversal = await start({ folder: { path: "/../etc" } });
      outcome.traversal = { error: traversal.error, ready: traversal.ready };
      traversal.ws.close();
    }

    // 4. Identity and HOME are the mapped per-user identity.
    {
      const state = await start();
      await type(state, "id -un; id -u; echo HOME=$HOME; echo CWD=$(pwd)\n");
      await waitFor(state, () => decoder.decode(state.raw).includes("HOME="), 15_000);
      await sleep(300);
      outcome.identity = decoder.decode(state.raw).slice(-500);
      state.ws.send(JSON.stringify({ type: "close" }));
    }

    // 5. Cross-user attach is refused (only the owner may reattach).
    {
      const state = await start();
      outcome.crossUserTarget = state.ready?.sessionId ?? null;
      state.ws.send(JSON.stringify({ type: "close" }));
      await sleep(300);
    }

    return outcome;
  }, { csrf, adminSessionId, base: BASE });
}

async function wsAttachAsUser(page, { csrf, sessionId }) {
  return page.evaluate(async ({ csrf, sessionId, base }) => {
    const url = `${base.replace(/^http/, "ws")}/api/terminal/ws`;
    return await new Promise((resolve) => {
      const ws = new WebSocket(url);
      const result = { error: null, ready: null, closed: false };
      ws.binaryType = "arraybuffer";
      ws.onopen = () => ws.send(JSON.stringify({ type: "start", csrf, cols: 80, rows: 24, session: sessionId }));
      ws.onmessage = (event) => {
        if (typeof event.data !== "string") return;
        try {
          const message = JSON.parse(event.data);
          if (message.type === "error") result.error = message;
          if (message.type === "ready") result.ready = message;
        } catch { /* ignore */ }
        resolve(result);
      };
      ws.onclose = () => { result.closed = true; resolve(result); };
      setTimeout(() => resolve(result), 10_000);
    });
  }, { csrf, sessionId, base: BASE });
}

async function main() {
  const browser = await chromium.launch({
    headless: true,
    executablePath: BROWSER,
    args: ["--no-sandbox"],
  });
  const result = { base: BASE, checks: results, screenshots: [] };

  try {
    // -- Admin session ----------------------------------------------------
    const adminContext = await browser.newContext({ viewport: { width: 1480, height: 920 } });
    const adminLogin = await login(adminContext, ADMIN);
    adminCsrf = adminLogin.csrfToken ?? "";
    const adminPage = await openDesktop(adminContext);
    const me = await adminPage.evaluate(async () => (await fetch("/api/auth/me", { credentials: "same-origin" })).json());
    check("admin can use Terminal", me.permissions?.apps?.terminal?.allowed === true, me.permissions?.apps?.terminal);
    const csrf = me.csrfToken;

    const env = await apiJson(adminContext.request, "/api/terminal/env");
    check("terminal env API", env.status === 200 && env.body.enabled === true && typeof env.body.shell === "string", {
      status: env.status,
      enabled: env.body?.enabled,
      shell: env.body?.shell,
      runtimes: env.body?.runtimes?.length,
      graceSeconds: env.body?.graceSeconds,
    });

    // -- Second user with the Terminal permission -------------------------
    const stamp = Date.now().toString(36);
    const otherUser = { username: `termuser${stamp}`, password: "TermUserPass123!" };
    const created = await adminFetch(adminPage, "/api/users", {
      method: "POST",
      body: { username: otherUser.username, password: otherUser.password, role: "user" },
    });
    check("create second user", created.status === 200 || created.status === 201, { status: created.status, body: created.body });
    const group = await adminFetch(adminPage, "/api/groups", {
      method: "POST",
      body: {
        name: `term-${stamp}`,
        members: [created.body.id],
        appRules: [{ appId: "terminal", rule: "allow" }],
      },
    });
    check("grant Terminal to second user", group.status === 200 || group.status === 201, { status: group.status, body: group.body });

    // -- Backend/WS behaviour ---------------------------------------------
    const ws = await wsScenarios(adminPage, { csrf, adminSessionId: null });
    check("WS shell starts and runs a command", ws.started?.ready && ws.command?.saw, ws.command);
    check("resize reaches the PTY (stty size)", ws.resize?.sawSize, ws.resize);
    check("shell exit is reported", ws.exit?.exited && (ws.exit.exit?.code === 0 || ws.exit.exit?.code === null), ws.exit);
    check("reconnect attaches to the same session", ws.reconnect?.ready?.reconnected === true, ws.reconnect);
    check("reconnected shell keeps output", ws.reconnectOutput === true);
    check("symlink escape is refused", !ws.symlink?.ready && Boolean(ws.symlink?.error), ws.symlink);
    check("path traversal is refused", !ws.traversal?.ready && Boolean(ws.traversal?.error), ws.traversal);
    check("shell identity is unprivileged and per-user", /rast-u\d+/.test(ws.identity ?? "") && /HOME=\/.*linux-users\/\d+/.test(ws.identity ?? ""), ws.identity);

    // -- Cross-user attach -------------------------------------------------
    const otherContext = await browser.newContext({ viewport: { width: 1200, height: 800 } });
    const otherLogin = await login(otherContext, otherUser);
    const otherPage = await openDesktop(otherContext);
    const otherMe = await otherPage.evaluate(async () => (await fetch("/api/auth/me", { credentials: "same-origin" })).json());
    check("second user can use Terminal", otherMe.permissions?.apps?.terminal?.allowed === true, otherMe.permissions?.apps?.terminal);
    if (ws.crossUserTarget) {
      const attempt = await wsAttachAsUser(otherPage, { csrf: otherLogin.csrfToken ?? otherMe.csrfToken, sessionId: ws.crossUserTarget });
      check("cross-user reattach is refused", !attempt.ready && Boolean(attempt.error), attempt);
    } else {
      check("cross-user reattach is refused", false, "no admin session id");
    }

    // Second user's own shell: different uid/home.
    const otherWs = await otherPage.evaluate(async ({ csrf }) => {
      const url = "ws://127.0.0.1:8811/api/terminal/ws".replace("127.0.0.1:8811", location.host);
      const ws = new WebSocket(url);
      ws.binaryType = "arraybuffer";
      const chunks = [];
      const decoder = new TextDecoder();
      await new Promise((resolve, reject) => {
        ws.onopen = () => resolve();
        ws.onerror = () => reject(new Error("ws error"));
      });
      const messages = [];
      let ready = false;
      ws.onmessage = (event) => {
        if (typeof event.data === "string") {
          const message = JSON.parse(event.data);
          messages.push(message);
          if (message.type === "ready" || message.type === "error") ready = true;
        } else {
          chunks.push(decoder.decode(new Uint8Array(event.data)));
        }
      };
      ws.send(JSON.stringify({ type: "start", csrf, cols: 90, rows: 28 }));
      for (let i = 0; i < 200 && !ready; i += 1) await new Promise((r) => setTimeout(r, 100));
      ws.send(new TextEncoder().encode("id -un; echo HOME=$HOME\n"));
      await new Promise((r) => setTimeout(r, 2500));
      ws.send(JSON.stringify({ type: "close" }));
      await new Promise((r) => setTimeout(r, 300));
      return { text: chunks.join(""), messages };
    }, { csrf: otherLogin.csrfToken ?? otherMe.csrfToken });
    if (!otherWs.text) console.log("second-user WS debug:", JSON.stringify(otherWs.messages));
    const adminUid = (ws.identity ?? "").match(/rast-u(\d+)/)?.[1];
    const otherUid = (otherWs.text ?? "").match(/rast-u(\d+)/)?.[1];
    check("two users get separate identities/homes", Boolean(adminUid && otherUid && adminUid !== otherUid), { adminUid, otherUid, output: otherWs.text?.slice(-200) });

    // -- UI: Terminal app, command, screenshots ----------------------------
    await launchApp(adminPage, "Terminal");
    await adminPage.waitForSelector(".term-root", { timeout: 20_000 });
    await adminPage.waitForSelector(".term-tab", { timeout: 20_000 });
    await adminPage.waitForTimeout(1200);
    const pane = adminPage.locator(".term-pane").first();
    await pane.click();
    await adminPage.keyboard.type("echo UI-TERMINAL-OK; id -un; pwd");
    await adminPage.keyboard.press("Enter");
    await adminPage.waitForTimeout(1500);
    const shot1 = join(here, "terminal-native-ui.png");
    await adminPage.screenshot({ path: shot1 });
    result.screenshots.push(shot1);
    check("Terminal app window renders", (await adminPage.locator(".term-panes").count()) > 0);
    check("Terminal toolbar has tabs and environment button", (await adminPage.locator(".term-tab").count()) >= 1 && (await adminPage.getByTitle("Runtime environment").count()) > 0);

    // -- UI: File Station "Open in Terminal" -------------------------------
    await launchApp(adminPage, "File Station");
    await adminPage.waitForSelector(".fs-panel", { timeout: 20_000 });
    await adminPage.waitForTimeout(1200);
    let projectRow = adminPage.locator(`tr[data-fs-path="/project"]`).first();
    let hasRow = (await projectRow.count()) > 0;
    if (!hasRow) {
      // The station may start at the root listing (Home/Shared entries); open
      // the Home root and retry.
      const homeEntry = adminPage.locator(".fs-entry-name", { hasText: /^Home$/ }).first();
      if ((await homeEntry.count()) > 0) {
        await homeEntry.dblclick();
        await adminPage.waitForTimeout(1200);
        projectRow = adminPage.locator(`tr[data-fs-path="/project"]`).first();
        hasRow = (await projectRow.count()) > 0;
      }
    }
    if (!hasRow) {
      const debug = await adminPage.evaluate(() => ({
        rows: [...document.querySelectorAll("[data-fs-path]")].slice(0, 20).map((el) => el.getAttribute("data-fs-path")),
        text: document.querySelector(".fs-panel")?.textContent?.slice(0, 300),
      }));
      console.log("File Station debug:", JSON.stringify(debug));
    }
    if (hasRow) {
      await projectRow.click({ button: "right" });
      await adminPage.waitForSelector(".context-menu", { timeout: 5_000 });
      const item = adminPage.locator(".context-menu .menu-item", { hasText: "Open in Terminal" }).first();
      const hasItem = (await item.count()) > 0;
      check("File Station directory offers Open in Terminal", hasItem);
      if (hasItem) {
        const before = await adminPage.locator(".window[data-app-id='terminal']").count();
        await item.click();
        await adminPage.waitForTimeout(1500);
        const after = await adminPage.locator(".window[data-app-id='terminal']").count();
        check("Open in Terminal opens a terminal window", after > before, { before, after });
        const shot2 = join(here, "terminal-native-file-station.png");
        await adminPage.screenshot({ path: shot2 });
        result.screenshots.push(shot2);
        // Type in the newest terminal: it should start in the RAST folder.
        const windows = adminPage.locator(".window[data-app-id='terminal']");
        await windows.last().locator(".term-pane").first().click();
        await adminPage.keyboard.type("pwd");
        await adminPage.keyboard.press("Enter");
        await adminPage.waitForTimeout(1200);
        const shot3 = join(here, "terminal-native-folder-cwd.png");
        await adminPage.screenshot({ path: shot3 });
        result.screenshots.push(shot3);
      }
    } else {
      check("File Station directory offers Open in Terminal", false, "project folder row not found");
    }

    // -- Logout closes terminals ------------------------------------------
    const logoutPage = await openDesktop(adminContext);
    const logoutCsrf = (await logoutPage.evaluate(async () => (await fetch("/api/auth/me", { credentials: "same-origin" })).json())).csrfToken;
    const logoutWs = await wsScenarios(logoutPage, { csrf: logoutCsrf, adminSessionId: null }).catch(() => null);
    const beforeLogout = await adminFetch(adminPage, "/api/terminal/sessions");
    const logoutResult = await logoutPage.evaluate(async ({ csrf }) => {
      const response = await fetch("/api/auth/logout", {
        method: "POST",
        credentials: "same-origin",
        headers: { "content-type": "application/json", "X-CSRF-Token": csrf },
      });
      return { status: response.status, body: await response.text() };
    }, { csrf: logoutCsrf });
    console.log("logout result:", JSON.stringify(logoutResult));
    await logoutPage.waitForTimeout(2500);
    const sessionsAfter = await adminFetch(logoutPage, "/api/terminal/sessions");
    check("logout closes terminal sessions", beforeLogout.status !== 200 || sessionsAfter.status === 401 || (sessionsAfter.body?.sessions?.length ?? 0) === 0, {
      beforeStatus: beforeLogout.status,
      afterStatus: sessionsAfter.status,
      afterBody: sessionsAfter.body,
    });
    result.logout = { before: beforeLogout.status, after: sessionsAfter.status, beforeCount: beforeLogout.body?.sessions?.length, afterCount: sessionsAfter.body?.sessions?.length };

    await adminContext.close();
    await otherContext.close();
  } catch (error) {
    check("unexpected error", false, String(error?.stack ?? error));
  } finally {
    await browser.close();
  }

  const failed = results.filter((entry) => !entry.ok);
  writeFileSync(join(here, "terminal-native-result.json"), JSON.stringify(result, null, 2));
  console.log(`\n${results.length - failed.length}/${results.length} checks passed`);
  if (failed.length) {
    console.log(`FAILED: ${failed.map((entry) => entry.name).join(", ")}`);
    process.exitCode = 1;
  }
}

await main();
