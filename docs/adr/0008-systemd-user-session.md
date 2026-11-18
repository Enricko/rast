# 0008. One systemd user session per one-HOME account

- Status: accepted (agent decision, pending maintainer confirmation)
- Date: 2026-11-17; revised 2026-11-18 after the independent review
  (`docs/remake/review-adr-0008.md`, verdict "approve with changes"; every
  finding is answered in "Review response")

## Context

Phase 2 (`CLAUDE.md`) runs the Xpra session, the agent and the apps of a
user under that user's own systemd user manager, with one D-Bus session bus,
one `XDG_RUNTIME_DIR` and one sound server for all apps, and per-user
cgroup limits. ADR 0004 (decisions 1 and 2) asks this task to settle unit
lifetime and limits; the maintainer's preferences of 2026-11-16
(`docs/remake/PROGRESS.md`) bind it: one mechanism for every systemd version
(the subreaper wrapper) unless a strong reason for `ExitType=cgroup` is
found; per-app `MemoryHigh` under a per-session `MemoryMax`, the
`linux_app_*` key names stay and their new meaning goes into an upgrade
note; `NoNewPrivileges=yes` only once the Chrome and Flatpak sandboxes are
shown to work with it.

### What the code does today

- **Units.** The root helper starts every unit with `systemd-run` on the
  **system** manager (`build_systemd_run_args`,
  `crates/rast-helper/src/native/daemon.rs`): `rast-xpra-u<id>` (session)
  and `rast-app-u<uid>-<app>-<n>` (apps) in `rast-linux.slice`, as
  `--uid/--gid` of the identity, `Type=exec`, `KillMode=control-group`, no
  `ExitType=` (so `main`), each with its own `MemoryMax`, `CPUQuota` and
  `TasksMax`. The file agent `rast-files-u<uid>` (socket-activated, socket
  `root:rast 0660` in `/run/rast/files/`) is in `rast-linux.slice` too;
  terminals `rast-term-u<uid>-<sid>` are in `system.slice` with their own
  `terminal_*` limits and `NoNewPrivileges=yes`, `ProtectSystem=full`,
  `PrivateTmp=yes`.
- **Runtime directory.** `/run/rast-linux/<uid>` (0700, made by the helper)
  with `sockets/` (Xpra `--socket-dir`), `tmp/` (`TMPDIR`), `cache/`,
  `pulse/` and `xpra.log`. The server never enters it: it reaches Xpra only
  through `127.0.0.1:<port>`, which the nftables rule `meta skuid != <rast
  uid> reject` protects (`firewall.rs`). The port is read back from the Xpra
  command line of the unit's process tree (`session_port`) and added to the
  set after `systemd-run` returns (`daemon.rs`); the set is reconciled
  against live `rast-xpra-*` units when the helper starts.
- **D-Bus.** There is no session bus. Xpra runs with `--dbus=no`; Chromium
  apps are wrapped in `/usr/bin/dbus-run-session --`
  (`native_launch_request`, `crates/rast-session/src/linux_apps/mod.rs`),
  one private bus per launch.
- **Audio.** Xpra starts its own PulseAudio child (`audio.rs`:
  `pulseaudio -n`, module loading disabled, a null sink `Xpra-Speaker`,
  `--exit-idle-time=-1`, socket `/run/rast-linux/<uid>/pulse/native`,
  cookie auth); every app gets `PULSE_SERVER`, `PULSE_COOKIE` and friends
  pinned to it.
- **Adoption.** `NativeSessions` reads `linux_sessions`/`linux_runs` and the
  helper's `units_list` (system units only) at start.
- **Group restarts.** `share_session_groups` (`shares.rs`) counts a
  process as "in the user's units" when the first non-slice component of
  its cgroup is `rast-xpra-u<id>`, `rast-files-u<uid>`, `rast-app-u<uid>-…`
  or `rast-term-u<uid>-…`; ADR 0005 notes that Phase 2 must extend this.

### Host facts (production, checked 2026-11-17 and by the review on 2026-11-18, read-only, no root)

| Fact | Value |
| --- | --- |
| systemd | 259 (`259.9-0ubuntu0.1`), built `-UTMP` |
| `user@.service` | `PAMName=systemd-user`, `Slice=user-%i.slice`, `Delegate=pids memory cpu`, `DelegateSubgroup=init.scope`, `TasksMax=infinity`, `OOMScoreAdjust=100`, `OOMPolicy=continue`, `KillMode=mixed`, `TimeoutStopSec=120s`; the limit files of `user@<uid>.service` (`memory.max`, `memory.high`, `cpu.max`, `pids.max`, `cgroup.kill`) are root's |
| `user-.slice.d/10-defaults.conf` | `StopWhenUnneeded=yes`, `TasksMax=33%` |
| `/usr/lib/pam.d/systemd-user` | `common-account`, `pam_loginuid`, `pam_limits`, `common-session-noninteractive`, `pam_keyinit`, `pam_systemd` |
| logind | defaults (`KillUserProcesses=no`, `UserStopDelaySec=10`, `RemoveIPC=yes`, `RuntimeDirectorySize=10%`); linger on for `lufria` and `root` only (no `rast-u*`) |
| logind sessions of a lingering user | every user manager registers a session of class `manager` (`Active=yes`, `Remote=no`, no seat, no scope): `loginctl` shows `2 1000 lufria - 2874 manager` |
| `--machine=<user>@.host` (sd-bus, `libsystemd-shared-259`) | runs `systemd-run -M .host -PGq --wait -pUser=<user> -pPAMName=login systemd-stdio-bridge` for **every** connection |
| `/etc/pam.d/login` | `pam_nologin`, `pam_loginuid`, `pam_motd` (runs `update-motd.d` as root), `pam_env`, `pam_limits`, `pam_mail`, `pam_keyinit`, `common-session` with `pam_systemd` |
| D-Bus | `dbus-daemon` 1.16.2 and `dbus-user-session` installed; user `dbus.socket` (`%t/bus`) enabled globally; `dbus-broker` not installed; `dbus-update-activation-environment` (`dbus-bin`) present |
| Sound | `pulseaudio` and `pulseaudio-utils` 17.0 installed; user `pulseaudio.socket` (`%t/pulse/native`, `ConditionUser=!root`) and `pulseaudio.service` (`default.target.wants`, `--daemonize=no`) enabled globally in `/etc/systemd/user/`; default `exit-idle-time` 20 s; PipeWire not installed |
| Other user units enabled globally | `gpg-agent*.socket`, `dirmngr.socket`, `keyboxd.socket`, `ssh-agent.socket`, `snapd.session-agent.socket`, `pk-debconf-helper.socket`, `launchpadlib-cache-clean.timer`; `graphical-session-pre.target.wants`: `xdg-user-dirs`, `user-session-migration`, `xdg-desktop-portal-rewrite-launchers` |
| User unit search path | `~/.config/systemd/user.control/` ranks above `$XDG_RUNTIME_DIR/systemd/transient/`, so a user drop-in can override any property of a transient user unit; user generators are read only from `/run`, `/etc`, `/usr/local/lib`, `/usr/lib` |
| Portals | `xdg-desktop-portal` 1.21.1, `xdg-desktop-portal-gtk` installed |
| Not installed | `flatpak`, `gnome-keyring`, `systemd-oomd`, `systemd-container` |
| polkit | 127. `org.freedesktop.login1.set-self-linger`: `allow_any=yes`; `set-user-linger`: `auth_admin_keep`. The only other `allow_any=yes` actions are `login1.inhibit-block-idle`, `inhibit-delay-shutdown` and `inhibit-delay-sleep` (217 actions). `allow_active=yes` covers `login1.reboot`, `power-off`, `set-reboot-*` and `udisks2.filesystem-mount`. `/etc/polkit-1/rules.d` is `root:polkitd 0750` |
| `rast-native.service` | `ProtectSystem=no`, `ProtectHome=read-only` (a slave mount: runtime directories mounted after the helper starts appear writable in its namespace) |
| Memory | 31 GiB RAM, 4 GiB swap file with 3.6 GiB in use |
| Sandboxes | `kernel.apparmor_restrict_unprivileged_userns=1`; AppArmor profiles `chrome`, `chromium`, `flatpak`, `bwrap-userns-restrict`; `/opt/google/chrome/chrome-sandbox` is set-uid root; Yama `ptrace_scope=1` |

### Two gaps that exist today (not caused by this ADR)

1. **Self-linger.** Because `set-self-linger` is `allow_any=yes`, every
   `rast-u<id>` identity can run `loginctl enable-linger` itself today (from
   a terminal or an app). It then gets a user manager that starts at boot
   and runs timers and services outside RAST's units, which sidesteps the
   cron and `at` deny lists of agent decision A7. No identity lingers on
   production now. Agent decision 1 closes it first, in its own
   pre-release.
2. **The display proxy trusts whoever listens.** When a session's Xpra
   stops, any local uid may bind its port (binding is not filtered, only
   outgoing connections are, `firewall.rs`), and the server would proxy the
   session's viewer, keystrokes and clipboard included, to that process.
   User managers make an unnoticed stop more likely (`systemctl --user
   stop`, a manager crash, an OOM kill at the slice limit). Section 3
   ("Display proxy") closes it, in its own early commit.

## Decision

### 0. Agent decisions, to be confirmed by the maintainer

(Keputusan agent, perlu konfirmasi maintainer.) Each is the option that
gives the least access and keeps data; the coordinator chose them on
2026-11-18 from the review's recommendations.

