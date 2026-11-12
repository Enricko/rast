# Native blockers observed by the E2E harness (2026-11-12)

Instance: `/tmp/rast-native-e2e`, 127.0.0.1:8732, native mode, RAST 0.13.0.
Evidence from journald, helper logs and the E2E scenario runs.

## 1. Identity home owned by the bare user id (helper provisioning bug)

```
getent passwd rast-u2  -> rast-u2:x:210002:210002:...:/tmp/rast-native-e2e/data/linux-users/2
getent passwd rast-u3  -> rast-u3:x:210003:210003:...:/tmp/rast-native-e2e/data/linux-users/3
# at provisioning time (before the manual fix):
/tmp/rast-native-e2e/data/linux-users/2  -> uid 2 (bin)  gid 2   mode 0700
/tmp/rast-native-e2e/data/linux-users/3  -> uid 3 (sys)  gid 3   mode 0700
```

Consequence: the identity cannot traverse its own HOME; `$HOME/.Xauthority` and
the per-app profile are unusable. Manual `chown 210002:210002` moved Chrome one
step further (it ran ~5 s instead of failing instantly).

## 2. App units cannot use the X server

```
Nov 12 22:29:25 google-chrome-stable[1497824]: ERROR ... Missing X server or $DISPLAY
Nov 12 22:31:40 google-chrome-stable[1508224]: FATAL:base/path_service.cc:264] Failed to get the path for 1001
Nov 12 22:33:42 google-chrome-stable[1517175]: FATAL:base/path_service.cc:264] Failed to get the path for 1001
Nov 12 22:33:43 google-chrome-stable[1517370]: Authorization required, but no authorization protocol specified
Nov 12 22:33:43 google-chrome-stable[1517370]: ERROR ... Missing X server or $DISPLAY
Nov 12 22:36:38 google-chrome-stable[1530549]: Authorization required, but no authorization protocol specified
Nov 12 22:36:38 google-chrome-stable[1530549]: ERROR ... Missing X server or $DISPLAY
Nov 12 22:36:31 xpra[1528752]: X11 fatal IO error
```

The Xpra session itself starts correctly (`xpra is ready.`, display :102,
Xvfb running) and the helper emits `--setenv=DISPLAY=... --setenv=XAUTHORITY=...
--setenv=HOME=...` (see daemon.rs unit tests), but the app child cannot use the
X cookie. `$HOME/.Xauthority` kept the mtime of the previous session (21:32)
after the session was recreated at 22:36 — a stale auth file for the new Xvfb is
the leading hypothesis to verify.

## 3. Instance restarts during E2E runs

`/tmp/rast-native-e2e` was restarted by another agent at 22:22 and 22:34 while
scenario batches were running (`TypeError: fetch failed` for every later
scenario). Native instances are host-global in terms of unit names; only one
may exist, so explicit handoff is required before starting/restarting one.

## Native scenario results around these blockers

| Scenario | Result | Why |
|---|---|---|
| s0 smoke | PASS | API/login/desktop, no native units needed |
| a install .deb + launch | PASS (placeholder window; API/session assertions) | real app window not mapped while Chrome exits |
| c shared session Chrome + fixture app | PASS (session/API assertions) | both runs are in one session; windows are placeholders |
| f concurrent launch reuse | PASS | 8 concurrent launches → one session/run |
| g refresh reconnect | PASS | same display/runId before and after reload |
| d three Chrome windows | FAIL | no mirrored Chrome window appears (app exits) |
| e two users | FAIL | second launch during instance restart (`fetch failed`) |
| h close preserves / i idle / j restart / k failed installs / l uninstall / m migration | FAIL/blocked | interrupted by the 22:34 restart; Chrome-dependent steps blocked by #1/#2 |
