# Remake progress

Living log for the RAST remake (see `CLAUDE.md` and `docs/adr/0001-incremental-remake.md`).
Update it at the end of every session.

- **Current phase:** Phase 0 — Foundation
- **Branch / version:** `Remake` / 1.0.1 (not bumped per phase; releases are pre-release only until the final remake gate)
- **Last updated:** 2026-11-16

## Phase 0 checklist

- [x] Split the repo into the target workspace with pure moves (task 0.2, ADR 0002)
- [x] Create `rast-api` with `EntryDto` and `ListingDto` (from `crates/rast-base/src/storage/ops.rs`) and generate TypeScript types with `ts-rs` (task 0.3)
- [x] Create `rast-ui` with one Leptos component mounted in a RAST window, built with Trunk, embedded with `rust-embed`
- [x] Record the current compressed JS bundle size as the baseline
- [ ] Run the app matrix on 0.18.x and store results in `tests/e2e/results/` (task 0.6, waiting for a separate test host; see Next)
- [x] Create `docs/remake/PROGRESS.md` and the first ADRs
- [x] Fix the README: duplicated "Linux apps" row and stale status line (task 0.5)
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
    `QuotaDto` (JSON unchanged); no workspace dependencies; first in `members`
    only, moved to `default-members` later (see "Clean regeneration").
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

- **Gates after 0.4:** fmt and clippy clean; `cargo test --workspace`: 819 passed
  (811 + 6 server tests for `/ui/`, `.wasm` mime, CSP and the unchanged
  frontend cache policy + 2 `rast-ui`), 0 failed;
  npm test 152 passed (149 + 3 loader tests); npm build OK; shellcheck clean on
  all scripts; third-party licenses regenerated (425 crates);
  `cargo build --release --locked` OK with both dists and fails with a clear
  message when `crates/rast-ui/dist` is missing.

- **Clean regeneration of API types** (`f8f86e1`):
  - `scripts/gen-api-types.sh` empties `frontend/src/generated/` (keeps the
    README) and runs `cargo test --locked -p rast-api`.
  - The CI step runs the script and fails on any `git status` difference in
    that directory, so stale files are caught.
  - `rast-api` is in `default-members`; ADR 0002 records that only wasm-only
    crates (`rast-ui`) stay in `members` only.

- **JS bundle baseline** (`frontend/dist`, 2026-11-15, commit `f8f86e1`), measured
  with `scripts/measure-bundle.sh frontend/dist --group-ext jpg` (gzip -9, brotli
  quality 11 through node's zlib):

  | File | Raw (B) | Gzip -9 (B) | Brotli q11 (B) |
  | --- | ---: | ---: | ---: |
  | `assets/index-*.js` | 720639 | 207754 | 166428 |
  | `assets/xterm-bundle-*.js` | 362837 | 92744 | 73710 |
  | `assets/rfb-*.js` (noVNC) | 181861 | 54624 | 46332 |
  | `assets/index-*.css` | 225191 | 40155 | 32246 |
  | `assets/xterm-bundle-*.css` | 5175 | 1903 | 1415 |
  | `boot-wallpaper.js` | 1297 | 637 | 517 |
  | `index.html` | 4959 | 1928 | 1515 |
  | wallpapers (4 jpg) | 763415 | 742815 | 736119 |
  | **Total, all files** | **2265374** | **1142560** | **1058282** |
  | **Total without wallpapers (JS + CSS + HTML)** | **1501959** | **399745** | **322163** |

  The wallpapers are already compressed images and not code, so the remake
  baseline is the line "without wallpapers". The final-gate budget (1.5x) is
  therefore 1.5 x 399745 = 599617 B gzip, stored in
  `crates/rast-ui/wasm-budget.txt` and enforced by `scripts/check-wasm-size.sh`.
- **Task 0.4, `rast-ui` skeleton** (`f9c2eb2`; ADR 0003):
  - `crates/rast-ui`: Leptos 0.8.23 (`csr`), `wasm-bindgen`, `web-sys`,
    `console_error_panic_hook` (the four approved dependencies; 83 new
    transitive crates, 425 in `Cargo.lock`). One component, "About RAST
    (Leptos)", exported as `mount_about(el) -> MountHandle` (`dispose()`
    unmounts). In `members` only; `[profile.wasm-release]` added.
  - Trunk 0.22.1 (installed with `cargo install trunk --locked`), stable output
    names `rast-ui.js` and `rast-ui_bg.wasm` under `/ui/`; output embedded by a
    second `RustEmbed` in `rast-server` (`.wasm` as `application/wasm`,
    `no-cache` + ETag, unknown `/ui/*` is 404). `build.rs` fails a release build
    without a real `crates/rast-ui/dist`.
  - CSP: `script-src 'self' 'wasm-unsafe-eval'` (approved by the maintainer).
  - Frontend: `about-leptos` app, loader `frontend/src/core/wasm-ui.ts`
    (load-once, retry after failure), 3 new unit tests; Vite dev proxy for `/ui`.
  - CI and release workflows: new `ui` job (pinned Trunk 0.22.1 binary with
    SHA-256 check, `scripts/build-ui.sh`, `scripts/check-wasm-size.sh`), artifact
    `rast-ui-dist` consumed by the release builds, which also verify the UI is
    not the placeholder.
  - `wasm-opt` needs explicit feature flags with Rust 1.99
    (`data-wasm-opt-params` in `crates/rast-ui/index.html`).
  - WASM size (`scripts/check-wasm-size.sh`): `rast-ui_bg.wasm` 65283 B raw /
    26993 gzip / 22918 brotli; `rast-ui.js` 25394 / 5778 / 5057; total 90677 /
    32771 / 27975, against a gzip budget of 599617 B (baseline 399745 B).
  - Licenses of the new crates: all MIT, Apache-2.0 or dual MIT/Apache-2.0,
    except Zlib (`const_format`, `const_format_proc_macros`, `konst`,
    `konst_macro_rules`, `slotmap`), CC0-1.0 (`base16`) and BSL-1.0
    (`xxhash-rust`); all permissive.
  - Verified in headless Chromium against a stand-in server with the new CSP:
    module loads, component renders its text, `dispose()` empties the host, no
    console errors (see the task report; the disposable RAST instance was not
    started because `dev-instance.sh up` needs sudo).
  - Coordinator review: the cache refactor in `server.rs` had made `/` and
    `/index.html` cacheable for an hour (was `no-cache`); fixed before the
    commit and covered by `frontend_cache_policy_is_unchanged`. The pinned
    Trunk SHA-256 matches the digest GitHub publishes for the v0.22.1 asset.
