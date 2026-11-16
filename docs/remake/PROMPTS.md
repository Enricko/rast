# Claude Code prompts for the RAST remake

Ready-to-paste prompts for working through the RAST remake plan with Claude
Code, with the model and effort for each task. Keep this file as
`docs/remake/PROMPTS.md`, and `CLAUDE.md` in the repo root.

## How to use

1. One task = one session. Open a terminal in the RAST repo root, run the
   task's `claude …` command, then paste its prompt.
2. Tasks labelled **plan mode** use `--permission-mode plan`: Claude Code drafts
   a plan and waits for your approval before changing files. With the
   `opusplan` model, Opus makes the plan and Sonnet does the work after
   approval.
3. In a running session, switch with `/model <name>` then `/effort <level>`.
   Type them separately, each followed by Enter.
4. For one turn that needs deeper thinking without raising the session effort,
   add the word `ultrathink` to the prompt.
5. Work in order. Do not move to the next phase before the previous phase's
   "Exit" criteria pass (see `CLAUDE.md`).
6. Replace text inside `<…>` before sending a prompt.

## Choosing model and effort

Opus 5.5, Sonnet 5.5 and Haiku 5.5 support the effort levels `low`, `medium`,
`high`, `xhigh` and `max`. The default is `medium`.

| Kind of work | Model | Effort |
| --- | --- | --- |
| Design, data migration, root helper code, auth, Xpra input debugging | `opus` | `xhigh` |
| End-of-phase security review, Phase 7 research | `opus` | `max` |
| Large refactors that need a plan first | `opusplan` | `high` |
| Multi-file features with a clear spec | `sonnet` | `high` |
| Routine code, tests, porting simple apps to Leptos, CI fixes | `sonnet` | `medium` |
| Updating PROGRESS/CHANGELOG/README, searching code, summarising errors | `haiku` | `low` |

Start with the cheapest row that still fits. If the result is not careful
enough, raise the effort one level before changing the model.

---

## Phase 0 — Foundation

### 0.1 Progress log and ADRs

```bash
claude --model sonnet --effort medium
```

```text
Read CLAUDE.md in full, then study the repo structure (src/, frontend/src/, docs/, tests/e2e/).
Create docs/remake/PROGRESS.md: current phase (Phase 0), the Phase 0 task checklist from CLAUDE.md, and the sections Done, In progress, Next and Open questions.
Create docs/adr/0000-template.md (context, decision, consequences) and docs/adr/0001-incremental-remake.md recording the decision to remake incrementally in the same repo.
Do not change any code. Commit: "docs: add remake progress log and ADR template".
```

### 0.2 Split the repo into a workspace (plan mode)

```bash
claude --model opusplan --effort high --permission-mode plan
```

```text
Do the Phase 0 task "split the repo into a workspace" following the Target architecture section of CLAUDE.md.
Rules: move code only, no behaviour change, still one rast binary, and every quality gate passes.
Plan first: which crates, which modules move to which crate, the order of the moves (one crate per commit), and the risks (dependency cycles, include_str! such as xpra_shim.js, rust-embed for frontend/dist, sqlx migrations, build.rs).
After I approve the plan, work crate by crate. Run the gates after each crate and commit "refactor: move <module> into crates/<crate>".
Record the decision in docs/adr/0002-workspace-layout.md and update PROGRESS.md.
```

### 0.3 Shared types in rast-api + ts-rs

```bash
claude --model sonnet --effort medium
```

```text
Create the rast-api crate following CLAUDE.md. Move EntryDto and ListingDto (from src/storage/ops.rs) into rast-api and add the ts_rs::TS derive with export.
Generate the TypeScript types into frontend/src/generated/ and use them in frontend/src/apps/files/types.ts in place of the hand-written definitions.
rast-api must build for wasm32-unknown-unknown.
Dependency I approve for this task: ts-rs (latest stable version).
Run the gates, regenerate docs/THIRD-PARTY-LICENSES.md, update PROGRESS.md, then commit.
```

### 0.4 rast-ui skeleton (Leptos)

```bash
claude --model sonnet --effort high
```

