# RAST input-latency stage profile — summary of the measured run (2026-11-24)

Status: **valid stream-path measurement** captured on production build
`RAST 0.18.3 (93585ba2c0ce)` with `tests/e2e/perf/latency.mjs`
(run started 06:57 UTC, evidence file `perf-input-latency-2026-11-24-evidence.txt`,
console log `perf-input-latency-2026-11-24-93585ba2c0ce-run.log`).

The raw JSON of this run was overwritten by a later instrumentation attempt on
the next build (`…-02c1722e731f-handshake-failed.json`), which failed its
focus handshake and is kept only as an attempt log. All numbers below are from
the valid run: the rendered evidence file, the run log and the session-level
`xpra info` snapshots are committed next to this summary.

## Hidden-viewer vs main-document rAF

| Condition | main document | off-screen viewer iframe |
|---|---|---|
| idle (5 s) | 60.0 fps | 60.0 fps |
| during typing | 59.9 fps | 59.9 fps |

=> The off-screen `.linux-native-session-frame` is **not render-throttled**.

## Stage latencies (per keystroke)

| Stage | p50 | p95 | n |
|---|---|---|---|
| input delay (event start → handler start) | 0.25 ms | 0.55 ms | 6 |
| RAST handler processing | 0.15 ms | 0.53 ms | 6 |
| keydown event timestamp → shim.sendKey | 0.25 ms | 0.79 ms | 24 (raw capture listener) |
| dispatch to capture listener | 0.30 ms | 0.99 ms | 24 |
| keypress (Node) → outgoing WebSocket frame | 11.0 ms | 16.9 ms | 24 |
| keypress → incoming WebSocket frame | 21.0 ms | 32.2 ms | 24 |

The application-acknowledgement stage (key → title change) is **not reported
from this run**: the fixture window stopped receiving keystrokes in the
automated post-prewarm builds (the title channel produced no fresh entries
while `shim.sendKey` was still called 24×), so the value would be meaningless.
The pre-deploy matrix run proves the title channel works (exact PASTED/COPIED
titles) and measures paste/copy round trips of 313–352 ms; the focus handshake
added to `latency.mjs` now detects exactly this condition and aborts instead of
emitting wrong numbers.

## Xpra transport / damage (client `xpra info` of the test identity's session)

| Metric | idle | mid-typing | after |
|---|---|---|---|
| client ping latency p50 / p90 | 62 / 101 ms | 62 / 101 ms | 57 / 68 ms |
| damage client-latency | 104 ms | 94 ms | 94 ms |
| damage frame-total latency | 104 ms | 94 ms | 94 ms |
| damage target-latency (window) | 71 ms | 71 ms | 71 ms |
| batch delay avg / max (cur) | 8 / 21 (2) ms | 8 / 21 (2) ms | 8 / 21 (2) ms |
| batch config min / max / expire | 16 / 500 / 250 ms | 16 / 500 / 250 ms | 16 / 500 / 250 ms |
| compressor / encoder | lz4 / openh264 | lz4 / openh264 | lz4 / openh264 |

## Cloudflare tunnel (HTTP RTT from the RAST host)

| Path | p50 | p95 | n |
|---|---|---|---|
| loopback `http://127.0.0.1:8720/healthz` | 2.14 ms | 3.10 ms | 40 |
| public `https://desktop.lufria.tech/healthz` | 67.33 ms | 95.10 ms | 40 |
| **inferred tunnel overhead** | **65.19 ms** | — | — |

Cloudflare colo: `cf-ray a5f68928187f68b7-SIN` (Singapore edge).

## Estimated floor

`0.25 (RAST handler) + 31.0 (ping/2, client→server) + 94 (server damage→frame)
+ 31.0 (ping/2, server→client) + 16.7 (one viewer frame) ≈ 173 ms` for one
keystroke to become a painted frame. Assumptions are listed in
docs/BENCHMARKS.md §8.6; this is an estimate, not an individual measurement.

## Attempts that are NOT measurements

* `…-02c1722e731f-handshake-failed.{json,md}` + `…-02c1722e731f-attempt.log`:
  the post-deploy re-run aborted in the new focus handshake (fixture did not
  receive a probe key — see the file's `focusHandshake` evidence). It is kept
  as evidence of the harness guard, not as latency data.
* `…-ab9b821d18cb-run.log`: earlier same-day run (build `ab9b821d18cb`) that
  produced consistent numbers (rAF 59.4/59.4 idle, 59.0/59.0 typing; ping p50
  55 ms; damage 92–98 ms; loopback 2.30 ms vs public 67.7 ms → overhead
  65.4 ms; floor ≈164 ms) but without the WebSocket-frame stage.