- **Task 0.5, README** (`79f0116`): removed the truncated duplicate
  "Linux apps" row; the Status line now says v1.0.1, unreleased on `Remake`
  (latest release v0.18.2).
- **Plan: apps that launch other apps** (2026-11-16, docs only; ADR 0004
  `docs/adr/0004-nested-launchers.md`). Launchers such as ArtCraft Launcher
  and JetBrains Toolbox install apps into HOME and open them as child
  processes. Today `build_systemd_run_args`
  (`crates/rast-helper/src/native/daemon.rs`) starts app units with
  `Type=exec`, `KillMode=control-group` and no `ExitType=`, so closing the
  launcher kills the apps it opened, and they share its `linux_app_*` limits;
  `resolveSlug` (`frontend/src/shell/xpra-native.ts`) gives their windows the
  launcher's name. Planned:
  - Phase 2 (task 2.1): `ExitType=cgroup` on systemd 250+, a subreaper wrapper
    as the fallback for older systemd; main CPU/RAM limit per user session,
    per-app limits that cannot OOM-kill apps opened by a launcher.
  - Phase 3: the index watches `~/.local/share/applications` (task 3.1); own
    name and icon per window, End application warning, Pin to launcher (new
    task 3.4); clear message for `sudo`/`pkexec`/`apt`, no shortcut to root,
    AppImage via `APPIMAGE_EXTRACT_AND_RUN` (new task 3.5); Beta gate moves to
    task 3.6.
  - App matrix: "launcher in a launcher" scenario (ArtCraft Launcher,
    JetBrains Toolbox), part of the Phase 3 exit (Beta gate) on the
    maintainer's decision.
  - Updated: `CLAUDE.md` (Phase 2, Phase 3, app matrix) and
    `docs/remake/PROMPTS.md` (tasks 2.1, 3.1, 3.4–3.6).
- **Housekeeping:** `stash@{0}` (`docker-move-wip`) dropped on the
  maintainer's decision (its code equals `7663996`; its doc draft was
  superseded by `cc274e1`).

## In progress

- Nothing.

## Next

1. Phase 0.6 — baseline app matrix. **Waiting for a separate test host:**
   `docs/NATIVE-LINUX-APPS.md` allows only one native RAST instance per host
   (systemd unit names are keyed by user id), and production uses it on this
   host. Do not run `dev-instance.sh up native` here.
2. Then the Phase 0 exit check (CI green, single release binary, no behaviour
   change).

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
- Does `scripts/changelog-section.sh` accept the `## [1.0.1] - Unreleased`
  heading? Yes: it only reads the text inside the square brackets (checked by
  the maintainer).
- Which release is the cut-off for removing the legacy Docker/Selkies Linux-app
  backend (Phase 4)?
- **WASM budget baseline:** the 1.5x budget uses the gzip size of
  `frontend/dist` without the four wallpaper JPEGs (399745 B, not 1142560 B),
  because they are images, not code. Confirm with the maintainer.
- **Tests need a short `TMPDIR`:** with a long `TMPDIR` (for example a
  per-session scratch path) four `rast-agent` tests fail because unix-socket
  paths exceed ~108 bytes. When `/tmp` is nearly full, use a short directory on
  disk such as `TMPDIR=~/.cache/rast-tmp`.
- **Nested launchers (ADR 0004), to decide in the implementing tasks:**
  - Raise the systemd minimum from 249 to 250 instead of the subreaper
    fallback? It would drop Ubuntu 22.04 (task 2.1).
  - New meaning of the `linux_app_*` keys (per-app `MemoryHigh` instead of
    `MemoryMax`, `TasksMax` raised) is a config change and needs approval
    (task 2.1).
  - `NoNewPrivileges=yes` for app units: evaluate against Chromium's setuid
    sandbox fallback, then ask (task 3.5).
  - How ArtCraft Launcher is distributed (AppImage, archive) is not recorded
    yet; check before writing the e2e scenario (task 3.6).
- **ADR numbers in `docs/remake/PROMPTS.md` are stale:** tasks 1.1–1.5 name
  `0003-one-home.md` / "ADR 0003" (0003 is `rast-ui-leptos`), 4.2 names
  `0005-wayland.md` and Phase 7 `0006-display-research.md`. Task 2.1 now says
  "next free number"; fix the others when those tasks start.
- `yamllint` is not installed for the default `python3` on this host (a
  RAST-managed Python comes first in `PATH`), so the workflow files were not
  linted locally.

## Repository map

See `CLAUDE.md` (Repository map) and ADR 0002 for the crate layout.
