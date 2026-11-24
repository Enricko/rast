# RAST native runtime performance — runtime21-acceptance-2026-11-24

Generated 2026-11-24T11:45:19.103Z against `https://desktop.lufria.tech` as `t-alice` (Google Chrome, `google-chrome`, backend `native`).
Raw JSON: `perf-runtime2-runtime21-acceptance-2026-11-24-2026-11-24.json`; evidence: `perf-runtime2-runtime21-acceptance-2026-11-24-2026-11-24-evidence.txt`.

## Environment

| Fact | Value |
|---|---|
| RAST version | RAST 0.18.3 (8a08a9a39c7d) |
| Xpra | xpra v6.5.5-r0 |
| Chrome | Google Chrome 158.0.8127.36 (deb 158.0.8127.36-1) |
| Display | Xvfb software (:101), 12 vCPU Intel(R) Core(TM) i7-9750H CPU @ 2.60GHz, load 3.23 start → 4.70 end |
| Session unit | `rast-xpra-u127` (uid 200127) |
| rast.service | active since Tue 2026-11-24 11:42:37 UTC |
| Negotiated encoder | `h264` (1438x806, quality 70, speed 67) |
| Xpra batch config | min-delay 16 ms, max-delay 500 ms, expire 250 ms |
| Encodings offered by client | 'jpeg', 'png', 'png/P', 'png/L', 'rgb', 'rgb32', 'rgb24', 'webp', 'void', 'avif', 'h264', 'vp8' |
| Failure injection / profile | see the AFTER section and the deploy note in docs/BENCHMARKS.md |

## Negotiated Xpra encoding / latency evidence

`xpra info` of the benchmark user's session, captured before the interaction phases, while the person was typing (mid-interaction), and after the measured phases. `batch delay` is Xpra's adaptive batching delay: the larger it is, the longer a keypress can wait before its frame is encoded (this is the main suspect for typing/scroll lag).

| Field | before | mid-interaction | after |
|---|---|---|---|
| encoder (dominant by painted pixels) | h264 | h264 | h264 |
| window size | 1438x806 | 1440x860 | 720x430 |
| quality.cur / avg | 70 / 68 | 84 / 75 | 94 / 73 |
| speed.cur / avg | 67 / 69 | 64 / 67 | 80 / 71 |
| batch delay avg (ms) | 4.0 | 3.0 | 13.0 |
| batch delay max (ms) | 23.0 | 23.0 | 33.0 |
| batch min-delay / max-delay (ms) | 16 / 500 | 16 / 500 | 16 / 500 |
| damage target-latency (ms) | 84.0 | 81.0 | 100.0 |
| damage frame-total latency (ms) | 191.0 | 136.0 | 124.0 |
| ping latency p50 / p90 / max (ms) | 56 / 59 / 59 | 56 / 59 / 59 | 58 / 106 / 218 |
| compressor | lz4 | lz4 | lz4 |

Server-side totals at the end of the run: n/a damage fps, output packetcount 15276, input packetcount 2474.

## Baseline table

| Measurement | p50 | p95 | min–max | n | unit | notes |
|---|---|---|---|---|---|---|
| Cold launch — click → adopted window | 4523.0 | 4911.0 | 4523.0–4911.0 | 2 | ms | session stopped first |
| Cold launch — click → placeholder window | 267.0 | 287.0 | 267.0–287.0 | 2 | ms | shell responsiveness, not app |
| Cold launch — adopted window → first painted | 215.0 | 329.0 | 215.0–329.0 | 2 | ms | additional paint wait |
| Warm launch — click → adopted window | 869.0 | 945.0 | 803.0–945.0 | 4 | ms | shared session kept |
| Warm launch — click → placeholder window | 261.0 | 285.0 | 229.0–285.0 | 4 | ms |  |
| Warm launch — adopted window → first painted | 495.0 | 521.0 | 396.0–521.0 | 4 | ms |  |
| Input latency — keydown → changed pixel | 183.0 | 313.0 | 117.0–375.0 | 20 | ms | screenshot polling; poll granularity 55.8 ms |
| Typing burst 40 chars — total (first key → final frame) | 374.0 | 551.0 | 334.0–551.0 | 3 | ms | upper bound |
| Typing burst 40 chars — dispatch (first → last key) | 26.0 | 26.0 | 25.0–26.0 | 3 | ms |  |
| Typing burst 40 chars — backlog drain (last key → final frame) | 349.0 | 525.0 | 308.0–525.0 | 3 | ms | upper bound |
| Idle CPU (session + app units) | 31.1 | — | — | 1 | % of one core | 30 s quiet |
| Idle memory — memory.current | 133.2 | — | — | 1 | MiB |  |
| Idle memory — memory.peak | 166.3 | — | — | 1 | MiB |  |
| Idle painted frames (screencast) | 0.4 | — | — | 1 | fps | baseline noise on a static page |
| Scroll — server-encoded frames | 3.2 | — | — | 1 | fps | xpra info total_frames delta / duration |
| Scroll — painted frames (screencast) | 0.6 | — | — | 1 | fps | 2 distinct frames in 3207 ms |
| Scroll — painted frames (screenshot sampling) | 1.2 | — | — | 1 | fps | lower bound, 27 samples |
| Scroll — frame interval p50 / p95 | 100.0 | — | — | 1 | ms | p95 550 ms |
| Scroll — CPU during phase | 29.2 | — | — | 1 | % of one core |  |
| Animation (60 Hz canvas) — painted frames (screencast) | 20.0 | — | — | 1 | fps |  |
| Animation (60 Hz canvas) — server-encoded frames | 60.3 | — | — | 1 | fps |  |
| Animation — CPU during phase | 77.1 | — | — | 1 | % of one core |  |
| Video (local VP8) — painted frames (screencast) | 18.3 | — | — | 1 | fps |  |
| Video (local VP8) — server-encoded frames | 32.6 | — | — | 1 | fps |  |
| Video — CPU during phase | 29.4 | — | — | 1 | % of one core |  |
| Direct Xpra session page — time to first paint | 2872.0 | — | — | 1 | ms | desktop shell excluded |
| bandwidth — session total received | 41.6 | — | — | 1 | MiB | 15305 WS frames |
| bandwidth — session total sent | 0.1 | — | — | 1 | MiB | 2498 WS frames |

## Per-phase resources and bandwidth

| Phase | duration s | CPU % of one core | memory start → end (MiB) | memory peak (MiB) | server-encoded frames | WS received KiB | WS sent KiB |
|---|---|---|---|---|---|---|---|
| inputLatency | 12.2 | 22.9 | 146.8 → 147.5 | 166.3 | 65 | 5471 | 4 |
| typingBurst | 3.6 | 35.3 | 147.9 → 148.1 | 166.3 | 9 | 2956 | 13 |
| idle | 33.0 | 31.1 | 148.5 → 133.2 | 166.3 | 94 | 5339 | 4 |
| scroll | 11.9 | 29.2 | 133.8 → 133.5 | 166.3 | 38 | 2115 | 2 |
| animation | 12.6 | 77.1 | 133.6 → 141.2 | 166.3 | 759 | 4623 | 32 |
| video | 16.1 | 29.4 | 146.9 → 146.6 | 166.3 | 524 | 2691 | 23 |

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