1. **Deny linger changes to RAST identities, first.** A deny-only polkit
   rule ships in its own pre-release, before any other part of this ADR.
   It is installed by the installer and, on every update, by the helper at
   start (section 2, "Polkit rule"), so the deploy needs no manual step. A
   `pkcheck` gate test runs after that deploy and again after the user
   session deploy.
2. **Linger only while RAST needs the user manager**, never permanently,
   plus a helper-side fail-safe at helper start (section 2).
3. **The defaults of `linux_session_memory_mb` and `linux_session_tasks`
   become 8192**, with an upgrade note; an explicit value in `rast.toml`
   (or the environment) still wins (section 9).
4. **The account's own distro PulseAudio** is the sound server of
   one-HOME accounts, under the review's conditions: the helper runs no
   sound program as the identity (finding 1), and the session's sink
   survives the daemon's idle exit and restarts (finding 9; section 6).
5. **No `ExitType=cgroup`**: the subreaper wrapper only, on every systemd
   version (section 8).
6. **Session admission does not reserve the whole account limit** (a
   consequence of 3, found in this revision): a session start is admitted
   with `min(linux_session_memory_mb, linux_app_memory_mb)` plus the
   reserve, in both modes; each app keeps its own admission (section 9).

### 1. Scope: one-HOME accounts only

The user manager is used exactly for accounts in **one-HOME mode** (HOME
owned by the identity, ADR 0005 section 1). The helper derives the mode
from the HOME owner, as it does today (`Identity::one_home`); the server
cannot put a legacy account into a user manager. **Legacy native accounts
keep today's units unchanged**: system units in `rast-linux.slice`,
`/run/rast-linux/<uid>`, per-app profiles, `dbus-run-session` for Chromium,
Xpra's private PulseAudio and per-app `MemoryMax`. Reasons: a shared bus and
shared D-Bus-activated services only make sense with one HOME (per-app
profile HOMEs would get services started with another HOME), and legacy
system units have no per-user slice to put a hard cap on, so `MemoryHigh`
without `MemoryMax` would remove their only memory limit. Since one-HOME
mode is pilot-only (ADR 0007), so is the user manager. Legacy accounts get
it through `rast migrate home`.

A one-HOME account that is **stranded** (outside the pilot after the pilot
shrank, or with `native_enabled = false`) gets no `ensure`: its terminals
start as today, without `XDG_RUNTIME_DIR` and a session bus, and the
server's start-up reconcile stops any user manager it still has.

### 2. Linger, as RAST needs it (helper operations)

Linger is the only way an identity gets a user manager: identities have the
nologin shell and no password, so they never log in. A lingering manager
does register a logind session of class `manager` (host facts); section 10
covers what that means for polkit. RAST turns linger on when a one-HOME
account starts native work and off when the account has had none for
`linux_idle_minutes` (agent decision 2). It is **not** permanent: a
permanent linger keeps a user manager, PulseAudio, the D-Bus daemon and
every unit the user enabled running for every account from boot on (about
20 MB per account and a cron equivalent that A7 denies), and in task 2.2 it
would keep the keyring unlocked until reboot.

New operations (strict JSON, `deny_unknown_fields`, no path fields; they
take a per-uid lock in the helper, so ensure, release and starts of one
account never interleave):

| Operation | Fields | Behaviour |
| --- | --- | --- |
| `user_manager_ensure` | `userId` | Identity from `require_identity` (passwd entry with the expected uid, gid and nologin shell). HOME must be owned by the identity, otherwise `legacy_home`. **Fail-closed order:** (1) set the configured session limits on `user-<uid>.slice` with `systemctl set-property user-<uid>.slice MemoryMax=<M>M CPUQuota=<C>% TasksMax=<T>` when `systemctl show` reports other values; the values are the helper's own `session_limits` from the configuration, never taken from the request. A slice unit loads without a unit file, so this works before the slice exists (checked by step 5, not assumed); a refusal ends the call with `limits_failed`, before any linger. (2) `loginctl enable-linger rast-u<id>` when `/var/lib/systemd/linger/rast-u<id>` is missing. (3) When `user@<uid>.service` is `deactivating`, return the retryable `manager_stopping` (the server retries for up to 130 s, then tells the user the session is still stopping); otherwise `systemctl start user@<uid>.service` when it is not active. (4) Wait up to 10 s for `ActiveState=active` and check `/run/user/<uid>` (real directory, not a link, owner uid, mode 0700). (5) Read the slice properties back from the system manager; on any mismatch run the `stop` steps and return `limits_failed`. Returns `{"uid":200127,"runtimeDir":"/run/user/200127","lingering":true,"started":false,"limits":{"memoryMaxMb":8192,"cpuQuotaPercent":400,"tasksMax":8192},"cpuDelegated":true}`. Idempotent: a second call runs only the `systemctl show` queries. |
| `user_manager_release` | `userId`, `mode`: `"idle"`, `"stop"` or `"remove"`; optional `wait` (bool, default false) | `idle`: refuses with `busy` while a terminal unit of the identity is active (system manager) **or** the helper holds a port record for the uid (section 3). Both are helper state; the helper never consults the user manager's unit list, which the user controls (a fake "running" unit must not keep the manager alive). The terminal check stays because logind has `RemoveIPC=yes`: when the identity's user record ends, its SysV and POSIX IPC objects are deleted, which would break programs in a running terminal. `stop`: first stops the identity's terminals and file agent, refuses with `identity_in_use` if one stays active. Both then run `loginctl disable-linger rast-u<id>` and `systemctl stop --no-block user@<uid>.service`, which ends every user unit (RAST's session and apps included). With `wait: true` the helper waits up to 130 s (`TimeoutStopSec=120s` plus a margin) for `ActiveState=inactive` and for the uid's last process in `user@<uid>.service` to end, else `manager_stopping`. `rast migrate home` sends `wait: true`. `remove`: as `stop`, always waiting, then deletes the drop-ins that `set-property` wrote (`/etc/systemd/system.control/user-<uid>.slice.d/50-{MemoryMax,CPUQuota,TasksMax}.conf`, opened without following links; the stopped slice loads without them next time). Port records are dropped as section 3 says. Returns `{"lingering":false,"managerActive":false,"stopped":["…"]}` (`managerActive` true while the manager is still stopping). Idempotent; an unknown identity in `remove` is a no-op. |
| `user_managers_list` | none | RAST identities that linger or whose `user@<uid>.service` is active: names in `/var/lib/systemd/linger/` that match `rast-u<digits>` with a uid in the identity range, plus `systemctl show 'user@*.service'` filtered to that range. Returns `{"managers":[{"userId":127,"uid":200127,"lingering":true,"active":true}]}` (at most 10 000). |

`loginctl enable-linger <name>` calls logind's `SetUserLinger` on the system
bus; logind accepts it from root without asking polkit, writes
`/var/lib/systemd/linger/<name>` and starts `user-runtime-dir@<uid>.service`
and `user@<uid>.service`. The helper already has the system bus and full
root. `identity_remove` runs the `remove` steps (with the wait) before
`userdel`, which refuses while processes of the uid run.

**When linger goes off:**

| Event | Call |
| --- | --- |
| No RAST session, app or terminal for `linux_idle_minutes` (server task, injected clock) | `release` `idle`. On `busy` because of a port record while the server has no session row for the account, the server first sends `session_stop` (which drops the record once the port is free, section 3) and retries at the next tick |
| Account deactivated (`api/users.rs`, next to `stop_user_sessions`) | `release` `stop` |
| Share access granted ("Restart session now") or revoked (after the 60 s warning), D7, `share_groups::restart` | `release` `stop`: the manager keeps the supplementary groups it started with, so a session restart alone would not apply the change; the next launch starts the manager with the new groups |
| Server start: a listed manager whose account is inactive, missing, outside the pilot, not one-HOME, or `native_enabled = false` | `release` `stop` |
| Server start: a listed manager with no adopted session, run or terminal (for example after a reboot, or a manager whose `units_list` answer is `unknown`, section 7) | nothing at once; `release` `idle` only after `linux_idle_minutes` counted from the server start, if the account still has no work |
| `rast migrate home` apply (step 1, "stop the user's units") | `release` `stop` with `wait: true` |
| `rast migrate home --rollback` | `release` `remove` |
| `identity_remove` | the `remove` steps, before `userdel` |
| **Helper start (fail-safe; trusts only helper state)** | for every `rast-u<digits>` linger file in the identity range whose uid has no port record (records live under `/run/rast-linux`, so a reboot clears them) and no active terminal unit: `disable-linger` and `systemctl stop --no-block user@<uid>.service`. This covers a host where `rast.service` fails to start, or only terminals use the helper, and a linger left over from a crash that would otherwise start the manager at every boot |

**Polkit rule (agent decision 1).** `deploy/rast-identities.polkit.rules`,
installed as `/etc/polkit-1/rules.d/49-rast-identities.rules`:

```js
// RAST (ADR 0008): only the RAST helper (root) turns linger on or off
// for RAST identities; the identities cannot do it themselves.
polkit.addRule(function (action, subject) {
    if ((action.id == "org.freedesktop.login1.set-self-linger" ||
         action.id == "org.freedesktop.login1.set-user-linger") &&
        /^rast-u[0-9]+$/.test(subject.user)) {
        return polkit.Result.NO;
    }
});
```

