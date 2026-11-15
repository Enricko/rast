# 0003. rast-ui: Leptos CSR, built with Trunk and embedded under /ui/

- Status: accepted
- Date: 2026-11-15

## Context

The remake moves the frontend from TypeScript to Leptos (Rust compiled to
WASM), app by app (`CLAUDE.md`, Phase 5). Until then Leptos components and
TypeScript apps must run side by side, inside the same RAST windows, in one
binary with no CDN and with the strict CSP of the web UI
(`default-src 'self'; script-src 'self'`). Compiling WebAssembly needs
`'wasm-unsafe-eval'` in `script-src` (or the broader `'unsafe-eval'`).

## Decision

- **Crate.** `crates/rast-ui` (Leptos 0.8, `csr`; `wasm-bindgen`, `web-sys`,
  `console_error_panic_hook`). It is in the workspace `members` but not in
  `default-members`: it only runs as WebAssembly. It still compiles and lints
  natively, so `cargo clippy/test --workspace` stay green.
- **Build.** Trunk 0.22 (`scripts/build-ui.sh`, `crates/rast-ui/Trunk.toml`)
  with the `wasm-release` profile and `wasm-opt -Oz`, into
  `crates/rast-ui/dist` (git-ignored). `public_url = "/ui/"`.
- **Stable file names.** `filehash = false`, so the output is always
  `rast-ui.js` (wasm-bindgen glue) and `rast-ui_bg.wasm`. The TypeScript
  loader (`frontend/src/core/wasm-ui.ts`) imports `/ui/rast-ui.js` with a
  runtime URL (`/* @vite-ignore */`), calls the default `init` once (the promise
  is cached; a failure is not) and then `mount_about(el)`. No manifest is
  needed. Because names are stable, the server answers `/ui/*` with
  `Cache-Control: no-cache` plus an ETag, so a new binary never pairs a cached
  glue file with a different `.wasm`.
- **Embedding.** A second `RustEmbed` in `crates/rast-server/src/server.rs`
  serves `crates/rast-ui/dist` under `/ui/`. Trunk's `index.html` is excluded
  (it has an inline loader the CSP would block and nothing needs it). An unknown
  `/ui/...` path is a plain 404, never the SPA shell. `.wasm` is served as
  `application/wasm`.
- **Release gate.** `crates/rast-server/build.rs` fails a release build when
  `crates/rast-ui/dist` is missing or the placeholder (marker
  `rast-placeholder-ui`); debug builds embed a placeholder so clippy and tests
  need no Trunk. CI (`ui` job) and the release workflow build the UI once with a
  pinned, checksummed Trunk binary and hand it to both architecture builds.
- **CSP.** Only the token `'wasm-unsafe-eval'` is added to `script-src`. It
  allows compiling and instantiating WebAssembly, but not `eval`, `new
  Function` or string timeouts. Scripts and modules are still limited to
  `'self'` (no inline, no external hosts), and WASM bytes can only come from
  our own origin (`connect-src 'self'`).
- **Mounting.** `mount_about(el)` returns a handle with `dispose()`; the window
  disposes it when it closes, which drops Leptos' unmount handle.
- **Size budget.** `crates/rast-ui/wasm-budget.txt` holds the maximum gzip size
  of `dist` (WASM + glue JS): 1.5 x the gzip size of the JS/CSS/HTML in
  `frontend/dist` at the start of the remake (baseline in
  `docs/remake/PROGRESS.md`). `scripts/check-wasm-size.sh` enforces it in CI.

## Consequences

- Release builds need Rust target `wasm32-unknown-unknown` and Trunk 0.22
  (packagers, CI). Installed servers need nothing new.
- A Leptos app costs one extra request pair (glue + wasm) the first time it is
  opened; the TypeScript bundle does not grow.
- The CSP is slightly wider for every page, by a capability that cannot run
  JavaScript. Revisit if a stricter alternative appears.
- wasm-bindgen CLI and crate versions must match; Trunk downloads the CLI
  version found in `Cargo.lock`.
- Every new Leptos app adds to the same `rast-ui.wasm`; if that grows too large,
  split by lazy-loaded modules in a later ADR.
