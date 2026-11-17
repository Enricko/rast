# 0008. One systemd user session per one-HOME account

- Status: proposed (task 2.1 plan; items marked **Maintainer** need the
  maintainer's approval before they are implemented)
- Date: 2026-11-17

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
  command line of the unit's process tree (`session_port`) and the set is
  reconciled against live `rast-xpra-*` units when the helper starts.
- **D-Bus.** There is no session bus. Xpra runs with `--dbus=no`; Chromium
  apps are wrapped in `/usr/bin/dbus-run-session --`
  (`native_launch_request`, `crates/rast-session/src/linux_apps/mod.rs`),
  one private bus per launch.
- **Audio.** Xpra starts its own PulseAudio child (`audio.rs`:
  `pulseaudio -n`, module loading disabled, a null sink `Xpra-Speaker`,
  socket `/run/rast-linux/<uid>/pulse/native`, cookie auth); every app gets
  `PULSE_SERVER`, `PULSE_COOKIE` and friends pinned to it.
- **Adoption.** `NativeSessions` reads `linux_sessions`/`linux_runs` and the
  helper's `units_list` (system units only) at start.
- **Group restarts.** `share_session_groups` (`shares.rs`) counts a
  process as "in the user's units" when the first non-slice component of
  its cgroup is `rast-xpra-u<id>`, `rast-files-u<uid>`, `rast-app-u<uid>-…`
  or `rast-term-u<uid>-…`; ADR 0005 notes that Phase 2 must extend this.

### Host facts (production, checked 2026-11-17, read-only, no root)

| Fact | Value |
| --- | --- |
| systemd | 259 (`259.9-0ubuntu0.1`), built `-UTMP` |
| `user@.service` | `PAMName=systemd-user`, `Slice=user-%i.slice`, `Delegate=pids memory cpu`, `DelegateSubgroup=init.scope`, `TasksMax=infinity`, `OOMScoreAdjust=100`, `KillMode=mixed`, `TimeoutStopSec=120s` |
| `user-.slice.d/10-defaults.conf` | `StopWhenUnneeded=yes`, `TasksMax=33%` |
| `/usr/lib/pam.d/systemd-user` | `common-account`, `pam_loginuid`, `pam_limits`, `common-session-noninteractive`, `pam_keyinit`, `pam_systemd` |
| logind | defaults (`KillUserProcesses=no`, `UserStopDelaySec=10`, `RuntimeDirectorySize=10%`); linger on for `lufria` and `root` only (no `rast-u*`) |
| `--machine=<user>@.host` (sd-bus, `libsystemd-shared-259`) | runs `systemd-run -M .host -PGq --wait -pUser=<user> -pPAMName=login systemd-stdio-bridge` for **every** connection |
| `/etc/pam.d/login` | `pam_nologin`, `pam_loginuid`, `pam_motd` (runs `update-motd.d` as root), `pam_env`, `pam_limits`, `pam_mail`, `pam_keyinit`, `common-session` with `pam_systemd` |
| D-Bus | `dbus-daemon` 1.16.2 and `dbus-user-session` installed; user `dbus.socket` (`%t/bus`) enabled globally; `dbus-broker` not installed; `dbus-update-activation-environment` (`dbus-bin`) present |
| Sound | `pulseaudio` and `pulseaudio-utils` 17.0 installed; user `pulseaudio.socket` (`%t/pulse/native`, `ConditionUser=!root`) and `pulseaudio.service` (`default.target.wants`) enabled globally in `/etc/systemd/user/`; PipeWire not installed |
| Other user units enabled globally | `gpg-agent*.socket`, `dirmngr.socket`, `keyboxd.socket`, `ssh-agent.socket`, `snapd.session-agent.socket`, `pk-debconf-helper.socket`, `launchpadlib-cache-clean.timer`; `graphical-session-pre.target.wants`: `xdg-user-dirs`, `user-session-migration`, `xdg-desktop-portal-rewrite-launchers` |
| Portals | `xdg-desktop-portal` 1.21.1, `xdg-desktop-portal-gtk` installed |
| Not installed | `flatpak`, `gnome-keyring`, `systemd-oomd`, `systemd-container` |
| polkit | `org.freedesktop.login1.set-self-linger`: `allow_any=yes`; `set-user-linger`: `auth_admin_keep` |
| Sandboxes | `kernel.apparmor_restrict_unprivileged_userns=1`; AppArmor profiles `chrome`, `chromium`, `flatpak`, `bwrap-userns-restrict`; `/opt/google/chrome/chrome-sandbox` is set-uid root; Yama `ptrace_scope=1` |

**Finding (exists today, not caused by this ADR).** Because
`set-self-linger` is `allow_any=yes`, every `rast-u<id>` identity can run
`loginctl enable-linger` itself today (from a terminal or an app). It then
gets a user manager that starts at boot and runs timers and services outside
RAST's units, which sidesteps the cron and `at` deny lists of agent decision
A7. No identity lingers on production now. Decision 2 closes this.

## Decision

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

### 2. Linger, as RAST needs it (helper operations)

Linger is the only way an identity gets a user manager: identities have the
nologin shell and no password, so no logind session ever starts for them.
RAST turns linger on when a one-HOME account starts native work and off
when the account has had none for `linux_idle_minutes`. It is **not**
permanent: a permanent linger keeps a user manager, PulseAudio, the D-Bus
daemon and every unit the user enabled running for every account from boot
on (about 20 MB per account and a cron equivalent that A7 denies), and in
task 2.2 it would keep the keyring unlocked until reboot.

New operations (strict JSON, `deny_unknown_fields`, no path fields; they
take a per-uid lock in the helper, so ensure, release and starts of one
account never interleave):

| Operation | Fields | Behaviour |
| --- | --- | --- |
| `user_manager_ensure` | `userId` | Identity from `require_identity` (passwd entry with the expected uid, gid and nologin shell). HOME must be owned by the identity, otherwise `legacy_home`. (1) Sets the configured session limits on `user-<uid>.slice` (section 3, "Limits") with `systemctl set-property user-<uid>.slice MemoryMax=<M>M CPUQuota=<C>% TasksMax=<T>` when `systemctl show` reports other values; the values are the helper's own `session_limits` from the configuration, never taken from the request. (2) `loginctl enable-linger rast-u<id>` when `/var/lib/systemd/linger/rast-u<id>` is missing. (3) `systemctl start user@<uid>.service` when it is not active (also after a user stopped its own manager or a stop was pending). (4) Waits up to 10 s for `ActiveState=active` and checks `/run/user/<uid>` (real directory, not a link, owner uid, mode 0700). Returns `{"uid":200127,"runtimeDir":"/run/user/200127","lingering":true,"started":false,"limits":{"memoryMaxMb":4096,"cpuQuotaPercent":400,"tasksMax":2048},"cpuDelegated":true}`. Idempotent: a second call runs only the two `systemctl show` queries. |
| `user_manager_release` | `userId`, `mode`: `"idle"`, `"stop"` or `"remove"` | `idle`: refuses with `busy` (and the unit names) while a terminal unit of the identity is active (system manager); the server sends it only when its own records show no session, app or terminal, and the helper does not consult the user manager's unit list, which the user controls (a fake "running" unit must not keep the manager alive). `stop`: first stops the identity's terminals and file agent, refuses with `identity_in_use` if one stays active. Both then run `loginctl disable-linger rast-u<id>` and `systemctl stop --no-block user@<uid>.service`, which ends every user unit (RAST's session and apps included) and releases the session's recorded port. `remove`: as `stop`, then deletes the drop-ins that `set-property` wrote (`/etc/systemd/system.control/user-<uid>.slice.d/50-{MemoryMax,CPUQuota,TasksMax}.conf`, opened without following links; the stopped slice loads without them next time). Returns `{"lingering":false,"managerActive":false,"stopped":["…"]}` (`managerActive` true while the manager is still stopping). Idempotent; an unknown identity in `remove` is a no-op. |
| `user_managers_list` | none | RAST identities that linger or whose `user@<uid>.service` is active: names in `/var/lib/systemd/linger/` that match `rast-u<digits>` with a uid in the identity range, plus `systemctl show 'user@*.service'` filtered to that range. Returns `{"managers":[{"userId":127,"uid":200127,"lingering":true,"active":true}]}` (at most 10 000). |

`loginctl enable-linger <name>` calls logind's `SetUserLinger` on the system
bus; logind accepts it from root without asking polkit, writes
`/var/lib/systemd/linger/<name>` and starts `user-runtime-dir@<uid>.service`
and `user@<uid>.service`. The helper already has the system bus and full
root. In step (1) of `ensure`, if systemd refuses the property change of a
slice that is not loaded yet, the step runs again after step (3).

**When linger goes off:**

| Event | Call |
| --- | --- |
| No RAST session, app or terminal for `linux_idle_minutes` (server task, injected clock) | `release` `idle` (the helper checks for terminals again) |
| Account deactivated (`api/users.rs`, next to `stop_user_sessions`) | `release` `stop` |
| Share access revoked, after the 60 s warning (D7, `share_groups::restart`) | `release` `stop` (the next launch starts the manager with the new groups) |
| Server start: a listed manager whose account is inactive, missing, outside the pilot, not one-HOME, or `native_enabled = false` | `release` `stop` |
| Server start: a listed manager with no adopted session, run or terminal (for example after a reboot) | `release` `idle` |
| `rast migrate home` apply (step 1, "stop the user's units") | `release` `stop` |
| `rast migrate home --rollback` | `release` `remove` |
| `identity_remove` | the same steps as `remove`, before `userdel` |

**Maintainer: deny self-linger.** The installer adds
`/etc/polkit-1/rules.d/49-rast-identities.rules` (source
`deploy/rast-identities.polkit.rules`):

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
shortcut" ADR 0004 forbids), and it also closes the existing gap above. On
hosts whose polkit reads only `.pkla` files (0.105, Ubuntu 22.04) the
installer writes the equivalent local authority entry instead. The
uninstaller removes it; `rast doctor` and `native-helper --check` warn when
it is missing. Without it, linger stays something an identity can turn on
by itself; RAST then still turns it off at every point in the table and at
every server start.

### 3. Where units run and how the helper starts them

| Unit | Manager | Slice | Notes |
| --- | --- | --- | --- |
| `rast-xpra-u<id>.service` (Xpra) | user | `session.slice` | `Type=exec`, `KillMode=control-group`, `Restart=no`, `ExitType=main` (default); no per-unit limits (the user slice caps it) |
| `rast-app-u<uid>-<app>-<n>.service` | user | `app.slice` | main process `rast app-reaper` (section 8), limits in section 9 |
| distro user units (`dbus.socket`, `pulseaudio.socket`, agents) | user | as shipped | started by `default.target`/`sockets.target` |
| `rast-files-u<uid>` (file agent) | system | `user-<uid>.slice` | unchanged otherwise: socket `root:rast 0660`, hardening, `MemoryMax=512M`; an active agent in another slice is restarted once |
| `rast-term-u<uid>-<sid>` (one-HOME terminal) | system | `user-<uid>.slice` | unchanged hardening and `terminal_*` limits; gets `XDG_RUNTIME_DIR=/run/user/<uid>` and `DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/<uid>/bus` when that directory is the identity's |
| admin root shells, `rast-backup-reader`, `rast-rootfs` | system | unchanged | no identity |
| legacy accounts' units | system | `rast-linux.slice` | unchanged |

Unit names are unchanged, so `validate_unit`, `is_user_unit`, the database
rows and the shim keys stay as they are. The manager follows from the
account mode; the server states it and the helper checks it: `session_start`,
`session_stop`, `app_start`, `app_stop` and `unit_status` get an optional
field `"manager": "system" | "user"` (default `system`, so an older server
keeps today's behaviour). `user` is refused with `legacy_home` for a legacy
HOME and with `user_manager_unavailable` when `user@<uid>.service` is not
active. With `user`, `runtimeDir` must be absent. `units_list` returns the
units of both managers, each with `"manager"`.

**The helper drives a user manager as the identity, without PAM.** For
`systemctl --user`, `systemd-run --user`, `pactl` and
`dbus-update-activation-environment` the helper forks, drops to the
identity (`setgroups` to none, `setgid`, `setuid`; `std::process::Command`
`uid`/`gid`), clears the environment to `XDG_RUNTIME_DIR=/run/user/<uid>`,
`DBUS_SESSION_BUS_ADDRESS=unix:path=/run/user/<uid>/bus`, a fixed `PATH`
and `LC_ALL=C`, uses `/` as working directory, passes `--no-ask-password`,
and keeps the usual timeouts and output caps. `systemctl --user` and
`systemd-run --user` talk to `$XDG_RUNTIME_DIR/systemd/private`. Before
each call root checks `/run/user/<uid>` with `lstat` (real directory, owner
uid, 0700); root never opens a path below it. `rast-native.service` keeps
`ProtectHome=read-only`, which makes `/run/user` read-only for the helper
and its children; connecting to a socket on a read-only mount still works
(checked on the test host with the first call).

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

**The user manager is the identity's: its answers are untrusted input.**
The user owns `/run/user/<uid>`, can replace the manager's private socket,
stop RAST's units and start its own units with RAST's names. So:

- the helper parses `systemctl --user show` strictly (known keys, bounded
  size), keeps only unit names that `is_user_unit` accepts for that
  identity, and reports a `MainPID` only when `/proc/<pid>` belongs to the
  identity's uid and its cgroup is below `user@<uid>.service`;
- nothing root does depends on it: no path, port or pid from it is used for
  a root action;
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

**Firewall.** Ports are no longer read from process trees for new
sessions. `session_start` records the port it was given (validated, as
today) in `<run_dir>/xpra-ports/<uid>` (root 0600, directory root 0700,
written with `O_NOFOLLOW`) before it adds it to `xpra_ports`;
`session_stop` and `user_manager_release` release only the recorded port;
the reconcile at helper start keeps a recorded port while a LISTEN socket
of the identity's uid is on it (`/proc/net/tcp`, kernel data) and drops the
record otherwise. A port whose LISTEN socket belongs to another uid than
the session's identity is refused (`port_conflict`). Reason: in a user manager
the user can replace `rast-xpra-u<id>` with a process whose command line
names **another user's** port; if the helper released that port, the other
user's Xpra (`--auth=none`) would be open to every local account. Process
tree discovery stays only for legacy system units started before the
record existed (the first helper start after the upgrade).

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
user-started services and timers. A revocation restart ends the manager
(`release` `stop`), because every user unit inherits the manager's
supplementary groups from its start.

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
(nftables), `/run/rast/files/u<uid>.sock` (system socket unit, `root:rast
0660`), terminal sockets in `<run_dir>/terminal/<sid>/` and the helper
socket. It reaches nothing in `/run/user/<uid>` (0700, `EACCES`, tested). A
later agent service that the server must reach (tasks 2.3, 2.4) gets a
system socket unit under `/run/rast/` like the file agent, never a socket
in `/run/user`. `/run/rast-linux/<uid>` of a one-HOME identity is removed
(no-follow) by `ensure` once no legacy unit of that identity is active.

### 5. One D-Bus session bus

The bus is the distro's user `dbus.socket` at `/run/user/<uid>/bus`
(`dbus-daemon` from `dbus-user-session`; a host with `dbus-broker` works the
same way). Every user unit gets `DBUS_SESSION_BUS_ADDRESS` from the manager
(set by `dbus.socket`). For one-HOME accounts:

- `dbus-run-session` is no longer added to Chromium launches (legacy keeps
  it);
- Xpra gets `--dbus-launch=` (empty: never a bus of its own) and keeps
  `--dbus=no` and `--notifications=no` (task 2.4 owns notifications);
- after the session unit starts, the helper runs, as the identity,
  `dbus-update-activation-environment --systemd DISPLAY=:<n>
  XAUTHORITY=<HOME>/.Xauthority`, so D-Bus- and systemd-activated GUI
  services (a portal file chooser before task 2.3, keyring prompts in task
  2.2) open in the session; `session_stop` runs `systemctl --user
  unset-environment DISPLAY XAUTHORITY`;
- RAST does not start `graphical-session.target` in this task (it would
  run `xdg-user-dirs` and other distro services that write into HOME);
  tasks 2.3 and 2.4 revisit it.

### 6. Audio

One-HOME accounts use **their user manager's own sound server**: the
distro's socket-activated `pulseaudio.socket` (PulseAudio 17 on production;
on a host with `pipewire-pulse.socket` the same path and protocol). Xpra
becomes a client of it: `--pulseaudio=no --speaker=on
--audio-source=pulse:device=Xpra-Speaker.monitor --microphone=disabled`.
Before starting the session unit the helper runs, as the identity, `pactl
list short sinks` (bounded, parsed strictly) and, when `Xpra-Speaker` is
missing, `pactl load-module module-null-sink sink_name=Xpra-Speaker
channels=2 rate=48000`, then `pactl set-default-sink Xpra-Speaker`; a
failure is logged and the session starts without sound. Apps get no
`PULSE_*` variables: libpulse, Flatpak's `--socket=pulseaudio` and
D-Bus-activated services all find `$XDG_RUNTIME_DIR/pulse/native` by
default, and same-uid clients are authorised by `SCM_CREDENTIALS`. Without
hardware access (identities are not in `audio` and have no seat) the server
has only null sinks and their monitors.

Rejected: Xpra's private PulseAudio at the standard path (it would fight the
globally enabled `pulseaudio.socket` for the same socket, which needs a
per-user mask that the user can undo, and the distro daemon would still
start at every manager start); keeping the private path with pinned
`PULSE_*` variables (D-Bus-activated services and Flatpak apps do not get
them); PipeWire now (new packages on production, and on Ubuntu it replaces
PulseAudio for every account on the host). The cost: the account's server
uses the distro `default.pa`, which allows module loading; that is the
account's own server under its own uid, so it is not a boundary between
users.

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
  row is adopted from the manager the unit is found in. A one-HOME
  account's session still running as a system unit (started by the
  previous release) is ended once, like the sessions without audio today.
  Then the server reconciles managers (section 2).