```text
Create the rast-ui crate (Leptos CSR, latest stable version) with one small component, "About RAST (Leptos)", shown inside a RAST window.
Build it with Trunk following the Leptos and WASM conventions section of CLAUDE.md (wasm-release profile, wasm-opt z). Embed the build output in the binary with rust-embed next to frontend/dist, with no CDN.
Register the app in frontend/src/apps/registry.ts; its window calls the mount function from the WASM module.
Add the rast-ui build and the WASM size check to CI. Record the current JS bundle size (frontend/dist, compressed) as the baseline in PROGRESS.md.
Dependencies I approve: leptos, wasm-bindgen, web-sys, console_error_panic_hook.
```

### 0.5 Tidy the README

```bash
claude --model haiku --effort low
```

```text
Fix README.md: remove the truncated, duplicated "Linux apps" table row, then update the Status line to match the version in Cargo.toml (summarised from the latest CHANGELOG.md entry). Do not change anything else.
Commit: "docs: fix duplicated Linux apps row and stale status".
```

### 0.6 Baseline app matrix

```bash
claude --model sonnet --effort medium
```

```text
Create an "app matrix" e2e scenario in tests/e2e with the five checks from CLAUDE.md (appears in the launcher, opens as a RAST window, accepts typing, a saved file is visible in File Station, closes normally) for Chrome, Firefox, VS Code and LibreOffice.
Checks that 0.18 does not support yet (for example a file from an app not yet visible in File Station) are marked "expected fail" with the reason.
Ask me before running the native instance (see Production safety). Store the results in tests/e2e/results/ and the summary in PROGRESS.md.
```

## Phase 1 — One HOME

### 1.1 One-HOME ADR design (plan mode)

```bash
claude --model opus --effort xhigh --permission-mode plan
```

```text
Phase 1: design "one user = one Linux account = one HOME" following CLAUDE.md.
First study src/storage/, src/rootfs/, src/terminal/, src/native/identity.rs, src/native/daemon.rs (path_acl, profile_prepare), src/linux_apps/native_session.rs (start_run) and docs/NATIVE-LINUX-APPS.md. If the workspace has been split, look in crates/.
Draft the content of docs/adr/0005-one-home.md as a plan: file ownership model, how File Station forwards Home requests to the per-user file agent, new helper operations (name, fields, validation), shared folders as Linux groups, XDG Trash, quota, per-app profile changes, the migration plan (dry-run, permission backup, rollback), impact on Backup Center, and the test list (including cross-user access).
Mark every decision that needs my approval, for example a database migration. After I approve the plan, write the ADR only; do not change code.
```

### 1.2 Per-user file agent

```bash
claude --model opus --effort xhigh
```

```text
Implement the per-user file agent following docs/adr/0005-one-home.md.
Generalise src/rootfs/agent.rs so it can run as rast-u<id> with its root at the user's HOME, started by the helper through a new, strictly validated operation. /api/files requests for Home are forwarded to the agent with the same JSON shapes.
Add tests: cross-user access fails, traversal and symlink escape are rejected, chunked upload still works.
Follow the Security invariants in CLAUDE.md. Commit in small steps and update PROGRESS.md.
```

### 1.3 Shared folders as Linux groups

```bash
claude --model opus --effort high
```

```text
Implement shared folders as Linux groups following ADR 0005: one read-write and one read-only group per shared folder, setgid directories with default ACLs, a link under ~/Shared, and membership computed from the current Effective Access rules (src/access/).
An access change restarts the user's session, with a warning in the UI.
Implement rast migrate shares --dry-run, rast migrate shares and rast migrate shares --rollback (ADR 0005, section 8): per shared folder, idempotent, with a journal of owners, modes and ACLs before any change; it runs before rast migrate home.
Evaluate and report, without deciding: (a) whether group quota on each share's rw group is enough to enforce shares.quota_bytes under Q-a, including the gaps (mv, chgrp, group-preserving copies) and a bounded group-repair pass (D3); (b) a per-user deny ACL entry u:rast-u<id>:--- on the share's container as an alternative to the forced session restart, including the gaps (cwd inside the share, open descriptors, hard links) (D7).
Document in docs/NATIVE-LINUX-APPS.md that administrator apps and terminals can open every shared folder, so administrators should use a non-admin account for everyday app use (D6).
Add tests for membership, read-only permissions and access revocation.
```

