# Review of ADR 0008 (systemd user session), task 2.1 critique

- Reviewed: `docs/adr/0008-systemd-user-session.md` (status proposed, 2026-11-17)
- Date: 2026-11-18
- Reviewer: independent subagent (security and systems), docs only
- Inputs checked: `CLAUDE.md` (Phase 2, Security invariants), PROGRESS.md
  (maintainer decisions 2026-11-16, A5, A7), ADRs 0004, 0005, 0007, the code
  in `crates/rast-helper/src/native/`, `crates/rast-session/src/linux_apps/`,
  `deploy/`, and the host (read-only commands as `lufria`, no root).

## Verdict

**Approve with changes.** The overall direction is sound: user manager only
for one-HOME pilot accounts, limits on `user-<uid>.slice` set by the system
manager, no `--machine=`, no drop-in for `user@.service`, user manager output
treated as untrusted, one lifetime mechanism (subreaper wrapper), and the
polkit deny rule. Most of the ADR's host facts check out. Five high findings
must be resolved in the ADR before implementation starts; none is critical
as long as they are fixed before the pilot runs native apps again.

## Host facts re-checked

| ADR claim | Result | Evidence |
| --- | --- | --- |
| systemd 259 | confirmed | `systemctl --version`: `systemd 259 (259.9-0ubuntu0.1)` |
| `Delegate=pids memory cpu` on `user@.service` | confirmed | `systemctl show user@1000.service`: `Delegate=yes`, `DelegateControllers=cpu memory pids`, `DelegateSubgroup=init.scope`, `KillMode=mixed`, `OOMScoreAdjust=100` |
| `user@.service` OOM behaviour | **not stated in the ADR**; harmless | `OOMPolicy=continue` (delegated units default to `continue`), so an app OOM kill never stops the whole manager |
| Slice limit files stay root's | confirmed | `/sys/fs/cgroup/user.slice/user-1000.slice/user@1000.service/`: `memory.max`, `memory.high`, `cpu.max`, `pids.max`, `cgroup.kill` are `root root`; `cgroup.procs`, `cgroup.subtree_control`, `cgroup.threads`, `memory.oom.group` are the user's |
| "no logind session ever starts" for identities (sections 2, 10) | **wrong** | `loginctl list-sessions`: `2 1000 lufria - 2874 manager` (lufria has linger and no other session); `loginctl show-session 2`: `Class=manager Active=yes Remote=no Seat= Scope=` (finding 4) |
| `set-self-linger` is `allow_any=yes` | confirmed | `pkaction --verbose --action-id org.freedesktop.login1.set-self-linger`: implicit any/inactive/active `yes`; polkit 127 |
| Other `allow_any=yes` actions | audited here | only `org.freedesktop.login1.inhibit-block-idle`, `inhibit-delay-shutdown`, `inhibit-delay-sleep` and `set-self-linger` (217 actions listed) |
| `ProtectHome=read-only` makes `/run/user` read-only for the helper | **partly wrong** | `/proc/<rast-native pid>/mountinfo`: `/run/user ro … master:13`, existing `/run/user/1000 ro`; the bind is a mount slave, so a runtime dir mounted after the helper started propagates in with its own (`rw`) flags (finding 13) |
| PulseAudio 17, `pulseaudio.socket` global | confirmed; **idle exit not considered** | `/etc/pulse/daemon.conf`: `; exit-idle-time = 20` (default 20 s); `/usr/lib/systemd/user/pulseaudio.service`: `ExecStart=/usr/bin/pulseaudio --daemonize=no --log-target=journal` (finding 9) |
| Transient units are the user's | confirmed, stronger than stated | `systemd.unit(5)` user search path: `~/.config/systemd/user.control/*` ranks **above** `$XDG_RUNTIME_DIR/systemd/transient/*`; user generators are read only from `/run`, `/etc`, `/usr/local/lib`, `/usr/lib` (not from HOME); `~/.config/systemd/user.conf` and `environment.d` are read |
| Logind defaults | confirmed | `/etc/systemd/logind.conf`: `#KillUserProcesses=no`, `#UserStopDelaySec=10`, `#RemoveIPC=yes` (so IPC objects of an identity are removed when its user record ends; the `idle` terminal check is required, see finding 6) |
| Memory | 31 GiB RAM, 4 GiB swap file with 3.6 GiB used | `free -m`, `/proc/swaps` (finding 11) |
| Linger | `lufria` and `root` only | `ls -l /var/lib/systemd/linger/` |

