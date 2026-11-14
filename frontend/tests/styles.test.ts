import assert from "node:assert/strict";
import { readdirSync, readFileSync } from "node:fs";
import { join } from "node:path";
import test from "node:test";

// `npm test` runs from the `frontend/` directory; the bundles live in a temp
// dir, so resolve sources from the working directory instead of import.meta.url.
const stylesDir = join(process.cwd(), "src/styles");

// A missing closing brace silently nests every following rule inside the last
// selector. That once scoped the whole native-Linux-app styling (including the
// off-screen Xpra viewer size) under `.package-app-frame`, which shrank Xpra's
// virtual desktop to the browser's default iframe size and clamped pointer
// coordinates so clicks inside native application windows missed their target.
test("stylesheet blocks are balanced", () => {
  const files = readdirSync(stylesDir).filter((name) => name.endsWith(".css"));
  assert.ok(files.length > 0, "no stylesheets found");
  for (const name of files) {
    const css = readFileSync(join(stylesDir, name), "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
    const open = (css.match(/\{/g) ?? []).length;
    const close = (css.match(/\}/g) ?? []).length;
    assert.equal(open, close, `${name} has unbalanced CSS blocks (${open} open, ${close} close)`);
  }
});

test("the native session viewer is a top-level rule", () => {
  const apps = readFileSync(join(stylesDir, "apps.css"), "utf8").replace(/\/\*[\s\S]*?\*\//g, "");
  const selector = ".linux-native-session-frame";
  assert.ok(apps.includes(selector), "the native session viewer rule is missing");
  // A top-level rule starts at the beginning of a line with no preceding "& ".
  const topLevel = apps.split("\n").some((line) => line.trim().startsWith(`${selector} {`));
  assert.ok(topLevel, `${selector} must be a top-level rule, not nested inside another selector`);
});