### 1.4 XDG Trash and quota

```bash
claude --model sonnet --effort high
```

```text
Replace the File Station recycle bin for Home with XDG Trash (~/.local/share/Trash, per the freedesktop Trash spec) managed by the file agent, so Linux apps and File Station share one trash.
Replace Home quota accounting with per-uid filesystem quota (project quota for shared folders) following ADR 0005, falling back to the old accounting when the filesystem does not support quota.
Keep the current API shapes. Add tests, then update docs and PROGRESS.md.
```

### 1.5 HOME migration command

```bash
claude --model opus --effort xhigh
```

```text
Implement rast migrate home --dry-run, rast migrate home and rast migrate home --rollback following ADR 0005: per user, idempotent, and always taking a backup of the owner/mode list before changing anything.
The dry-run report covers data size, files that change owner, app profiles that get merged, and conflicts.
Remove the per-launch setfacl -R in start_run and the read-only RAST symlink in app profiles.
Test on the disposable instance (scripts/e2e/dev-instance.sh); ask me before running native mode. Write an Upgrade note in CHANGELOG.md.
```

### 1.6 Phase 1 security review

```bash
claude --model opus --effort max
```

```text
Security-review all Phase 1 changes (compare against the last commit before Phase 1 recorded in PROGRESS.md).
Focus: new helper operations, the file agent, the migration, cross-user access, path traversal and symlinks, races during migration, permission leaks in shared folders.
Write the findings in docs/remake/review-phase-1.md with severity and suggested fixes. Do not fix anything in this session.
```

## Phase 2 — Desktop session

### 2.1 systemd --user, D-Bus and audio (plan mode)

```bash
claude --model opus --effort xhigh --permission-mode plan
```

```text
Phase 2: run the Xpra session, agent and apps under rast-u<id>'s own systemd user manager following CLAUDE.md.
Plan: the helper operation for linger, how to start user units (systemd-run --user --machine=rast-u<id>@.host), the cgroup limits that must keep applying, one D-Bus session bus and XDG_RUNTIME_DIR for all apps, per-user PipeWire/PulseAudio, and how RAST adopts an already-running session after a restart.
Apps that open other apps (docs/adr/0004-nested-launchers.md, decisions 1 and 2):
- App units get ExitType=cgroup on systemd 250+ and keep KillMode=control-group. For systemd < 250 (Ubuntu 22.04 ships 249), plan the subreaper wrapper from the ADR (hidden internal subcommand of the rast binary, PR_SET_CHILD_SUBREAPER, reaps all descendants, forwards SIGTERM). The helper detects the systemd version; native-helper --check and rast doctor report the mode.
- Contract tests for both paths: the unit stays active while a detached grandchild runs after the main process exits, becomes inactive after the last process exits, and stop kills all processes.
- The main CPU/RAM limit moves to one per-user slice that holds the session, the agent and every app (the linux_session_* values). Per-app limits must never make an app opened from a launcher run out of memory; the ADR proposes per-app MemoryHigh, OOMPolicy=continue, per-app CPUQuota, and TasksMax raised to the session value. Ask me before changing the meaning of any linux_app_* key, and write an Upgrade note if it changes.
After I approve the plan, implement it with tests, record it in a new ADR with the next free number (docs/adr/NNNN-systemd-user-session.md; 0004 is taken), and update docs/NATIVE-LINUX-APPS.md (unit lifetime, limits, the End application row of the lifecycle table).
```

### 2.2 Keyring unlocked at login

```bash
claude --model opus --effort xhigh
```

```text
When a user logs in to RAST, unlock their session keyring: the agent hands the password to gnome-keyring-daemon --unlock over stdin, then drops it from memory immediately.
The password must never be written to disk, logs or error messages. Make sure the flow is safe when the session is not running yet or the login fails.
Once the keyring works, remove --password-store=basic for Chromium and its warning in the install preview.
Add tests for the failure paths and make sure the password never appears in logs.
```

### 2.3 Portal backend and link opener

```bash
claude --model opus --effort high
```

