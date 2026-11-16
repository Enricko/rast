# CLAUDE.md — RAST remake

Instructions for Claude Code in this repository. Read this whole file at the
start of every session, then read `docs/remake/PROGRESS.md`.

## Mission

RAST 0.18.x is a self-hosted server administration toolkit with a web desktop:
one Rust binary (Axum + SQLite) that embeds a TypeScript/Vite frontend. Native
Linux apps already run per user inside an Xpra seamless session and are adopted
into the RAST window manager.

The remake turns RAST into a **Linux Web OS, full-stack Rust**:

1. Every RAST user is a real Linux account (`rast-u<id>`) with **one HOME**,
   shared by File Station, Terminal and every app.
2. **Every installed Linux app** (`.deb`, Flatpak, AppImage, or anything already
   on the host) shows up in the launcher and runs as a RAST window: Chrome,
   Firefox, VS Code, LibreOffice, Electron apps.
3. The frontend moves from TypeScript to **Leptos (Rust → WASM)**, app by app.
   Only xterm.js and the Xpra HTML5 client stay JavaScript, behind thin
   `wasm-bindgen` bindings.

This is an **incremental remake inside this repo, not a rewrite from scratch.**
RAST must keep working, and the existing tests must stay green, at every commit.

## Language

- Talk to the maintainer in **Bahasa Indonesia**.
- Write code, comments, docs, commit messages and CHANGELOG entries in
  **English** (repository convention).

## Production safety (read first)

