import { defineConfig } from "vite";

// The built assets are embedded into the Rust binary and served from `/`.
// During development, `pnpm dev` proxies API calls to a locally running
// `cargo run -- serve` instance on port 8720.
export default defineConfig({
  base: "/",
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