```text
Build an xdg-desktop-portal backend in rast-agent with ashpd (the FileChooserImpl trait for org.freedesktop.impl.portal.FileChooser) on top of zbus. Open/save dialogs appear as the File Station picker in the browser (new UI written in Leptos), and the selection is returned to the app. Register the backend through a .portal file and portals.conf for RAST sessions.
So that links from apps (xdg-open, OpenURI) open in the user's browser tab, register a small RAST-owned handler for x-scheme-handler/http and https in mimeapps.list.
Dependencies I approve: ashpd, zbus. Test with one Flatpak app and one GTK app.
```

### 2.4 Notifications to toasts

```bash
claude --model sonnet --effort medium
```

```text
Add an org.freedesktop.Notifications service to rast-agent (zbus) that forwards app notifications to RAST over the existing SSE events, shown as toasts.
Support at least summary, body, icon and the default action (click opens the app window). Notifications that cannot be shown are dropped without error.
Add protocol tests and update docs/REALTIME.md if there are new events.
```

## Phase 3 — Launcher for every app

### 3.1 .desktop index and Leptos launcher

```bash
claude --model sonnet --effort high
```

```text
Build an app index from .desktop files in all the locations CLAUDE.md names (Phase 3) with freedesktop-desktop-entry, watch for changes with notify, honour NoDisplay/Hidden/OnlyShowIn, and take icons from icon themes, including ~/.local/share/icons.
Serve it through the API (types in rast-api) and push updates over SSE.
Port the launcher (frontend/src/apps/launcher) to Leptos as the first consumer of this index. An app installed with apt from the Terminal must appear without a reload, and so must a .desktop file that an app writes into ~/.local/share/applications (as a launcher such as JetBrains Toolbox does; see docs/adr/0004-nested-launchers.md). Add a test for both.
Dependency I approve: freedesktop-desktop-entry.
```

### 3.2 Per-user Flatpak in App Center

```bash
claude --model sonnet --effort high
```

```text
Add per-user Flatpak to App Center: search and install apps from Flathub with flatpak --user as rast-u<id> (through the agent, not root), plus update and uninstall, with job progress like the existing install flow.
New UI for the Flatpak section is written in Leptos. .deb stays admin-only.
Re-enable AppImage only after it passes the app matrix.
Measure the HOME usage of a few typical per-user Flatpak installs and review the 20 GiB default quota for new accounts (ADR 0005, D4); propose a change, do not make it.
Add tests and update docs/NATIVE-LINUX-APPS.md.
```

### 3.3 Open files with an app

```bash
claude --model sonnet --effort medium
```

```text
Add "Open with" to File Station: read MIME associations from mimeapps.list (system and user) and MimeType in .desktop files, open the file straight in the chosen Linux app, and store the user's default choice.
Required examples: .docx opens in LibreOffice, .png opens in GIMP.
```

### 3.4 Windows of apps opened by a launcher, and Pin to launcher

```bash
claude --model sonnet --effort high
```

```text
Implement decisions 3 and 5 of docs/adr/0004-nested-launchers.md.
- Match every top-level Xpra window to an entry of the .desktop index from task 3.1 (StartupWMClass, then desktop file id or Exec name against WM_CLASS, then _GTK_APPLICATION_ID), not only to apps that RAST installed. A window without a match uses its own title and _NET_WM_ICON; it must never borrow the name or icon of the app launched last (today's fallback in resolveSlug, frontend/src/shell/xpra-native.ts). The taskbar groups windows by the matched app.
- End application on a run whose unit also holds other apps asks for confirmation and names the apps that will close with it.
- A "Pin to launcher" window action for windows without a match: the agent, as rast-u<id>, writes ~/.local/share/applications/rast-pinned-<slug>.desktop through the safe-path layer (Name, Exec from the window's process via _NET_WM_PID only when it belongs to the same uid, Icon from _NET_WM_ICON under ~/.local/share/icons/hicolor/, StartupWMClass). Unpin removes only rast-pinned-* files.
New UI is written in Leptos. Add unit tests for the matching and for the .desktop file the agent writes (quoting, a process of another uid is refused), and an e2e check that the pinned app appears in the launcher.
```

