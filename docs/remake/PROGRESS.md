# Remake progress

Living log for the RAST remake (see `CLAUDE.md` and `docs/adr/0001-incremental-remake.md`).
Update it at the end of every session.

- **Current phase:** Phase 1 — One HOME (tasks 1.1 to 1.5 done, task 1.4 reviewed; their kernel-level and native tests are pending on the test host, which is now production with test accounts; next: the review of task 1.5, then the first pre-release deploy, task 1.5b and task 1.6). Phase 0 closed on 2026-11-16, task 0.6 deferred
- **Branch / version:** `Remake` / 1.0.1 (not bumped per phase; pre-releases are `1.0.1-alpha.N`, each with a `chore(release)` commit; releases are pre-release only until the final remake gate)
- **Last updated:** 2026-11-17
- **Test host:** production (`desktop.lufria.tech`) doubles as the test host;
  there is no separate VM or VPS (maintainer's decision, 2026-11-16). See
  "Testing on production" below and Production safety in `CLAUDE.md`.

## Phase 0 checklist

- [x] Split the repo into the target workspace with pure moves (task 0.2, ADR 0002)
- [x] Create `rast-api` with `EntryDto` and `ListingDto` (from `crates/rast-base/src/storage/ops.rs`) and generate TypeScript types with `ts-rs` (task 0.3)
- [x] Create `rast-ui` with one Leptos component mounted in a RAST window, built with Trunk, embedded with `rust-embed`
- [x] Record the current compressed JS bundle size as the baseline
- [ ] Run the app matrix on 0.18.x and store results in `tests/e2e/results/` (task 0.6, **deferred** by the maintainer: it runs on production with a test account while production still runs 0.18.x, before the first 1.0.1 pre-release is deployed)
- [x] Create `docs/remake/PROGRESS.md` and the first ADRs
- [x] Fix the README: duplicated "Linux apps" row and stale status line (task 0.5)
- [x] **Exit:** CI green, still a single release binary, no behaviour change (accepted by the maintainer on 2026-11-16, with 0.6 deferred)

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

- **Maintainer decisions, 2026-11-16 (Phase 0 closed):**
  - Manual check on the disposable instance in docker mode: login, File
    Station, Terminal, App Center and the "About RAST (Leptos)" window work
    normally.
  - Phase 0 exit accepted with task 0.6 deferred. 0.6 runs on a separate test
    host with RAST 0.18.2, before the Phase 1 results are tested on that host.
  - WASM budget baseline confirmed: the wallpapers stay excluded (399745 B
    gzip), because they keep being served as static files. In Phase 5 the WASM
    size is measured together with the remaining JS (for example xterm.js).
  - Preferences for ADR 0004, to be confirmed again in tasks 2.1 and 3.5 (the
    ADR itself is not changed now):
    - One mechanism for every systemd version: the subreaper wrapper. The
      systemd minimum does not rise and Ubuntu 22.04 stays supported.
      `ExitType=cgroup` is used only if task 2.1 finds a strong reason.
    - Per-app `MemoryHigh` with a per-session `MemoryMax`: agreed. The
      `linux_app_*` key names stay; their new meaning is explained in the
      CHANGELOG upgrade notes.
    - `NoNewPrivileges=yes`: not used unless tests on the test host prove
      that the Chrome and Flatpak sandboxes still work.
    - ArtCraft Launcher: AlternativeTo lists Debian, Flatpak and AppImage
      builds; verify in task 3.6.
  - Development host: Ubuntu 26.04.1 LTS, systemd 259 (259.9-0ubuntu0.1).

- **Test hygiene** (2026-11-16):
  - `d439b68`: no more `.tmp*` folders left in `TMPDIR`. The background
    runtime probe that `AppState` starts (`packages::system::read_version`)
    recreated `<data>/packages/.staging` with `create_dir_all` after a test had
    removed its data directory (28 leaked trees per full run, from
    `tests/api_e2e.rs` and `tests/native_linux_e2e.rs`). It now recreates only a
    missing scratch directory, never its parents, so production still heals a
    deleted `.staging`. The `FakeHelper` in the `native_session.rs` tests keeps
    its fake homes and profiles in its own `TempDir` instead of fixed folders.
    Two full runs with an empty `TMPDIR` leave nothing behind.
  - `0ab47a6`: the four `rast-agent` unix-socket tests use
    `test_support::socket_tempdir()`, which falls back to `/tmp` when `TMPDIR`
    is too long; they pass with a 110-character `TMPDIR`.
- **Docs** (`de69f17`): one-HOME ADR is `docs/adr/0005-one-home.md` in tasks
  1.1–1.5; tasks 4.2 and 7 use the next free ADR number. README lists Trunk
  0.22 and the `wasm32-unknown-unknown` target as build requirements.
- **Task 1.1, one-HOME design** (2026-11-16, docs only; ADR 0005
  `docs/adr/0005-one-home.md`, status proposed):
  - Ownership: HOME `<storage>/homes/<username>` owned by `rast-u<id>` 0700
    and used as the passwd home; a user's mode (one HOME or legacy) is derived
    from the owner of the HOME directory, no database column.
  - Per-user file agent `rast file-agent` (generalised root agent) in a
    socket-activated unit `rast-files-u<uid>`; `api/homefs.rs` forwards
    `/api/files/*` with the same JSON shapes after the usual RAST checks.
  - New helper operations: `home_ensure`, `files_agent_start`/`_stop`,
    `share_group_ensure`/`_remove`, `share_members_set`,
    `share_dir_prepare`/`_remove`, `quota_status`, `quota_set`,
    `share_quota_set`, `quota_report`, `home_migrate_*`, `shares_migrate_*`,
    `backup_reader_start`. None takes a raw path.
  - Shared folders: one rw and one ro group per share, setgid 2770 with
    default ACLs, `~/Shared/<name>` symlinks; membership from Effective
    Access.
  - XDG Trash in HOME and `<share>/.Trash-<uid>`.
  - Quota: kernel uid quota when enforced, RAST fallback otherwise. Apps
    installed by launchers (ADR 0004) and per-user Flatpak (Phase 3) live in
    HOME and count.
  - Production host (read-only `findmnt -T /srv/rast`): `/srv/rast` is on the
    root ext4 filesystem (`rw,relatime`, effective option `noquota`), shared
    with `/var/lib/rast` and the OS; no user or project quota is active, so
    production uses the RAST fallback until the maintainer enables kernel
    quota (D1). The ext4 `quota`/`project` features and the free space in
    `ubuntu-vg` need root to confirm (commands in the ADR).
  - **Bug found (0.18.x):** `start_run` (`native_session.rs`) and the
    Docker → native migration (`native_apps.rs`) build the RAST home from the
    Linux account name `rast-u<id>`, so they link and ACL-grant an empty
    `<homes>/rast-u<id>` instead of the person's home. Production has such
    folders (`rast-u1`, `rast-u97` … `rast-u104`, `rast-u999999`). Native
    apps never saw the person's files. Fix and cleanup: D13.
- **ADR 0005 accepted** (2026-11-16, maintainer decisions D1–D14, recorded in
  the ADR's decision table):
  - Root checks on production: ext4 has neither the `quota` nor the
    `project` feature, inode size 256; `ubuntu-vg` has 2 PV, 2 LV,
    VSize <1.14t, VFree 0.
  - D1: Q-c needs an extra disk. Production stays on the RAST fallback until
    just before Phase 3 reaches production; then the maintainer enables Q-a
    (journaled user and group quota on `/`). Runbook in ADR 0005, section 9
    (package `quota`, modules `quota_v2` and `quota_tree` from
    `linux-modules-$(uname -r)`). Q-c only with a new disk.
  - D3: migration 0015, nullable `shares.quota_bytes` (`NULL` = no limit), UI
    shows "enforced by filesystem" or "RAST only"; task 1.3 evaluates group
    quota on the rw group under Q-a and its gaps.
  - D6: accepted; administrators should use a non-admin account for everyday
    apps (into `docs/NATIVE-LINUX-APPS.md` in task 1.3).
  - D7: accepted for now; task 1.3 evaluates a per-user deny ACL entry on the
    share's container instead of a forced restart.
  - D13: fixed and released as 0.18.3 (2026-11-16, PR #2, merged into
    `Remake`); the maintainer removes the stray `homes/rast-u*` folders on
    production by hand after the fix is installed. 0.18.3 also runs the
    recursive home grant in the background (at most one per user, helper
    limit 13 s) and adds a default ACL entry `d:u:rast-u<id>:rx` to every
    folder in the home.
  - D2, D4, D5, D8–D12, D14 as recommended; the 20 GiB default stays and is
    reviewed in task 3.2.
  - `rast migrate shares` added to `CLAUDE.md` (Phase 1) and
    `docs/remake/PROMPTS.md` (task 1.3, with the D3, D6 and D7 items; task
    3.2 reviews the default quota).
- **Gates at the end of Phase 0:** fmt and clippy clean; `cargo test
  --workspace`: 819 passed, 0 failed; npm test 152 passed; npm build OK;
  third-party licenses up to date.
- **Docs fix outside 1.2** (`be5d688`): `docs/native-linux-apps-handoff.md`
  said restarting `rast-native.service` stops every session. The code says
  otherwise: the helper starts sessions and apps as transient units in
  `rast-linux.slice` through `systemd-run`, never as its own children, and
  neither its shutdown nor its start-up stops them (start-up only reconciles
  the nftables display-port set). `docs/OPERATIONS.md` was right; the
  handoff doc now says the same.
- **Task 1.2, per-user file agent** (2026-11-16, ADR 0005):
  - `8b986d8` refactor(storage): safe-path resolution
    (`StorageRoot::resolve`, `ResolvedPath::child_target`) and database-free
    file operations (`read_entries`, `mkdir_in`, `rename_in`, `search_in`,
    `text_preview_of`, `paginate`), so the agent can use them; no behaviour
    change.
  - `ebd1f2d` feat(helper): `home_ensure`, `files_agent_start`,
    `files_agent_stop` (strict JSON, user id only, no paths). The agent unit
    `rast-files-u<uid>` runs as `rast-u<id>`, socket-activated on
    `/run/rast/files/u<uid>.sock` (`root:rast` 0660 in a `root:rast` 0750
    directory), token by `LoadCredential`, hardened (no capabilities,
    `NoNewPrivileges`, `PrivateNetwork`, `AF_UNIX` only,
    `ProtectSystem=strict` with HOME and the shared root writable, 512M,
    64 tasks). A running agent is reused; `identity_remove` also stops it.
  - `da0dc58` refactor(agent): the shared core of the root and per-user
    agents (`fileagent`: token and peer-uid guard, upload sessions, client,
    idle stop) moved out of `rootfs`; no behaviour change.
  - `08f7ef0` feat(agent): `rast file-agent` (hidden subcommand,
    `homefs::agent`): File Station over one HOME with the safe-path layer,
    `O_NOFOLLOW` on chunk reopen, `/shared/<name>` only through
    `~/Shared/<name>` links to a direct child of the shared root, `writable`
    from `faccessat`, `EDQUOT` → 413. No delete route (recycle bin in 1.4).
  - `d1e7c57` feat(agent): a realtime watch stream (NDJSON) from the agent;
    dot folders are not watched.
  - `d8a0898` test(helper): the peer-uid test accepts a connection reset as
    a dropped connection (flaky under parallel load; 30/30 runs pass).
  - `8b81ebd` feat(server): `api/homefs.rs` forwards `/api/files/*` of a
    one-HOME account (HOME owned by its Linux identity, `native_enabled` on
    Linux) to its agent after session, CSRF, path, shared-folder access
    (Effective Access), quota reservation and audit; same JSON shapes; `503`
    without the helper, never an in-process fallback. Copy and move run as
    jobs with a quota budget header the browser cannot set.
    `tests/homefs_e2e.rs`: real router and agent, fake helper.
  - `a77ccf2` fix(realtime): the server watcher watches each home on its own
    (one unreadable HOME no longer ends the recursive watch for the others)
    and skips one-HOME homes, which their agent reports.
  - `e3d6da1` feat(files): "Show hidden files" toggle; `hidden=false`
    on listing and search (in process, per-user agent and root agent).
  - Gates at the end of task 1.2: fmt and clippy clean; `cargo test
    --workspace`: 861 passed, 0 failed; npm test 154 passed; npm build OK.
    No dependency added.
  - Deviations and notes:
    - Shared folders of one-HOME accounts work only through `~/Shared`
      links; nothing creates them yet (task 1.3 adds the groups and links).
      Superseded by `da39cc6`: the server names the containers.
    - Delete, the recycle bin and `GET /api/files/usage` answer `503` for
      one-HOME accounts until task 1.4 (XDG Trash).
    - Quota stays the RAST fallback (`storage_usage`). If a copy fails half
      way, the bytes already written are not counted; task 1.4's HOME scan
      corrects it. `quota::recalculate` cannot read a one-HOME HOME (0700).
    - Nothing calls `home_ensure` yet: no account becomes one-HOME before
      `rast migrate home` (task 1.5).
- **Task 1.2 security review** (2026-11-16, independent review of
  `8b986d8`..`f2a1746` against ADR 0005). Nothing was exploitable on
  production, where every account is still legacy. Fixed:
  - `b5afbec` test(helper): unit tests wait out `ETXTBSY` on fake binaries
    (the home and file agent tests failed about one run in ten).
  - `1a04a46` fix(helper): `home_ensure` reopened a folder it had just made
    with `mkdirat` by name, so a directory renamed into its place was
    chowned (and, for the HOME, chmodded) by root; the reopen now requires
    an empty folder owned by the helper.
  - `79ca283` fix(server): streamed agent replies keep only the file
    headers; the server sets `nosniff` and the sandboxing preview CSP, so
    an agent taken over by the user's other processes cannot set cookies
    or run script on the RAST origin.
  - `d298a7f` fix(agent): `rast file-agent` is non-dumpable
    (`PR_SET_DUMPABLE` 0), so same-uid processes cannot ptrace it where
    `ptrace_scope` is 0.
  - `445b7b0` fix(server): chunked uploads of one-HOME accounts have the
    in-process limits (8 per account, 64 in all, `429`).
  - Gates after the review: fmt and clippy clean; `cargo test --workspace`
    867 passed, 0 failed; npm test 154 passed; npm build OK.
- **Task 1.2 review follow-up** (2026-11-16, the maintainer's answers to the
  open findings; done before task 1.3):
  - `164ba06` refactor(base): file serving and text previews can take an
    open file (`serve_open_file`, `text_preview_from`, `entry_from_parts`,
    `breadcrumbs`); no behaviour change.
  - `2e10625` fix(agent): every file operation of the per-user agent runs on
    directory descriptors (`homefs::fsat`): `openat2` with
    `RESOLVE_BENEATH | RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS` below
    the namespace root (an `O_NOFOLLOW` walk without `openat2`), `*at`
    calls by single names, `O_CREAT | O_EXCL | O_NOFOLLOW`,
    `RENAME_NOREPLACE`; copy, move and upload keep their folders open and
    compare inodes. Race tests swap entries of a share for links in the
    middle of a copy and an upload (source and destination side). The
    race claim of ADR 0005 is corrected. `rustix` 1.1.6 becomes a direct
    dependency of `rast-agent` (already in `Cargo.lock`, Apache-2.0 OR
    MIT, no new crates; the third-party list is unchanged).
  - `da39cc6` fix(agent,server): the server sends the containers of the
    shares a request names (`x-rast-shares`, server-only); the agent opens
    them directly below the shared root, checks owner and group (the
    shared root's, or root with `rast-s<h>-rw`), and never follows or
    lists `~/Shared/<name>`. ADR 0005 section 2 rewritten.
  - `58770a2` fix(server): one lock per account while its agent starts.
  - `ccdb04c` fix(server): agent errors keep 403, 404, 413, 415 and 429,
    with static messages from a known list and bounded free text; a 401 or
    redirect from an agent becomes 502.
  - `3f507b0` fix(server): a refused token is read again from the helper
    once; a streamed upload body cannot be replayed and gets 503.
  - `dc6f5b2` fix(server): deactivating an account stops its file agent.
  - docs: the "agent output is untrusted" invariant in CLAUDE.md and ADR
    0005; the extra unit hardening moved to "Pending on the test host".
  - Gates: fmt and clippy clean; `cargo test --workspace` 886 passed, 0
    failed; npm test 154 passed; npm build OK.
  - Not changed: the root agent's client (`RootFs`) has no token retry. The
    root agent creates its own socket (it is not socket-activated), so a
    restarted root agent normally leaves no socket behind and the server
    starts it again; see "Open questions".

- **Review of the directory-descriptor layer and ADR 0005 §2** (2026-11-16,
  before task 1.3):
  - F1, fixed in `175b98f`: files the agent wrote into a share got mode
    0644/0755 from the source, which narrowed the ACL mask under the
    default ACL, so the group lost write access. Files written into a
    share are now 0660 (0770 for executables).
  - F2, fixed in `175b98f`: a File Station move between HOME and a share
    was a rename and kept the old group and ACL. It is now copy plus
    delete, so the entry gets the share's group, ACL and (later) quota.
  - F3, fixed in `a44bc5d` (maintainer: below share containers only): the
    agent's `openat2` had no `RESOLVE_NO_XDEV`. Paths below a share
    container now use it (the fallback walk compares devices) and an entry
    on another file system is refused; HOME is unchanged, so the user's own
    mounts there stay reachable. The agent's tree walks (search, copy,
    size, remove) below a share were completed in `10116ce` (task 1.3
    security review).
  - F4, covered by the daily repair pass (`bfe360e`): legacy in-process
    moves and recycle-bin restores into a prepared share keep group `rast`
    and no share ACL until the next pass; recorded in the runbook.
  - The `x-rast-shares` header and the agent's container check were found
    sound.
- **Task 1.3, shared folders as Linux groups** (2026-11-16, ADR 0005
  sections 4, 8 and the new section 10):
  - `fcc8e62` refactor(helper): the descriptor helpers of `homes` become
    `pub(crate)`.
  - `6fc48ff` feat(helper): `share_group_ensure`, `share_group_remove`,
    `share_members_set`, `share_dir_prepare`, `share_dir_remove`,
    `shares_migrate_scan`/`_apply`/`_rollback`; `native::acl` (POSIX ACL
    xattr codec on descriptors); journal under
    `<data_dir>/migrations/shares/<shareId>/<stamp>/journal.jsonl`.
  - `81214ed` feat(helper): `share_members_set` skips (and reports) users
    without a Linux account; `share_dir_release` for deleted shares.
  - `e81eb59` feat(agent): `/internal/shared-links` keeps `~/Shared/<name>`.
  - `940c44e` feat(server): `share_groups` syncer (Effective Access →
    groups), agent restart, `~/Shared` links, 60-second forced restart on
    revocation and "Restart session now" offer on grant
    (`/api/session/restart`, events `session.restart`/`session.restarted`,
    desktop toast); share create/delete hooks; role and status changes bump
    the access generation.
  - `01aa950` feat(cli): `rast migrate shares [--share] [--dry-run |
    --rollback] [--json]`, instance lock for apply and rollback (D9).
  - docs: ADR 0005 section 10 (implementation notes, deviations, the D3 and
    D7 evaluations), section 3 table; `docs/NATIVE-LINUX-APPS.md` (groups
    and the D6 administrator note); `docs/OPERATIONS.md` runbook;
    `docs/API.md`; `docs/REALTIME.md`; CHANGELOG.
  - Tests: helper (fake `getent`/`groupadd`/`gpasswd`/`groupdel` over a
    text group database: names, gid range, reuse, conflicts, membership
    replacement, validation, skipped users; prepare with modes and ACLs,
    no link followed, hard links skipped; migrate journal, resume,
    rollback, tampered journals refused, set-uid dropped; release; remove),
    protocol strictness, agent link maintenance, server membership from
    Effective Access (admin, legacy read, rule write, deny over legacy
    write, deactivated, no identity), revocation vs grant, restart timer,
    the CLI's per-share order, and `tests/homefs_e2e.rs` (real router and
    agent, fake helper: groups and links follow a write → read → none
    change, the agent restarts, the restart is forced and reported by
    `GET /api/session/restart`).
  - Gates: fmt and clippy clean; `cargo test --workspace` 912 passed, 0
    failed; npm test 156 passed; npm build OK.
  - Measured: the helper's walker takes 0.85 s for 100 000 files when
    nothing changes, 1.4 s when everything changes (release, warm cache).
- **Task 1.3 follow-up: the maintainer's decisions** (2026-11-16; the
  1.3 commits were pushed to `Remake` first):
  - `0e82a2d` docs(changelog): the heading of the "Show hidden files"
    entry, cut by an edit in `940c44e`, is back.
  - `58e5e74` fix(helper): the rollback reports every entry restored
    without set-uid, or without set-gid on a regular file (set-gid on
    folders stays): `droppedBits` in the reply (first 2 000) and
    `dropped-bits.txt` in the rolled-back journal folder; the CLI prints
    them.
  - `f05ddd9` feat(helper): the dry run lists every entry whose ACL holds
    entries RAST does not manage, with the entries (`foreignAclEntries`);
    runbook note to read it and save them with `getfacl -R` first.
  - `a44bc5d` fix(agent): F3, `RESOLVE_NO_XDEV` below share containers.
  - `4ca2d47` feat(helper): `share_dir_repair`, `share_deny_set` (at most
    256 users; every prepare keeps the entries) and the read-only
    `share_session_groups` (`Groups:` of `/proc/<pid>/status`, at most 200
    users per call).
  - `cec3297` feat(server): D7, `u:rast-u<id>:---` on the container of
    every share lost entirely, for the warning, lifted after the restart
    or a new grant; every pass finds sessions that still carry a lost
    group (also after a server restart, and for deactivated accounts) and
    warns and restarts them again; a read → write move is no longer a
    revocation.
  - `bfe360e` feat(server): D3, the repair pass at start-up (after every
    `rast migrate shares`) and daily.
  - Gates: fmt and clippy clean; `cargo test --workspace` 923 passed, 0
    failed; npm test 156 passed; npm build OK. No dependency, migration or
    deploy change.