It only takes rights away (it grants nothing, so it is not the "polkit
shortcut" ADR 0004 forbids). It sorts before
`/usr/share/polkit-1/rules.d/49-ubuntu-admin.rules` and after the
root-only `10-systemd-logind-root-ignore-inhibitors.rules`, and it matches
`subject.user` only, so `lufria` and root keep their linger rights. On
hosts whose polkit reads only `.pkla` files (0.105) the equivalent
`deploy/rast-identities.pkla` goes to
`/etc/polkit-1/localauthority/50-local.d/49-rast-identities.pkla`
(`Identity=unix-user:rast-u*`, both actions, `ResultAny=no`,
`ResultInactive=no`, `ResultActive=no`). polkit reloads its rules when a
file in these directories changes; no restart is needed.

Delivery, so that no deploy needs a manual step:

- **Installer:** writes the file (0644 `root:root`) when the native backend
  is enabled, the JS rule when `/usr/share/polkit-1/rules.d` exists, else
  the `.pkla` when `/etc/polkit-1/localauthority` exists. The uninstaller
  removes it.
- **`rast update`, through the helper.** `src/update.rs` refreshes native
  assets by running the release's own installer with `--no-start` only when
  `native_assets_missing` reports the unit, the tmpfiles file or the
  layout directory missing, and that check runs in the **installed (old)
  binary**, which does not know the new file. So the asset refresh cannot
  deliver it on the first update. Every `rast update` does restart the
  helper on the new binary (`restart_native_helper`: `systemctl
  try-restart rast-native.service`), so the helper writes the rule at
  start: both files are embedded with `include_str!` from `deploy/`; it
  writes only when the file is missing or different, through a temporary
  file in the same directory (`O_NOFOLLOW | O_EXCL`, `fsync`, `rename`),
  never through a link at the target, and only when the directory is a
  real, root-owned directory not writable by others (`rast-native.service`
  has `ProtectSystem=no`). A failure is logged and never stops the helper.
  Adding the file to `native_assets_missing` was rejected: the helper
  already writes it, and that check would re-run the whole installer.
- `native-helper --check` and `rast doctor` report the rule as present,
  missing or stale. `rast doctor` also warns when an identity is a member
  of `sudo`, `admin`, `wheel` or `empower` (`empower.rules` grants every
  action to that group).

**Gate test after the polkit pre-release** (coordinator, production, test
accounts). In a RAST terminal of `t-alice`: `loginctl enable-linger` is
refused; `pkcheck --action-id org.freedesktop.login1.set-self-linger
--process $$` answers "not authorized" (exit 1); `pkcheck` for
`org.freedesktop.login1.reboot`, `org.freedesktop.login1.power-off` and
`org.freedesktop.udisks2.filesystem-mount` does not answer "authorized"
(exit 0; "not authorized" or "challenge" pass). As `lufria`, `pkcheck` for
`set-self-linger` still answers "authorized". The same three `pkcheck`
calls run again after the user session pre-release, from a `t-alice` user
unit (`systemd-run --user --wait --pipe sh -c 'pkcheck --action-id <id>
--process $$'`), where the `manager` session exists. Any action that
answers "authorized" is added to the rule (it only takes rights away) in a
pre-release of its own before the pilot uses user sessions again.

### 3. Where units run and how the helper starts them

| Unit | Manager | Slice | Notes |
| --- | --- | --- | --- |
| `rast-xpra-u<id>.service` (Xpra) | user | `session.slice` | `Type=exec`, `KillMode=control-group`, `Restart=no`, `ExitType=main` (default); `ExecStartPre=-/usr/bin/dbus-update-activation-environment --systemd …` and `ExecStopPost=-/usr/bin/systemctl --user unset-environment …` (section 5); no per-unit limits (the user slice caps it) |
| `rast-audio-u<id>.service` (sound sink keeper, section 6) | user | `session.slice` | `Type=exec`, `BindsTo=` and `After=` the Xpra unit, `Restart=on-failure`; main process `rast session-audio` |
| `rast-app-u<uid>-<app>-<n>.service` | user | `app.slice` | main process `rast app-reaper` (section 8), limits in section 9 |
| distro user units (`dbus.socket`, `pulseaudio.socket`, agents) | user | as shipped | started by `default.target`/`sockets.target` |
| `rast-files-u<uid>` (file agent) | system | `user-<uid>.slice` | unchanged otherwise: socket `root:rast 0660`, hardening, `MemoryMax=512M`; an active agent in another slice is restarted once |
| `rast-term-u<uid>-<sid>` (one-HOME terminal) | system | `user-<uid>.slice` | unchanged hardening and `terminal_*` limits; gets `XDG_RUNTIME_DIR=/run/user/<uid>` and `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/<uid>/bus` when that directory is the identity's |
| admin root shells, `rast-backup-reader`, `rast-rootfs` | system | unchanged | no identity |
| legacy accounts' units | system | `rast-linux.slice` | unchanged |

Unit names are unchanged; `validate_unit` and `is_user_unit` add
`rast-audio-u<id>.service`, so the database rows and the shim keys stay as
they are. The manager follows from the account mode; the server states it
and the helper checks it: `session_start`, `session_stop`, `app_start`,
`app_stop` and `unit_status` get an optional field `"manager": "system" |
"user"` (default `system`, so an older server keeps today's behaviour).
`user` is refused with `legacy_home` for a legacy HOME and with
`user_manager_unavailable` when `user@<uid>.service` is not active. With
`user`, `runtimeDir` must be absent. `units_list` returns the units of both
managers, each with `"manager"`, plus a state per user manager:
`{"managers":[{"uid":200127,"state":"ok"}]}` or `"unknown"` when the query
failed or timed out. The per-manager queries run in parallel, each with its
own 5 s timeout and a 15 s bound for the whole call, so one bad manager
cannot stall the others.

**The helper drives a user manager as the identity, without PAM, and runs
only `systemctl --user` and `systemd-run --user` there.** Both read no
user configuration. Every program that does (`pactl` reads
`~/.config/pulse/client.conf`, which can set `autospawn` and
`daemon-binary`; `dbus-update-activation-environment`) runs inside the
user's own units instead (sections 5 and 6), so nothing of the identity
ever runs in `rast-native.service`'s cgroup, outside the slice limits and
outside `share_session_groups`. The identity runner:

- forks, drops to the identity (`setgroups` to none, `setgid`, `setuid`;
  `std::process::Command` `uid`/`gid`), with `env_clear()` and exactly
  `XDG_RUNTIME_DIR=/run/user/<uid>`,
  `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/<uid>/bus`, a fixed `PATH`
  and `LC_ALL=C`; working directory `/`; stdin `/dev/null`; `--no-pager
  --no-ask-password`;
- puts the child in its own process group and kills the group on timeout,
  then reads the pipes for at most one more second (a grandchild that keeps
  stdout open can neither hang the request nor the per-uid lock);
- calls `close_range(3, ~0)` in `pre_exec`, behind `O_CLOEXEC` on every
  helper open;
- before each call checks `/run/user/<uid>` with `lstat` (real directory,
  owner uid, 0700); root never opens a path below it. `ProtectHome=read-only`
  of `rast-native.service` is **not** a guard here (runtime directories
  mounted after the helper started are writable in its namespace), and the
  design does not rely on it.

The shared runner of the helper (`native/mod.rs`) also gets the
process-group kill and the bounded pipe reads for every command.

Rejected:

- **`systemd-run --user --machine=rast-u<id>@.host`** (the roadmap's
  suggestion). In sd-bus every such connection runs a transient system unit
  with `PAMName=login` and `systemd-stdio-bridge`: the full `login` PAM stack
  per call (on this host `pam_motd` runs `update-motd.d` as root,
  `pam_nologin` refuses during shutdown, `pam_loginuid`, `pam_mail`), one
  logind session per call, and a few hundred milliseconds per call. The
  helper makes many calls (status, list, stop).
- **System units with `User=`** (today's model, extended). It cannot give
  apps what the user manager gives for free: D-Bus activation of user
  services (portals, keyring, notifications in later tasks), `flatpak run`
  and `systemd-run --user` scopes, user sockets such as `pulseaudio.socket`.
  RAST would have to rebuild that plumbing.
- **Root talks D-Bus to the user manager directly** (for example with
  `zbus`). The helper would parse messages from a socket that the user
  controls, in the root process, and it would need a new dependency.
- **The helper runs `pactl` and `dbus-update-activation-environment` as the
  identity** (the first draft): finding 1.

**The user manager is the identity's: its answers are untrusted input.**
The user owns `/run/user/<uid>`, can replace the manager's private socket,
stop RAST's units, start its own units with RAST's names, and override any
property of RAST's transient units with a drop-in in
`~/.config/systemd/user.control/`. So:

- the helper parses `systemctl --user show` strictly (known keys, bounded
  size), keeps only unit names that `is_user_unit` accepts for that
  identity, and reports a `MainPID` only when `/proc/<pid>` belongs to the
  identity's uid and its cgroup is below `user@<uid>.service`;
- nothing root does depends on it: no path, port or pid from it is used for
  a root action, and changed unit properties change no helper result;
- the limits that matter are on `user-<uid>.slice`, set by the system
  manager, outside the subtree delegated to the user.

**Limits.** `user-<uid>.slice` is the only common parent of
`user@<uid>.service` and the system units placed in it, so it carries the
`linux_session_*` limits (section 9). They are set with `systemctl
set-property` without `--runtime`, so systemd keeps them as drop-ins in
`/etc/systemd/system.control/user-<uid>.slice.d/` and they apply from boot
on, whatever starts first; they need no `daemon-reload`. Rejected: limits on
`user@<uid>.service` (they would miss the file agent and terminals); a
template drop-in in `/etc/systemd/system/user-.slice.d/` (it would also
limit real host accounts such as `lufria`); `--runtime` properties (gone
after a reboot while a stale linger may already have started the manager).
`Delegate=pids memory cpu` is already on `user@.service` (systemd 259), so
per-app `MemoryHigh`, `CPUQuota` and `TasksMax` work in user units. RAST adds
no drop-in to `user@.service` (it would change every account on the host);
where `cgroup.controllers` of `user@<uid>.service` lacks `cpu`, per-app
`CPUQuota` is not enforced, `ensure` returns `cpuDelegated: false`, and
`rast doctor` says so. The slice limits always apply.

**Firewall: port records.** Ports are no longer read from process trees for
new sessions, legacy sessions included. Rules:

- One record per uid, `<run_dir>/xpra-ports/<uid>` (root 0600, directory
  root 0700, written with `O_NOFOLLOW`), holding the port that
  `session_start` was given (validated, as today).
- `session_start` first refuses with `port_conflict` when a LISTEN socket
  of another uid is on the port (`/proc/net/tcp` **and** `/proc/net/tcp6`,
  kernel data; a dual-stack wildcard listener overlaps `127.0.0.1`) or the
  port is recorded for another uid; then it writes the record, then adds
  the port to `xpra_ports`, and only then runs `systemd-run`. If writing
  the record or adding the port fails, no unit starts; if the start fails,
  the unit is stopped and the record is dropped by the rule below.
- A record and its set entry are dropped only by `session_stop`,
  `user_manager_release` (`stop`, `remove`) and `identity_remove`, or
  replaced by a later `session_start` of the same uid, and in each case
  only once the kernel shows no LISTEN socket on the port. While one
  remains, the record stays (the call reports the retryable `port_busy`)
  and keeps protecting whoever listens; an entry with no listener is
  harmless (it only refuses non-`rast` connections). For a one-HOME
  account whose own process still listens after `session_stop` stopped the
  session unit, the helper runs the `stop` steps for the manager, so a user
  cannot keep its manager alive by holding its own port.
- The reconcile at helper start restores every valid record into the set
  (validated range, at most one per uid, regular file owned by root, no
  links) and never drops one because nothing listens: a session that has
  not bound yet keeps its protection.

Reason: in a user manager the user can replace `rast-xpra-u<id>` with a
process whose command line names **another user's** port; if the helper
released that port, the other user's Xpra (`--auth=none`) would be open to
every local account. Process tree discovery stays only for legacy system
units started before the record existed (the first helper start after the
upgrade).

**Display proxy: the listener must be the session's identity** (gap 2, its
own commit, all accounts). On every upstream connect
(`crates/rast-server/src/native_proxy.rs`, both connect paths) the server
looks up the established pair in `/proc/self/net/tcp` and `tcp6` (kernel
data, readable under `ProtectProc=invisible`; the accepted socket carries
the listener's uid): the entry whose local address is the Xpra port and
whose remote address is the server's end. It proxies only when that uid is
the session identity's (`linux_uid_base + user id`). A missing, malformed
or foreign entry refuses the connection with an error to the viewer and a
warning in the log. This also covers a squatter that wins the race right
after the helper's `port_conflict` check.

**Pilot gate, quota, backup.** The pilot stays a server decision (ADR
0007): the server calls `ensure` only for pilot accounts, and its start-up
reconcile releases managers outside the pilot; the helper stays
pilot-agnostic. Kernel uid quota is unchanged (files the manager writes in
HOME count, as everything the user owns). `/run/user/<uid>` is a tmpfs
whose pages are charged to the user's slice, not to the storage quota. The
backup reader is unchanged: it reads HOME and never `/run/user`.

**Share groups.** `parse_proc_cgroup`/`is_user_unit` also count
`user@<uid>.service` (every process of the user manager, whatever unit the
user put it in) as the user's unit, so `share_session_groups` sees
user-started services and timers. Both a grant's "Restart session now" and
a revocation restart end the manager (`release` `stop`), because every
user unit inherits the manager's supplementary groups from its start.

### 4. `XDG_RUNTIME_DIR=/run/user/<uid>`

One-HOME accounts use logind's `/run/user/<uid>` (tmpfs, 0700, made by
`user-runtime-dir@<uid>.service`) for every unit, terminals included, so
everything that defaults to `$XDG_RUNTIME_DIR` (the bus, the sound server,
Xpra's sockets, Flatpak, portals) agrees. Moves:

| Item | Before | After (one-HOME) |
| --- | --- | --- |
| Xpra `--socket-dir` | `/run/rast-linux/<uid>/sockets` | `/run/user/<uid>/xpra` |
| Xpra log | `/run/rast-linux/<uid>/xpra.log` | `--log-dir=/run/user/<uid>/xpra` |
| Pulse socket | `/run/rast-linux/<uid>/pulse/native` | `/run/user/<uid>/pulse/native` (user `pulseaudio.socket`) |
| `TMPDIR`, `XDG_CACHE_HOME` | `/run/rast-linux/<uid>/{tmp,cache}` | not set (`/tmp`, `~/.cache`; ADR 0005 section 7, task 3.5) |
| `XAUTHORITY` | discovered from Xvfb's `-auth` as root | `<HOME>/.Xauthority` for Xpra and apps; `discover_session_xauthority` stays for legacy sessions only |
| Xpra audio config `/run/rast-linux/.xpra-audio` | used | not used (no Xpra PulseAudio child) |

What `rast serve` (uid 993) reaches does not change: the Xpra TCP port
(nftables, plus the listener check of section 3),
`/run/rast/files/u<uid>.sock` (system socket unit, `root:rast 0660`),
terminal sockets in `<run_dir>/terminal/<sid>/` and the helper socket. It
reaches nothing in `/run/user/<uid>` (0700, `EACCES`, tested). A later agent
service that the server must reach (tasks 2.3, 2.4) gets a system socket
unit under `/run/rast/` like the file agent, never a socket in `/run/user`.
`/run/rast-linux/<uid>` of a one-HOME identity is removed (no-follow) by
`ensure` once no legacy unit of that identity is active; the port records
in `<run_dir>/xpra-ports/` are not below it.

### 5. One D-Bus session bus

The bus is the distro's user `dbus.socket` at `/run/user/<uid>/bus`
(`dbus-daemon` from `dbus-user-session`; a host with `dbus-broker` works the
same way). Every user unit gets `DBUS_SESSION_BUS_ADDRESS` from the manager
(set by `dbus.socket`). For one-HOME accounts:

- `dbus-run-session` is no longer added to Chromium launches (legacy keeps
  it);
- Xpra gets `--dbus-launch=` (empty: never a bus of its own) and keeps
  `--dbus=no` and `--notifications=no` (task 2.4 owns notifications);
- the Xpra unit carries `ExecStartPre=-/usr/bin/dbus-update-activation-environment
  --systemd DISPLAY=:<n> XAUTHORITY=<HOME>/.Xauthority` (display number and
  HOME validated by the helper) and
  `ExecStopPost=-/usr/bin/systemctl --user unset-environment DISPLAY
  XAUTHORITY`. The user manager runs them as the identity inside the slice,
  so D-Bus- and systemd-activated GUI services (a portal file chooser before
  task 2.3, keyring prompts in task 2.2) open in the session; a failure is
  the user's own and only logged (`-`);
- RAST does not start `graphical-session.target` in this task (it would
  run `xdg-user-dirs` and other distro services that write into HOME);
  tasks 2.3 and 2.4 revisit it.

### 6. Audio (agent decision 4)

One-HOME accounts use **their user manager's own sound server**: the
distro's socket-activated `pulseaudio.socket` (PulseAudio 17 on production;
on a host with `pipewire-pulse.socket` the same path and protocol). Xpra
becomes a client of it: `--pulseaudio=no --speaker=on
--audio-source=pulse:device=Xpra-Speaker.monitor --microphone=disabled`.
Apps get no `PULSE_*` variables: libpulse, Flatpak's `--socket=pulseaudio`
and D-Bus-activated services all find `$XDG_RUNTIME_DIR/pulse/native` by
default, and same-uid clients are authorised by `SCM_CREDENTIALS`. Without
hardware access (identities are not in `audio` and have no seat) the server
has only null sinks and their monitors.

**The sink lives as long as the session** (finding 9). The distro daemon
exits 20 s after its last client (`exit-idle-time`), and a sink loaded at
runtime goes with it, for example before the viewer enables sound. The
helper therefore starts `rast-audio-u<id>.service` right after the Xpra
unit (`BindsTo=` it, so it stops with it). Its main process is `rast
session-audio` (hidden subcommand, new module
`crates/rast-agent/src/session_audio.rs`, dispatched in `main()` before
the tokio runtime, no configuration), running as the identity:

1. `pactl list short sinks` (bounded, parsed strictly); when `Xpra-Speaker`
   is missing, `pactl load-module module-null-sink sink_name=Xpra-Speaker
   channels=2 rate=48000`; then `pactl set-default-sink Xpra-Speaker`;
2. runs `pactl subscribe` as a child: a connected client, so the daemon
   never reaches its idle exit while the session runs; a `remove` event on
   a sink (lines bounded) repeats step 1;
3. when `pactl subscribe` ends (the daemon was restarted or crashed),
   repeats from step 1 after a back-off of 1 s up to 30 s.

`pactl` reads the identity's own `client.conf` there, inside the slice:
whatever it starts is the user's own. A failure only means no sound; the
session never depends on it. The extra modules of `/etc/pulse/default.pa`
(`module-udev-detect`, `module-gsettings`, `module-esound-protocol-unix`,
`module-bluetooth-*` when installed) run under the identity's uid with no
device access, so they are not a boundary between users. The microphone
stays `disabled`.

Rejected: Xpra's private PulseAudio at the standard path (it would fight the
globally enabled `pulseaudio.socket` for the same socket, which needs a
per-user mask that the user can undo, and the distro daemon would still
start at every manager start); keeping the private path with pinned
`PULSE_*` variables (D-Bus-activated services and Flatpak apps do not get
them); PipeWire now (new packages on production, and on Ubuntu it replaces
PulseAudio for every account on the host); the sink set up by the helper
(finding 1); a user `daemon.conf` with `exit-idle-time = -1` (it would mean
RAST writing into HOME, and the user can change it).

Packages: **none new on production** (present: `dbus-daemon`, `dbus-bin`,
`dbus-user-session`, `libpam-systemd`, `pulseaudio`, `pulseaudio-utils`,
`xpra-audio`, `gstreamer1.0-pulseaudio`, polkit). `docs/INSTALL.md` and
the installer's prerequisite check add `dbus-user-session`,
`libpam-systemd` and `pulseaudio-utils` (for `pactl`). Not needed:
`dbus-broker`, `pipewire*`, `systemd-container`. `gnome-keyring` comes with
task 2.2, `flatpak` with Phase 3.

### 7. Adoption after a restart

- **`rast serve` restarts.** As today, from `linux_sessions`/`linux_runs`
  plus `units_list`, which now also lists the RAST units of every manager
  from `user_managers_list` (as the identity, filtered as in section 3). A
  row is adopted from the manager the unit is found in. For a manager whose
  state is `unknown`, the server keeps its rows and the manager untouched
  (no `session_stop`, no release) and retries the list every 30 s; only an
  `ok` answer without the unit ends a row. This changes today's `recover()`
  (`native_session.rs`), which keeps rows only when the whole call fails. A
  one-HOME account's session still running as a system unit (started by the
  previous release) is ended once, like the sessions without audio today.
  Then the server reconciles managers (section 2): a manager without adopted
  work is released only after `linux_idle_minutes` from the server start.