### 3.5 Root requests from apps, and AppImages in launched apps

```bash
claude --model opus --effort xhigh
```

```text
Implement decisions 4 and 6 of docs/adr/0004-nested-launchers.md. This touches the root helper's unit environment, so follow the Security invariants in CLAUDE.md.
- Apps installed from inside a launcher go into the user's HOME: check that HOME is writable and executable by rast-u<id> and that ~/.local/bin is on the session PATH.
- sudo, pkexec, apt and apt-get from an app fail at once with a clear message that installing system packages needs an administrator, who can install the app from App Center: stubs in a RAST directory placed first on the session PATH, which also raise a RAST notification and exit non-zero. The stubs never call anything privileged. No sudoers entry, no polkit rule and no helper operation is reachable from an app.
- Evaluate NoNewPrivileges=yes for app units (it can break Chromium's setuid sandbox fallback when unprivileged user namespaces are blocked). Report the result and ask me before adopting it.
- App units get APPIMAGE_EXTRACT_AND_RUN=1, so a type-2 AppImage that a launcher downloads runs without FUSE; units get no /dev/fuse. Document that type-1 AppImages are not supported.
Add tests: the generated unit environment, each stub's message and exit status, an AppImage started without FUSE, and that the account still has no way to root. Update docs/NATIVE-LINUX-APPS.md.
```

### 3.6 Beta gate

```bash
claude --model sonnet --effort medium
```

```text
Run the app matrix for the Beta gate (Chrome, Firefox, VS Code, LibreOffice) on the disposable instance; ask me before native mode.
Also create (if missing) and run the "launcher in a launcher" scenario from CLAUDE.md (App matrix) with ArtCraft Launcher and JetBrains Toolbox: each launcher passes the five checks, then installs one app through it. The scenario passes when that app opens as a RAST window, keeps running after the launcher is closed, and its shortcut appears in the RAST launcher.
For every failing check, find the cause and write the list of fixes in PROGRESS.md. Fix small ones directly; turn big ones into separate tasks.
Beta passes only when all four apps pass all five checks and both launchers pass the launcher scenario.
```

## Phase 4 — Compatibility and display

### 4.1 Xpra adapter + contract tests

```bash
claude --model opus --effort xhigh
```

```text
Wrap the Xpra integration (crates/rast-server/src/xpra_shim.js and frontend/src/shell/xpra-native.ts) behind one versioned adapter.
Write contract tests in CI based on scripts/xpra-poc that check window adoption, focus, resize, keyboard/mouse input, transient dialogs and clipboard. Pin the Xpra version that passes and document how to upgrade.
Do not change any user-visible behaviour.
```

### 4.2 HiDPI and Wayland

```bash
claude --model opus --effort high
```

```text
Replace the pinned DPI 96 with a per-user scale that follows the browser's devicePixelRatio (session DPI + canvas size), then test on a HiDPI screen.
Try the Wayland server in Xpra 6.5 for Wayland-native apps, with X11/XWayland as the fallback. Write the test results in a new ADR docs/adr/NNNN-wayland.md (NNNN = the next free ADR number), including the decision to adopt or defer.
```

### 4.3 Tray and clipboard paste

```bash
claude --model sonnet --effort medium
```

```text
Make rast-agent a StatusNotifierItem host (zbus) and show app tray icons in the RAST taskbar, including menus and clicks.
Add a "Paste into app" window action that reads the browser clipboard after the user clicks, then sends it to the app.
Test with Discord or Telegram (Flatpak).
```

### 4.4 Remove the legacy Docker backend for Linux apps (plan mode)

```bash
claude --model opusplan --effort high --permission-mode plan
```

```text
Plan the removal of the Docker/Selkies and noVNC backend for Linux apps (the file list is in the Repository map in CLAUDE.md), without touching Docker Compose apps.
Before deleting, make sure the Docker-to-native migration path has shipped, and ask me which release is the cut-off.
After approval: remove the code, tests and unused dependencies (including @novnc/novnc), regenerate licenses, and write an Upgrade note in CHANGELOG.md.
```

## Phase 5 — Frontend to Leptos

### 5.1 Port one app to Leptos (reuse for each app)

