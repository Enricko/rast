# RAST native runtime performance — after-responsive

Generated 2026-11-24T05:08:21.798Z against `https://desktop.lufria.tech` as `t-alice` (Google Chrome, `google-chrome`, backend `native`).
Raw JSON: `perf-runtime2-run-2026-11-24.json`; evidence: `perf-runtime2-run-2026-11-24-evidence.txt`.

## Environment

| Fact | Value |
|---|---|
| RAST version | RAST 0.18.3 (049be2d60558) |
| Xpra | xpra v6.5.5-r0 |
| Chrome | Google Chrome 158.0.8127.36 (deb 158.0.8127.36-1) |
| Display | Xvfb software (:102), 12 vCPU Intel(R) Core(TM) i7-9750H CPU @ 2.60GHz, load 3.03 start → 4.74 end |
| Session unit | `rast-xpra-u127` (uid 200127) |
| rast.service | active since Tue 2026-11-24 05:04:24 UTC |
| Negotiated encoder | `webp` (1438x256, quality n/a, speed n/a) |
| Xpra batch config | min-delay 16 ms, max-delay 120 ms, expire 100 ms |
| Encodings offered by client | 'jpeg', 'png', 'png/P', 'png/L', 'rgb', 'rgb32', 'rgb24', 'webp', 'void', 'avif', 'h264', 'vp8' |
| Failure injection / profile | see the AFTER section and the deploy note in docs/BENCHMARKS.md |

## Negotiated Xpra encoding / latency evidence

`xpra info` of the benchmark user's session, captured before the interaction phases, while the person was typing (mid-interaction), and after the measured phases. `batch delay` is Xpra's adaptive batching delay: the larger it is, the longer a keypress can wait before its frame is encoded (this is the main suspect for typing/scroll lag).

| Field | before | mid-interaction | after |
|---|---|---|---|
| encoder (dominant by painted pixels) | webp | webp | webp |
| window size | 1438x256 | 1440x860 | 1374x773 |
| quality.cur / avg | n/a / n/a | n/a / n/a | n/a / n/a |
| speed.cur / avg | n/a / n/a | n/a / n/a | n/a / n/a |
| batch delay avg (ms) | 4.0 | 3.0 | 1.0 |
| batch delay max (ms) | 12.0 | 12.0 | 5.0 |
| batch min-delay / max-delay (ms) | 16 / 120 | 16 / 120 | 16 / 120 |
| damage target-latency (ms) | 88.0 | 82.0 | 77.0 |
| damage frame-total latency (ms) | 162.0 | 125.0 | 119.0 |
| ping latency p50 / p90 / max (ms) | 80 / 80 / 80 | 80 / 86 / 86 | 64 / 86 / 98 |
| compressor | lz4 | lz4 | lz4 |

Server-side totals at the end of the run: n/a damage fps, output packetcount 13665, input packetcount 2394.

## Baseline table

| Measurement | p50 | p95 | min–max | n | unit | notes |
|---|---|---|---|---|---|---|
| Cold launch — click → adopted window | 12028.0 | 12110.0 | 12028.0–12110.0 | 2 | ms | session stopped first |
| Cold launch — click → placeholder window | 231.0 | 268.0 | 231.0–268.0 | 2 | ms | shell responsiveness, not app |
| Cold launch — adopted window → first painted | 70.0 | 85.0 | 70.0–85.0 | 2 | ms | additional paint wait |
| Warm launch — click → adopted window | 767.0 | 880.0 | 762.0–880.0 | 4 | ms | shared session kept |
| Warm launch — click → placeholder window | 257.0 | 292.0 | 255.0–292.0 | 4 | ms |  |
| Warm launch — adopted window → first painted | 263.0 | 392.0 | 201.0–392.0 | 4 | ms |  |
| Input latency — keydown → changed pixel | 198.0 | 287.0 | 61.0–366.0 | 20 | ms | screenshot polling; poll granularity 58.8 ms |
| Typing burst 40 chars — total (first key → final frame) | 367.0 | 368.0 | 333.0–368.0 | 3 | ms | upper bound |
| Typing burst 40 chars — dispatch (first → last key) | 27.0 | 27.0 | 27.0–27.0 | 3 | ms |  |
| Typing burst 40 chars — backlog drain (last key → final frame) | 340.0 | 341.0 | 306.0–341.0 | 3 | ms | upper bound |
| Idle CPU (session + app units) | 22.1 | — | — | 1 | % of one core | 30 s quiet |
| Idle memory — memory.current | 271.9 | — | — | 1 | MiB |  |
| Idle memory — memory.peak | 288.9 | — | — | 1 | MiB |  |
| Idle painted frames (screencast) | 0.1 | — | — | 1 | fps | baseline noise on a static page |
| Scroll — server-encoded frames | 12.9 | — | — | 1 | fps | xpra info total_frames delta / duration |
| Scroll — painted frames (screencast) | 20.0 | — | — | 1 | fps | 71 distinct frames in 3547 ms |
| Scroll — painted frames (screenshot sampling) | 0.4 | — | — | 1 | fps | lower bound, 26 samples |
| Scroll — frame interval p50 / p95 | 34.0 | — | — | 1 | ms | p95 66 ms |
| Scroll — CPU during phase | 41.6 | — | — | 1 | % of one core |  |
| Animation (60 Hz canvas) — painted frames (screencast) | 26.0 | — | — | 1 | fps |  |
| Animation (60 Hz canvas) — server-encoded frames | 50.7 | — | — | 1 | fps |  |
| Animation — CPU during phase | 113.7 | — | — | 1 | % of one core |  |
| Video (local VP8) — painted frames (screencast) | 26.0 | — | — | 1 | fps |  |
| Video (local VP8) — server-encoded frames | 32.4 | — | — | 1 | fps |  |
| Video — CPU during phase | 73.2 | — | — | 1 | % of one core |  |
| Direct Xpra session page — time to first paint | 1799.0 | — | — | 1 | ms | desktop shell excluded |
| bandwidth — session total received | 40.0 | — | — | 1 | MiB | 13674 WS frames |
| bandwidth — session total sent | 0.1 | — | — | 1 | MiB | 2418 WS frames |