- **The helper restarts.** User managers and their units are not children
  of the helper and keep running. The helper restores `xpra_ports` from its
  port records (section 3) and runs the linger fail-safe (section 2);
  `user_managers_list` recovers everything else from logind's linger files
  and `user@<uid>.service`. It keeps no other state.
- **A user manager stops by itself** (the user ran `systemctl --user exit`,
  or it crashed). Its transient units are gone; `unit_status` reports them
  inactive, the session ends as after an app crash (`session_stop` drops the
  port record once the port is free), and the next launch's `ensure` starts
  the manager again.
- **Reboot.** Transient units and port records are gone. A linger left over
  from a crash starts the manager at boot; the helper's fail-safe stops it
  at helper start, and the server's reconcile would release it after the
  idle time. The boot path cannot be tested on production (no reboot); unit
  tests of the fail-safe cover it.

### 8. Unit lifetime: the subreaper wrapper (agent decision 5)

App units in the user manager get the wrapper as their main process:
`<rast_bin> app-reaper -- <argv…>`. The helper builds the prefix itself
(the server never sends it) from the fixed path `/usr/local/bin/rast`
(test override), which it checks at start: the file and every parent
directory are root-owned and not group- or other-writable. It does not use
`/proc/self/exe`, which becomes "(deleted)" after `rast update`.