## Findings

### 1. High: the helper runs identity programs that read identity-controlled configuration, outside the user's slice

- **Section:** 3 ("The helper drives a user manager as the identity"), 5, 6.
- **Problem:** `pactl` and `dbus-update-activation-environment` would run as
  children of the root helper, so in `rast-native.service`'s cgroup and mount
  namespace, outside `user-<uid>.slice`. libpulse reads the identity's own
  `~/.config/pulse/client.conf` (the user file replaces
  `/etc/pulse/client.conf`; HOME comes from passwd even when the variable is
  cleared), which sets `default-server`, `autospawn` and `daemon-binary`.
  Where autospawn is honoured, a user-chosen binary is forked by a
  helper-spawned process. Any process forked there escapes the slice limits
  (the "limits only root can change" argument of section 3), is invisible to
  `share_session_groups` (which looks for `user@<uid>.service` or RAST unit
  cgroups), survives `user_manager_release stop`, and lives until
  `rast-native.service` restarts: the cron-equivalent that A7 denies.
- **Evidence:** today's runner does not clear the environment and kills only
  the direct child: `crates/rast-helper/src/native/mod.rs:883`
  (`.envs(...)` on top of the inherited environment), `:907`
  (`child.start_kill()` on timeout, no process group), `:909` and `:917`
  (`stdout_task.await` with no deadline, so a grandchild that keeps stdout
  open hangs the request, and the per-uid lock with it). `audio.rs`
  (`pulseaudio_command`) shows why the current design pins every `PULSE_*`
  variable.
- **Required change:** (a) the helper runs only `systemctl --user` and
  `systemd-run --user` as the identity, programs that read no user
  configuration. The sink setup (`pactl load-module`, `set-default-sink`) and
  `dbus-update-activation-environment` move into the Xpra unit
  (`ExecStartPre=`/`ExecStartPost=` of the transient unit, run by the user
  manager inside the slice); a failure there is the user's own. (b) The new
  runner: `env_clear()` plus the fixed list, stdin `/dev/null`, `--no-pager
  --no-ask-password`, its own process group with the group killed on
  timeout, a deadline on reading the pipes after the kill, and
  `close_range(3, ~0)` in `pre_exec` as a second line behind `O_CLOEXEC`
  (the helper's own opens use `O_CLOEXEC`: checked in `homes.rs:128`,
  `shares.rs:163`, `job_deny.rs:208`). (c) Tests: the child's environment and
  fds; on the test host, `systemd-cgls -u rast-native.service` shows no
  process of the identity after `ensure`, `session_start` and `units_list`,
  with a hostile `~/.config/pulse/client.conf` (`autospawn = yes`,
  `daemon-binary = <HOME>/x`) in `t-alice`'s HOME.

### 2. High: port records can drop a live session's protection, and the rule is added after the unit starts

- **Section:** 3 ("Firewall").
- **Problem:** the reconcile at helper start "keeps a recorded port while a
  LISTEN socket of the identity's uid is on it … and drops the record
  otherwise". A session whose Xpra has not bound yet (start takes seconds)
  or is between sockets loses its record and its set entry; when Xpra binds,
  its `--auth=none` port is reachable by every local uid, including other
  RAST users' apps. The ADR also does not fix the order: today the port is
  added after `systemd-run` returns (`daemon.rs:1290` starts the unit,
  `:1295` calls `allow_port`). An entry in `xpra_ports` with no listener is
  harmless (it only refuses non-`rast` connections), so the safe direction is
  to keep entries.
- **Required change:** `session_start` writes the record and adds the port
  to the set **before** `systemd-run --user`, and stops the unit if either
  fails. Records go only through `session_stop`, `user_manager_release`
  and `identity_remove`; the helper-start reconcile restores every valid
  record (validated range, at most one per uid) and never drops one because
  nothing listens. The `/proc/net/tcp` owner check stays for `port_conflict`
  at start, and must read `/proc/net/tcp6` too (a dual-stack wildcard
  listener overlaps `127.0.0.1`). Test: helper restart during session start
  keeps the port in the set (fake nft host).

