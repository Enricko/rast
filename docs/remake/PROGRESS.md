# Remake progress

Living log for the RAST remake (see `CLAUDE.md` and `docs/adr/0001-incremental-remake.md`).
Update it at the end of every session.

- **Current phase:** Phase 0 — Foundation
- **Branch / version:** `Remake` / 1.0.1 (not bumped per phase; releases are pre-release only until the final remake gate)
- **Last updated:** 2026-11-15

## Phase 0 checklist

- [x] Split the repo into the target workspace with pure moves (task 0.2, ADR 0002)
- [x] Create `rast-api` with `EntryDto` and `ListingDto` (from `crates/rast-base/src/storage/ops.rs`) and generate TypeScript types with `ts-rs` (task 0.3)
- [ ] Create `rast-ui` with one Leptos component mounted in a RAST window, built with Trunk, embedded with `rust-embed`
- [ ] Record the current compressed JS bundle size as the baseline
- [ ] Run the app matrix on 0.18.x and store results in `tests/e2e/results/`
- [x] Create `docs/remake/PROGRESS.md` and the first ADRs
- [ ] Fix the README: duplicated "Linux apps" row and stale status line
- [ ] **Exit:** CI green, still a single release binary, no behaviour change

## Done

- Remake instructions (`CLAUDE.md`) and prompt pack (`docs/remake/PROMPTS.md`).
- Versioning rule: the whole remake uses 1.0.1 on the `Remake` branch, not
  bumped per phase; GitHub releases from the branch must be pre-release until
  the final remake gate passes (commit `aeeec51`).
- Progress log and ADR template (`docs/adr/0000-template.md`,
  `docs/adr/0001-incremental-remake.md`).
- **Task 0.2, workspace split** (ADR 0002). Commits, in order:
  - `dedd14f` break module dependency cycles (no behaviour change)
  - `c949b1b` Cargo workspace; `--workspace` for clippy and test (CI, CLAUDE.md, CONTRIBUTING.md)
  - `962955f` release builds fail when the frontend is missing or is the placeholder page
  - `3f42f29` auth, access and the Docker proxy no longer name `AppState`
  - `fc8b3eb` `rast-base`, `1f3b138` `rast-apps`, `7663996` `rast-docker`,
    `a465e8c` `rast-helper`, `dba65bd` `rast-agent`, `33c9e7e` `rast-session`,
    `6e47aa8` `rast-server` (`server.rs` and its frontend embed now live there)
  - The root package `rast` stays at the repository root (binary, `update.rs`,
    Git revision `build.rs`, facade `pub use`); `tests/*.rs` are unchanged
    except `tests/event_kinds.rs`, which now also scans `crates/*/src`.
  - Path-only change approved by the maintainer: `frontend/tests/xpra-input.test.ts`
    reads `xpra_shim.js` from its new location.
- **Quality gate baseline** (start of 0.2, and unchanged after every crate move):
  - `cargo fmt --all -- --check`, `cargo clippy --locked --workspace --all-targets -- -D warnings`: clean
  - `cargo test --locked --workspace --all-targets`: 807 tests in total (750 + 33 + 15 + 7 + 2 at the start; the split across crates changes, the total does not), 0 failed
  - `npm --prefix frontend test`: 149 passed, 0 failed
  - `npm --prefix frontend run build`: OK (existing chunk-size warning above 600 kB)
  - `scripts/generate-third-party-licenses.sh --check`: up to date (no new external dependency)

- **Flaky tests stabilised** (`2b9b8a5`):
  - `restore_refuses_while_a_server_holds_the_instance_lock` moved, unchanged,
    into its own integration-test binary `tests/instance_lock.rs`, so no other
    test forks while it holds the `flock`. Restore spawns no processes, so no
    retry is needed.
  - `install_threads_the_visibility_to_the_new_native_app_row` waits for the
    job's terminal state through `GET /api/jobs/{id}` (30 s safety deadline)
    instead of polling the row for 2 s.
  - Independent check: `cargo test --locked --workspace --all-targets` 10 runs
    in a row, 807 passed and 0 failed in every run.