- **Task 1.3 security review** (2026-11-16, independent review of
  `036b7bf..4be92ae` against ADR 0005). Fixed:
  - `898421f` fix(helper): the shared folder walk (prepare, repair,
    migration apply and rollback) skipped only root-owned entries and
    changed group, bits and ACL of entries owned by any other account.
    With `fs.protected_hardlinks = 0` a member could hard-link a file of
    another service into a share; once its original name was gone (nlink
    back to 1), the next repair pass made it group read-write. Now only
    entries of `rast` and RAST identities are changed; others are reported
    as `otherOwner`.
  - `3da3267` fix(helper): `dir_names` read a whole folder into memory
    before the 1 000 000-entry limit counted it; a member's folder with
    tens of millions of empty files made the root helper allocate
    gigabytes on the repair pass. Reads are now capped by the remaining
    budget.
  - `602cdac` fix(native): `NativeClient::call_json` gave the shared
    folder walks the 15-second quick deadline instead of the protocol's
    hour (and two minutes to the group operations). `rast migrate shares`
    reported a large share as failed while the helper went on applying,
    and released the instance lock, so a rollback could run next to it.
  - `7c9d9eb` fix(server) and `32de67b` fix(server): `sync_once` stopped at
    the first share whose groups failed: later shares never got their
    members set (revoked members kept the groups for new sessions), the
    revocations already applied never restarted sessions or set deny
    entries, and stale-session checks and the repair pass never ran. Each
    share is now synced on its own; a failing one is retried after 30 s.
  - `10116ce` fix(agent): F3 completed, see above.
  - `558d339` docs: file permissions inside shared folders are managed by
    RAST (the repair pass undoes `chmod 600`; private files belong in
    HOME). File Station has no in-app help.
  - Found sound: protocol validation of the new operations (share id and
    container only, `deny_unknown_fields`, bounded user lists); the
    journal as untrusted input (no uid or gid 0, container owner must be
    `rast`, set-uid and set-gid on files never restored, changes only
    through the walk inside the container); deny entries (named user
    before groups, removed after the restart or a grant, leftovers cleaned
    on the first pass); `share_session_groups` against pid reuse (one read
    of `status`, nothing acts on the pid); `/api/session/restart` (session,
    CSRF and origin on POST, only the caller's own session); `~/Shared`
    link maintenance (creates with `symlinkat`, never replaces, removes
    only links that point into the shared root).
  - Gates: fmt and clippy clean; `cargo test --workspace` 928 passed, 0
    failed; npm test 156 passed; npm build OK. No dependency, migration,
    protocol or deploy change.

- **Task 1.3 security review: the maintainer's decisions** (2026-11-16,
  the five questions of the review):
  - `80078f0` fix(helper): `share_session_groups` counts only processes in
    the user's own units (first non-slice cgroup component:
    `rast-xpra-u<id>`, `rast-files-u<uid>`, `rast-app-u<uid>-…`,
    `rast-term-u<uid>-…`) whose real and effective uid are the identity;
    status and cgroup are read through one `/proc/<pid>` descriptor.
  - `468e691` fix(helper): an account `rast-u<id>` with an unexpected uid
    is never added, is taken out of the groups and is reported
    (`conflicts`); the other members are still set; the server logs it and
    `rast doctor` lists it.
  - `da68e37` fix(helper): mount ids (`statx` `STATX_MNT_ID`) in every
    helper walk and in `share_dir_remove`; a container that is a mount
    point is refused.
  - `271fbe9` fix(base): the traverse bit on the data directory is still
    needed (app profiles, identity homes, package runtimes), so the
    database and `secret.key` move into `<data_dir>/private` (0700) at the
    first server start, with links at the old names; `backups/` 0700;
    `rast doctor` checks `fs.protected_hardlinks = 1`, the folder and
    conflicts (ADR 0006). The helper already skipped files with more than
    one link (also those of `rast`); a test pins it.
  - `58d5fc3` fix(helper): per-share lock in the helper for every container
    operation (`share_busy` after 10 s) and a `flock` in the journal
    folder for apply and rollback.
  - Gates: fmt and clippy clean; `cargo test --workspace` 937 passed, 0
    failed; npm test 156 passed; npm build OK. On-disk layout change
    (ADR 0006, approved); no dependency, migration or deploy change.