### 3. High: the server proxies a viewer to whoever listens on the port (pre-existing, now in scope)

- **Section:** Consequences, "Found, outside this ADR".
- **Problem:** in a user manager the session's Xpra can end without the
  server or helper doing anything (`systemctl --user stop/exit`, a manager
  crash, an OOM kill at the slice limit), and adoption reads unit state
  from untrusted output. While the port is free, another identity can bind
  it (bind is not filtered; only outgoing connections are, `firewall.rs:177`);
  the server then sends that user's keystrokes and clipboard to the
  squatter, who can draw a fake desktop. The same happens if a squatter wins
  the race right after the `port_conflict` check. This is a cross-user
  break.
- **Required change:** part of task 2.1, before user units ship: on every
  upstream connect (`native_proxy.rs:479`, `:659`) the server looks up the
  established socket pair in `/proc/net/tcp` (kernel data; readable under
  `ProtectProc=invisible` through `/proc/self/net`) and refuses unless the
  listener side's uid is the session's identity. Test with a fake
  `/proc/net/tcp` and on the test host (`t-bob` listening on `t-alice`'s
  port while her session is stopped: the viewer gets an error).

### 4. High: identities do get a logind session; the polkit argument must be verified

- **Section:** 2 (first paragraph), 10 ("Why that is acceptable").
- **Problem:** on systemd 259 every user manager registers a session of
  class `manager`, and it is `Active=yes`, `Remote=no` (table above). The
  ADR's reasoning "no logind session, so `allow_active` never applies" rests
  on a false premise. On this host `allow_active=yes` covers
  `org.freedesktop.login1.reboot`, `power-off`, `set-reboot-*` and
  `org.freedesktop.udisks2.filesystem-mount`. polkit resolves a subject's
  session from its cgroup scope (manager sessions have `Scope=` empty) and
  then from the user's display session (`loginctl show-user lufria`:
  `Display=` empty), so user-unit processes should fall back to
  `allow_any`, but nobody has tested it, and the result would be host-wide.
- **Required change:** correct sections 2 and 10. Add a gate test on the
  test host, before the pilot gets native apps back: from a `t-alice` user
  unit (`systemd-run --user --wait --pipe`), `pkcheck --action-id
  org.freedesktop.login1.reboot --process $$` and the same for `power-off`
  and `udisks2.filesystem-mount` must answer "not authorized". If any
  answers yes, add those actions to the deny rule for `rast-u` identities
  (it only takes rights away).

### 5. High: adoption and start-up reconcile can kill running work

- **Section:** 2 (table "When linger goes off"), 7.
- **Problem:** "Server start: a listed manager with no adopted session, run
  or terminal → `release idle`" runs at once. `units_list` now merges one
  query per manager; a manager that is slow or does not answer within the
  timeout looks like a manager with no units. `recover()`
  (`native_session.rs:1812-1850`) keeps the database only when the whole
  call fails; with a partial list it drops the rows and calls
  `session_stop`, and the reconcile then releases the manager: an innocent
  user loses unsaved work after a `rast.service` restart.
- **Required change:** `units_list` returns per manager `ok` or `unknown`;
  the server keeps the rows and the manager of an `unknown` one and retries.
  A manager without adopted work is released only after
  `linux_idle_minutes` counted from server start. The helper refuses `idle`
  (`busy`) while it holds a port record for that uid (helper state it
  trusts), which also closes the race between a launch and the idle task.
  Per-manager queries run in parallel with their own timeout, so one bad
  manager cannot stall the others.

### 6. Medium: linger on/off sequencing

- **Section:** 2.
- `release` stops with `--no-block` while `user@.service` has
  `TimeoutStopSec=120s`; a following `ensure` (10 s wait) fails while the
  manager is still deactivating, and `identity_remove` (`userdel` refuses
  while processes run) and the migration's `refuse_running`
  (`home_migrate.rs:970-1004`, counts every process of the uid) fail
  spuriously. Required: `stop` and `remove` wait (bounded, about 130 s) for
  `ActiveState=inactive` when called from `identity_remove` or the
  migration; `ensure` on a deactivating manager returns a retryable
  `manager_stopping`, and the server retries.
