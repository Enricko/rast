// Build a Package Center app's window: one ES module (ui/app.js) and its
// styles (ui/app.css), from an entry whose default export is
// `{ mount(ctx, sdk) }`.
//
//   node scripts/build-app.mjs <entry.ts> <out dir>
//
// Imports of RAST's shared modules (`src/core/<name>`) become the SDK RAST
// lends the window (`globalThis.__RAST_SDK__.<name>`, see src/core/sdk.ts),
// so the app uses the desktop's own copies. Only SDK v1 modules may be
// imported (type-only imports, like core/app, vanish when compiled).
import { build } from "esbuild";
import { readFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const here = dirname(fileURLToPath(import.meta.url));
const core = resolve(here, "../src/core");
const sdk = JSON.parse(readFileSync(join(core, "sdk-v1.json"), "utf8"));
const [entry, outDir] = process.argv.slice(2);
if (!entry || !outDir) {
  console.error("usage: node scripts/build-app.mjs <entry.ts> <out dir>");
  process.exit(2);
}

const lendSdk = {
  name: "rast-sdk",
  setup(build) {
    build.onResolve({ filter: /\/core\/[a-z-]+$/ }, (args) => {
      const target = resolve(args.resolveDir, args.path);
      if (dirname(target) !== core) return undefined;
      const name = target.slice(core.length + 1);
      if (!(name in sdk)) {
        return { errors: [{ text: `core/${name} is not part of the window SDK v1 (${Object.keys(sdk).join(", ")})` }] };
      }
      return { path: name, namespace: "rast-sdk" };
    });
    build.onLoad({ filter: /.*/, namespace: "rast-sdk" }, (args) => ({
      contents: `module.exports = globalThis.__RAST_SDK__[${JSON.stringify(args.path)}];`,
      loader: "js",
    }));
  },
};

const result = await build({
  entryPoints: { app: resolve(entry) },
  bundle: true,
  format: "esm",
  target: "es2022",
  minify: true,
  sourcemap: false,
  legalComments: "none",
  outdir: resolve(outDir),
  plugins: [lendSdk],
  metafile: true,
  logLevel: "warning",
});
for (const [file, info] of Object.entries(result.metafile.outputs)) {
  console.log(`${file.split("/").slice(-2).join("/")}  ${(info.bytes / 1024).toFixed(1)} KB`);
}
