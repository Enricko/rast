// Leptos UI loader: load once, share the promise, retry after a failure.
import { test } from "node:test";
import assert from "node:assert/strict";
import { createRastUiLoader, RAST_UI_GLUE_URL, type RastUiModule } from "../src/core/wasm-ui";

function fakeModule(log: string[]): RastUiModule {
  return {
    default: async () => {
      log.push("init");
    },
    mount_about: () => ({ dispose: () => log.push("dispose") }),
  };
}

test("the glue is imported and initialised once, even for concurrent callers", async () => {
  const log: string[] = [];
  const urls: string[] = [];
  const loader = createRastUiLoader(async (url) => {
    urls.push(url);
    return fakeModule(log);
  });
  const [a, b] = await Promise.all([loader.load(), loader.load()]);
  assert.equal(a, b);
  await loader.load();
  assert.deepEqual(urls, [RAST_UI_GLUE_URL]);
  assert.deepEqual(log, ["init"]);
  assert.equal(RAST_UI_GLUE_URL, "/ui/rast-ui.js");
});

test("an import failure rejects and the next call retries", async () => {
  const log: string[] = [];
  let calls = 0;
  const loader = createRastUiLoader(async () => {
    calls += 1;
    if (calls === 1) throw new Error("HTTP 404");
    return fakeModule(log);
  });
  await assert.rejects(loader.load(), /HTTP 404/);
  const mod = await loader.load();
  assert.equal(calls, 2);
  assert.equal(typeof mod.mount_about(document_stub()).dispose, "function");
});

test("an init failure is not cached either", async () => {
  let inits = 0;
  const loader = createRastUiLoader(async () => ({
    default: async () => {
      inits += 1;
      if (inits === 1) throw new Error("CompileError: wasm disabled by CSP");
    },
    mount_about: () => ({ dispose() {} }),
  }));
  await assert.rejects(loader.load(), /CompileError/);
  await loader.load();
  assert.equal(inits, 2);
});

function document_stub(): HTMLElement {
  return {} as HTMLElement;
}