- **The helper restarts.** User managers and their units are not children
  of the helper and keep running. The helper reconciles `xpra_ports` from
  its port records (section 3); `user_managers_list` recovers everything
  else from logind's linger files and `user@<uid>.service`. It keeps no
  other state.
- **A user manager stops by itself** (the user ran `systemctl --user exit`,
  or it crashed). Its transient units are gone; `unit_status` reports them
  inactive, the session ends as after an app crash, and the next launch's
  `ensure` starts the manager again.
- **Reboot.** Transient units are gone. A linger left over from a crash
  starts the manager at boot; the server's start-up reconcile releases it
  (`idle`).

### 8. Unit lifetime: the subreaper wrapper

App units in the user manager get the wrapper as their main process:
`<rast_bin> app-reaper -- <argv…>`. The helper builds the prefix itself
(the server never sends it) from the fixed path `/usr/local/bin/rast`
(test override), which it checks at start (root-owned, not group- or
other-writable), not from `/proc/self/exe`, which becomes "(deleted)" after
`rast update`.

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
   living parent below it), so leave the loop. A forwarded signal: send it
   to every direct child listed in `/proc/self/task/<pid>/children`
   (orphans the wrapper adopted included) and keep reaping. It never signals
   a process that is not its child.
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

**`ExitType=cgroup`: no strong reason found**, so it is not used, on any
systemd version. Checked: it would save one small process per app (the
wrapper's private memory is a few MB; the binary's pages are shared) and
keep `MainPID` on the app, but nothing in RAST needs that (window matching
uses `_NET_WM_PID`, ADR 0004 decision 5 reads `/proc/<_NET_WM_PID>`);
the user manager supports it on 259, but Ubuntu 22.04 (249) does not, and two
mechanisms need two sets of contract tests. `native-helper --check` and
`rast doctor` report "app unit lifetime: subreaper wrapper".

