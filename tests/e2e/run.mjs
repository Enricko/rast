// RAST E2E scenario runner.
//
// Usage:
//   node tests/e2e/run.mjs --list
//   node tests/e2e/run.mjs --mode=docker --scenario=all
//   node tests/e2e/run.mjs --mode=native --scenario=a,c,d
//   node tests/e2e/run.mjs --mode=docker --scenario=a --headless=false
//
// Results are written to tests/e2e/results/<mode>-<timestamp>.json plus a
// markdown summary; screenshots go to tests/e2e/screenshots/<mode>/<scenario>/.
// Exit code 0 when every selected scenario passed (skips are reported but do
// not fail the run), 1 otherwise.

import { mkdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

// Instance selection must happen before any module reads its configuration:
// several dev instances can coexist on the host (docker on 8731, native on
// 8732), each with its own credentials.
function prescanInstanceRoot(argv) {
  for (let index = 0; index < argv.length; index += 1) {
    if (argv[index] === "--instance-root" && argv[index + 1]) return argv[index + 1];
    if (argv[index].startsWith("--instance-root=")) return argv[index].slice("--instance-root=".length);
  }
  return null;
}
const instanceRoot = prescanInstanceRoot(process.argv.slice(2));
if (instanceRoot) process.env.RAST_E2E_ROOT = instanceRoot;

const { modeFromInstance, config, RESULTS_DIR, SCREENSHOTS_DIR } = await import("./lib/config.mjs");
const { loginClients } = await import("./lib/rast-api.mjs");
const { launchBrowser, screenshot } = await import("./lib/browser.mjs");
const host = await import("./lib/host.mjs");
const { scenarios } = await import("./scenarios/index.mjs");

class SkipError extends Error {
  constructor(reason) {
    super(reason);
    this.name = "SkipError";
  }
}

function parseArgs(argv) {
  const args = {
    mode: null,
    only: null,
    headless: true,
    bail: false,
    timeoutMs: 20 * 60_000,
    list: false,
    verbose: false,
    instanceRoot: null,
  };
  for (const item of argv) {
    if (item === "--list") args.list = true;
    else if (item === "--bail") args.bail = true;
    else if (item === "--verbose") args.verbose = true;
    else if (item === "--headless=false") args.headless = false;
    else if (item.startsWith("--headless=")) args.headless = item.split("=")[1] !== "false";
    else if (item.startsWith("--mode=")) args.mode = item.slice("--mode=".length);
    else if (item.startsWith("--scenario=")) args.only = item.slice("--scenario=".length).split(",").map((s) => s.trim()).filter(Boolean);
    else if (item.startsWith("--only=")) args.only = item.slice("--only=".length).split(",").map((s) => s.trim()).filter(Boolean);
    else if (item.startsWith("--timeout=")) args.timeoutMs = Number(item.split("=")[1]) * 1000;
    else if (item === "--instance-root") args.instanceRoot = argv[argv.indexOf(item) + 1];
    else if (item.startsWith("--instance-root=")) args.instanceRoot = item.slice("--instance-root=".length);
    else if (item === "--base-url") args.baseUrl = argv[argv.indexOf(item) + 1];
    else if (item.startsWith("--base-url=")) args.baseUrl = item.slice("--base-url=".length);
    else {
      console.error(`unknown argument: ${item}`);
      process.exit(2);
    }
  }
  return args;
}

function timestamp() {
  return new Date().toISOString().replace(/[:.]/g, "-").replace("Z", "");
}

function statusIcon(status) {
  return status === "pass" ? "PASS" : status === "fail" ? "FAIL" : "SKIP";
}

function formatDuration(ms) {
  if (ms < 1000) return `${ms} ms`;
  if (ms < 60_000) return `${(ms / 1000).toFixed(1)} s`;
  return `${Math.floor(ms / 60_000)}m ${Math.round((ms % 60_000) / 1000)}s`;
}

function renderMatrix(results, mode) {
  const lines = [
    `# RAST E2E scenario matrix — ${mode} backend`,
    "",
    `Generated ${new Date().toISOString()} against ${config.baseUrl}`,
    "",
    "| Scenario | Result | Duration | Evidence | Notes |",
    "|---|---|---|---|---|",
  ];
  for (const result of results) {
    const evidence = result.evidence.length
      ? result.evidence.map((item) => (item.startsWith("/") ? `\`${item}\`` : item)).join("<br>")
      : "";
    const notes = result.error ? String(result.error).replace(/\|/g, "\\|").slice(0, 300) : (result.details ?? []).join("<br>").replace(/\|/g, "\\|").slice(0, 300);
    lines.push(
      `| ${result.id} — ${result.title.replace(/\|/g, "\\|")} | **${statusIcon(result.status)}** | ${formatDuration(
        result.durationMs,
      )} | ${evidence} | ${notes} |`,
    );
  }
  const passed = results.filter((result) => result.status === "pass").length;
  const failed = results.filter((result) => result.status === "fail").length;
  const skipped = results.filter((result) => result.status === "skip").length;
  lines.push("", `Totals: ${passed} passed, ${failed} failed, ${skipped} skipped.`, "");
  return lines.join("\n");
}

async function main() {
  const args = parseArgs(process.argv.slice(2));
  if (args.baseUrl) config.baseUrl = args.baseUrl.replace(/\/$/, "");
  if (args.instanceRoot) config.instanceRoot = args.instanceRoot;

  if (args.list) {
    for (const scenario of scenarios) {
      console.log(`${scenario.id}\t${scenario.modes.join(",")}\t${scenario.title}`);
    }
    return 0;
  }

  const mode = args.mode || modeFromInstance();
  if (!["docker", "native"].includes(mode)) {
    console.error(`invalid mode: ${mode} (docker|native)`);
    return 2;
  }
  if (!config.admin.password) {
    console.error(
      "No credentials in tests/e2e/.env.local — start the dev instance first:\n" +
        "  scripts/e2e/dev-instance.sh up docker|native",
    );
    return 2;
  }

  const selected = args.only
    ? scenarios.filter((scenario) => args.only.includes(scenario.id))
    : scenarios.filter((scenario) => scenario.modes.includes(mode));
  if (selected.length === 0) {
    console.error(`no scenarios selected (ids: ${scenarios.map((scenario) => scenario.id).join(", ")})`);
    return 2;
  }

  const stamp = timestamp();
  const runDir = join(RESULTS_DIR, `${mode}-${stamp}`);
  const shotRoot = join(SCREENSHOTS_DIR, mode);
  mkdirSync(runDir, { recursive: true });
  mkdirSync(shotRoot, { recursive: true });

  console.log(`\nRAST E2E — mode ${mode}, ${selected.length} scenario(s)`);
  console.log(`server: ${config.baseUrl}`);
  console.log(`results: ${runDir}\n`);

  const clients = await loginClients(config);
  let browser = await launchBrowser({ headless: args.headless });

  const shared = {};
  const results = [];
  let bail = false;

  for (const scenario of selected) {
    if (bail) break;
    // Chromium can die under host pressure or be left behind by a crashed
    // scenario; a fresh browser per scenario keeps later scenarios meaningful.
    if (!browser.isConnected()) {
      console.log("browser disconnected — relaunching");
      browser = await launchBrowser({ headless: args.headless });
    }
    const shotDir = join(shotRoot, scenario.id);
    mkdirSync(shotDir, { recursive: true });
    const result = {
      id: scenario.id,
      title: scenario.title,
      mode,
      status: "pass",
      startedAt: new Date().toISOString(),
      durationMs: 0,
      details: [],
      evidence: [],
      metrics: {},
      screenshots: [],
      error: null,
    };
    const started = Date.now();
    process.stdout.write(`[${scenario.id}] ${scenario.title} ... `);
    const ctx = {
      mode,
      config,
      shared,
      admin: clients.admin,
      users: clients.users,
      user: (index) => clients.users[index],
      browser,
      host,
      fixtures: config.fixtures,
      runDir,
      shotDir,
      log: (...parts) => {
        const text = parts.map((part) => (typeof part === "string" ? part : JSON.stringify(part))).join(" ");
        if (args.verbose) console.log(`\n    ${text}`);
        result.details.push(text);
      },
      record: (text) => result.details.push(text),
      metric: (name, value, unit = "") => {
        result.metrics[name] = { value, unit };
      },
      screenshot: async (page, name) => {
        const path = await screenshot(page, name, { dir: shotDir });
        result.screenshots.push(path);
        result.evidence.push(path);
        return path;
      },
      skip: (reason) => {
        throw new SkipError(reason);
      },
      sleep: host.sleep,
    };
    try {
      const work = scenario.run(ctx);
      let timer;
      const timeout = new Promise((_, reject) => {
        timer = setTimeout(
          () => reject(new Error(`scenario timed out after ${scenario.timeoutMs ?? args.timeoutMs} ms`)),
          scenario.timeoutMs ?? args.timeoutMs,
        );
      });
      await Promise.race([work, timeout]).finally(() => clearTimeout(timer));
      result.status = "pass";
    } catch (error) {
      if (error instanceof SkipError) {
        result.status = "skip";
        result.details.push(`skipped: ${error.message}`);
      } else {
        result.status = "fail";
        result.error = error?.stack ?? String(error);
        if (bail) bail = true;
      }
    }
    // Close whatever a failed scenario left open (contexts, pages).
    for (const stale of browser.isConnected() ? browser.contexts() : []) {
      await stale.close().catch(() => {});
    }
    result.durationMs = Date.now() - started;
    results.push(result);
    console.log(
      `${statusIcon(result.status)} (${formatDuration(result.durationMs)})${result.error ? `\n    ${String(result.error).split("\n")[0]}` : ""}`,
    );
    if (result.error) console.log(String(result.error).split("\n").slice(0, 6).map((line) => `    ${line}`).join("\n"));
  }

  await browser.close();

  const runJson = {
    mode,
    baseUrl: config.baseUrl,
    startedAt: results[0]?.startedAt ?? new Date().toISOString(),
    finishedAt: new Date().toISOString(),
    scenarios: results,
  };
  writeFileSync(join(runDir, "results.json"), JSON.stringify(runJson, null, 2));
  const matrix = renderMatrix(results, mode);
  writeFileSync(join(runDir, "matrix.md"), matrix + "\n");
  writeFileSync(join(RESULTS_DIR, `${mode}-latest.json`), JSON.stringify(runJson, null, 2));
  writeFileSync(join(RESULTS_DIR, `${mode}-latest.md`), matrix + "\n");
  console.log(`\n${matrix}\n`);
  console.log(`results written to ${runDir}`);

  const failed = results.filter((result) => result.status === "fail").length;
  return failed === 0 ? 0 : 1;
}

main()
  .then((code) => process.exit(code))
  .catch((error) => {
    console.error("runner crashed:", error);
    process.exit(2);
  });