`rast app-reaper` (hidden subcommand, new module
`crates/rast-agent/src/reaper.rs`) is dispatched in `main()` **before** the
tokio runtime is built, so it has exactly one thread. It reads no
configuration and logs only to stderr (the unit's journal).

1. `prctl(PR_SET_CHILD_SUBREAPER, 1)`; block `SIGCHLD`, `SIGTERM`,
   `SIGINT`, `SIGHUP`, `SIGQUIT`, `SIGUSR1`, `SIGUSR2`.
2. Spawn `argv` with `std::process::Command` (inherits environment, working
   directory and fds 0–2; every other fd is close-on-exec; std resets the
   signal mask and `SIGPIPE` in the child). Exec failure: a message, exit
   127 (`ENOENT`) or 126 (other errors).
3. Loop on `sigwaitinfo`. `SIGCHLD`: reap with `waitpid(-1, WNOHANG |
   __WALL)` until nothing is left to reap; record the status of the first
   child; when `waitpid` reports `ECHILD`, no descendant is left (an
   orphaned descendant is re-parented to the wrapper, a living one has a
   living parent below it), so leave the loop. **Any other signal is not
   forwarded**: with `KillMode=control-group`, `systemctl stop` already
   sends `SIGTERM` to every process of the cgroup, so forwarding would give
   each process a second one, which some programs treat as "quit now,
   skip saving" (finding 8). The wrapper logs the signal and keeps
   reaping; it never exits before its last descendant, so systemd does not
   signal the rest of the cgroup a second time either.
4. Exit with the first child's code; if that child died from a signal,
   reset the signal to its default action, set `RLIMIT_CORE` to 0, unblock
   and raise it, so systemd records the same result (`SIGTERM` counts as
   clean, as for a service without the wrapper).

`KillMode=control-group` stays: **stop** (End application, session stop,
account deactivation) sends `SIGTERM` to every process in the unit's cgroup
at once and `SIGKILL` after `TimeoutStopSec`, so stop ends everything,
including processes that are not the wrapper's descendants. The session
unit keeps `ExitType=main` without the wrapper. A process that a user moves
into an app's cgroup is not the wrapper's descendant; when the wrapper
exits, systemd stops the unit and kills it.

With `Type=exec` the unit's start now succeeds even when the app's binary
is missing (the wrapper exists); the app fails a moment later with exit
127. The server reads `unit_status` for a run that ends within the window
adoption timeout and shows "The app could not start (exit 127: command not
found)" instead of an empty window.

