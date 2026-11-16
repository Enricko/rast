# Remake progress

Living log for the RAST remake (see `CLAUDE.md` and `docs/adr/0001-incremental-remake.md`).
Update it at the end of every session.

- **Current phase:** Phase 1 — One HOME (tasks 1.1 and 1.2 done, cross-user and native e2e tests of 1.2 pending on the test host; next task 1.3). Phase 0 closed on 2026-11-16, task 0.6 deferred
- **Branch / version:** `Remake` / 1.0.1 (not bumped per phase; releases are pre-release only until the final remake gate)
- **Last updated:** 2026-11-16

## Phase 0 checklist

- [x] Split the repo into the target workspace with pure moves (task 0.2, ADR 0002)
- [x] Create `rast-api` with `EntryDto` and `ListingDto` (from `crates/rast-base/src/storage/ops.rs`) and generate TypeScript types with `ts-rs` (task 0.3)
- [x] Create `rast-ui` with one Leptos component mounted in a RAST window, built with Trunk, embedded with `rust-embed`
- [x] Record the current compressed JS bundle size as the baseline
- [ ] Run the app matrix on 0.18.x and store results in `tests/e2e/results/` (task 0.6, **deferred** by the maintainer: it runs on a separate test host with RAST 0.18.2 before the Phase 1 results are tested there)
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
  - Open findings for the maintainer: see "Open questions".

## Phase 1 checklist

- [x] Task 1.1: one-HOME design, `docs/adr/0005-one-home.md` (accepted 2026-11-16)
- [x] Task 1.2: per-user file agent (unit and fake-helper integration tests; the tests below under "Pending on the test host" are still open)
- [ ] Task 1.3: shared folders as Linux groups
- [ ] Task 1.4: XDG Trash and quota
- [ ] Task 1.5: HOME migration command
  - `rast migrate home` must also remove the default ACL entries
    (`d:u:rast-u<id>`) that 0.18.3 adds to every folder in the person's home,
    together with the access entries `u:rast-u<id>:rx`, and `--rollback` must
    restore both.
- [ ] Task 1.6: Phase 1 security review
- [ ] **Exit:** a file saved by Chrome appears in File Station and vice versa; cross-user tests pass; dry-run and rollback tested on the disposable instance

## In progress

- Nothing.

## Pending on the test host (task 1.2)

These need root, real `rast-u<id>` accounts or a native RAST instance, so
they run on the separate test host (only one native RAST per host, and this
host runs production). Until then 1.2 is covered by unit tests and
`tests/homefs_e2e.rs` (real router, real agent running as the test user,
fake helper).

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

## Next

1. Task 1.3 (shared folders as Linux groups); it also creates the
   `~/Shared/<name>` links the agent already resolves.
2. Run the "Pending on the test host" list above on the test host.
3. 0.18.3 (D13) is released: the maintainer installs it on production, then
   removes the stray `homes/rast-u*` folders (read-only listing script in
   PR #2).
4. Task 0.6 (deferred): baseline app matrix on a separate test host with RAST
   0.18.2, before the Phase 1 results are tested there.
   `docs/NATIVE-LINUX-APPS.md` allows only one native RAST instance per host
   (systemd unit names are keyed by user id), and production uses it on this
   host. Do not run `dev-instance.sh up native` here.

## Open questions

- **From the task 1.2 security review (need a decision before task 1.3
  creates `~/Shared` links or gives members kernel access to shares):**
  - The server checks Effective Access by share *name*, but the agent
    resolves `/shared/<name>` through `~/Shared/<name>`, a link the user can
    repoint to any container of the shared root. The kernel still bounds
    access, but audit entries and realtime audiences then name the wrong
    share. Proposal: the server sends the share's container from the
    database (a server-only header, like the quota header) and the agent
    accepts the link only when it points there, or uses the container
    directly. Touches ADR 0005, section 2.
  - Copy, move and upload in the agent use path-based calls
    (`std::fs::copy` follows a symlink at the target; intermediate folders
    are looked up again on every call). Once other members can write a
    share, they can redirect user A's agent with planted links, for example
    copying A's `~/.ssh` key into a share they read, or writing files into
    A's `~/.config/autostart`. ADR 0005 says a race only reaches what the
    agent's uid can reach, which is true but misses this confused-deputy
    case. Proposal: `openat`/`O_NOFOLLOW`/`O_EXCL` operations on directory
    descriptors for everything in shared folders, before task 1.3.
  - Lower priority: the server holds one lock for all accounts while the
    helper starts an agent (one slow start stalls every one-HOME account);
    agent errors on mkdir, rename and copy come back as `400` instead of
    `403`/`404`; a socket unit restarted outside RAST leaves the server
    with a stale token (`403` until the server restarts); deactivating an
    account does not stop its agent yet (ADR 0005, section 7); extra unit
    hardening beyond the ADR (`ProtectKernelTunables`,
    `ProtectKernelModules`, `ProtectControlGroups`, `RestrictNamespaces`,
    `LockPersonality`, `RestrictSUIDSGID`, `SystemCallFilter=@system-service`)
    to be tried on the test host.
- Task 1.3 results for the maintainer: group quota as the share limit under
  Q-a (D3) and the deny ACL entry instead of a forced restart (D7).
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
- Does `scripts/changelog-section.sh` accept the `## [1.0.1] - Unreleased`
  heading? Yes: it only reads the text inside the square brackets (checked by
  the maintainer).
- Which release is the cut-off for removing the legacy Docker/Selkies Linux-app
  backend (Phase 4)?
- `yamllint` is not installed for the default `python3` on this host (a
  RAST-managed Python comes first in `PATH`), so the workflow files were not
  linted locally.

## Repository map

See `CLAUDE.md` (Repository map) and ADR 0002 for the crate layout.