## Per-phase resources and bandwidth

| Phase | duration s | CPU % of one core | memory start → end (MiB) | memory peak (MiB) | server-encoded frames | WS received KiB | WS sent KiB |
|---|---|---|---|---|---|---|---|
| inputLatency | 11.7 | 24.1 | 271.9 → 271.2 | 288.9 | 48 | 5296 | 4 |
| typingBurst | 3.4 | 37.1 | 271.5 → 272.0 | 288.9 | 28 | 1188 | 11 |
| idle | 33.0 | 22.1 | 272.8 → 271.9 | 288.9 | 56 | 311 | 2 |
| scroll | 12.2 | 41.6 | 272.4 → 272.3 | 288.9 | 158 | 2197 | 11 |
| animation | 12.5 | 113.7 | 272.3 → 272.4 | 288.9 | 634 | 6559 | 26 |
| video | 16.0 | 73.2 | 274.2 → 269.8 | 288.9 | 519 | 13595 | 23 |

## Method

* Headless Playwright Chromium (same build as `tests/e2e/lib/browser.mjs`) logs in through the real login form, then drives the real desktop: start menu, adopted native windows, taskbar.
* The remote Chrome is navigated (through its own address bar) to a benchmark page served by the harness on `127.0.0.1`; the page paints a large swatch and changes the whole page background on every key, so one changed window pixel proves the key reached the display. A pure-black strip lets the harness locate the remote page origin inside the adopted window.
* Input latency: 20 samples of keydown → first changed screenshot pixel, reported p50/p95. Screenshot polling makes this an upper bound; the measured poll granularity is included.
* Typing burst: 40 chars with `delay: 0`; dispatch time, backlog drain (last key → first stable frame) and total are reported.
* Launch latency: cold = the user's native session is stopped first (`POST /api/linux-apps/session/close`), warm = the shared Xpra session stays alive and only the app is restarted; the click target is the start-menu tile with the menu already open, the end point is the first adopted RAST window (`linux:<app>:<wid>`).
* FPS: (a) distinct CDP screencast frames (painted compositor frames, hash of each JPEG frame), (b) distinct fixed-interval screenshots (lower bound), (c) server-encoded frames from consecutive `xpra info` `total_frames.*` deltas.
* CPU/memory: cgroup v2 `cpu.stat`/`memory.current`/`memory.peak` of `rast-xpra-u<id>` plus every active `rast-app-*` unit of that identity, sampled before/after each phase.
* Bandwidth: WebSocket payload bytes of the Xpra HTML5 client, observed by Playwright `websocket` events (application-level, after Xpra compression).
* Evidence: `xpra info` of the benchmark user's session (`sudo -u rast-u<id> xpra info --socket-dir=/run/rast-linux/<uid>/sockets :<display>`), `systemctl show` of the session unit, and the encoding lines of the session journal.

## Limitations

* The browser is headless Chromium; software Xvfb on the server; no GPU and no hardware video decode anywhere in the path.
* `--base-url` points at the public HTTPS URL by default, so timings include Cloudflare/nginx and the WAN round trip (the actual user path). Use the loopback URL for a LAN-free reference, but keep the same URL for BEFORE/AFTER.
* Input latency, typing drain and screenshot-based FPS are upper bounds limited by the display-sampling interval; they are comparable across runs on the same host.
* The CDP screencast used for 'painted compositor frames' is throttled by headless Chromium (~20 fps ceiling on this host), so it is a lower bound and not a 60 fps-capable measurement; the server-side encoded-frame count from `xpra info` is the authoritative frame-rate number.
* `xpra info` ping/latency fields measure the server↔proxy/browser path as seen by the Xpra server and are corroboration, not an independent measurement.
* One benchmark user at a time; the host may run other workloads (load average recorded in the JSON).

## Comparison with the other conditions

The three-condition comparison (BEFORE / AFTER-balanced / AFTER-responsive), the
deployment/environment delta, the final profile decision and the interpretation of
the encoder-selection difference live in `docs/BENCHMARKS.md` §7. Re-run this
benchmark with the same command and a new `--label` to extend the comparison:

```bash
node tests/e2e/perf/bench.mjs --label=<condition> --base-url=https://desktop.lufria.tech
```

