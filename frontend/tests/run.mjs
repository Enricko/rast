// Frontend unit tests: bundle each `tests/*.test.ts` with esbuild (already used
// by Vite) and run the bundles with Node's built-in test runner. Tests import
// pure modules from `src/` and must not need a browser.
import { build } from "esbuild";
import { mkdtempSync, readdirSync, rmSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join } from "node:path";
import { spawnSync } from "node:child_process";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const tests = readdirSync(here).filter((name) => name.endsWith(".test.ts"));
const out = mkdtempSync(join(tmpdir(), "rast-frontend-tests-"));
try {
  const bundles = [];
  for (const name of tests) {
    const outfile = join(out, name.replace(/\.ts$/, ".mjs"));
    await build({
      entryPoints: [join(here, name)],
      bundle: true,
      format: "esm",
      platform: "node",
      target: "node20",
      outfile,
      logLevel: "warning",
    });
    bundles.push(outfile);
  }
  const result = spawnSync(process.execPath, ["--test", ...bundles], { stdio: "inherit" });
  process.exitCode = result.status ?? 1;
} finally {
  rmSync(out, { recursive: true, force: true });
}
