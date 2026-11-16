# 0004. Nested launchers: apps that install and open other apps

- Status: accepted (plan; implemented in Phases 2 and 3)
- Date: 2026-11-16

## Context

Some Linux apps are launchers: they download other apps into the user's HOME,
create shortcuts for them and start them as their own child processes.
Examples are ArtCraft Launcher and JetBrains Toolbox (which installs IDEs under
`~/.local/share/JetBrains/Toolbox/` and writes `.desktop` files to
`~/.local/share/applications/`). The child often detaches (double fork,
`setsid`), and the launcher may exit, or hide itself, while the child keeps
running. The RAST launcher still lists the installed apps as planned
(Phase 3); this ADR covers what has to change so that such apps work.

What the code does today:

- **Unit lifetime.** `build_systemd_run_args` in
  `crates/rast-helper/src/native/daemon.rs` starts every app as a transient
  service `rast-app-u<uid>-<app>-<n>` with `Type=exec` and
  `KillMode=control-group`, and no `ExitType=`. The default is
  `ExitType=main`: when the main process (the launcher) exits, systemd stops
  the unit and `KillMode=control-group` kills every process left in its
  cgroup, including the apps that the launcher opened.
- **Limits.** The app unit carries `MemoryMax`, `CPUQuota` and `TasksMax` from
  `linux_app_*` (defaults: 2048 MB, 2 CPUs, 1024 tasks). A child stays in the
  launcher's cgroup, so the launcher and every IDE it opened share one 2 GB
  limit. An OOM kill inside a service also stops the whole unit (the default
  `OOMPolicy=stop`). The `linux_session_*` limits apply only to the Xpra
  session unit, which is a sibling of the app units in `rast-linux.slice`, so
  nothing limits a user's total.
- **Window identity.** `resolveSlug` in `frontend/src/shell/xpra-native.ts`
  matches a window's `WM_CLASS` against installed RAST apps only. A window of
  an app that RAST did not install is given the app launched in the last
  120 seconds, or the first native app, so an IDE opened by Toolbox appears in
  the taskbar as Toolbox, with the generic icon.
- **Root.** `rast-u<id>` accounts have no sudo or polkit rights. A launcher
  that calls `sudo`, `pkexec` or `apt` to install a system dependency waits for
  a password prompt nobody can answer, or fails with a message that does not
  say what to do.
- **AppImage.** Type-2 AppImages mount themselves with FUSE. App units have no
  FUSE setup, and App Center does not offer AppImage yet
  (`docs/NATIVE-LINUX-APPS.md`).
- **Host support.** `docs/NATIVE-LINUX-APPS.md` supports systemd 249+.
  `ExitType=` exists since systemd 250; Ubuntu 22.04 ships 249.

## Decision

1. **App units live as long as they have processes.** App units get
   `ExitType=cgroup` (systemd 250+), so the unit stays active while any
   process is left in its cgroup and becomes inactive after the last one exits.
   `KillMode=control-group` stays, so **End application** and session stop
   still end everything in the unit. The session unit keeps `ExitType=main`.
   This applies to the system units of today and to the user units of Phase 2
   (`systemd-run --user`). The helper detects the systemd version at start
   and `rast native-helper --check` and `rast doctor` report which mode is in
   use.

   Fallback for systemd < 250, evaluated:

   | Option | Verdict |
   | --- | --- |
   | **Subreaper wrapper.** A hidden internal subcommand of the single `rast` binary is the unit's main process, running as `rast-u<id>`. It sets `PR_SET_CHILD_SUBREAPER`, starts the app, reaps every orphaned descendant and exits only when none is left, with the exit status of the first child. `SIGTERM` from systemd is forwarded to all descendants. | **Recommended.** Same behaviour as `ExitType=cgroup` on any systemd; no root; one binary. Costs new code in the launch path with careful signal and reaping tests. |
   | `RemainAfterExit=yes` | Rejected: the unit stays "active (exited)" after the last process is gone, so RAST would have to poll the cgroup to know that an app ended. |
   | Start apps as scopes (`systemd-run --scope`) | Rejected: the helper would become the parent of user processes and lose the service model it uses today (main PID, status, `--collect`). |
   | Raise the minimum to systemd 250 | Possible, but it drops Ubuntu 22.04. Needs the maintainer's approval; not decided here. |

   The same contract tests run against both paths: the unit stays active while
   a detached grandchild runs after the main process exits, becomes inactive
   after the last process exits, and stop kills all of them.

2. **The main CPU and RAM limit is per user session.** One per-user slice
   holds the Xpra session, the agent and every app unit of that user, and it
   carries the `linux_session_*` limits. Before Phase 2 this is a slice below
   `rast-linux.slice` (for example `rast-linux-u<id>.slice`); from Phase 2 it
   is the user's systemd user manager. The exact names are chosen in task 2.1.
   Per-app limits may stay, but they must never cause an app opened by a
   launcher to run out of memory. Recommended for task 2.1: per-app memory
   becomes `MemoryHigh` (reclaim and throttle, no OOM kill) instead of
   `MemoryMax`, app units get `OOMPolicy=continue`, per-app `CPUQuota` stays
   (throttling kills nothing), and per-app `TasksMax` is raised to the session
   value. **Changing the meaning of the `linux_app_*` keys needs the
   maintainer's approval** (config keys, `deploy/rast.toml.example`).

