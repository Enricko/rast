import { createHash } from "node:crypto";
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { defineConfig, type Plugin } from "vite";

/**
 * Classic scripts from `public/` keep their name, and the server (and
 * Cloudflare in front of it) caches such files for hours. index.html is never
 * cached, so give each one a content-hash query: a changed script is a new URL.
 */
function versionPublicScripts(names: string[]): Plugin {
  return {
    name: "rast-version-public-scripts",
    transformIndexHtml(html) {
      let out = html;
      for (const name of names) {
        const file = fileURLToPath(new URL(`./public/${name}`, import.meta.url));
        const hash = createHash("sha256").update(readFileSync(file)).digest("hex").slice(0, 12);
        const tag = `src="/${name}"`;
        if (!out.includes(tag)) throw new Error(`index.html does not load /${name}`);
        out = out.replace(tag, `src="/${name}?v=${hash}"`);
      }
      return out;
    },
  };
}

// The built assets are embedded into the Rust binary and served from `/`.
// During development, `pnpm dev` proxies API calls to a locally running
// `cargo run -- serve` instance on port 8720.
export default defineConfig({
  base: "/",
  plugins: [versionPublicScripts(["boot-splash.js"])],
  // Keep `/*! … */` license notices of bundled libraries (xterm.js) in the
  // minified output.
  esbuild: {
    legalComments: "inline",
  },
  build: {
    outDir: "dist",
    emptyOutDir: true,
    assetsDir: "assets",
    target: "es2022",
    sourcemap: false,
    chunkSizeWarningLimit: 600,
  },
  server: {
    host: "127.0.0.1",
    port: 5273,
    proxy: {
      "/api": {
        target: "http://127.0.0.1:8720",
        changeOrigin: false,
      },
      "/healthz": {
        target: "http://127.0.0.1:8720",
        changeOrigin: false,
      },
    },
  },
});