Order: Activity Logs → Resource Monitor → Control Panel → Backup → File
Station → App Center. Use `--effort high` for File Station and App Center.

```bash
claude --model sonnet --effort medium
```

```text
Port the <APP NAME> app (frontend/src/apps/<folder>) to Leptos in rast-ui.
Keep the same behaviour, look, shortcuts and API calls; use types from rast-api. Convert the relevant frontend tests into Rust tests (wasm-bindgen-test) or e2e.
Once the Leptos version passes tests and e2e, delete its TypeScript version and record the JS bundle size reduction in PROGRESS.md.
```

### 5.2 Port Terminal

```bash
claude --model sonnet --effort high
```

```text
Port Terminal (frontend/src/apps/terminal) to Leptos. xterm.js stays in use through one wasm-bindgen binding module.
Keep tabs, reconnect, search, scrollback, font size, the runtime panel, and the handling of browser-reserved shortcuts.
Test with tests/e2e/terminal-bench.mjs and tests/e2e/terminal-native.mjs.
```

### 5.3 Port the shell, window manager and Xpra integration (plan mode)

```bash
claude --model opusplan --effort xhigh --permission-mode plan
```

```text
Plan the port of the most sensitive parts to Leptos: the shell (frontend/src/shell), the window manager (frontend/src/core/wm.ts) and the Xpra integration (frontend/src/shell/xpra-native.ts).
The plan must cover an order of steps that can always be rolled back, how to preserve the input and focus behaviour that has already been fixed (see CHANGELOG 0.13.x), and the e2e tests that must pass at every step.
After approval, work incrementally. The Xpra adapter contract tests from Phase 4 must keep passing.
```

### 5.4 Remove Node and Vite from the build

```bash
claude --model sonnet --effort high
```

```text
Once every app is Leptos, remove TypeScript, Vite and Node from the build. Keep xterm.js as a static asset (with its license); the Xpra client still comes from the installed Xpra package.
Update CI, scripts/build-release.sh, CONTRIBUTING.md and README.md. Make sure the release binary is still a single file.
Check the final remake gate: compressed WASM size at most 1.5x the Phase 0 JS baseline, and the app matrix fully green.
```

## Phase 6 — RAST OS

### 6.1 OS image with mkosi

```bash
claude --model sonnet --effort high
```

```text
Create an mkosi configuration in os/ for a Debian-based image with RAST, Xpra, Flatpak, PipeWire, acl and nftables preinstalled.
On first boot, print a one-time token on the console to create the super administrator through the web.
Build the image in CI (not on the production host) and write docs/OS-IMAGE.md.
```

## Phase 7 — Research (optional, after the final remake gate)

```bash
claude --model opus --effort max
```

```text
Research the feasibility of two options: (1) our own Xpra client in Rust/WASM using WebCodecs to replace the iframe and shim, (2) our own Wayland compositor with Smithay that streams each window to the browser.
Compare effort, risk, performance and license impact (Xpra GPL/MPL). Write the recommendation in a new ADR docs/adr/NNNN-display-research.md (NNNN = the next free ADR number). Do not write production code.
```

---

## Routine prompts

### Continue with the next task

Pick the model for the kind of the next task (table above). When unsure, use this.

```bash
claude --model sonnet --effort medium
```

```text
Read docs/remake/PROGRESS.md, then do the next task in the current phase. If that task needs a higher model or effort per CLAUDE.md, tell me before starting.
```

### Red CI

```bash
claude --model sonnet --effort medium
```

```text
CI failed in <job name or link>. Reproduce the failure locally, find the root cause, and fix it without weakening tests. Explain the cause briefly in the commit message.
```

### Close the session

```bash
claude --model haiku --effort low
```

```text
Look at git log and the diff since the last entry in docs/remake/PROGRESS.md. Update PROGRESS.md (Done, In progress, Next, Open questions) and add a CHANGELOG.md [Unreleased] entry for user-visible changes. Commit: "docs: update remake progress".
```

### Ask about code

```bash
claude --model haiku --effort low
```

```text
Explain where and how <topic> is handled in this repo, with file paths and line numbers. Do not change any files.
```