3. **Windows keep their own identity.** Each top-level window is matched to an
   entry of the Phase 3 `.desktop` index (by `StartupWMClass`, then the
   desktop file id or `Exec` name against `WM_CLASS`, then `_GTK_APPLICATION_ID`),
   not only to apps that RAST installed. A window without a match uses its own
   title and `_NET_WM_ICON`. It must never borrow the name or icon of the app
   launched last. The taskbar groups windows by the matched app, not by the run
   that started them. **End application** on a run whose unit also holds other
   apps (windows that matched other entries, or processes left after the
   launcher's main process exited) asks for confirmation and names the apps
   that will close with it. Closing a launcher's window still sends its normal
   close request; with decision 1 the apps it opened keep running.

4. **Apps installed from a launcher go into HOME and need no root.** The
   Phase 1 HOME is writable and executable by `rast-u<id>` (not mounted
   `noexec`), and `~/.local/bin` is on the session `PATH`. When an app tries
   `sudo`, `pkexec` or `apt`/`apt-get`, that step fails at once with a clear
   message: installing system packages needs an administrator, who can install
   the app from App Center. Proposed mechanism: small stubs for those commands
   in a RAST directory placed first on the session `PATH`, which print that
   message, raise a RAST notification (Phase 2) and exit non-zero. The stubs are
   only for the message; the barrier stays the account having no rights. A call
   by absolute path reaches the real program and fails with the system's own
   error. **There is no shortcut to root**: no sudoers entry, no polkit rule,
   no helper operation reachable from an app (`/run/rast/native.sock` stays
   `0660 root:rast`, and `rast-u<id>` is not in the `rast` group).
   `NoNewPrivileges=yes` on app units would make every setuid path fail too,
   but it can break Chromium's setuid sandbox fallback on hosts that block
   unprivileged user namespaces; evaluate it in task 3.5 and ask the
   maintainer before adopting it.

5. **Launcher shortcuts appear in the RAST launcher.** The `.desktop` index
   watches `~/.local/share/applications` (and icons under
   `~/.local/share/icons`), so a shortcut a launcher creates appears without a
   reload. For a window that matches no entry, a new window action **Pin to
   launcher** makes the agent, as `rast-u<id>`, write
   `~/.local/share/applications/rast-pinned-<slug>.desktop` through the
   safe-path layer: `Name` from the title, `Exec` from the window's process
   (`_NET_WM_PID`, read from `/proc` only when that process belongs to the
   same uid, quoted per the Desktop Entry spec), `Icon` from `_NET_WM_ICON`
   saved under `~/.local/share/icons/hicolor/`, and `StartupWMClass` from
   `WM_CLASS` so later windows match it. **Unpin** removes only files with the
   `rast-pinned-` prefix.

6. **AppImages run without FUSE.** App units get
   `APPIMAGE_EXTRACT_AND_RUN=1`, so a type-2 AppImage that a launcher downloads
   extracts itself to the user's temporary directory and runs without FUSE.
   Units get no `/dev/fuse` access. Type-1 AppImages and custom runtimes that
   ignore the variable are not supported and are documented as such. Whether
   App Center's own AppImage install (re-enabled in Phase 3) uses the same mode
   or extracts once at install time is decided in task 3.2.

## Consequences

- **Phase 2 (task 2.1):** `ExitType=cgroup` or the subreaper wrapper, the
  per-user slice and the new per-app limits, with helper tests for the
  generated unit properties and the contract tests above. Upgrade note in
  `CHANGELOG.md` if the `linux_app_*` keys change meaning.
- **Phase 3:** the index watches the user's directories (task 3.1); window
  matching, the End application warning and Pin/Unpin to launcher (task 3.4);
  the root stubs and message, and `APPIMAGE_EXTRACT_AND_RUN` (task 3.5). The
  behaviour is user-visible, so the e2e scenario below is required (task 3.6,
  Beta gate).
- **App matrix:** a "launcher in a launcher" scenario with ArtCraft Launcher
  and JetBrains Toolbox. It passes when an app installed through the launcher
  opens as a RAST window, keeps running after the launcher is closed, and its
  shortcut appears in the RAST launcher. The maintainer decided on 2026-11-16
  that it is part of the Phase 3 exit (Beta gate).
- **Docs:** `docs/NATIVE-LINUX-APPS.md` (unit lifetime, limits, lifecycle
  table for End application, AppImage, root requests) is updated with the
  implementation, not before.
- **Risks:**
  - A stray background process (updater, crash reporter, a launcher that stays
    in the tray) keeps a unit alive with no window. RAST shows such a run as
    running in the background, and End application stops it.
  - Until the tray host (Phase 4), a launcher that hides to the tray when its
    window closes can only be brought back by launching it again.
  - Processes started through D-Bus activation, `flatpak run` or
    `systemd-run --user` leave the launcher's cgroup. They are not killed with
    it and are tracked as separate units, which is the intended outcome.
  - `MemoryHigh` without a per-app `MemoryMax` lets one app use the whole
    session budget; the session slice's `MemoryMax` is the hard stop.
  - The `PATH` stubs only cover calls resolved through `PATH`.