Contract tests:

- **CI, no systemd** (`tests/app_reaper.rs`, runs the built binary with
  `/bin/sh`, synchronised through pipes, no sleeps): a grandchild detached
  with `setsid` keeps the wrapper alive after the direct child exits; the
  wrapper exits after the last descendant with the first child's exit code;
  signal death is mirrored; `SIGTERM` to the wrapper reaches the direct
  child and adopted orphans; exec failure gives 127 or 126; the child
  starts with an empty signal mask and default dispositions and only fds
  0–2.
- **Real systemd, opt-in** (`tests/user_unit_contract.rs`, `#[ignore]`,
  `RAST_SYSTEMD_CONTRACT=1`): uses the caller's own `systemd-run --user`
  (no root) with the unit properties the helper generates. The unit stays
  active while a detached grandchild runs after the main process exited;
  it becomes inactive after the last process exits; `systemctl --user stop`
  kills all, a grandchild that ignores `SIGTERM` included (with
  `TimeoutStopSec=2s`). Run on the test host by the coordinator.
- **Helper argv tests** (fake `systemd-run`): app units carry the wrapper
  prefix, `KillMode=control-group` and no `ExitType=`.

### 9. Limits

| Key (env) | Legacy accounts (unchanged) | One-HOME accounts (new meaning) |
| --- | --- | --- |
| `linux_session_memory_mb` | `MemoryMax` of the Xpra unit | `MemoryMax` of `user-<uid>.slice`: session, every app, terminals, file agent and user services together |
| `linux_session_cpus` | `CPUQuota` of the Xpra unit | `CPUQuota` of `user-<uid>.slice` |
| `linux_session_tasks` | `TasksMax` of the Xpra unit | `TasksMax` of `user-<uid>.slice`, and of each app unit |
| `linux_app_memory_mb` (and an app's own memory setting) | `MemoryMax` of the app unit | `MemoryHigh` of the app unit (throttle and reclaim, never a kill), with `MemoryMax=infinity` |
| `linux_app_cpus` (and an app's own CPU setting) | `CPUQuota` of the app unit | the same, inside the slice's quota (needs `cpu` delegated) |
| `linux_app_tasks` | `TasksMax` of the app unit | not used (apps get `linux_session_tasks`) |
| `linux_idle_minutes` | session idle time | also how long the user manager stays after the last session, app or terminal |
| `linux_admission_reserve_mb` | admission check | unchanged (an app's admission uses its `MemoryHigh` value) |
| `terminal_*` | per terminal | unchanged, inside the slice |

App units also get `OOMPolicy=continue` (when the slice reaches its
`MemoryMax`, the kernel ends one process and systemd keeps the unit and the
app's other processes) and `OOMScoreAdjust=200` (above the manager's 100,
so the kernel picks an app process before Xpra, Xvfb, the bus or the sound
server; raising the value needs no privilege). The Xpra unit keeps the
default `OOMPolicy=stop`. Admission and the configuration ranges are
unchanged; the helper accepts an app `tasksMax` up to the session maximum
for `manager: "user"`. `rast-linux.slice`'s aggregate guardrail does not
cover one-HOME accounts any more (their slices are in `user.slice`, next to
host accounts); the per-account slice limits and the admission check are
what remains. `rast doctor` warns when `systemd-oomd` is active, because its
pressure-based kills would defeat `MemoryHigh`.

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
>   systemd user manager (`user@<uid>.service`, with linger while the
>   account uses native apps or terminals) and runs the graphical session
>   and apps there; the file agent and terminals join the same per-account
>   slice `user-<uid>.slice`. The limits change meaning for these accounts
>   only:
>   - `linux_session_memory_mb`, `linux_session_cpus` and
>     `linux_session_tasks` now limit everything of one account together
>     (graphical session, every app, terminals, file agent, user services):
>     `MemoryMax`, `CPUQuota` and `TasksMax` of `user-<uid>.slice`. They
>     used to limit the graphical session only, with each app's limits on
>     top, so an account could use the session limit plus one app limit
>     per running app. Raise `linux_session_memory_mb` if your users run
>     several large apps at once.
>   - `linux_app_memory_mb` (and an app's own memory setting) is now a soft
>     limit (`MemoryHigh`): above it an app is slowed down and its memory
>     reclaimed, but it is not killed for it. When the account reaches
>     `linux_session_memory_mb`, the kernel ends one app process and the
>     app's other processes keep running.
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
  anyway), raise its per-app `MemoryHigh`/`CPUQuota` (not a boundary);
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
stays uid-based. The identity has no logind session, so polkit actions that
are `allow_active` never apply to it; actions that are `allow_any=yes` were
reachable before (the system bus is open to every local uid), and the only
polkit change is the deny rule in section 2. Auditing the other
`allow_any=yes` actions is part of the Phase 2 security review.

**What stays forbidden:** no polkit rule that grants anything; no sudoers
entry; `rast-u<id>` is not in the `rast` group; `/run/rast/native.sock`
stays `0660 root:rast`; no helper operation is reachable from an app; the
cron and `at` deny lists of one-HOME identities stay (A7); no `--machine=`
login sessions; no drop-in for `user@.service` or `user-.slice` (they apply
to every account on the host); the helper takes no path, port or pid from a
user manager for a root action and never opens a path below `/run/user`;
the cgroup files of `user-<uid>.slice` and of `user@<uid>.service` itself
stay root's (only the subtree below is delegated).

**Cross-user tests that must stay:** every test in ADR 0005 "Tests"
(HOMEs, agent sockets, shares, migration) and the Xpra port firewall test,
plus, on the test host:

- `t-bob`'s processes get `EACCES` on `t-alice`'s
  `/run/user/<uid>/{bus,systemd/private,pulse/native}`, and the `rast`
  service gets `EACCES` on every `/run/user/<uid>`;
- as an identity, `loginctl enable-linger` (with the rule) and `systemctl
  set-property user-<uid>.slice …` are refused, and writing `memory.max` of
  `user-<uid>.slice` or of `user@<uid>.service` gives `EACCES`;
- one account reaching its slice `MemoryMax` kills only its own processes;
  the other account's session keeps running;
- after a share revocation, no process below `t-bob`'s `user@<uid>.service`
  keeps the share's group.

Unit tests: the helper never adds or releases a port taken from a (fake)
user manager's output; `user` mode is refused for a legacy HOME; foreign
unit names and pids of another uid in `systemctl --user show` output are
dropped.

### 11. Migration and rollback

- **Installs without one-HOME accounts:** nothing changes (no linger, no
  slice properties). The installer adds the polkit rule (**Maintainer**)
  and checks the new prerequisites.
- **Accounts already in one-HOME mode** (`t-alice`, `t-bob` on
  production): their next session or terminal start runs `ensure`; a
  graphical session still running as a system unit is ended at the first
  server start; an active file agent in `rast-linux.slice` is restarted
  into `user-<uid>.slice`; terminals started before keep running in
  `system.slice` until they close.
- **Legacy native accounts:** unchanged until `rast migrate home`. Its apply
  stops the user manager too (`release` `stop`; `stop_user_units` and the
  identity-in-use check include `user@<uid>.service` and the linger file).
  Its rollback runs `release` `remove`, so a rolled-back account has no
  linger and no slice properties; the user manager's files in HOME
  (`~/.config/systemd`, `~/.local/state`) are handled like every other entry
  created after the migration.
- **Release rollback** (deploy checklist): with the new binary, for each
  test account in one-HOME mode end the sessions and run `sudo loginctl
  disable-linger rast-u<id>` and `sudo systemctl stop user@<uid>.service`,
  and (after asking the maintainer, it deletes files) remove
  `/etc/systemd/system.control/user-<uid>.slice.d/50-{MemoryMax,CPUQuota,TasksMax}.conf`;
  then `sudo rast update --version <rollback version>` and the health check. The old release runs
  these accounts with system units again. The polkit rule may stay (it only
  denies). No database migration is involved.

### 12. Implementation plan

Each commit passes the quality gates and updates the docs it touches.
Commits 4 and 6 depend on each other only through the optional `manager`
field, so each stays working alone.

1. `feat(agent): rast app-reaper keeps an app unit alive while its
   processes run`: new `crates/rast-agent/src/reaper.rs`,
   `crates/rast-agent/src/lib.rs`, `src/main.rs` (hidden `AppReaper`,
   dispatched before the runtime), new `tests/app_reaper.rs` (section 8).
2. `feat(helper): run user-manager commands as the identity`: new
   `crates/rast-helper/src/native/user_manager.rs` (runner, runtime
   directory check, bounded strict parsing), `native/mod.rs`
   (`NativeOptions`: `loginctl_bin`, `pactl_bin`, `dbus_env_bin`,
   `rast_bin`, `user_runtime_root`, `linger_dir`, test overrides),
   `native/daemon.rs` (shared `systemctl show` parser). Tests: argv, cleared
   environment, working directory, no supplementary groups (as the test's
   own uid); a link, wrong owner or mode of the runtime directory is
   refused; oversized or malformed output refused; foreign names and pids
   dropped.
3. `feat(helper): user_manager_ensure, user_manager_release and
   user_managers_list`: `native/protocol.rs`, `native/daemon.rs`,
   `native/user_manager.rs`, `native/client.rs` (timeouts),
   `native/identity.rs` (`remove_identity`), `native/home_migrate.rs`
   (`stop_user_units`, identity-in-use check). Tests with fake `loginctl`
   and `systemctl`: linger only when the file is missing, slice properties
   only when they differ, `user@` started when inactive, values from the
   helper's configuration, `legacy_home`, unknown fields, `busy` in `idle`
   mode only for a terminal (a fake "running" user unit and the file agent
   do not count), stop order, `remove` deletes exactly its three drop-ins,
   list filtered to the identity range, migration refused while `user@` is
   active.
4. `feat(helper): session and app units in the user manager`:
   `native/daemon.rs` (`manager` field, user-unit arguments, wrapper
   prefix, app limits, `pactl` sink, activation environment, `units_list`
   and `unit_status` for both managers), `native/protocol.rs` (field and
   app `tasksMax` rule), `native/firewall.rs` (port records,
   `/proc/net/tcp` owner check, discovery for legacy units only),
   `native/shares.rs` (`user@<uid>.service`). Tests: today's legacy
   arguments unchanged (snapshot); user arguments for session and app;
   refusals; a fake manager that claims another user's port changes
   nothing; `share_session_groups` counts a process below `user@<uid>`.
5. `feat(helper): file agents and one-HOME terminals in user-<uid>.slice`:
   `native/homes.rs`, `native/daemon.rs` (terminal arguments and
   environment). Tests: arguments per mode; environment only for a valid
   runtime directory; an agent in the old slice is restarted.
6. `feat(session): one-HOME sessions under the user manager` (after the
   current `native_session.rs` work has landed):
   `crates/rast-session/src/linux_apps/native_session.rs` (`ensure` before
   the first start, `manager: "user"`, user-mode `xpra_args`, no
   `/run/rast-linux` preparation, adoption per manager, ending old system
   sessions), `crates/rast-session/src/linux_apps/mod.rs` (no
   `dbus-run-session` for one-HOME), `crates/rast-agent/src/terminal/`
   (`ensure` before a one-HOME terminal). Tests with the fake helper: call
   order, legacy unchanged, Chromium wrapper for legacy only, adoption.
7. `feat(server): user managers follow account state and the native
   pilot`: new `crates/rast-server/src/user_managers.rs` (idle release with
   an injected clock, start-up reconcile), `crates/rast-server/src/state.rs`,
   `crates/rast-server/src/api/users.rs` (deactivation),
   `crates/rast-server/src/share_groups.rs` (`restart`), the `migrate home`
   CLI, `src/main.rs` (`rast doctor`) and `native/mod.rs` (`--check`):
   lifetime mode, managers, `cpu` delegation, polkit rule, `dbus.socket`,
   sound socket, `systemd-oomd`. Tests: reconcile matrix (active × pilot ×
   one-HOME × native × units), deactivation, revocation, idle timing.
8. `build(deploy): deny self-linger to RAST identities` (**Maintainer**):
   new `deploy/rast-identities.polkit.rules`, `installer/install.sh` (via
   `scripts/sync-installer-assets.sh`), `installer/uninstall.sh`. Tests:
   `sync-installer-assets.sh --check`, `shellcheck`,
   `scripts/test-installer-clean.sh` (containers).
9. `docs: systemd user session (ADR 0008)`: `docs/NATIVE-LINUX-APPS.md`
   (how it works, unit lifetime, limits, the End application row and a new
   "launcher exits" row of the lifecycle table, audio, recovery, security
   model), `docs/ARCHITECTURE.md`, `docs/INSTALL.md`, `docs/OPERATIONS.md`
   (stopping a user's session, `systemd-cgls /user.slice/user-<uid>.slice`,
   rollback), `docs/CONFIGURATION.md`, `deploy/rast.toml.example` (+
   `installer/install.sh`), `SECURITY.md`, `CHANGELOG.md` (upgrade note
   above), a note in ADR 0004 (decisions 1 and 2 refined here) and ADR 0005
   (agent socket, `share_session_groups`), this ADR's status,
   `docs/remake/PROGRESS.md`.

**On the test host** (production, `t-` accounts, after a pre-release
deploy; root steps by the coordinator with the maintainer's permission):
linger on at the first launch and off after `linux_idle_minutes`; slice
properties (`systemctl show user-<uid>.slice`) and `systemd-cgls` showing
Xpra, apps, file agent and terminals in the slice; the real-systemd
contract test; Chrome passes the app matrix with no `dbus-run-session` and
plays sound; a memory test app is throttled above `linux_app_memory_mb`
(`memory.events` `high` rises, `oom_kill` stays 0) and only its own account
is hit at the slice limit; the cross-user tests of section 10; adoption
after the maintainer restarts `rast.service` and `rast-native.service`; a
revocation restart; `rast migrate home` rollback and apply with a running
manager; the `NoNewPrivileges` experiment.

## Consequences

- **Easier:** apps get the standard desktop plumbing (bus, activation,
  sockets, `systemd-run --user`) that tasks 2.2–2.4 and Flatpak need;
  launchers keep the apps they open (ADR 0004); one hard limit per account
  instead of a sum; sound works for Flatpak and D-Bus-activated apps
  without variables.
- **Harder:** two unit paths (legacy system units, one-HOME user units)
  until the last legacy account is migrated; the helper must treat a user
  manager's answers as untrusted; `systemctl stop rast-linux.slice` no
  longer ends every native session (one-HOME accounts: `release` `stop`, or
  `systemctl stop user-<uid>.slice`).
- **Risks:**
  - The default `linux_session_memory_mb = 4096` now covers a whole
    account; heavy users may hit it sooner than before (open question).
  - Without the polkit rule an identity can keep its manager alive by
    itself.
  - The distro's user units run for every lingering identity; a package
    that enables a heavy user service globally runs it for them too.
  - Task 2.2 must decide whether a RAST login keeps the user manager, so
    that the keyring it unlocks stays unlocked while the account is signed
    in but idle.
  - `systemd-oomd` on a host would kill throttled apps (doctor warns).
- **Open questions for the maintainer:**
  1. The polkit deny rule (section 2), shipped by the installer; and
     whether the existing self-linger gap is fixed before Phase 2 lands.
  2. A larger default for `linux_session_memory_mb` now that it is a
     whole-account limit (for example 8192).
  3. Linger only while RAST needs the manager (decided here) instead of
     permanently.
  4. The account's own distro PulseAudio (decided here) instead of Xpra's
     playback-only private one.
- **Found, outside this ADR:** when a session's Xpra stops, another
  identity can bind its port before the server notices, and the server
  would proxy that session's viewer to it (it exists today too). Proposed
  follow-up: the server checks the listener's uid (`/proc/net/tcp`) before
  it proxies a display.
