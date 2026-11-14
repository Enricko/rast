#!/usr/bin/env node
// Measured terminal performance against a running RAST instance (manual).
// Reports: time from start message to ready (shell spawn + attach), input
// round-trip (keystroke echo → output seen), and bulk throughput.
//
// Usage: cd tests/e2e && TMPDIR=$HOME/.cache/rast-tests node terminal-bench.mjs
// Env: RAST_E2E_BASE_URL, RAST_E2E_ADMIN_USER, RAST_E2E_ADMIN_PASSWORD.

import { chromium } from "playwright";

const BASE = (process.env.RAST_E2E_BASE_URL ?? "http://127.0.0.1:8811").replace(/\/$/, "");
const user = process.env.RAST_E2E_ADMIN_USER ?? "admin";
const password = process.env.RAST_E2E_ADMIN_PASSWORD ?? "TermAdminPass123!";
const BROWSER = process.env.RAST_E2E_BROWSER ?? "/home/lufria/.cache/ms-playwright/chromium-1248/chrome-linux64/chrome";

const browser = await chromium.launch({ headless: true, executablePath: BROWSER, args: ["--no-sandbox"] });
const context = await browser.newContext();
await context.request.post(`${BASE}/api/auth/login`, { data: { username: user, password } });
const page = await context.newPage();
await page.goto(`${BASE}/`, { waitUntil: "load" });
await page.waitForSelector(".taskbar");

const result = await page.evaluate(async ({ base }) => {
  const url = `${base.replace(/^http/, "ws")}/api/terminal/ws`;
  const me = await (await fetch("/api/auth/me", { credentials: "same-origin" })).json();
  const csrf = me.csrfToken;
  const sleep = (ms) => new Promise((r) => setTimeout(r, ms));
  const enc = new TextEncoder();
  const dec = new TextDecoder();

  async function open() {
    const ws = new WebSocket(url);
    ws.binaryType = "arraybuffer";
    const state = { ws, text: "", messages: [] };
    ws.onmessage = (event) => {
      if (typeof event.data === "string") state.messages.push(JSON.parse(event.data));
      else state.text += dec.decode(new Uint8Array(event.data));
    };
    await new Promise((resolve, reject) => { ws.onopen = resolve; ws.onerror = () => reject(new Error("ws")); });
    return state;
  }
  async function waitFor(fn, timeout = 30_000) {
    const deadline = performance.now() + timeout;
    while (performance.now() < deadline) {
      if (fn()) return true;
      await sleep(10);
    }
    return false;
  }

  const starts = [];
  for (let i = 0; i < 3; i += 1) {
    const state = await open();
    const t0 = performance.now();
    state.ws.send(JSON.stringify({ type: "start", csrf, cols: 120, rows: 32 }));
    const ok = await waitFor(() => state.messages.some((m) => m.type === "ready"));
    starts.push(ok ? Math.round(performance.now() - t0) : null);
    state.ws.send(JSON.stringify({ type: "close" }));
    await sleep(150);
  }

  const state = await open();
  state.ws.send(JSON.stringify({ type: "start", csrf, cols: 120, rows: 32 }));
  await waitFor(() => state.messages.some((m) => m.type === "ready"));
  await sleep(300);
  // Input round-trip: type a marker and time until the shell output shows it.
  state.text = "";
  const tInput = performance.now();
  state.ws.send(enc.encode("echo RTT-MARKER\n"));
  await waitFor(() => state.text.includes("RTT-MARKER"));
  const inputRoundTrip = Math.round(performance.now() - tInput);

  // Bulk throughput: 200k lines (~1.2 MB) of shell output.
  state.text = "";
  const tBulk = performance.now();
  state.ws.send(enc.encode("seq 1 200000\n"));
  let bytes = 0;
  let previous = 0;
  await waitFor(() => {
    bytes = state.text.length;
    const done = state.text.includes("200000") && state.text.includes("$");
    return done;
  }, 60_000);
  const bulkMs = performance.now() - tBulk;
  const bulkBytes = state.text.length;
  const bulkBytesPerSec = Math.round((bulkBytes / Math.max(bulkMs, 1)) * 1000);
  state.ws.send(JSON.stringify({ type: "close" }));
  await sleep(200);
  return {
    startToReadyMs: starts,
    inputRoundTripMs: inputRoundTrip,
    bulkOutputBytes: bulkBytes,
    bulkOutputMs: Math.round(bulkMs),
    bulkBytesPerSec,
    note: "measurable wall-clock in headless Chromium against a local dev instance",
  };
}, { base: BASE });

console.log(JSON.stringify(result, null, 2));
await browser.close();