The development host runs the **production** RAST, `desktop.lufria.tech`
(`127.0.0.1:8720`, behind nginx on `8721`). **Production is also the test
host**: there is no separate VM or VPS (maintainer's decision, 2026-11-16).

- Never stop, restart, reconfigure, upgrade or reinstall it. Never touch
  `rast.service`, `rast-worker.service`, `rast-native.service`,
  `rast-linux.slice`, `/etc/rast`, `/var/lib/rast` or `/srv/rast`.
- **Only the maintainer deploys to production**: a pre-release cut from
  `Remake`, installed with `sudo rast update --version <version>`. Never run
  `rast update`, the installer or a release yourself.
- **Before every deploy**, prepare a checklist for the maintainer: a backup
  of `/etc/rast`, `/var/lib/rast` and `/srv/rast` (commands and target
  location), the rollback version (the version production runs before the
  deploy), and the rollback steps (first `--rollback` of every migration
  that ran for test accounts, with the new binary; then
  `sudo rast update --version <rollback version>`; then a health check).
- **Tests on production use test accounts only** (usernames starting with
  `t-`) and test shared folders. Never log in as, change, or read the files
  of a real account, and never change a real shared folder.
- On production, `rast migrate shares` and `rast migrate home` run **only
  for test accounts and test shares** (always with `--share <name>` or the
  per-account option, never for all) until the maintainer approves the
  migration of real accounts. Steps that need `rast.service` stopped are
  run by the maintainer.
- Disposable instance: `scripts/e2e/dev-instance.sh` (`/tmp/rast-e2e`, port
  `8731`) in **docker mode only**.
- **Only one native RAST instance may run per host**: systemd unit names are
  keyed by user id (`rast-xpra-u<id>`, `rast-app-u<uid>-…`). Never run
  `scripts/e2e/dev-instance.sh up native` or start a second
  `rast native-helper`. Native tests use the production instance with test
  accounts, after the maintainer has deployed the build under test.
- "Pending on the test host" in `docs/remake/PROGRESS.md` means: tests on
  production with test accounts, after a deploy.
- Never run the installer (`installer/install.sh`) on this host. Use
  `scripts/test-installer-clean.sh`, which runs in containers.

## Versioning and releases

- The whole remake uses version **1.0.1** on the `Remake` branch. Do not bump
  the version per phase or per task.
- **Pre-releases are `1.0.1-alpha.N`** (maintainer's decision, 2026-11-16).
  Each one gets its own commit `chore(release): 1.0.1-alpha.N` that sets the
  version in `Cargo.toml` and `Cargo.lock` to `1.0.1-alpha.N`, so
  `rast version` on production always shows which build is installed. That
  commit is the only exception to "do not bump". Plain `1.0.1` (no suffix)
  is used only for the final release, after the final remake gate.
- Any GitHub release cut from this branch **must be a pre-release** until
  the final remake gate passes. `rast update` (`src/update.rs`,
  `/releases/latest`) offers the newest non-pre-release to every installed
  server, production included.
- The Release workflow enforces this through `scripts/release-check.sh`
  (tests in `tests/release_scripts.rs`): a version with a `-suffix` is
  always published as a pre-release, from a pushed tag or a manual run; a
  version without a suffix is refused unless its commit is on `main`, even
  with `prerelease` ticked.
- `CHANGELOG.md` keeps one `## [1.0.1]` section for every pre-release:
  `scripts/changelog-section.sh` maps `1.0.1-alpha.N` to it (a section of
  its own wins if there is one). Do not add `[1.0.1-alpha.N]` headings.
- Record all remake changes under `## [1.0.1] - Unreleased` in `CHANGELOG.md`
  until the maintainer releases it. Never tag, push or publish without being
  asked.

## Every session

1. Read `docs/remake/PROGRESS.md` (create it in Phase 0): current phase, what is
   done, what is next, open questions. Before touching an area, read its doc:
   `docs/ARCHITECTURE.md`, `docs/NATIVE-LINUX-APPS.md`, `docs/TERMINAL.md`,
   `docs/API.md`, `docs/REALTIME.md`.
2. Work on **one phase at a time, in order** (see Roadmap). Do not start a
   phase until the previous phase's exit criteria pass.
3. For changes that touch more than one module, plan first: list the files you
   will change and the tests you will add, then implement.
4. Make small, reviewable commits using Conventional Commits (`feat:`, `fix:`,
   `refactor:`, `test:`, `docs:`, `chore:`, `perf:`, `ci:`, `build:`). Pure
   moves and refactors go in their own commits, with no behaviour change.
5. Run the quality gates before every commit. Never commit with a failing
   gate, and never weaken or delete a test to make it pass.
6. At the end of the session, update `docs/remake/PROGRESS.md` and add
   `CHANGELOG.md` entries under `[Unreleased]` for user-visible changes.
7. Record significant decisions as short ADRs in `docs/adr/NNNN-title.md`
   (context, decision, consequences).

## Models and effort

The maintainer starts each session with a model and effort chosen for the task
(`claude --model <alias> --effort <level>`). The prompt pack with the
recommended setting for every task is `docs/remake/PROMPTS.md`.

- If the task is clearly above the current setting (root helper operations,
  auth, data migration, the Xpra input path), say so before starting and
  suggest `opus` with `xhigh`.
- Subagents (the Agent tool's `model` parameter): `haiku` for read-only
  searches, inventories and summaries of logs or CI output; `sonnet` for
  well-specified implementation subtasks and test writing; `opus` for design
  and security reviews.
- Never delegate edits to the root helper, auth, migrations or storage code to
  a `haiku` subagent.

## Ask the maintainer before

- Adding or changing anything in `migrations/` (forward-only; owned by the
  project lead).
- Breaking the public API (`docs/API.md`), config keys
  (`deploy/rast.toml.example`, `.env.example`), CLI output or on-disk layout.
- Adding a dependency (crate or npm package). State why, its license and its
  size impact.
- Running anything as root, installing packages on the host, or touching
  systemd units, nftables or the paths listed under Production safety.
- Removing a code path that existing installs still use (for example the
  legacy Docker backend) before its migration has shipped.

## Quality gates

Run before every commit:

```bash
cargo fmt --all -- --check
cargo clippy --locked --workspace --all-targets -- -D warnings
cargo test --locked --workspace --all-targets
npm --prefix frontend ci              # first time, or after lockfile changes
npm --prefix frontend test
npm --prefix frontend run build       # tsc --noEmit + vite build
```

When relevant:

```bash
scripts/generate-third-party-licenses.sh --check   # after adding or updating deps (regenerate without --check)
scripts/sync-installer-assets.sh --check           # after editing deploy/* (run without --check, commit installer/install.sh)
shellcheck -s sh -x installer/install.sh installer/uninstall.sh
shellcheck -s bash -x scripts/*.sh
```

Once `crates/rast-ui` exists, also run its Trunk release build and the WASM
size check in CI.

Test rules (from CONTRIBUTING.md): tests must not need Docker, network, root or
a live systemd; use `tempfile`; never touch `/etc/rast` or `/var/lib/rast` in
tests; prefer injected clocks over sleeps.

## Running locally

```bash
rustup show                                    # installs the toolchain pinned in rust-toolchain.toml
(cd frontend && npm ci && npm run build)       # the frontend is embedded into the binary
source scripts/dev-source-env.sh               # RAST_* -> target/dev-data, target/dev-storage
cargo run -- serve                             # http://127.0.0.1:8720 — ONLY on a machine without production RAST
cargo run -- user create-admin admin --password-stdin   # second terminal, after sourcing the env script
```

On the production host, use the disposable e2e instance instead:

```bash
scripts/e2e/dev-instance.sh build              # snapshot binary + frontend
scripts/e2e/dev-instance.sh up docker          # never `up native` here (see Production safety)
(cd tests/e2e && npm install && npx playwright install chromium)
node tests/e2e/run.mjs --list
node tests/e2e/run.mjs --mode=docker --scenario=all
scripts/e2e/dev-instance.sh down
```

## Repository map (today)

Code lives in the root package `rast` and in the crates under `crates/` (ADR
0002). Module paths inside a crate are unchanged: `crate::config::Config` is
still `crates/rast-base/src/config.rs`.

| Path | What it is |
| --- | --- |
| `src/main.rs`, `src/update.rs`, `build.rs`, `src/lib.rs` | The `rast` binary: CLI (`serve`, `doctor`, `user`, `backup`, `restore`, `update`, `worker`, `native-helper`, agents), self-update, Git revision, and `pub use` of every module so tests and `main.rs` keep their paths |
| `crates/rast-server/src/api/` | Axum routes (REST + SSE) |
| `crates/rast-server/src/{state,backup,monitor,server}.rs` | App state, backups, monitoring, embedded frontend (`server.rs`, with `build.rs` that fails a release build when the frontend is missing) |
| `crates/rast-base/src/auth/` | Argon2id, server-side sessions, CSRF, rate limiting |
| `crates/rast-base/src/storage/` | File Station backend: safe paths, quota, trash, shares, chunked uploads, file streaming |
| `crates/rast-base/src/` (others) | `error`, `config`, `db` (embeds `migrations/`), `events`, `secrets`, `users`, `access`, `audit`, `jobs`, `realtime`, `appearance` |
| `crates/rast-agent/src/rootfs/` | Root file agent: the File Station API over a real filesystem, started by the helper. **Template for the per-user file agent.** |
| `crates/rast-agent/src/terminal/` | PTY agent that runs as `rast-u<id>`, and the terminal manager |
| `crates/rast-helper/src/native/` | Root helper: JSON-lines protocol, identities, transient units, nftables |
| `crates/rast-session/src/linux_apps/native_*.rs` | Xpra sessions, native app install/update |
| `crates/rast-server/src/native_proxy.rs`, `crates/rast-server/src/xpra_shim.js`, `frontend/src/shell/xpra-native.ts` | Authenticated Xpra proxy and window adoption from the Xpra HTML5 client (iframe + injected shim) |
| `crates/rast-session/src/linux_apps/{mod.rs,proxy.rs,shim.js}`, `crates/rast-docker/src/docker/linux.rs`, `crates/rast-docker/src/worker/linux.rs`, `crates/rast-docker/src/worker/linux_assets/`, `frontend/src/shell/linux-windows.ts` | **Legacy** Docker/Selkies + noVNC backend for Linux apps — remove in Phase 4 |
| `crates/rast-apps/src/packages/` | App Center: runtime catalog, GitHub packages |
| `crates/rast-docker/src/{docker,worker}/` | Docker Compose app and its privileged worker (keep) |
| `frontend/src/` | TypeScript UI: `core/wm.ts` window manager, `shell/`, `apps/*` |
| `migrations/` | SQLx migrations (forward-only) |
| `deploy/`, `installer/` | systemd units, config template, POSIX-sh installer |
| `tests/` | Rust integration tests (`tests/*.rs`) and `tests/e2e/` Playwright scenarios and perf harness |

## Security invariants (never break)

- `rast serve` runs as the unprivileged `rast` user. It never runs privileged
  commands and never opens the Docker socket.
- The root helper is the only root component: strict JSON-lines protocol,
  `deny_unknown_fields`, per-field validation, no shell, bounded timeouts.
  Every new privileged action is a new, narrowly validated helper operation
  with tests — never a generic "run this command".
- Per-user isolation is enforced by Linux uids. Agents, sessions and apps of a
  user run as `rast-u<id>` only. Cross-user access must fail (`EACCES`/403) and
  must stay covered by tests.
- A per-user agent runs with the same uid as every app of that user, so the
  server treats all agent output as untrusted input: strict parsing, size
  limits, and headers the server sets itself (only file headers of a reply
  pass; share containers and quota go to the agent in server-only headers).
  Agent file operations work on directory descriptors (`openat2` with
  `RESOLVE_BENEATH`, no symlinks followed), never on re-resolved paths.
- Mutating HTTP requests require session + CSRF. Display WebSockets keep the
  5-second login revalidation and same-origin checks.
- All path handling, including inside agents, goes through the safe-path layer:
  no traversal, no symlink escape.
- Never add `--no-sandbox` to Chromium.
- Never store user passwords. Keyring unlock passes the password once via
  stdin, then drops it. Never log it.
- Xpra stays an external program (GPL server; MPL HTML5 client served from the
  installed package). Never vendor, link or copy Xpra code into this repo.
- No secrets in the repo. Test values must be obvious dummies (`example.test`,
  `192.0.2.0/24`).

## Target architecture

- **Browser:** `rast-ui` (Leptos/WASM) renders the shell, window manager,
  launcher and all apps. JavaScript kept: xterm.js and the Xpra HTML5 client.
- **`rast serve`** (Axum, user `rast`): auth, REST + SSE (same contract as
  today), authenticated proxy for Xpra streams, embedded UI.
- **`rast native-helper`** (root): identities, linger, systemd units, APT,
  nftables.
- **Per-user session**, under that user's systemd user manager: `rast-agent`
  (file API, PTY, portal backend, notifications, tray host), the Xpra session
  (X11 now, Wayland later) and the apps.
- **One HOME per user:** `/srv/rast/homes/<username>`, owned by `rast-u<id>`.

Target workspace (still ONE binary with subcommands). Task 0.2 created the
crates marked "exists"; the others come with their phase:

```text
crates/rast-api      shared DTOs (serde; ts-rs during the transition)               (task 0.3)
crates/rast-core     pure logic: logical paths, access rules, name validation
                     (native + wasm32)                                               (deferred, see PROGRESS.md)
crates/rast-base     exists. Foundation: error, config, db + migrations, events, secrets,
                     users, auth, access, audit, jobs, storage, realtime, appearance.
                     Deviation from the original target: it breaks the dependency cycles
                     (ADR 0002) and will be split later.
crates/rast-docker   exists. Docker Compose app + worker (docker, worker)
crates/rast-apps     exists. App Center runtime catalog (packages). Launcher index,
                     Flatpak, AppImage come in Phase 3
crates/rast-helper   exists. Root helper (native)
crates/rast-agent    exists. rootfs and terminal (file agent, PTY agent, terminal manager).
                     Portal, notifications and tray come later
crates/rast-session  exists. Xpra sessions, app launch and the legacy Docker Linux-app
                     backend (linux_apps)
crates/rast-server   exists. api, backup, monitor, state, server (frontend embed),
                     native_proxy (+ xpra_shim.js)
crates/rast-ui       Leptos CSR app                                                  (task 0.4)
crates/rast          the single binary. For now the root package `rast` (src/main.rs,
                     update.rs, build.rs) re-exports every module so tests/*.rs
                     and main.rs keep their paths. It moves to crates/rast later.
```

Dependency order: `rast-base` < `rast-docker` < `rast-apps` < `rast-helper` <
`rast-agent` < `rast-session` < `rast-server` < `rast`. A lower crate must never
name a higher one; talk through a trait (see `AuthState`, `HasLinuxApps` and
`packages::service::Helper`). Each crate keeps the old module names, and its
`lib.rs` re-imports the modules of lower crates, so `crate::config::Config`
still resolves.

`rast-core` and `rast-api` must build for `wasm32-unknown-unknown`: no
filesystem, process or tokio code in them.

## Roadmap

Do phases in order. Week numbers are estimates for one full-time developer.

### Phase 0 — Foundation (weeks 1–2)

- Split the repo into the workspace above with pure moves; all tests pass
  unchanged.
- Create `rast-api`, starting with `EntryDto` and `ListingDto` from
  `src/storage/ops.rs`. Generate their TypeScript types with `ts-rs` and use
  them in `frontend/src/apps/files/types.ts`.
- Create `rast-ui` with one Leptos component mounted inside a RAST window,
  built with Trunk next to Vite and embedded with `rust-embed`. Record the
  current JS bundle size as the baseline.
- Run the app matrix on 0.18.x as a baseline; store results in
  `tests/e2e/results/`.
- Create `docs/remake/PROGRESS.md` and the first ADRs. Fix the README: the
  duplicated "Linux apps" row and the stale "v0.13.3" status line.
- **Exit:** CI green, still a single release binary, no behaviour change.

### Phase 1 — One HOME (weeks 3–6)

- Per-user file agent (generalise `src/rootfs/agent.rs`), started by the helper
  as `rast-u<id>`. `/api/files` requests for Home are forwarded to it with the
  same JSON shapes.
- HOME = `/srv/rast/homes/<username>`, owned by `rast-u<id>`, used by Terminal
  and all apps. Per-app profiles become opt-in. Remove the per-launch
  `setfacl -R` in `start_run` and the read-only `RAST` symlink.
- Shared folders become Linux groups (read-write and read-only), setgid
  directories with default ACLs, linked under `~/Shared`. An access change
  restarts the user's session, with a warning. `rast migrate shares`
  (`--dry-run`, `--rollback`) applies the groups and ACLs to existing shared
  folders before `rast migrate home`.
- Recycle bin uses XDG Trash (`~/.local/share/Trash`). Quota uses filesystem
  user quota (project quota for shared folders).
- `rast migrate home --dry-run` and `--rollback`, per user and idempotent, with
  a backup of owners and modes. Backup Center reads homes through the helper.
- **Exit:** a file saved by Chrome appears in File Station and vice versa;
  cross-user tests pass; dry-run and rollback tested on production with test
  accounts and test shares (see Production safety).

### Phase 2 — Desktop session (weeks 7–9)

- Enable linger for `rast-u<id>`. Run the session, agent and apps as user units
  (`systemd-run --user --machine=rast-u<id>@.host`), keeping the cgroup limits.
- App units live as long as they have processes (ADR 0004): `ExitType=cgroup`
  on systemd 250+, and on older systemd a RAST subreaper wrapper as the main
  process, with the same contract tests. `KillMode=control-group` stays, so
  closing a launcher such as JetBrains Toolbox no longer kills the apps it
  opened, while End application still stops everything in the unit.
- Resource limits: the main CPU and RAM limit applies per user session (one
  per-user slice holding the session, the agent and every app). Per-app limits
  may stay, but must never make an app opened from a launcher run out of
  memory (proposed: per-app `MemoryHigh` and `OOMPolicy=continue`). Changing
  the meaning of the `linux_app_*` keys needs the maintainer's approval.
- One D-Bus session bus and `XDG_RUNTIME_DIR` for all apps; PipeWire or
  PulseAudio per user.
- Keyring: at login, `gnome-keyring-daemon --unlock` receives the password via
  stdin. Remove `--password-store=basic` for Chromium.
- Portal backend in Rust: `ashpd` backend (`FileChooserImpl`) on `zbus`. File
  dialogs open as the RAST File Station picker.
- Links opened by apps (`xdg-open`, portal OpenURI) reach the user's browser
  tab through a RAST URL handler registered for `x-scheme-handler/http` and
  `x-scheme-handler/https`.
- `org.freedesktop.Notifications` in the agent, forwarded to RAST toasts.
- **Exit:** Chrome stores a password in the keyring; a Flatpak app's Open
  dialog shows the RAST picker; app notifications reach the browser.

### Phase 3 — Launcher for every app (weeks 10–11)

- Index `.desktop` files from `/usr/share/applications`,
  `/usr/local/share/applications`, `~/.local/share/applications` and the
  Flatpak exports (system and user) with `freedesktop-desktop-entry`. Watch
  them with `notify`, so a shortcut that a launcher app writes to
  `~/.local/share/applications` appears at once. Honour `NoDisplay`, `Hidden`
  and `OnlyShowIn`; take icons from icon themes (including
  `~/.local/share/icons`).
- Apps opened by a launcher app (ADR 0004): each window is matched to its own
  `.desktop` entry and shows its own name and icon in the taskbar, never the
  launcher's. End application on a launcher warns that the apps it opened
  close too. A "Pin to launcher" window action writes a `.desktop` file in the
  user's HOME for a window that has no shortcut.
- Apps installed from inside a launcher go into the user's HOME and need no
  root. `sudo`, `pkexec` and `apt` from an app fail at once with a clear
  message that points to an administrator and App Center. No shortcut to root.
- Flatpak per user (Flathub, `flatpak --user`) in App Center. `.deb` stays
  admin-only. Re-enable AppImage after it passes the matrix. AppImages that a
  launcher downloads run without FUSE (`APPIMAGE_EXTRACT_AND_RUN=1`).
- "Open with" in File Station via `mimeapps.list`.
- From this phase on, **all new UI is written in Leptos.**
- **Exit (Beta gate):** Chrome, Firefox, VS Code and LibreOffice pass the app
  matrix, and ArtCraft Launcher and JetBrains Toolbox pass the "launcher in a
  launcher" scenario.

### Phase 4 — Compatibility and display (weeks 12–14)

- Wrap the Xpra shim behind one versioned adapter with contract tests in CI
  (start from `scripts/xpra-poc`). Pin the Xpra version that passes.
- Try the Wayland server in Xpra 6.5 for Wayland-native apps; keep X11 and
  XWayland as the fallback.
- HiDPI: session DPI and canvas scale follow `devicePixelRatio`, replacing the
  pinned DPI 96.
- Tray: StatusNotifierItem host in the agent, shown in the taskbar. Add a
  "Paste into app" window action for the clipboard.
- Remove the legacy Docker/Selkies Linux-app backend and noVNC, after their
  migration path has shipped.
- **Exit:** Discord, Slack, Telegram, GIMP and Thunderbird pass the matrix.

### Phase 5 — Frontend to Leptos (weeks 15–26)

- Port in this order: Activity Logs → Resource Monitor → Control Panel →
  Backup → File Station → App Center → Terminal (xterm.js binding) → shell,
  window manager and Xpra integration last.
- Keep the REST + SSE API; use `rast-api` types on both sides.
- When nothing depends on them any more, remove TypeScript, Vite and Node from
  the build. Ship xterm.js as a vendored static asset with its license.
- **Exit (final remake gate):** no TypeScript beyond thin bindings; `npm` not needed
  to build; compressed WASM bundle at most 1.5× the Phase 0 JS baseline; full
  app matrix green.

### Phase 6 — RAST OS image (weeks 27–28)

- mkosi image with RAST, Xpra, Flatpak, PipeWire, acl and nftables
  preinstalled; first-boot setup in the browser with a one-time token.

### Phase 7 — Research (optional, after the final remake gate)

- A Rust/WASM Xpra client (WebCodecs) or an own Wayland compositor (Smithay).
  Only if the Phase 4 adapter keeps causing problems.

## App matrix ("works" means)

An app works when its e2e scenario passes all five checks: it appears in the
launcher, opens as a RAST window, accepts typing, a file it saves is visible in
File Station immediately, and it closes normally.

| App | Source | Phase |
| --- | --- | --- |
| Google Chrome | `.deb` | 1 |
| Firefox, VS Code, LibreOffice, GIMP, Inkscape, Thunderbird | Flatpak or `.deb`/`apt` | 3 |
| Discord, Slack (Electron), Telegram (Qt) | Flatpak | 4 |
| ArtCraft Launcher, JetBrains Toolbox ("launcher in a launcher") | Vendor download into HOME | 3 |

**Launcher in a launcher.** For apps that install and open other apps (ADR
0004). The launcher itself must pass the five checks above. The scenario then
installs one app through the launcher and passes when that app opens as a
RAST window, keeps running after the launcher is closed, and its shortcut
appears in the RAST launcher.

Not targeted: 3D games and video editors (GPU and latency), webcam, microphone,
USB and printers, and apps that modify the system (drivers, kernel modules).

## Leptos and WASM conventions

- Use the latest **stable** Leptos (0.8.x as of November 2026). Move to 0.9
  only after it is stable, and only between phases.
- CSR built with Trunk 0.22. In `crates/rast-ui/index.html`:
  `<link data-trunk rel="rust" data-wasm-opt="z" data-cargo-profile-release="wasm-release" />`.
- Root `Cargo.toml` gets a WASM profile (leave `[profile.release]` as it is):

  ```toml
  [profile.wasm-release]
  inherits = "release"
  opt-level = "z"
  lto = true
  codegen-units = 1
  panic = "abort"
  ```

- During the migration, Leptos components are mounted into RAST window bodies,
  so Leptos apps and TypeScript apps run side by side.
- xterm.js and the Xpra client are each reached through exactly one binding
  module.
- Respect the CSP (`default-src 'self'`): no CDNs; every asset is embedded in
  the binary.

## Docs to keep in sync

`README.md`, `docs/*.md`, `deploy/rast.toml.example`, `.env.example`,
`CHANGELOG.md` (add an "Upgrade note" for migrations and breaking changes) and
`docs/THIRD-PARTY-LICENSES.md` (regenerated by the script).

## Definition of done (every change)

- Quality gates green; new behaviour tested (unit tests, plus e2e when it is
  user-visible).
- Security invariants intact.
- Docs, CHANGELOG and `docs/remake/PROGRESS.md` updated.