**`ExitType=cgroup`: not used, on any systemd version.** No strong reason
was found: it would save one small process per app (the wrapper's private
memory is a few MB; the binary's pages are shared) and keep `MainPID` on the
app, but nothing in RAST needs that (window matching uses `_NET_WM_PID`,
ADR 0004 decision 5 reads `/proc/<_NET_WM_PID>`); the user manager supports
it on 259, but Ubuntu 22.04 (249) does not, and two mechanisms need two sets
of contract tests. `native-helper --check` and `rast doctor` report "app
unit lifetime: subreaper wrapper".

Contract tests:

- **CI, no systemd** (`tests/app_reaper.rs`, runs the built binary with
  `/bin/sh`, synchronised through pipes, no sleeps): a grandchild detached
  with `setsid` keeps the wrapper alive after the direct child exits; the
  wrapper exits after the last descendant with the first child's exit code;
  signal death is mirrored; `SIGTERM` to the wrapper is not passed on and
  the wrapper keeps running until its descendants exit; exec failure gives
  127 or 126; the child starts with an empty signal mask and default
  dispositions and only fds 0–2.
- **Real systemd, opt-in** (`tests/user_unit_contract.rs`, `#[ignore]`,
  `RAST_SYSTEMD_CONTRACT=1`): uses the caller's own `systemd-run --user`
  (no root) with the unit properties the helper generates. The unit stays
  active while a detached grandchild runs after the main process exited;
  it becomes inactive after the last process exits; during `systemctl
  --user stop` each process receives exactly one `SIGTERM` (counted by
  trap handlers); stop kills all, a grandchild that ignores `SIGTERM`
  included (with `TimeoutStopSec=2s`); `ExecStartPre=`/`ExecStopPost=`
  properties are accepted. Run on the test host by the coordinator.
- **Helper argv tests** (fake `systemd-run`): app units carry the wrapper
  prefix, `KillMode=control-group` and no `ExitType=`.

### 9. Limits (agent decisions 3 and 6)

| Key (env) | Legacy accounts (unchanged meaning) | One-HOME accounts (new meaning) |
| --- | --- | --- |
| `linux_session_memory_mb` (default **8192**, was 4096) | `MemoryMax` of the Xpra unit | `MemoryMax` of `user-<uid>.slice`: session, every app, terminals, file agent and user services together |
| `linux_session_cpus` | `CPUQuota` of the Xpra unit | `CPUQuota` of `user-<uid>.slice` |
| `linux_session_tasks` (default **8192**, was 2048) | `TasksMax` of the Xpra unit | `TasksMax` of `user-<uid>.slice`, and of each app unit |
| `linux_app_memory_mb` (and an app's own memory setting) | `MemoryMax` of the app unit | `MemoryHigh` of the app unit (throttle and reclaim, never a kill), with `MemoryMax=infinity` |
| `linux_app_cpus` (and an app's own CPU setting) | `CPUQuota` of the app unit | the same, inside the slice's quota (needs `cpu` delegated) |
| `linux_app_tasks` | `TasksMax` of the app unit | not used (apps get `linux_session_tasks`) |
| `linux_idle_minutes` | session idle time | also how long the user manager stays after the last session, app or terminal |
| `linux_admission_reserve_mb` | admission check | admission check |
| `terminal_*` | per terminal | unchanged, inside the slice |

**Defaults.** An explicit value in `rast.toml` or the environment still
wins; the configuration ranges are unchanged (512–262144 MiB, 64–131072
tasks). The installer writes `deploy/rast.toml.example` with every value
spelled out, so `/etc/rast/rast.toml` of an existing install keeps
`linux_session_memory_mb = 4096` and `linux_session_tasks = 2048` unless an
administrator edits it; `rast doctor` says so for one-HOME accounts. The
template shows 8192 for new installs. Reasons: 2048 tasks for Xpra, every
app, terminals (512 each by default) and the file agent together is easy
to reach with Chrome plus VS Code threads, and `clone` failures crash apps;
4096 MiB for a whole account means more OOM kills. Neither widens access
between accounts.

**Admission.** A session start is admitted with
`min(linux_session_memory_mb, linux_app_memory_mb)` plus
`linux_admission_reserve_mb`, in both modes (today: the full session
limit). The session limit is a ceiling, not a reservation, and with the
new default the old rule would refuse every session on a host with less
than 9 GiB free. Each app keeps its own admission (its memory value plus
the reserve). `rast doctor` warns when the native pilot accounts times
`linux_session_memory_mb` exceed the host's RAM (overcommit: the slices can
together ask for more than the host has).

**OOM.** App units also get `OOMPolicy=continue` (when the slice reaches its
`MemoryMax`, the kernel ends one process and systemd keeps the unit and the
app's other processes) and `OOMScoreAdjust=200` (above the manager's 100,
so the kernel picks an app process before Xpra, Xvfb, the bus or the sound
server; raising the value needs no privilege). The Xpra unit keeps the
default `OOMPolicy=stop`; `user@.service` itself has `OOMPolicy=continue`,
so an OOM kill never stops the whole manager. The helper accepts an app
`tasksMax` up to the session maximum for `manager: "user"`.
`rast-linux.slice`'s aggregate guardrail does not cover one-HOME accounts
any more (their slices are in `user.slice`, next to host accounts); the
per-account slice limits and the admission check are what remains. `rast
doctor` warns when `systemd-oomd` is active, because its pressure-based
kills would defeat `MemoryHigh`.

**Throttling.** Above `MemoryHigh` the kernel reclaims the app's memory,
which goes to swap. With swap nearly full (production: 3.6 of 4 GiB), a
throttled app can stall for long periods instead of being killed. The
test-host memory test therefore also checks responsiveness: while the test
app is throttled, typing in another app of the same session echoes within
one second (the probe of the app matrix).

**`NoNewPrivileges`** stays off for app units. Test before any use, on the
test host, as a `t-` identity: start Google Chrome as a user unit with
`-p NoNewPrivileges=yes` and check `chrome://sandbox` (namespace sandbox
"Yes", SUID sandbox "No", seccomp-BPF "Yes") and the journal (no AppArmor
`DENIED` for the `chrome` profile; this host restricts unprivileged user
namespaces to AppArmor-allowed programs, and `chrome-sandbox` is the
set-uid fallback that NNP disables). The Flatpak half needs `flatpak`
installed and moves to task 3.5 (ADR 0004 decision 4). Adopted only with
both results and the maintainer's approval.

**Upgrade note** (text for `CHANGELOG.md`, `## [1.0.1]`):

> - **Native apps of one-HOME accounts run in a systemd user session.**
>   For accounts in one-HOME mode, RAST now starts the account's own
>   systemd user manager (`user@<uid>.service`, with linger only while the
>   account uses native apps or terminals) and runs the graphical session
>   and apps there; the file agent and terminals join the same per-account
>   slice `user-<uid>.slice`. The limits change meaning for these accounts
>   only:
>   - `linux_session_memory_mb`, `linux_session_cpus` and
>     `linux_session_tasks` now limit everything of one account together
>     (graphical session, every app, terminals, file agent, user services):
>     `MemoryMax`, `CPUQuota` and `TasksMax` of `user-<uid>.slice`. They
>     used to limit the graphical session only, with each app's limits on
>     top. The task limit now also counts every thread of every app and
>     terminal (each terminal may use up to `terminal_tasks`).
>   - **New defaults:** `linux_session_memory_mb` and `linux_session_tasks`
>     default to 8192 (were 4096 and 2048), for legacy accounts too (their
>     graphical session gets the larger limit). The installer writes both
>     values into `/etc/rast/rast.toml`, so an existing install keeps 4096
>     and 2048 until you change them there; `rast doctor` reminds you. If
>     users run several large apps at once, raise them.
>   - A graphical session is now admitted when the server has the smaller
>     of `linux_session_memory_mb` and `linux_app_memory_mb`, plus
>     `linux_admission_reserve_mb`, free (it used to need the whole session
>     limit free).
>   - `linux_app_memory_mb` (and an app's own memory setting) is now a soft
>     limit (`MemoryHigh`): above it an app is slowed down and its memory
>     reclaimed (moved to swap), but it is not killed for it. On a host
>     whose swap is nearly full, such an app can stall for a long time;
>     raise `linux_app_memory_mb` for apps that need more. When the account
>     reaches `linux_session_memory_mb`, the kernel ends one app process and
>     the app's other processes keep running.
>   - `linux_app_cpus` keeps its meaning, inside the account's CPU limit.
>   - `linux_app_tasks` no longer applies to these accounts; each app may
>     use up to `linux_session_tasks`.
>   - `linux_idle_minutes` also sets how long the user manager stays after
>     the account's last session, app or terminal.
>
>   Accounts in the legacy layout keep the old meaning of every key.
>   Graphical sessions of one-HOME accounts that were started by the
>   previous release end at the first start of this one (save work first).
>   New prerequisites for native apps: `dbus-user-session`,
>   `libpam-systemd`, `pulseaudio-utils`. Rolling back: see
>   `docs/OPERATIONS.md` ("Rollback").

### 10. Security

**What an identity can do now that it could not before** (one-HOME
accounts, while RAST keeps their manager):

- use its own systemd user manager: start, stop and change its own user
  units and timers, stop RAST's session and app units (its own processes
  anyway), override their properties with drop-ins and raise its per-app
  `MemoryHigh`/`CPUQuota` (not a boundary);
- own names on its own session bus and activate services under its uid;
- run the distro's user units (section "Host facts"), all as itself;
- use a logind tmpfs at `/run/user/<uid>` (capped at 10 % of RAM; its pages
  count against the account's slice);
- start processes from a terminal through `systemd-run --user` without the
  terminal unit's `NoNewPrivileges`, `ProtectSystem` and `PrivateTmp`. That
  is what its apps already have; the terminal hardening is defence in
  depth, not a boundary.

The PAM `systemd-user` stack runs at each manager start (account check, so
the shadow entry must not expire, as A7 noted; `pam_limits`,
`pam_loginuid`, `pam_keyinit`, `pam_systemd`).

**Why that is acceptable:** all of it runs as the identity's uid, inside
`user-<uid>.slice` with limits that only root can change, and it is what an
ordinary Linux account with a desktop session has. Isolation between users
stays uid-based. Nothing of the identity runs in the helper's cgroup
(section 3). polkit: the user manager registers a logind session of class
`manager` (`Active=yes`, `Remote=no`, no seat, no scope; host facts), so
"identities never have a session" is not true. polkit finds a subject's
session from the process's cgroup scope, then from the user's display
session; a manager session has no scope and identities have no display
session, so processes in user units are expected to fall back to
`allow_any`, as today's terminal and app units do. This is not assumed:
the gate test (section 2) checks `reboot`, `power-off` and
`udisks2.filesystem-mount` from a user unit before the pilot uses user
sessions, and any action that answers "authorized" joins the deny rule.
Actions that are `allow_any=yes` were reachable before (the system bus is
open to every local uid); the review audited them: apart from
`set-self-linger` (denied) only three inhibitor actions, whose delay locks
are capped by `InhibitDelayMaxSec`.

**What stays forbidden:** no polkit rule that grants anything; no sudoers
entry; `rast-u<id>` is not in the `rast` group (nor in `sudo`, `admin`,
`wheel` or `empower`; `rast doctor` checks); `/run/rast/native.sock` stays
`0660 root:rast`; no helper operation is reachable from an app; the helper
runs nothing of the identity except `systemctl --user` and `systemd-run
--user`; the cron and `at` deny lists of one-HOME identities stay (A7); no
`--machine=` login sessions; no drop-in for `user@.service` or `user-.slice`
(they apply to every account on the host); the helper takes no path, port
or pid from a user manager for a root action and never opens a path below
`/run/user`; the cgroup files of `user-<uid>.slice` and of
`user@<uid>.service` itself stay root's (only the subtree below is
delegated).

**Cross-user tests that must stay:** every test in ADR 0005 "Tests"
(HOMEs, agent sockets, shares, migration) and the Xpra port firewall test,
plus, on the test host:

- `t-bob`'s processes get `EACCES` on `t-alice`'s
  `/run/user/<uid>/{bus,systemd/private,pulse/native}`, and the `rast`
  service gets `EACCES` on every `/run/user/<uid>`;
- `t-bob` listening on `t-alice`'s port while her session is stopped: her
  viewer gets an error, never `t-bob`'s display;
- as an identity, `loginctl enable-linger` and `systemctl set-property
  user-<uid>.slice …` are refused, and writing `memory.max` of
  `user-<uid>.slice` or of `user@<uid>.service` gives `EACCES`;
- with a hostile `~/.config/pulse/client.conf` in `t-alice`'s HOME
  (`autospawn = yes`, `daemon-binary = <HOME>/x`), `systemd-cgls -u
  rast-native.service` shows no process of the identity after `ensure`,
  `session_start` and `units_list`;
- one account reaching its slice `MemoryMax` kills only its own processes;
  the other account's session keeps running;
- after a share grant or revocation restart, no process below `t-bob`'s
  `user@<uid>.service` has the old groups.

Unit tests: the helper never adds or releases a port taken from a (fake)
user manager's output; `user` mode is refused for a legacy HOME; foreign
unit names and pids of another uid in `systemctl --user show` output are
dropped; a fake manager that reports changed properties of RAST's units
(a user drop-in) changes no helper result; the identity runner's child has
the fixed environment and fds 0–2 only.

### 11. Migration and rollback

- **Installs without one-HOME accounts:** nothing changes (no linger, no
  slice properties) except the polkit rule (agent decision 1) and the new
  prerequisite check.
- **Accounts already in one-HOME mode** (`t-alice`, `t-bob` on
  production): their next session or terminal start runs `ensure`; a
  graphical session still running as a system unit is ended at the first
  server start; an active file agent in `rast-linux.slice` is restarted
  into `user-<uid>.slice`; terminals started before keep running in
  `system.slice` until they close.
- **Legacy native accounts:** unchanged until `rast migrate home`. Its apply
  stops the user manager too (`release` `stop` with `wait: true`;
  `stop_user_units` and the identity-in-use check include
  `user@<uid>.service` and the linger file). Its rollback runs `release`
  `remove`, so a rolled-back account has no linger and no slice properties;
  the user manager's files in HOME (`~/.config/systemd`, `~/.local/state`)
  are handled like every other entry created after the migration.
- **Release rollback** (deploy checklist): with the new binary, for each
  test account in one-HOME mode end the sessions and run `sudo loginctl
  disable-linger rast-u<id>` and `sudo systemctl stop user@<uid>.service`,
  and (after asking the maintainer, it deletes files) remove
  `/etc/systemd/system.control/user-<uid>.slice.d/50-{MemoryMax,CPUQuota,TasksMax}.conf`;
  then `sudo rast update --version <rollback version>` and the health
  check. The old release runs these accounts with system units again. The
  polkit rule stays (it only denies; an older helper does not touch it).
  Port records are under `/run` and an older helper ignores them. No
  database migration is involved.

### 12. Implementation plan

Each commit passes the quality gates and updates the docs it touches.
Commits that touch `src/main.rs` or `crates/rast-server/src/native_proxy.rs`
start after the work on those files that is in progress now has been
committed. Commits 11 and 12 depend on each other only through the optional
`manager` field, so each stays working alone.

**Pre-release A: close the self-linger gap (first, on its own).**

1. `build(deploy): deny linger changes to RAST identities in polkit`: new
   `deploy/rast-identities.polkit.rules` and `deploy/rast-identities.pkla`,
   `scripts/sync-installer-assets.sh` (two embeds), `installer/install.sh`
   (JS rule or `.pkla` as section 2 says, native backend only),
   `installer/uninstall.sh`. Tests: `sync-installer-assets.sh --check`,
   `shellcheck`, `scripts/test-installer-clean.sh` (file 0644 `root:root`
   after install, gone after uninstall).
2. `feat(helper): install the identity polkit rule at helper start`: new
   `crates/rast-helper/src/native/polkit.rs` (embedded files, compare and
   replace, directory and link checks), `native/daemon.rs` (call at start,
   never fatal), `native/mod.rs` (`--check`). Tests in a tempdir: written
   when missing, untouched when equal, replaced when different, a link at
   the target or a directory writable by others refused, `.pkla` chosen on
   a 0.105 layout; the embedded files equal `deploy/` and name exactly the
   two actions and `^rast-u[0-9]+$`. Docs: `docs/INSTALL.md`,
   `docs/OPERATIONS.md`, `SECURITY.md`, `CHANGELOG.md` (Security: the
   self-linger gap). The `rast doctor` line comes with commit 14.
3. `chore(release): 1.0.1-alpha.N` (the next number). The maintainer
   deploys with the usual checklist (rollback: the rule may stay). Then the
   polkit gate test of section 2.

**Fixes of existing gaps (next pre-release, on their own or with the user
session series).**

4. `fix(server): proxy a display only to a listener owned by the session
   identity` (gap 2, finding 3): new `crates/rast-server/src/socket_owner.rs`
   (parser of `/proc/self/net/tcp` and `tcp6` with an injectable root,
   bounded size, established-pair lookup, IPv4-mapped addresses),
   `crates/rast-server/src/native_proxy.rs` (both upstream connects).
   Tests: owner matches; another uid refused; missing entry refused; `tcp6`
   mapped entry; malformed and oversized files refused.
5. `fix(helper): record Xpra ports and protect them before the session
   starts` (finding 2, all sessions): `native/firewall.rs` (records,
   reconcile restores all, removal only when the port is free, `tcp6` in the
   conflict check), `native/daemon.rs` (`session_start` order,
   `session_stop`). Tests with the fake nft host and a fake `/proc/net`:
   record and set entry before `systemd-run`; a failed add starts no unit;
   a helper restart during a start keeps the entry; the reconcile never
   drops a record; a `tcp6` squatter gives `port_conflict`; the record stays
   while the port still listens.

**User session series (one pre-release at the end).**

6. `feat(agent): rast app-reaper keeps an app unit alive while its
   processes run`: new `crates/rast-agent/src/reaper.rs`,
   `crates/rast-agent/src/lib.rs`, `src/main.rs` (hidden `AppReaper`,
   dispatched before the runtime), new `tests/app_reaper.rs` (section 8,
   no signal forwarding).
7. `feat(agent): rast session-audio keeps the session's sound sink`: new
   `crates/rast-agent/src/session_audio.rs`, `src/main.rs` (hidden
   subcommand, before the runtime). Tests with a fake `pactl`: the sink is
   created once, re-created after a `remove` event and after `subscribe`
   ends, back-off bounded, long lines refused.
8. `feat(helper): run user-manager commands as the identity` (finding 1):
   new `crates/rast-helper/src/native/user_manager.rs` (identity runner,
   runtime directory check, strict bounded parsing), `native/mod.rs`
   (process-group kill and bounded pipe reads for every command;
   `NativeOptions`: `loginctl_bin`, `rast_bin`, `user_runtime_root`,
   `linger_dir`, test overrides), `native/daemon.rs` (shared `systemctl
   show` parser). Tests: the child's environment, working directory, groups
   and fds; the group killed on timeout; a grandchild that holds stdout
   does not hang the call; a link, wrong owner or mode of the runtime
   directory refused; malformed or oversized output refused; foreign names
   and pids dropped; changed properties from a fake manager change no
   result.
9. `feat(helper): user_manager_ensure, user_manager_release and
   user_managers_list` (findings 5 and 6): `native/protocol.rs`,
   `native/daemon.rs`, `native/user_manager.rs`, `native/client.rs`
   (timeouts, 150 s for `wait`), `native/identity.rs` (`remove_identity`),
   `native/home_migrate.rs` (`stop_user_units`, identity-in-use check).
   Tests with fake `loginctl` and `systemctl`: limits set before linger, a
   refusal leaves no linger, a mismatch after the start stops the manager;
   linger only when the file is missing; `user@` started when inactive;
   `manager_stopping` while deactivating; the `wait` bound; values from the
   helper's configuration; `legacy_home`; unknown fields; `busy` in `idle`
   only for a terminal or a port record (a fake "running" user unit and the
   file agent do not count); stop order; `remove` deletes exactly its three
   drop-ins; the helper-start fail-safe (record, terminal, neither); the
   list filtered to the identity range; migration refused while `user@` is
   active.
10. `feat(helper): file agents and one-HOME terminals in user-<uid>.slice`:
    `native/homes.rs`, `native/daemon.rs` (terminal arguments and
    environment). Tests: arguments per mode; environment only for a valid
    runtime directory; an agent in the old slice is restarted.
11. `feat(helper): session and app units in the user manager`:
    `native/daemon.rs` (`manager` field, user-unit arguments with
    `ExecStartPre=`/`ExecStopPost=`, `rast-audio-u<id>`, wrapper prefix and
    path check, app limits, `units_list` with per-manager state and
    parallel bounded queries, `unit_status`), `native/protocol.rs` (field,
    app `tasksMax` rule, unit names), `native/shares.rs` (`user@<uid>.service`,
    `rast-audio-u<id>`). Tests: today's legacy arguments unchanged
    (snapshot); user arguments for session, audio keeper and app; refusals;
    a fake manager that claims another user's port changes nothing; one
    manager timing out gives `unknown` and does not delay the others;
    `share_session_groups` counts a process below `user@<uid>`.
12. `feat(session): one-HOME sessions under the user manager`:
    `crates/rast-session/src/linux_apps/native_session.rs` (`ensure` before
    the first start with `manager_stopping` retries, `manager: "user"`,
    user-mode `xpra_args`, no `/run/rast-linux` preparation, adoption per
    manager with `unknown` kept, ending old system sessions, session
    admission of agent decision 6, the exit-127 message),
    `crates/rast-helper/src/native/daemon.rs` (`check_memory_admission` for
    sessions), `crates/rast-session/src/linux_apps/mod.rs` (no
    `dbus-run-session` for one-HOME), `crates/rast-agent/src/terminal/`
    (`ensure` before a one-HOME terminal, none for stranded accounts).
    Tests with the fake helper: call order, legacy unchanged, Chromium
    wrapper for legacy only, adoption with an `unknown` manager keeps the
    rows and sends no `session_stop`, admission values.
13. `feat(config): raise the linux_session_memory_mb and
    linux_session_tasks defaults to 8192`: `crates/rast-base/src/config.rs`,
    `deploy/rast.toml.example` (+ `installer/install.sh`),
    `docs/CONFIGURATION.md`. Tests: the new defaults; a file value and an
    environment value still win; ranges unchanged.
14. `feat(server): user managers follow account state and the native
    pilot`: new `crates/rast-server/src/user_managers.rs` (idle release with
    an injected clock, counted from the server start; start-up reconcile;
    `busy` handling; `unknown` managers retried),
    `crates/rast-server/src/state.rs`, `crates/rast-server/src/api/users.rs`
    (deactivation), `crates/rast-server/src/share_groups.rs` (grant and
    revocation restarts), the `migrate home` CLI (`wait: true`),
    `src/main.rs` (`rast doctor`: lifetime mode, managers, `cpu` delegation,
    polkit rule, identities in admin groups, `dbus.socket`, sound socket,
    `systemd-oomd`, old explicit limit values, pilot accounts times the
    session limit above RAM) and `native/mod.rs` (`--check`). Tests:
    reconcile matrix (active × pilot × one-HOME × native × units × `ok` or
    `unknown`), no release before the idle time after a start, deactivation,
    grant and revocation, idle timing, `busy` followed by `session_stop`.
15. `docs: systemd user session (ADR 0008)`: `docs/NATIVE-LINUX-APPS.md`
    (how it works, unit lifetime, limits, the End application row and a new
    "launcher exits" row of the lifecycle table, audio, recovery, security
    model), `docs/ARCHITECTURE.md`, `docs/INSTALL.md`, `docs/OPERATIONS.md`
    (stopping a user's session, `systemd-cgls /user.slice/user-<uid>.slice`,
    rollback), `SECURITY.md`, `CHANGELOG.md` (upgrade note above), a note in
    ADR 0004 (decisions 1 and 2 refined here) and ADR 0005 (agent socket,
    `share_session_groups`), `docs/remake/PROGRESS.md`.
16. `chore(release): 1.0.1-alpha.M`, then the test-host list below.

**On the test host** (production, `t-` accounts, after the user session
deploy; root steps by the coordinator with the maintainer's permission):
the `pkcheck` gate from a user unit (section 2); linger on at the first
launch and off after `linux_idle_minutes`; slice properties (`systemctl
show user-<uid>.slice`) and `systemd-cgls` showing Xpra, the audio keeper,
apps, file agent and terminals in the slice and no identity process in
`rast-native.service` (hostile `client.conf`); the real-systemd contract
test (one `SIGTERM` per process); Chrome passes the app matrix with no
`dbus-run-session` and plays sound, also after 60 s of silence and after
`systemctl --user restart pulseaudio`; a memory test app is throttled above
`linux_app_memory_mb` (`memory.events` `high` rises, `oom_kill` stays 0)
while typing in another app still echoes within one second, and only its
own account is hit at the slice limit; the cross-user tests of section 10,
`t-bob` squatting `t-alice`'s port included; adoption after the maintainer
restarts `rast.service` and `rast-native.service`; grant and revocation
restarts; `rast migrate home` rollback and apply with a running manager
(succeeds after the bounded wait); the `NoNewPrivileges` experiment. Not
testable on production: the boot path (no reboot); the fail-safe's unit
tests cover it.

## Review response

Review: `docs/remake/review-adr-0008.md` (2026-11-18). No finding is
rejected; one recommendation is delivered differently (row "Answer 1").

| Finding | Response | Sections changed |
| --- | --- | --- |
| 1 (high) identity programs in the helper's cgroup | Accepted. The helper runs only `systemctl --user` and `systemd-run --user` as the identity; the sink setup moves to `rast-audio-u<id>`, the activation environment to `ExecStartPre=`/`ExecStopPost=` of the Xpra unit; hardened runner (cleared environment, stdin `/dev/null`, process group killed, bounded pipe reads, `close_range`); hostile `client.conf` test | 3, 5, 6, 10, 12 (commits 7, 8) |
| 2 (high) port records | Accepted. Record and set entry before `systemd-run`; removal only by stop, release and remove, and only when the port is free; the reconcile restores every record; `tcp6` in the conflict check | 3 ("Firewall"), 12 (commit 5) |
| 3 (high) proxy to any listener | Accepted, in scope, own early commit for all accounts | Context (gap 2), 3 ("Display proxy"), 10, 12 (commit 4) |
| 4 (high) identities get a `manager` session | Accepted. Host facts and sections 2 and 10 corrected; `pkcheck` gate after both deploys; the rule grows if an action answers yes | Host facts, 2, 10, 12 |
| 5 (high) adoption can kill work | Accepted. Per-manager `ok`/`unknown`, rows of `unknown` managers kept; release only after `linux_idle_minutes` from the server start; `idle` refused while a port record exists; parallel bounded queries | 2, 3, 7, 12 (commits 9, 11, 12, 14) |
| 6 (medium) linger sequencing | Accepted. `wait` with a 130 s bound, `manager_stopping`, fail-closed limit order with read-back, helper-start fail-safe, terminal check kept (`RemoveIPC=yes`) | 2, 7, 12 (commit 9) |
| 7 (medium) share grants | Accepted. "Restart session now" also runs `release` `stop` | 2, 3, 10 |
| 8 (medium) duplicated `SIGTERM` | Accepted. The wrapper forwards no signal (`SIGUSR1`/`SIGUSR2` included); contract test counts one `SIGTERM` per process | 8 |
| 9 (medium) PulseAudio idle exit | Accepted. The keeper holds a client and re-creates the sink after a restart; sound tests after 60 s and after a restart | 6, 12 |
| 10 (medium) `units_list` fan-out | Accepted with finding 5 (5 s per manager, 15 s total, parallel) | 3, 7 |
| 11 (medium) whole-account defaults | Accepted. Defaults 8192 (agent decision 3); the upgrade note explains tasks, throttling and swap; responsiveness check; doctor overcommit warning. Added: session admission (agent decision 6) and the note that installer-written configurations keep 4096 and 2048 | 0, 9, 12 (commit 13) |
| 12 (low) polkit rule | Accepted. `lufria` keeps linger (gate test); doctor warns about identities in `sudo`, `admin`, `wheel`, `empower`; the `allow_any` audit is recorded | 2, 10 |
| 12 (low) `Type=exec` and exit 127 | Accepted. The server shows the unit's failure | 8, 12 (commit 12) |
| 12 (low) user drop-ins | Accepted. Stated; unit test with changed properties | 3, 10, 12 (commit 8) |
| 12 (low) stranded one-HOME accounts | Accepted. Stated | 1, 12 (commit 12) |
| 12 (low) wrapper path | Accepted. Parent directories checked too | 8 |
| 13 (low) `ProtectHome=read-only` | Accepted. Reworded; nothing relies on it | Host facts, 3 |
| Test plan gaps 1–7 | Accepted. 1–5 in the commit tests, 6 in the test-host lists, 7 (boot path) in section 7 and PROGRESS | 7, 12 |
| Answer 1 (polkit rule first) | Adopted as agent decision 1, delivered by the installer and by the helper at start instead of "the maintainer installs the file": `rast update`'s asset refresh is decided by the old binary and cannot deliver a new file on the first update | 0, 2, 12 (commits 1–3) |
| Answers 2–4, `ExitType=cgroup` | Adopted as agent decisions 3, 2, 4 and 5 | 0 |

## Consequences

- **Easier:** apps get the standard desktop plumbing (bus, activation,
  sockets, `systemd-run --user`) that tasks 2.2–2.4 and Flatpak need;
  launchers keep the apps they open (ADR 0004); one hard limit per account
  instead of a sum; sound works for Flatpak and D-Bus-activated apps
  without variables; two existing gaps (self-linger, proxy to any listener)
  close early.
- **Harder:** two unit paths (legacy system units, one-HOME user units)
  until the last legacy account is migrated; the helper must treat a user
  manager's answers as untrusted; `systemctl stop rast-linux.slice` no
  longer ends every native session (one-HOME accounts: `release` `stop`, or
  `systemctl stop user-<uid>.slice`); the helper now writes one fixed file
  under `/etc/polkit-1` at start.
- **Risks:**
  - Installer-written configurations keep `linux_session_memory_mb = 4096`
    and `linux_session_tasks = 2048`, which now cap a whole account; heavy
    users may hit them sooner than before until an administrator raises
    them (doctor reminds).
  - A throttled app can stall for long periods on a host with little free
    swap.
  - The distro's user units run for every lingering identity; a package
    that enables a heavy user service globally runs it for them too.
  - Task 2.2 must decide whether a RAST login keeps the user manager, so
    that the keyring it unlocks stays unlocked while the account is signed
    in but idle.
  - `systemd-oomd` on a host would kill throttled apps (doctor warns).
- **Decided** (agent decisions, to be confirmed by the maintainer): see
  section 0. They replace the open questions of the first draft.
- **Still open for the maintainer:**
  1. Whether production's `/etc/rast/rast.toml` should be raised to 8192
     (it probably holds the installer's 4096 and 2048; not read, the path
     is off limits).
  2. Agent decision 6 (session admission) and the helper writing the
     polkit rule at start, both new in this revision.
  3. If the `pkcheck` gate from a user unit answers "authorized" for any
     action, the rule grows in an extra pre-release before the pilot uses
     user sessions.