- **Task 0.3, `rast-api` + `ts-rs`** (`eed4876`):
  - `crates/rast-api` holds `EntryDto`, `ListingDto`, `BreadcrumbDto`,
    `QuotaDto` (JSON unchanged); no workspace dependencies; in `members` only.
    `rast-base` depends on it and re-exports the types; `EntryDto::new` is now
    the `EntryDtoExt` extension trait in `rast-base`.
  - `ts-rs` 12.0.1 (MIT; new transitive crates: `ts-rs-macros` MIT,
    `termcolor` Unlicense OR MIT) exports into `frontend/src/generated/` during
    `cargo test -p rast-api` (`TS_RS_EXPORT_DIR` in `.cargo/config.toml`).
    `i64` fields are typed `number`, `kind` stays `"file" | "dir"`.
  - `frontend/src/core/types.ts` (`FileEntry`, `Breadcrumb`, `Quota`,
    `FileListing`) and `frontend/src/apps/files/types.ts` use the generated
    types.
  - CI (`rust` job): fails when `frontend/src/generated` is stale after the
    tests, and runs `cargo check --target wasm32-unknown-unknown -p rast-api`.
- **Gates after 0.3:** fmt and clippy clean; `cargo test`: 811 passed
  (807 + 4 ts-rs export tests), 0 failed; npm test 149 passed; npm build OK;
  third-party licenses up to date (342 crates).

## In progress

- Nothing.

## Next

1. Phase 0.4 — `rast-ui` skeleton (needs approval for leptos, wasm-bindgen, web-sys, console_error_panic_hook).
2. Phase 0.5 — README fixes.
3. Phase 0.6 — baseline app matrix (ask before running the native instance).

## Open questions

- **`rast-core` (deferred in 0.2):** nothing in the code is pure enough for
  `wasm32` yet — `storage::safe_path` still uses `AppError` and the filesystem.
  Create the crate when the pure parts (logical paths, name validation, access
  rules) can be separated; see ADR 0002.
- **Follow-ups from ADR 0002:** move the root package to `crates/rast`; split
  `rast-base`; separate `native_apps`/`native_archive` (now in `rast-session`)
  from the session code, and the terminal manager from the PTY agent; drop the
  workspace dependencies that the root package no longer uses directly.
- **Leftover `.tmp*` folders in `/tmp`** (3,718 found on 2026-11-15, some of
  them from this host's earlier runs): find which test leaves them behind and
  fix it after 0.2. A full `/tmp` (tmpfs, 16 GB) made every SQLite test fail
  with `disk I/O error` and blocked the shell.
- **Timing-sensitive tests:** the two known flaky tests were fixed in
  `2b9b8a5` (see Done). Any other test that holds a `flock` while sibling tests
  fork processes has the same risk; keep such tests in their own binary.
- **ts-rs leaves stale files:** removing or renaming a type in `rast-api` does
  not delete its old `.ts` file, and the CI check does not catch that. Delete
  the file by hand when a type goes away (or add a clean step later).
- **`rast-api` is not in `default-members`** (wasm crate rule in
  `Cargo.toml`): a plain `cargo test` at the root does not regenerate the
  types; use `--workspace` (as the gates and CI do) or `-p rast-api`.
- Does `scripts/changelog-section.sh` accept the `## [1.0.1] - Unreleased`
  heading? Yes: it only reads the text inside the square brackets (checked by
  the maintainer).
- Which release is the cut-off for removing the legacy Docker/Selkies Linux-app
  backend (Phase 4)?
- Baseline JS bundle size (compressed `frontend/dist`) is not recorded yet.
- `yamllint` is not installed for the default `python3` on this host (a
  RAST-managed Python comes first in `PATH`), so the workflow files were not
  linted locally.

## Repository map

See `CLAUDE.md` (Repository map) and ADR 0002 for the crate layout.
