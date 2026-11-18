# 0002. Workspace layout

- Status: accepted
- Date: 2026-11-15

## Context

RAST was one crate (`src/`, about 100k lines). The remake needs separate
crates (`CLAUDE.md`, Target architecture) so that the pure parts can reach
`wasm32` and the privileged, per-user and server parts stop depending on each
other by accident.

The modules were not layered. Besides the direct cycles (`native` and
`packages`, `config` and `packages`, `rootfs` and `api`, `docker` and
`worker`), `AppState` was named by the auth and access middleware, by the
legacy Docker proxy and by the native proxy, and `state` names almost every
subsystem. A move that only relocates files could not compile.

Constraints for task 0.2: move code without changing behaviour; keep one
binary named `rast` at `target/release/rast`; keep the library crate name
`rast` so `tests/*.rs` are unchanged; keep every quality gate green at every
commit; keep the total test count (807 at the start).

## Decision

### Crates

| Crate | Modules |
| --- | --- |
| `rast-api` | `files` (File Station DTOs with generated TypeScript); no workspace dependencies, builds for wasm32 |
| `rast-base` | `error`, `config`, `db` (and the embedded migrations), `events`, `secrets`, `users`, `auth`, `access`, `audit`, `jobs`, `storage`, `realtime`, `appearance`, plus `NAME`, `VERSION`, `DESCRIPTION` |
| `rast-docker` | `docker`, `worker` |
| `rast-apps` | `packages` |
| `rast-helper` | `native` |
| `rast-agent` | `rootfs`, `terminal` |
| `rast-session` | `linux_apps` |
| `rast-server` | `api`, `backup`, `monitor`, `state`, `server` (frontend embed), `native_proxy` (with `xpra_shim.js`) |
| `rast` (repository root) | `main.rs`, `update.rs`, `build.rs` (Git revision), and `pub use` of every module path |

Dependency order: `rast-base` < `rast-docker` < `rast-apps` < `rast-helper` <
`rast-agent` < `rast-session` < `rast-server` < `rast`.

Modules keep their names and their file layout inside the crate, and each
crate root re-imports the modules of lower crates (`use rast_base::config;`),
so `crate::config::Config` still resolves everywhere. The root crate
re-exports every module, so `rast::config::Config` and the other paths used
by `main.rs` and `tests/*.rs` are unchanged.

### Deviations from the target layout

1. **`rast-base` is new.** The target has no foundation crate, but session,
   apps and agent code already uses auth, db, config, events, jobs and
   storage, and the server holds them all in `AppState`. Putting those in
   `rast-server` makes the graph cyclic. `rast-base` is the one place they can
   live. *Plan to split it:* once Phase 1 and 2 settle the interfaces, move
   `storage` and its access rules into `rast-core` (the parts that are pure),
   `auth` and `users` into their own crate, and leave `rast-base` with `error`,
   `config`, `db`, `events`, `secrets` and `jobs`. Each split is a pure move.
2. **The `rast` package stays at the repository root** instead of
   `crates/rast`. `build.rs` (Git revision),
   `frontend/dist`, `migrations/`, `tests/` and every script and workflow that
   expects `target/release/rast` stay valid. Moving it is a later, mechanical
   step.
3. **`rast-core` and `rast-api` are not created in 0.2.** `rast-api` is task
   0.3. `rast-core` waits until there is something pure to put in it: the
   logical-path code in `storage::safe_path` still uses `AppError` and the
   filesystem. (Tracked in `PROGRESS.md`.)
4. **Whole modules move, files are not split between crates.** So `rast-session`
   keeps the legacy Docker app code (`linux_apps/mod.rs`, `proxy.rs`),
   `native_apps.rs` and `native_archive.rs` stay with `native_session.rs`
   instead of `rast-apps`, and `terminal/manager.rs` stays with the PTY agent
   in `rast-agent`. `native_proxy.rs` is an Axum handler over `AppState` and
   lives in `rast-server`, not in `rast-session`; the same goes for the static file
   handler, because `api::router` uses it.

### Cycle breakers (no behaviour change)

- `OFFICIAL_CATALOG_URL`, `TrustedKey` and `parse_trusted_key` live in
  `config`; `packages::catalog` re-exports them.
- `RESERVED_NAME` and `validate_logical_path` live in `storage::safe_path`.
  The file streaming code (`Range`, `Content-Disposition`, preview CSP) moved
  from `api::files` to `storage::serve`, because the root file agent needs it.
- `validate_slug`, `MAX_SLUG_LEN` and `validate_app_id` live in `access`.
  `access::app_surface` lost an unused `AppState` parameter.
- `packages` reaches the helper through a `Helper` trait that `NativeClient`
  implements, so `packages` does not depend on `native` (and `native` may use
  `packages::accounts`).
- `LOGIN_SHELL_ENV` is defined in `native`.
- `AuthState` (db, config, secrets) and `HasLinuxApps` are traits implemented
  by `AppState`; the auth extractors and middleware, the access middleware and
  the legacy Docker proxy are generic over them.
- Test fixtures shared across crates sit behind `rast-base`'s `test-support`
  feature (`appearance::fixtures`).

### Paths that depend on the crate location

- The frontend embed (`server.rs`, `#[folder = "../../frontend/dist/"]`) lives
  in `rast-server`, together with the `build.rs` that creates the stand-in page
  when `frontend/dist` is missing. The release profile now fails if
  `frontend/dist/index.html` is missing or is that stand-in page (it carries
  the marker `rast-placeholder-frontend`); CI, the release workflow and
  `scripts/build-release.sh` also grep the release binary for it. The root
  `build.rs` only sets `RAST_GIT_SHA`, which `main.rs` and `update.rs` read.
- `sqlx::migrate!("../../migrations")` in `rast-base`, with a `build.rs`
  printing `cargo:rerun-if-changed=../../migrations`, because the macro does
  not always notice a migration that was only added. `migrations/` itself does
  not move.
- `include_str!` paths (`xpra_shim.js`, `shim.js`, `worker/linux_assets/`) are
  relative to the file and move with it.
- Versions and dependencies are shared through `[workspace.package]` and
  `[workspace.dependencies]`; the literal version stays in the root manifest,
  where `release.yml` and the scripts read it.
- `default-members` lists every crate that compiles natively, `rast-api`
  included (it also builds for `wasm32`, which CI checks separately), and CI
  and `CLAUDE.md` pass `--workspace` to `clippy` and `test`, so no crate is
  skipped. Only wasm-only crates such as `rast-ui` go in `members` only. A
  plain `cargo test` at the root therefore regenerates the TypeScript types.
- `tests/event_kinds.rs` scans `crates/*/src` as well as `src`.

## Consequences

- Each crate compiles and tests on its own; a change in `rast-base` rebuilds
  everything above it, so the base should stay small and stable.
- `pub(crate)` items used across crates became `pub`.
- The facade in `src/lib.rs` is temporary scaffolding for `tests/*.rs` and
  `main.rs`; new code should import from the owning crate.
- `native_proxy` and the legacy proxy still need `AppState`-shaped traits until
  the Xpra adapter (Phase 4) and the Docker backend removal change them.
- Moving the root package to `crates/rast`, splitting `rast-base` and
  separating `native_apps`/`native_archive` and the terminal manager are open
  follow-ups.