- `ensure` must be fail-closed on limits: set the slice properties before
  `enable-linger`, read them back after the manager is active, and run
  `release stop` if they do not match. The ADR's "runs again after step 3"
  lets the user's `default.target` units start without the account cap.
- The helper-side fail-safe is missing: when `rast.service` does not run
  (failed start, or a host where only terminals use the helper) a leftover
  linger keeps the manager from boot. At helper start, disable linger for
  every `rast-u` identity that has no port record (records live in
  `/run/rast-linux/xpra-ports`, so a reboot clears them) and no active
  terminal unit. That trusts only helper state.
- Keep the `idle` terminal check: with `RemoveIPC=yes`, logind deletes the
  identity's SysV and POSIX IPC objects when its user record ends, which
  would break programs running in a terminal.

### 7. Medium: share grants need a manager restart

- **Section:** 3 ("Share groups").
- The manager keeps the groups it started with for up to
  `linux_idle_minutes` after the last session. A grant's "Restart session
  now" (D7) must therefore run `release stop` for one-HOME accounts, not only
  `session_stop`. Add it to the table and the revocation test.

### 8. Medium: subreaper signal forwarding duplicates SIGTERM

- **Section:** 8.
- With `KillMode=control-group`, `systemctl stop` already sends SIGTERM to
  every process in the cgroup; the wrapper forwards it again to its direct
  children, so each gets two. Some programs treat a second termination
  signal as "quit now, skip saving". `/proc/self/task/<pid>/children` is also
  racy (an orphan adopted after the read is missed). Required: the wrapper
  does not forward signals that systemd delivers to the whole cgroup anyway
  (SIGTERM, SIGINT, SIGHUP, SIGQUIT); it keeps reaping and exits with the
  first child's status. If forwarding of SIGUSR1/SIGUSR2 is kept, document
  it. Contract test: during `systemctl --user stop`, each process receives
  exactly one SIGTERM. The rest of the wrapper design (one thread, block
  before spawn, `ECHILD` as the end condition, mirrored signal death with
  `RLIMIT_CORE=0`) is correct.

### 9. Medium: the distro PulseAudio exits when idle and loses the Xpra sink

- **Section:** 6.
- The user `pulseaudio.service` runs with the default `exit-idle-time` of
  20 s. A sink loaded at runtime (`module-null-sink sink_name=Xpra-Speaker`)
  disappears when the daemon exits with no clients, for example before the
  browser enables sound; the next socket-activated daemon only has
  `auto_null`. Today's private server runs with `--exit-idle-time=-1`
  (`audio.rs`). Required: create the sink from the Xpra unit (finding 1) and
  re-create it when audio forwarding starts, or keep a client connected for
  the session's lifetime; test on the test host that sound works after 60 s
  of silence and after `systemctl --user restart pulseaudio`. Microphone
  stays `disabled`. The extra modules of `/etc/pulse/default.pa`
  (`module-udev-detect`, `module-gsettings`, `module-esound-protocol-unix`,
  `module-bluetooth-*` when installed) all run under the identity's uid with
  no device access, so they are not a boundary between users.

### 10. Medium: `units_list` fan-out and timeouts

- **Section:** 7.
- One query per manager, run sequentially with a 10 s timeout each, makes a
  restart scale with the number of pilot accounts. Covered by the required
  change of finding 5 (parallel, per-manager timeout, bounded total).

### 11. Medium: defaults for whole-account limits

- **Section:** 9, upgrade note.
- `linux_session_tasks = 2048` (`deploy/rast.toml.example:214`) now covers
  Xpra, every app, terminals (512 each by default) and the file agent; Chrome
  plus VS Code can reach that in threads, and `clone` failures crash apps.
  The upgrade note mentions the memory sum but not tasks. `MemoryHigh` above
  the app limit throttles rather than kills; with swap almost full on this
  host (3.6 of 4 GiB) a throttled app can stall for long periods. Required:
  explain both in the upgrade note, add a responsiveness check to the
  `memory.events` test, and see the recommended defaults below.

### 12. Low

