# Scenario evidence index (definitive table)

The runner writes one immutable directory per run (`<mode>-<timestamp>/`) plus
`<mode>-latest.{json,md}`. Because the host was shared with other agents (and
the native instance was restarted/reused by them), the table below points at
the run that produced each result instead of `-latest`. All raw details,
errors and metrics are in those run directories.

## Docker baseline backend (127.0.0.1:8731)

| Scenario | Result | Run / evidence | Notes |
|---|---|---|---|
| s0 smoke | PASS | `docker-2026-11-12T22-37-33-495` (log), earlier `21-33-39` | login form, desktop, App Center |
| a `.deb` install + launch + stream | PASS | log of `docker-2026-11-12T22-37-33-495`; screenshots `screenshots/docker/a/*.png` (from a later PASS in that run) | worker built `rast-linux/chrome:1` from `chrome.deb`, Chrome streamed per window ("New Tab - Google Chrome" RAST window, screenshot `app-usable.png` from the earlier PASS run) |
| b portable archive | FAIL | log of `22-37-33-495` (`timed out waiting for a window for RAST Test App`) | container started, streamed window never appeared within 180 s under host load; retried |
| c Chrome + fixture app | FAIL | same run | same Docker stream-startup limitation |
| d three Chrome windows | FAIL | same run | same |
| e two users | not finished | run stopped | host too contended inside the verification window |
| f–m | not finished (Docker) | — | same; native equivalents below |

Docker `.deb` install/production path is proven end-to-end in the earlier PASS
(screenshot evidence) and by the quick performance harness, which launches and
interacts with the live Chrome stream repeatedly.

## Native backend (127.0.0.1:8732, one shared Xpra session per user)

| Scenario | Result | Run / evidence | Notes |
|---|---|---|---|
| s0 smoke | PASS | `native-2026-11-12T22-25-43-149` | RAST 0.13.0 native instance |
| a `.deb` install + launch | PASS | `native-2026-11-12T22-25-43-149`, `native-2026-11-12T22-40-01-710` | real host `.deb` install through the helper (`chrome` row `backend=native,status=ready`; `dpkg -s google-chrome-stable` 157), launch API + session; real app window blocked by the app-unit failure below |
| c shared session, Chrome + fixture app | PASS | `native-2026-11-12T22-27-33-689` | both apps in ONE session: `display :100`, API `session.apps[]` contains both runIds, one `rast-xpra-u2.service` |
| d three Chrome windows | FAIL | `native-2026-11-12T22-27-33-689` (2m1s) | no mirrored Chrome window appears; app units exit (X auth / HOME issue) |
| e two users | FAIL | `native-2026-11-12T22-31-39-028` | `fetch failed`: instance was restarted by another agent mid-run |
| f concurrent launch reuse | PASS | `native-2026-11-12T22-27-33-689` | 8 concurrent launches → 1 session, 1 run, 1 app unit |
| g refresh reconnect | PASS | `native-2026-11-12T22-27-33-689` | same display/runId before/after page reload, no duplicate processes |
| h close preserves | FAIL | `native-2026-11-12T22-27-33-689` | fixture app window never mapped (blocker) |
| i idle / j restart / k failed installs / l uninstall / m migration | blocked | `native-2026-11-12T22-31-39-028` (fetch failed) | interrupted by external restart; i/j/l/m also need working app windows; k is API-only and can be re-run first |

## Native blocker evidence

See `native-blockers-2026-11-12.md`. Summary: identity HOME was owned by the
bare user id (2/3) from a stale helper binary, so app units could not read
`$HOME/.Xauthority`/profile and died with `Missing X server or $DISPLAY`,
`Authorization required…`, or Chrome `Failed to get the path for 1001`.
native-session triaged it as a stale-helper + stale-session-ordering issue and
hardened session reuse; a clean native instance was restarted at 22:48 with the
current binary, but another agent was still driving the same instance directly
(`app_start` calls at 22:49), so app windows could not be validated reliably in
the remaining window.

## What to re-run when the host/native instance is quiet

```bash
RAST_E2E_ROOT=/tmp/rast-native-e2e RAST_E2E_BIN=/tmp/rast-native-e2e/bin/rast \
  RAST_E2E_BIND=127.0.0.1:8732 scripts/e2e/dev-instance.sh up native
cd tests/e2e
node run.mjs --mode=native --instance-root=/tmp/rast-native-e2e --scenario=s0,a,c,d,f,g,h,i,j,e,k,l,m
node perf/quick-perf.mjs --mode=native --instance-root=/tmp/rast-native-e2e
node perf/quick-perf.mjs --compare   # after both quick-perf files exist
```