- **Task 1.4, XDG Trash and quota** (2026-11-16, ADR 0005 sections 5, 6
  and 11, D2, D3, D4, D14):
  - `184002b` feat(base): freedesktop Trash data (`storage::xdg_trash`):
    `.trashinfo` encoding and strict parsing, `directorysizes`, opaque ids,
    origins that never leave their root; fixtures written by `gio trash`.
  - `d760d24` feat(agent): XDG Trash in the file agent (delete, list with
    retention purge, restore, delete for good, empty) through the
    descriptor layer; `.trashinfo` untrusted (restore only below HOME or a
    container the server named, else the top of the trash's root);
    `.Trash-<uid>` in shared folders (0700, no ACL, hidden, never served);
    `/internal/usage` scan (allocated bytes, each inode once, one file
    system, by location and type).
  - `6143907` feat(server): the delete, trash and usage routes of one-HOME
    accounts go to the agent (job, write checks, shares header with read or
    write access, strict reply checks, audit, realtime); the fallback quota
    follows the HOME scan; additive `UsageDto.byLocation` and
    `quotaBackend`; `.Trash*` hidden and reserved in process too.
  - `3eebeff` feat(helper): `quota_status`, `quota_set`,
    `share_quota_set`, `quota_report` (`quotactl_fd`, ids only, fake behind
    a trait); the share walk leaves members' `.Trash-<uid>` alone.
  - `2c7433a` feat(server): backend detection (start, every 10 min, on
    change), D4 limits for one-HOME accounts, kernel usage, the storage
    settings summary, `rast doctor`; the weekly recalculation no longer
    zeroes one-HOME accounts.
  - `172ee54` feat(storage): migration 0015 (`shares.quota_bytes`,
    approved as D3), `quotaBytes`/`quotaEnforcedBy` on `/api/shares`, the
    RAST check for File Station writes into a limited share (uploads,
    copies, moves; the agent's transfer budget covers share destinations),
    group limits on the rw gid when group quota is on.
  - `18ab765` feat(ui): share size limit field and "enforced by" line,
    quota backend in Control Panel → Storage, usage chips by location.
  - Deviations (ADR 0005, section 11): no project quota yet (no project
    ids are set; group quota only), grace times are reported but never set
    by RAST, the RAST share check does not reserve bytes or see members'
    trash, legacy recycle-bin rows move with `rast migrate home` (1.5).
  - Gates: fmt and clippy clean; `cargo test --workspace` 984 passed, 0
    failed; npm test 159 passed; npm build OK. Migration 0015 (approved,
    D3); no new dependency; no deploy or config change. No pre-release
    prepared (it follows task 1.5 and its review).

- **Task 1.4 security review** (2026-11-17, independent review of the
  commits after `7da51ae` against ADR 0005; ADR 0005 section 12). Bugs
  fixed, one commit each, each with a regression test that fails without
  the fix:
  - `bf617ae` fix(agent): a restore never targets a `.Trash*` name at the
    top of a shared folder.
  - `878acff` fix(agent): one entry named twice in a request (hex case) is
    handled once.
  - `5bffa60` fix(agent): "Empty" reads its folders in bounded batches.
  - `b215232` fix(agent): an own share trash is closed again (`0700` +
    set-gid, no ACL) whenever it is used; foreign info files are skipped.
  - `2b480eb` fix(agent): the share-limit message reaches the browser.
  - `ed0a457` fix(server): D4 limits are whole KiB.
  - `1166713` fix(server): a truncated HOME scan never lowers the usage.
  - `c7d5c7a` fix(helper): `quota_set` checks passwd (missing account
    `not_found`, other uid `identity_conflict`).
  Maintainer's decisions on the 1.4 report (2026-11-17), applied:
  - Project quota is deferred until a host supports it (production needs a
    new disk, D1); recorded in ADR 0005, section 11.
  - Grace times: reported by `rast doctor` and explained in the runbook
    (`docs/OPERATIONS.md`); RAST never sets them.
  - A `.Trash-<uid>` made first by another member is reported, never
    changed: `5a66cd2` feat(helper) (`foreignTrash` in the tree walk
    report, the daily repair pass log, `rast doctor`).
  - The three "RAST only" share-limit gaps are closed:
    `6553ac8` fix(storage) (uploads reserve their bytes while they run, in
    process and through the agent), `e76db3f` feat(helper) (new read-only
    helper operation `share_trash_usage`; members' recycle bins count
    towards the limit), `8d8ab5f` fix(storage) (a restore must fit: in
    process, and through the agent with the new server-only header
    `x-rast-share-free`).
  - Open, for the maintainer: migration 0015 makes a binary-only rollback
    to 0.18.x impossible (see "Open questions").
  - Gates: fmt and clippy clean; `cargo test --workspace` 1003 passed, 0
    failed (one run showed the timing flake below); no frontend change; no
    new dependency or migration; no deploy or config change.

- **Task 1.5, HOME migration command** (2026-11-17, ADR 0005 sections 8
  and 13). The maintainer's decisions for this task: `rast migrate shares`
  needs `--share <name>` or `--all` like `rast migrate home --user <name>
  | --all`; app profiles merge per file; the legacy path (`setfacl -R`
  grant, `RAST` link, profiles) stays for legacy accounts; new accounts
  stay legacy (D8 after verification); the backup reader (D12) is task
  1.5b. Commits:
  - `b52f133` refactor(helper): the descriptor walk and journal helpers of
    shares are shared (no behaviour change).
  - `85a21b7` feat(native): one-HOME accounts use their HOME for sessions,
    apps and terminals (no profile, `RAST` link, per-launch grant or
    terminal ACL; the helper detects one-HOME from passwd and the HOME's
    owner; a cached identity is asked again when the mode changed).
  - `2ff2fa2` fix(helper): the migration lock is retried for 500 ms (the
    `operations_on_one_shared_folder_never_overlap` flake: a forked child
    still held the released `flock`).
  - `0e2b542` feat(helper): `home_migrate_scan`, `home_migrate_apply`,
    `home_migrate_trash`, `home_migrate_rollback` (journal and plan first,
    owners and ACLs including the 0.18.3 `u:rast-u<id>:rx` and
    `d:u:rast-u<id>:rx`, per-file merge, passwd home, the HOME last;
    resume; exact rollback; recycle bin batches into XDG Trash).
  - `5e116f5` fix(backup): HOMEs in one-HOME mode are skipped with a
    warning (manifest, job result, Backup Center) instead of silently.
  - `b1bba53` feat(cli)!: `rast migrate home`; `rast migrate shares` needs
    `--share` or `--all`.
  - fix(helper) after a self-review of `home_migrate_trash`: a batch that
    stopped after its journal but before the move is planned again instead
    of reported `already` (which made the CLI delete rows whose data was
    still in the legacy recycle bin); info files are created after the
    batch journal and a rollback removes those of entries that never
    moved; a `.Trash-<uid>` made by another member fails only that shared
    folder's entries, not the whole batch.
  - Tests: 14 helper tests on temporary trees with an owner overlay
    (dry-run changes nothing; apply; 0.18.3 grant stripped and restored
    exactly; foreign ACL entries kept; hard links and planted links never
    followed; per-file conflicts; a profile of another identity never
    merged; interrupted apply resumes; apply twice is a no-op that keeps
    the session; tampered, world-readable and unknown-field journals
    refused; running processes and units that do not stop block it;
    recycle bin into XDG Trash of HOME and of a shared folder and back; a
    batch that stopped before its move; a squatted `.Trash-<uid>`);
    protocol and timeout tests; 6 CLI tests with a fake helper (dry-run
    report, batches of 256, rows saved and restored, legacy shares refused,
    one failing account, accounts made in one-HOME mode); per-share apply,
    dry-run and rollback of `rast migrate shares`; CLI parser tests for
    both selections; backup skip and unreadable-folder tests.
  - Not run: the dev instance in docker mode (`scripts/e2e/dev-instance.sh`
    needs `sudo` for the service user and a root worker, and docker mode
    has no native backend, so `rast migrate home` cannot run there); see
    "Open questions" for a risk in its `down`. Native tests are listed
    under "Pending on the test host (task 1.5)".
  - Gates: fmt and clippy clean; `cargo test --workspace` 1037 passed, 0
    failed; frontend 160 tests passed and build clean; no new dependency
    or migration.

## Phase 1 checklist

- [x] Task 1.1: one-HOME design, `docs/adr/0005-one-home.md` (accepted 2026-11-16)
- [x] Task 1.2: per-user file agent (unit and fake-helper integration tests; the tests below under "Pending on the test host" are still open)
- [x] Task 1.3: shared folders as Linux groups (unit and fake-helper tests; kernel-level tests under "Pending on the test host")
- [x] Task 1.4: XDG Trash and quota (unit and fake-helper tests; kernel quota and real apps under "Pending on the test host (task 1.4)"; security review 2026-11-17)
- [x] Task 1.5: HOME migration command (unit, fake-helper and CLI tests;
  native tests under "Pending on the test host (task 1.5)"; review pending)
  - `rast migrate home --user <name> | --all`, `rast migrate shares
    --share <name> | --all`, both with tests for every mode.
  - Removes the 0.18.3 entries `u:rast-u<id>:rx` and `d:u:rast-u<id>:rx`
    (and terminal grants); `--rollback` restores them.
- [ ] Task 1.5b: Backup Center reads HOMEs through the helper (D12,
  `backup_reader_start`); until then one-HOME HOMEs are skipped with a
  warning.
- [ ] Task 1.6: Phase 1 security review
- [ ] **Exit:** a file saved by Chrome appears in File Station and vice versa; cross-user tests pass; dry-run and rollback tested on production with test accounts and test shares

## In progress

- Nothing.

## Testing on production

Since 2026-11-16 production (`desktop.lufria.tech`) is the test host. The
rules (full text in `CLAUDE.md`, Production safety):

- Only the maintainer deploys: a pre-release from `Remake`, then
  `sudo rast update --version <version>`. Claude never stops, restarts or
  reconfigures the production services.
- Before every deploy Claude prepares a checklist: backup of `/etc/rast`,
  `/var/lib/rast` and `/srv/rast`, the rollback version, and the rollback
  steps (migration `--rollback` for test accounts with the new binary, then
  `sudo rast update --version <rollback version>`, then a health check).
- Tests use test accounts (prefix `t-`) and test shared folders only.
  `rast migrate shares` and `rast migrate home` run only for those until the
  maintainer approves migrating real accounts. Apply and rollback of
  `rast migrate shares` need `rast.service` stopped, so the maintainer runs
  them.
- One native instance per host: no second native instance; native tests use
  the production instance with test accounts. The dev instance in docker
  mode is still allowed.
- The "Pending on the test host" lists below mean: tests on production with
  test accounts, after the deploy that contains the change.

## Pre-release check (2026-11-16)

Checked whether a pre-release such as `1.0.1-alpha.1` can be published and
installed with `rast update --version`. No tag was made.

- **Workflow version format:** `.github/workflows/release.yml` and
  `installer/install.sh` (`normalize_version`) accept `X.Y.Z-suffix`, so
  `1.0.1-alpha.1` passes the format check.
- **Blocker, exact version match:** the workflow then requires the release
  version to equal `Cargo.toml` ("Cross-check Cargo.toml version") and the
  built binary's `rast version`, and `scripts/changelog-section.sh` needs a
  `## [1.0.1-alpha.1]` section. With `Cargo.toml` at `1.0.1` (the remake
  rule says do not bump), a `v1.0.1-alpha.1` release fails in the prepare
  job. The release installer and `rast update` also check that the binary
  reports exactly the release version. Options (maintainer's decision):
  (a) set `Cargo.toml` to `1.0.1-alpha.N` in a release commit for each
  pre-release, with a matching CHANGELOG heading, as an exception to the
  "no bump" rule; (b) let the workflow and `changelog-section.sh` map a
  `-suffix` release to the `[1.0.1]` section, which still needs (a) for the
  binary version; (c) publish only `v1.0.1` itself as a pre-release, which
  allows a single test deploy (a second one needs the tag and release
  deleted and recreated).
- **Risk, tag push:** `prerelease: ${{ inputs.prerelease || false }}` is
  `false` for a pushed tag, so `git push origin v1.0.1-alpha.1` publishes a
  **normal** release. GitHub then reports it as `/releases/latest`, and
  `rast update` (and the update notice) offers it to every server. Until the
  workflow derives `prerelease` from a `-` in the version, publish
  pre-releases only through the manual run (`workflow_dispatch`) with
  `prerelease` ticked.
- **`rast update --version <v>`** (`src/update.rs`): fetches
  `/releases/tags/v<v>`, which also returns pre-releases, and skips the
  "is it newer" check when `--version` is given, so it installs a
  pre-release and can go back to an older version (rollback). Without
  `--version` it uses `/releases/latest`, which never returns a
  pre-release, so other servers are not offered one.
- **Version order:** `version_is_newer` compares only `X.Y.Z` and ignores
  the suffix, so `1.0.1-alpha.2` and `1.0.1` count as equal to
  `1.0.1-alpha.1`. A server on a pre-release must move to the next
  pre-release or to the final `1.0.1` with `--version`; plain
  `rast update` reports "already up to date".
- **Rollback to 0.18.x:** since task 1.4, `Remake` adds migration `0015`
  (schema v15). The sqlx migrator of 0.18.x refuses a database with an
  applied migration it does not know (`VersionMissing(15)`), so a 0.18.x
  binary no longer starts on a database that a 1.0.1 build has opened: a
  rollback restores the database backup taken before the deploy
  (`/var/lib/rast`), then installs 0.18.x (see "Open questions"). This note
  said "no migration" before task 1.4. The move of `rast.db` and
  `secret.key` into `private/` keeps links an older binary follows (still
  to be verified on production, see "Pending on the test host (task 1.3)").
  A binary rollback does not undo groups, ACLs or owners set by
  `rast migrate shares` or `rast migrate home`; run their `--rollback` with
  the 1.0.1 binary first.

**Decision (maintainer, 2026-11-16): options (a) and (b) together, and
`prerelease` derived from the version.**

- Every pre-release is `1.0.1-alpha.N` and gets its own commit
  `chore(release): 1.0.1-alpha.N` that sets `Cargo.toml` and `Cargo.lock` to
  that version, so the exact-match checks (workflow, binary, installer,
  `rast update`) pass and `rast version` shows the installed build. Plain
  `1.0.1` is only the final release, after the final remake gate.
- `CHANGELOG.md` keeps one `## [1.0.1]` section. `scripts/changelog-section.sh`
  uses the `## [X.Y.Z]` section for an `X.Y.Z-suffix` version that has no
  section of its own.
- The Release workflow's "Determine version" step runs
  `scripts/release-check.sh`: a version with a `-suffix` is always published
  as a pre-release (pushed tag or manual run) and never made latest; a
  version without a suffix is refused unless its commit is on `origin/main`
  (the prepare job now checks out the full history), with or without the
  `prerelease` box. The "Risk, tag push" above no longer applies to suffixed
  versions. Tests: `tests/release_scripts.rs`.
- The "Version order" note above still holds: moving between pre-releases
  needs `rast update --version`.

## Pending on the test host (task 1.2)

These need root, real `rast-u<id>` accounts or a native RAST instance, so
they run on production with test accounts after the deploy that contains
task 1.2 (only one native RAST per host). Until then 1.2 is covered by unit
tests and `tests/homefs_e2e.rs` (real router, real agent running as the
test user, fake helper).

- Kernel-level cross-user isolation: an agent of `rast-u<a>` gets `EACCES`
  on the HOME of `rast-u<b>` and on shared containers it is not a member
  of; File Station of user A cannot reach B's files by any path, upload id
  or job.
- The user's own processes cannot connect to or replace
  `/run/rast/files/u<uid>.sock` (directory `root:rast` 0750, socket 0660),
  and a connection from a uid other than `rast` is dropped (`SO_PEERCRED`).
- `files_agent_start` under real systemd: socket activation (`LISTEN_FDS`),
  `LoadCredential=token`, the unit properties (`ProtectSystem=strict`,
  `ReadWritePaths`, `PrivateNetwork`, `RestrictAddressFamilies`, empty
  capability set, `MemoryMax`, `TasksMax`), reuse of a running agent, idle
  exit after 900 s, staying up while the user has a session, app or
  terminal, and `files_agent_stop` / `identity_remove` stopping it.
- `home_ensure` on a real account: owner, mode 0700, skeleton,
  `usermod --home`, refusal of a `rast`-owned (legacy) HOME and of
  symlinks.
- Realtime from a real agent: changes made by an app or Terminal as
  `rast-u<id>` appear in File Station; the server watcher does not watch the
  0700 HOME.
- Native e2e: a one-HOME account opens File Station, uploads, downloads,
  copies, renames; `503` when `rast-native.service` is stopped and the agent
  has exited.
- Directory-descriptor layer on the real kernel: `openat2` is used (not the
  fallback walk), and a share container `root:rast-s<h>-rw` 2770 passes the
  agent's owner and group check through the real group database, while a
  container owned by a member is refused.
- Extra unit hardening for `rast-files-u<uid>` (from the review): try
  `ProtectKernelTunables`, `ProtectKernelModules`, `ProtectKernelLogs`,
  `ProtectControlGroups`, `ProtectClock`, `ProtectHostname`,
  `RestrictNamespaces`, `RestrictRealtime`, `RestrictSUIDSGID`,
  `LockPersonality`, `MemoryDenyWriteExecute`,
  `SystemCallArchitectures=native` and `SystemCallFilter=@system-service`
  (check that `openat2`, `renameat2` and `getgrnam_r` still work), then
  add those that pass to `files_agent_start`, its tests and ADR 0005.
- A restarted socket unit: stop `rast-files-u<uid>` outside RAST, start it
  again through File Station, and check that the server reads the new
  token once (no `403` loop); deactivating the account stops the unit.

## Pending on the test host (task 1.3)

- Real groups: `groupadd -K` picks gids in the range; `gpasswd -M` with
  many members; `getent` batches; `share_group_remove` refuses groups
  outside the range.
- Read-only permissions with the kernel: a `-ro` member can list and read
  the container and files but `touch`, `rm` and `mkdir` fail with
  `EACCES`; a `-rw` member can write, and new files get the rw group and
  stay writable for the group; a non-member gets `EACCES`; `rast` still
  reads and writes (D5).
- Revocation: after write → read the restarted agent can no longer write,
  apps and terminals restart after 60 s, and afterwards `id` shows the new
  groups; "Restart session now" works.
- `rast migrate shares --share <test share>` on a test share set up like
  the production shares (`rast:rast` 0750, files 0644): dry-run report, apply, resume after
  killing the helper half way, rollback back to the same owners, modes and
  ACLs (compare `getfacl -R` before and after).
- The deny entry of D7 (decided: stopgap during the warning). As root:
  create a share with one member, have the member `cd` into it in a
  terminal and open a file in an app, revoke the member's access, then
  within the 60 seconds check `getfacl` of the container
  (`user:rast-u<id>:---`), a new `ls ~/Shared/<name>` (expect `EACCES`),
  the open shell (`ls .`, expected to still work), `ls /proc/<shell
  pid>/cwd` from a second process (expected to still work) and the open
  file; after the restart the entry is gone and `id` shows no share
  group. Grant access again during a warning: the entry goes at once.
  A move from write to read gets no entry.
- Stale sessions: revoke a member, stop `rast.service` within the 60
  seconds, start it again: the member gets the warning again and the
  session restarts (`share_session_groups` sees the old gid in
  `/proc/<pid>/status`); leftover deny entries from before the stop are
  removed by the first pass.
- Task 1.3 decisions: a bind mount (`mount --bind`) inside a share is
  left alone by the repair pass and stops `share_dir_remove`; a session
  restart is not triggered by a set-uid program of the user run by another
  account; the first start of 1.0.1 on production (after the maintainer's
  deploy and backup) moves
  `rast.db` and `secret.key` into `private/` (and an older binary still
  starts through the links).
- Task 1.3 review: a FUSE mount by a member inside a share (for example
  `sshfs` or `squashfuse`) is left out of File Station search, size, copy
  and delete (delete answers 403). The hard-link case needs
  `fs.protected_hardlinks = 0`, which must not be set on production; it
  stays covered by unit tests only (no throwaway VM any more).
- The repair pass: after `rast migrate shares`, `mv` a file from HOME into
  the share in a terminal (keeps the user's group), `chgrp` one to the
  user's primary group, then restart `rast.service`: both get the rw group
  and the ACL back, and the journal names the share.

## Pending on the test host (task 1.4)

- XDG Trash with real apps (one-HOME test account, after task 1.5 or a
  test account switched by hand): delete a file in a GTK app (GIO) and in
  an Electron app, see it in File Station's recycle bin and restore it;
  delete in File Station and see it in the app's trash (`gio list
  trash:///`); delete in a test share and check `.Trash-<uid>` (owner, mode
  `2700` or `0700`, no ACL with `getfacl`), that other members cannot list
  it and that the repair pass leaves it as it is.
- Crafted `.trashinfo` as the test account in a terminal (`Path=/etc/…`,
  `..`, a link in HOME pointing out): restore goes to the top of HOME, the
  link is refused, nothing is written outside.
- The usage view and the fallback: install a Flatpak or write into
  `~/.cache` and `~/.local/share/Trash`; after a listing (at most 10 min)
  or opening the usage view the used bytes follow.
- `rast doctor` on production prints "storage quota" with user quota
  `off` and the RAST backend (D1).
- After the maintainer enables Q-a (just before Phase 3): `quota_status`
  says `enforced`, `repquota -u /` shows the D4 limits of the test
  accounts (soft = quota, hard = +1 GiB), an app in the test account stops
  at the hard limit (`EDQUOT`), the usage view says "filesystem", a test
  share limit appears in `repquota -g /` for its rw group and the share
  says "filesystem, best effort".
- Review follow-up (2026-11-17), with a test share that has a size limit
  and two test accounts:
  - Delete into the share's recycle bin as one test account, then upload
    as the other: the limit counts the `.Trash-<uid>` bytes
    (`share_trash_usage`, answered by the helper as root).
  - Two large uploads into the share at once: the second is refused while
    the first runs; cancelling the first frees its bytes.
  - Delete a file of the share through `~/Shared/<name>` in a GTK app (GIO
    puts it in the HOME recycle bin), fill the share, restore in File
    Station: `413` with the limit message, nothing moved.
  - As one test account, create `.Trash-<uid of the other>` at the top of
    the test share: the other account's delete there answers `403` with
    the "belongs to another account" message, the repair pass logs it and
    `rast doctor` lists it.

## Pending on the test host (task 1.5)

Run by the maintainer (or with the maintainer's go-ahead) on production
after the deploy of the pre-release that contains task 1.5, with **test
accounts** (`t-…`) and **test shared folders** only. Use a non-admin test
account first: administrators can open every shared folder (D6), so an
administrator test account can only be migrated once every shared folder
is migrated. `<id>` below is the test account's user id, `<t-name>` its
username, `<share>` a test shared folder.

**Deploy checklist (before `sudo rast update --version 1.0.1-alpha.1`)**

1. Version check: production runs **0.18.3** (`rast version`:
   `RAST 0.18.3 (f69a4544e6fd)`, 2026-11-17). Rollback version: **0.18.3**.
2. Backups, as root, to a folder outside `/srv/rast` (check the space first
   with `df -h / /srv`):

   ```bash
   dest=/root/rast-predeploy-$(date +%Y%m%d-%H%M%S)
   sudo install -d -m 0700 "$dest"
   # Consistent database snapshot (VACUUM INTO) and configuration:
   sudo -u rast rast backup --scope db,config --note "before 1.0.1-alpha.1"
   # The three trees, with numeric owners, ACLs and xattrs (stop rast.service
   # first for a fully consistent copy of /var/lib/rast):
   sudo tar --numeric-owner --acls --xattrs -czf "$dest/etc-rast.tar.gz" -C / etc/rast
   sudo tar --numeric-owner --acls --xattrs -czf "$dest/var-lib-rast.tar.gz" -C / var/lib/rast
   sudo tar --numeric-owner --acls --xattrs -czf "$dest/srv-rast.tar.gz" -C / srv/rast
   sudo sh -c "cd '$dest' && sha256sum *.tar.gz > SHA256SUMS"
   ```

   `rast update` also makes a `db,config` snapshot ("pre-update snapshot
   (running 0.18.3)") in `/var/lib/rast/backups` and keeps the old binary
   as `/usr/local/bin/rast.rollback-0.18.3`.
3. Deploy (maintainer): `sudo rast update --version 1.0.1-alpha.1`, then
   `rast version`, `sudo rast status`, `sudo -u rast rast doctor` and
   `curl -fsS http://127.0.0.1:8720/healthz`.

**Rollback steps (if the deploy must be undone)**

1. With the **new** binary and `rast.service` stopped, undo every
   migration that ran, test accounts first, then test shares:
   `sudo -u rast rast migrate home --user <t-name> --rollback` for each
   migrated test account, then
   `sudo -u rast rast migrate shares --share <share> --rollback` for each
   migrated test share.
2. Migration 0015 makes a binary-only rollback impossible (0.18.3 refuses
   a schema-v15 database), and `rast update --version 0.18.3` run by the
   new binary would open (and keep) the v15 database for its own snapshot,
   then fail to start 0.18.3 and restore the new binary. So, with
   `rast.service` stopped: put the old binary back
   (`sudo install -m 0755 /usr/local/bin/rast.rollback-0.18.3 /usr/local/bin/rast`),
   check the pre-update archive with it
   (`sudo -u rast rast restore --from /var/lib/rast/backups/<pre-update snapshot>.tar.gz --dry-run`),
   restore it (`… --yes`), then restart `rast-native.service` and
   `rast-worker.service` so they run 0.18.3 too, and start `rast.service`.
   `sudo rast update --version 0.18.3` afterwards only confirms the
   version.
3. Health check: `rast version` shows 0.18.3; `sudo rast status`;
   `sudo -u rast rast doctor`; `curl -fsS http://127.0.0.1:8720/healthz`;
   sign in with a test account and open a file and a native app.
4. If anything is still wrong: restore `/etc/rast`, `/var/lib/rast` and
   `/srv/rast` from `$dest` (`tar --numeric-owner --same-owner --acls
   --xattrs -xzf … -C /`, services stopped).

**Tests (test accounts and test shares only)**

- Preparation, in legacy mode: the test account can open only test
  shares; it opens Chrome (sign-in or a bookmark, so its profile holds
  data), a GTK app, and a terminal on a HOME folder (terminal grant);
  it deletes a file in its HOME and one in a test share (legacy recycle
  bin). Save the state:
  `sudo getfacl -R -p /srv/rast/homes/<t-name> > /root/t-acl-before.txt`
  and `sudo find /srv/rast/homes/<t-name> -printf '%u %g %m %p\n' | sort > /root/t-owners-before.txt`.
  The 0.18.3 entries are there:
  `grep -c 'rast-u<id>' /root/t-acl-before.txt` > 0.
- `sudo -u rast rast migrate shares --share <share>` (dry-run, then apply
  with `rast.service` stopped) for every test share of the account.
- Dry run with the server running:
  `sudo -u rast rast migrate home --user <t-name> --dry-run`: the report
  lists the 0.18.3 entries, the Chrome profile merge and its conflicts,
  the recycle bin entries, the projected usage, a stray
  `homes/rast-u<id>` if any, running units; `getfacl`/`find` again show
  no change.
- Refusals: an administrator test account while a real shared folder is
  legacy (names the shared folders, nothing changes); a process of
  `rast-u<id>` that is not in a unit (for example a test `sleep` started
  as that user by the maintainer) gives `identity_in_use`.
- Apply (`rast.service` stopped):
  `sudo -u rast rast migrate home --user <t-name>`. Then:
  `stat -c '%U %G %a' /srv/rast/homes/<t-name>` = `rast-u<id> rast-u<id> 700`;
  `getent passwd rast-u<id>` has the HOME;
  `sudo getfacl -R -p /srv/rast/homes/<t-name> | grep -c 'rast-u'` = 0;
  `sudo ls -l /var/lib/rast/migrations/home/<id>/` has one stamp with
  `journal.jsonl`, `plan.jsonl`, `applied` (root, 0600); the account's
  units of before are stopped, other accounts' are not.
- After `rast.service` starts: File Station shows the HOME through the
  agent; the recycle bin lists the migrated entries (restore one, from the
  HOME and from the test share); Chrome opens with its bookmarks and
  sign-ins (merged into `~/.config/google-chrome`); **a file saved by
  Chrome appears in File Station, and a file uploaded in File Station opens
  in Chrome** (Phase 1 exit); a terminal starts in the HOME and gets no
  ACL grant; a Backup Center `users` backup lists the HOME under
  `warnings`.
- A second apply says "already in one-HOME mode" and leaves the session
  running.
- Rollback (`rast.service` stopped):
  `sudo -u rast rast migrate home --user <t-name> --rollback`; then
  `getfacl -R -p` and `find -printf` compared with the saved files
  (`diff`) show only entries made after the migration (now `rast`-owned);
  the Chrome profile is back in `<data_dir>/linux-apps/profiles/…`; the
  legacy recycle bin lists the entries again; apps start in legacy mode
  with the `RAST` link.
- Migrate the test account again and keep it in one-HOME mode for the
  task 1.4 list ("XDG Trash with real apps") and the Phase 1 exit.

## Next

1. Review of task 1.5 (the new root helper operations `home_migrate_*`,
   the journal and the CLI), before the first deploy.
2. First deploy to production (the maintainer), after that review
   (maintainer's instruction, 2026-11-16): a
   `chore(release): 1.0.1-alpha.1` commit (see "Pre-release check",
   decision), then the deploy checklist under "Pending on the test host
   (task 1.5)", then the "Pending on the test host" lists on production
   with test accounts.
3. Task 1.5b: Backup Center reads HOMEs through the helper (D12) and the
   restore guide moves to `tar --numeric-owner --same-owner --acls`.
4. Task 1.6 (Phase 1 security review): includes the new root helper
   operations of tasks 1.3 and 1.5 (`share_deny_set`, `share_dir_repair`,
   `share_session_groups`, `home_migrate_*`).
5. Production runs 0.18.3 (checked 2026-11-17, `rast version`: 0.18.3
   `f69a4544e6fd`). The maintainer removes the stray `homes/rast-u*`
   folders (read-only listing script in PR #2); `rast migrate home
   --dry-run` lists a stray folder per account.
5. Task 0.6 (deferred): baseline app matrix on production with a test
   account while it still runs 0.18.x, before the first 1.0.1 pre-release
   is deployed. `docs/NATIVE-LINUX-APPS.md` allows only one native RAST
   instance per host (systemd unit names are keyed by user id), so never
   run `dev-instance.sh up native` here.

## Open questions

- **`scripts/e2e/dev-instance.sh down` can stop production sessions
  (found in task 1.5, not changed):** `stop_all` calls
  `stop_native_units` when the instance is in native mode **or** a helper
  log exists in `/tmp/rast-e2e`, and that function stops every
  `rast-app-*` and `rast-xpra-*` unit on the host, which on this host are
  the production users' sessions and apps. A leftover helper log from an
  earlier native run is enough. Proposal: only stop units when the mode
  is native and refuse native mode on a host where `rast-native.service`
  is active, or list the units the dev helper started and stop only
  those. Until then: never run `down` with a helper log in
  `/tmp/rast-e2e`.

- **Migration 0015 and rollback to 0.18.x (task 1.4 review, 2026-11-17):**
  0.18.x cannot open a schema-v15 database (sqlx `VersionMissing(15)`), so
  the rollback steps of every deploy checklist must restore the database
  backup taken before the deploy. Options: (a) keep it so (no code
  change), (b) also release a 0.18.4 that runs its migrator with
  `ignore_missing`, so a binary rollback works again. Which one?
- Project quota for shared folders: deferred until a host supports it
  (maintainer, 2026-11-17; ADR 0005, section 11). Setting project ids
  (FS_IOC_FSSETXATTR with the inherit flag) comes with that host.

- **Root agent token (from the task 1.2 review follow-up):** should
  `RootFs` get the same "ask the helper again once" retry as the per-user
  agents? Its stale-token case is narrower (only when something other than
  this server starts a new root agent on the same socket path), so it was
  left out of this round.
- The root agent token question above stays open on purpose (maintainer,
  2026-11-16: no change now).
- **`rast-core` (deferred in 0.2):** nothing in the code is pure enough for
  `wasm32` yet — `storage::safe_path` still uses `AppError` and the filesystem.
  Create the crate when the pure parts (logical paths, name validation, access
  rules) can be separated; see ADR 0002.
- **Follow-ups from ADR 0002:** move the root package to `crates/rast`; split
  `rast-base`; separate `native_apps`/`native_archive` (now in `rast-session`)
  from the session code, and the terminal manager from the PTY agent; drop the
  workspace dependencies that the root package no longer uses directly.
- **Timing-sensitive tests:** the two known flaky tests were fixed in
  `2b9b8a5` (see Done). Any other test that holds a `flock` while sibling tests
  fork processes has the same risk; keep such tests in their own binary.
  Seen on 2026-11-17 under a full workspace run:
  `native::shares::tests::operations_on_one_shared_folder_never_overlap`
  (rast-helper) failed at the final `shares_migrate_rollback` after the
  test dropped its `flock`, because a sibling test's forked child still
  held a copy of the descriptor. Fixed in task 1.5 (`2ff2fa2`): the
  migration lock is retried for 500 ms; five crate runs and the full run
  passed since.
- Does `scripts/changelog-section.sh` accept the `## [1.0.1] - Unreleased`
  heading? Yes: it only reads the text inside the square brackets (checked by
  the maintainer).
- Which release is the cut-off for removing the legacy Docker/Selkies Linux-app
  backend (Phase 4)?
- `yamllint` is not installed for the default `python3` on this host (a
  RAST-managed Python comes first in `PATH`). Lint locally without
  installing anything system-wide:
  `uvx yamllint -c .github/yamllint.yml .github/workflows` and
  `uvx --from actionlint-py actionlint` (both clean on 2026-11-16).

## Repository map

See `CLAUDE.md` (Repository map) and ADR 0002 for the crate layout.