- **Polkit rule** (section 2): correct and effective. It sorts before
  `/usr/share/polkit-1/rules.d/49-ubuntu-admin.rules` (`r` < `u`) and after
  `10-systemd-logind-root-ignore-inhibitors.rules` (root only); it matches
  `subject.user` only, so `lufria` and root keep their linger rights. Add a
  test on the test host: as `lufria`, `pkcheck` for `set-self-linger` still
  answers yes. `empower.rules` grants every action to group `empower` (empty
  today); `rast doctor` should warn when an identity is in `sudo`, `admin`
  or `empower`. The `allow_any=yes` audit that section 10 defers is done
  above: three inhibitor actions (delay locks are capped by
  `InhibitDelayMaxSec`), nothing to add.
- **`Type=exec` with the wrapper:** an app whose binary is missing now
  starts successfully and fails a moment later (exit 127). The server must
  show that failure (unit status) instead of an empty window.
- **User drop-ins:** a file in `~/.config/systemd/user.control/` overrides
  any property of RAST's transient units. The ADR's stance (untrusted, slice
  limits matter) already covers it; add a unit test in which a fake manager
  reports changed properties and the helper's results do not change.
- **Stranded one-HOME accounts** (ADR 0007, pilot shrunk or native off):
  state that their terminals get no `ensure` and no `XDG_RUNTIME_DIR`.
- **Wrapper path check:** also check that every parent directory of
  `/usr/local/bin/rast` is root-owned and not writable by others.

### 13. Low: `ProtectHome=read-only` is not a guard

- **Section:** 3.
- Runtime directories mounted after the helper starts are writable in its
  namespace (evidence above). The design does not depend on it ("root never
  opens a path below it"); reword the sentence so nobody relies on it.

## Test plan gaps

Add to section 12 (helper and server unit tests unless noted):

1. Runner: cleared environment, fds 0-2 only, process group killed on
   timeout, pipe reads bounded after the kill (finding 1).
2. Port records: add-before-start order; helper restart during start keeps
   the entry; a record is never dropped by the reconcile; `tcp6` squatter
   gives `port_conflict` (finding 2).
3. Proxy listener check with a fake `/proc/net/tcp` (finding 3).
4. `units_list` with one manager timing out: rows kept, no `session_stop`
   (finding 5); `idle` refused while a port record exists.
5. Wrapper: exactly one SIGTERM per process on stop (finding 8).
6. Test host (coordinator, after a deploy): `pkcheck` gate (finding 4);
   hostile `client.conf` leaves no process in `rast-native.service`
   (finding 1); `t-bob` squatting `t-alice`'s port (finding 3); sound after
   60 s of silence (finding 9); `lufria`'s linger rights unchanged
   (finding 12); `migrate home --rollback` with a running manager succeeds
   after the bounded wait (finding 6).
7. Not verifiable on production (no reboot allowed): the boot path. Cover it
   with the helper-start fail-safe unit tests (finding 6) and say so in
   PROGRESS.md.

## Recommended answers to the open questions

1. **Polkit deny rule:** yes, ship it with the installer, and close the
   existing self-linger gap first, in its own commit and pre-release, before
   the pilot gets native apps again (the maintainer installs the file; no
   restart is needed, polkit reloads rules). It only removes rights from
   `rast-u` identities.
2. **`linux_session_memory_mb` default:** raise it to 8192, and
   `linux_session_tasks` to 8192 (finding 11). This keeps data (fewer OOM
   kills and `clone` failures inside one account) without widening access
   between accounts; the admission check still protects the host, and
   production keeps whatever `/etc/rast/rast.toml` sets. `rast doctor`
   should warn when pilot accounts times the limit exceed RAM.
3. **Linger only while RAST needs it:** agree, plus the helper-side fail-safe
   of finding 6.
4. **The account's own distro PulseAudio:** accept it, with the conditions of
   findings 1 and 9. Between users the access is the same as today (a
   uid-only socket in a 0700 runtime directory). Xpra's private server
   cannot serve Flatpak and D-Bus-activated apps without pinned variables.

Also confirmed: **no `ExitType=cgroup`.** No strong reason was found, which
matches the maintainer's preference for one mechanism.
