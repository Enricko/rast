# RAST native runtime performance — baseline

Generated 2026-11-24T04:55:19.166Z against `https://desktop.lufria.tech` as `t-alice` (Google Chrome, `google-chrome`, backend `native`).
Raw JSON: `perf-runtime2-run-2026-11-24.json`; evidence: `perf-runtime2-run-2026-11-24-evidence.txt`.

## Environment

| Fact | Value |
|---|---|
| RAST version | RAST 0.18.3 (f69a4544e6fd) |
| Xpra | xpra v6.5.5-r0 |
| Chrome | Google Chrome 158.0.8127.36 (deb 158.0.8127.36-1) |
| Display | Xvfb software (:102), 12 vCPU Intel(R) Core(TM) i7-9750H CPU @ 2.60GHz, load 5.18 start → 6.85 end |
| Session unit | `rast-xpra-u127` (uid 200127) |
| rast.service | active since Mon 2026-11-23 20:05:47 UTC |
| Negotiated encoder | `webp` (1438x806, quality 98, speed 56) |
| Xpra batch config | min-delay 16 ms, max-delay 500 ms, expire 250 ms |
| Encodings offered by client | 'jpeg', 'png', 'png/P', 'png/L', 'rgb', 'rgb32', 'rgb24', 'webp', 'void', 'avif', 'h264', 'vp8' |
| Failure injection / profile | see the AFTER section and the deploy note in docs/BENCHMARKS.md |

## Negotiated Xpra encoding / latency evidence

`xpra info` of the benchmark user's session, captured before the interaction phases, while the person was typing (mid-interaction), and after the measured phases. `batch delay` is Xpra's adaptive batching delay: the larger it is, the longer a keypress can wait before its frame is encoded (this is the main suspect for typing/scroll lag).

| Field | before | mid-interaction | after |
|---|---|---|---|
| encoder (dominant by painted pixels) | webp | h264 | h264 |
| window size | 1438x806 | 1440x848 | 916x514 |
| quality.cur / avg | 98 / 88 | 28 / 79 | 93 / 82 |
| speed.cur / avg | 56 / 62 | 55 / 58 | 77 / 67 |
| batch delay avg (ms) | 8.0 | 4.0 | 1.0 |
| batch delay max (ms) | 17.0 | 17.0 | 11.0 |
| batch min-delay / max-delay (ms) | 16 / 500 | 16 / 500 | 16 / 500 |
| damage target-latency (ms) | 90.0 | 90.0 | 78.0 |
| damage frame-total latency (ms) | 201.0 | 158.0 | 116.0 |
| ping latency p50 / p90 / max (ms) | 54 / 54 / 54 | 54 / 117 / 117 | 61 / 127 / 255 |
| compressor | lz4 | lz4 | lz4 |

Server-side totals at the end of the run: n/a damage fps, output packetcount 16104, input packetcount 3830.

## Baseline table

| Measurement | p50 | p95 | min–max | n | unit | notes |
|---|---|---|---|---|---|---|
| Cold launch — click → adopted window | 6534.0 | 12452.0 | 6534.0–12452.0 | 2 | ms | session stopped first |
| Cold launch — click → placeholder window | 255.0 | 256.0 | 255.0–256.0 | 2 | ms | shell responsiveness, not app |
| Cold launch — adopted window → first painted | 281.0 | 504.0 | 281.0–504.0 | 2 | ms | additional paint wait |
| Warm launch — click → adopted window | 764.0 | 772.0 | 758.0–772.0 | 4 | ms | shared session kept |
| Warm launch — click → placeholder window | 251.0 | 256.0 | 250.0–256.0 | 4 | ms |  |
| Warm launch — adopted window → first painted | 469.0 | 533.0 | 440.0–533.0 | 4 | ms |  |
| Input latency — keydown → changed pixel | 217.0 | 398.0 | 49.0–496.0 | 20 | ms | screenshot polling; poll granularity 56.0 ms |
| Typing burst 40 chars — total (first key → final frame) | 825.0 | 1043.0 | 704.0–1043.0 | 3 | ms | upper bound |
| Typing burst 40 chars — dispatch (first → last key) | 129.0 | 163.0 | 104.0–163.0 | 3 | ms |  |
| Typing burst 40 chars — backlog drain (last key → final frame) | 696.0 | 880.0 | 600.0–880.0 | 3 | ms | upper bound |
| Idle CPU (session + app units) | 10.6 | — | — | 1 | % of one core | 30 s quiet |
| Idle memory — memory.current | 389.5 | — | — | 1 | MiB |  |
| Idle memory — memory.peak | 437.9 | — | — | 1 | MiB |  |
| Idle painted frames (screencast) | 0.1 | — | — | 1 | fps | baseline noise on a static page |
| Scroll — server-encoded frames | 23.6 | — | — | 1 | fps | xpra info total_frames delta / duration |
| Scroll — painted frames (screencast) | 19.2 | — | — | 1 | fps | 70 distinct frames in 3642 ms |
| Scroll — painted frames (screenshot sampling) | 0.4 | — | — | 1 | fps | lower bound, 30 samples |
| Scroll — frame interval p50 / p95 | 34.0 | — | — | 1 | ms | p95 66 ms |
| Scroll — CPU during phase | 34.6 | — | — | 1 | % of one core |  |
| Animation (60 Hz canvas) — painted frames (screencast) | 19.7 | — | — | 1 | fps |  |
| Animation (60 Hz canvas) — server-encoded frames | 101.6 | — | — | 1 | fps |  |
| Animation — CPU during phase | 108.7 | — | — | 1 | % of one core |  |
| Video (local VP8) — painted frames (screencast) | 19.0 | — | — | 1 | fps |  |
| Video (local VP8) — server-encoded frames | 61.4 | — | — | 1 | fps |  |
| Video — CPU during phase | 55.5 | — | — | 1 | % of one core |  |
| Direct Xpra session page — time to first paint | 1945.0 | — | — | 1 | ms | desktop shell excluded |
| bandwidth — session total received | 41.5 | — | — | 1 | MiB | 16340 WS frames |
| bandwidth — session total sent | 0.2 | — | — | 1 | MiB | 3858 WS frames |

## Per-phase resources and bandwidth

| Phase | duration s | CPU % of one core | memory start → end (MiB) | memory peak (MiB) | server-encoded frames | WS received KiB | WS sent KiB |
|---|---|---|---|---|---|---|---|
| inputLatency | 12.2 | 26.3 | 356.8 → 390.8 | 436.4 | 90 | 6098 | 5 |
| typingBurst | 5.3 | 41.2 | 389.1 → 437.3 | 437.7 | 60 | 2505 | 12 |
| idle | 33.0 | 10.6 | 393.3 → 389.5 | 437.9 | 0 | 300 | 0 |
| scroll | 12.3 | 34.6 | 390.0 → 400.4 | 438.6 | 290 | 1722 | 17 |
| animation | 12.5 | 108.7 | 401.0 → 402.5 | 438.6 | 1273 | 3941 | 51 |
| video | 16.0 | 55.5 | 401.7 → 402.4 | 438.6 | 985 | 12027 | 43 |

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

