# RAST native runtime performance — runtime21-final-2026-11-24

Generated 2026-11-24T11:34:26.369Z against `https://desktop.lufria.tech` as `t-alice` (Google Chrome, `google-chrome`, backend `native`).
Raw JSON: `perf-runtime2-runtime21-final-2026-11-24-2026-11-24.json`; evidence: `perf-runtime2-runtime21-final-2026-11-24-2026-11-24-evidence.txt`.

## Environment

| Fact | Value |
|---|---|
| RAST version | RAST 0.18.3 (8a08a9a39c7d) |
| Xpra | xpra v6.5.5-r0 |
| Chrome | Google Chrome 158.0.8127.36 (deb 158.0.8127.36-1) |
| Display | Xvfb software (:101), 12 vCPU Intel(R) Core(TM) i7-9750H CPU @ 2.60GHz, load 1.18 start → 2.53 end |
| Session unit | `rast-xpra-u127` (uid 200127) |
| rast.service | active since Tue 2026-11-24 11:31:39 UTC |
| Negotiated encoder | `webp` (1352x178, quality 97, speed 55) |
| Xpra batch config | min-delay 16 ms, max-delay 500 ms, expire 250 ms |
| Encodings offered by client | 'jpeg', 'png', 'png/P', 'png/L', 'rgb', 'rgb32', 'rgb24', 'webp', 'void', 'avif', 'h264', 'vp8' |
| Failure injection / profile | see the AFTER section and the deploy note in docs/BENCHMARKS.md |

## Negotiated Xpra encoding / latency evidence

`xpra info` of the benchmark user's session, captured before the interaction phases, while the person was typing (mid-interaction), and after the measured phases. `batch delay` is Xpra's adaptive batching delay: the larger it is, the longer a keypress can wait before its frame is encoded (this is the main suspect for typing/scroll lag).

| Field | before | mid-interaction | after |
|---|---|---|---|
| encoder (dominant by painted pixels) | webp | h264 | h264 |
| window size | 1352x178 | 1440x860 | 1440x860 |
| quality.cur / avg | 97 / 85 | 29 / 66 | 24 / 67 |
| speed.cur / avg | 55 / 57 | 55 / 58 | 64 / 58 |
| batch delay avg (ms) | 20.0 | 13.0 | 15.0 |
| batch delay max (ms) | 28.0 | 31.0 | 37.0 |
| batch min-delay / max-delay (ms) | 16 / 500 | 16 / 500 | 16 / 500 |
| damage target-latency (ms) | 82.0 | 118.0 | 111.0 |
| damage frame-total latency (ms) | 117.0 | 341.0 | 452.0 |
| ping latency p50 / p90 / max (ms) | 116 / 116 / 116 | 94 / 281 / 281 | 69 / 94 / 281 |
| compressor | lz4 | lz4 | lz4 |

Server-side totals at the end of the run: n/a damage fps, output packetcount 12846, input packetcount 939.

## Baseline table

| Measurement | p50 | p95 | min–max | n | unit | notes |
|---|---|---|---|---|---|---|
| Cold launch — click → adopted window | 5174.0 | 5215.0 | 5174.0–5215.0 | 2 | ms | session stopped first |
| Cold launch — click → placeholder window | 264.0 | 290.0 | 264.0–290.0 | 2 | ms | shell responsiveness, not app |
| Cold launch — adopted window → first painted | 50.0 | 236.0 | 50.0–236.0 | 2 | ms | additional paint wait |
| Warm launch — click → adopted window | 846.0 | 2638.0 | 781.0–2638.0 | 4 | ms | shared session kept |
| Warm launch — click → placeholder window | 250.0 | 274.0 | 237.0–274.0 | 4 | ms |  |
| Warm launch — adopted window → first painted | 338.0 | 582.0 | 144.0–582.0 | 4 | ms |  |
| Input latency — keydown → changed pixel | 229.0 | 456.0 | 101.0–464.0 | 20 | ms | screenshot polling; poll granularity 59.3 ms |
| Typing burst 40 chars — total (first key → final frame) | 350.0 | 461.0 | 283.0–461.0 | 3 | ms | upper bound |
| Typing burst 40 chars — dispatch (first → last key) | 29.0 | 29.0 | 27.0–29.0 | 3 | ms |  |
| Typing burst 40 chars — backlog drain (last key → final frame) | 321.0 | 434.0 | 254.0–434.0 | 3 | ms | upper bound |
| Idle CPU (session + app units) | 25.2 | — | — | 1 | % of one core | 30 s quiet |
| Idle memory — memory.current | 153.3 | — | — | 1 | MiB |  |
| Idle memory — memory.peak | 194.5 | — | — | 1 | MiB |  |
| Idle painted frames (screencast) | 0.2 | — | — | 1 | fps | baseline noise on a static page |
| Scroll — server-encoded frames | 0.1 | — | — | 1 | fps | xpra info total_frames delta / duration |
| Scroll — painted frames (screencast) | 0.7 | — | — | 1 | fps | 2 distinct frames in 2870 ms |
| Scroll — painted frames (screenshot sampling) | 0.4 | — | — | 1 | fps | lower bound, 28 samples |
| Scroll — frame interval p50 / p95 | 341.0 | — | — | 1 | ms | p95 1000 ms |
| Scroll — CPU during phase | 27.1 | — | — | 1 | % of one core |  |
| Animation (60 Hz canvas) — painted frames (screencast) | 0.7 | — | — | 1 | fps |  |
| Animation (60 Hz canvas) — server-encoded frames | 0.3 | — | — | 1 | fps |  |
| Animation — CPU during phase | 28.9 | — | — | 1 | % of one core |  |
| Video (local VP8) — painted frames (screencast) | 0.5 | — | — | 1 | fps |  |
| Video (local VP8) — server-encoded frames | 0.0 | — | — | 1 | fps |  |
| Video — CPU during phase | 28.8 | — | — | 1 | % of one core |  |
| Direct Xpra session page — time to first paint | 1552.0 | — | — | 1 | ms | desktop shell excluded |
| bandwidth — session total received | 12.1 | — | — | 1 | MiB | 12848 WS frames |
| bandwidth — session total sent | 0.0 | — | — | 1 | MiB | 970 WS frames |

## Per-phase resources and bandwidth

| Phase | duration s | CPU % of one core | memory start → end (MiB) | memory peak (MiB) | server-encoded frames | WS received KiB | WS sent KiB |
|---|---|---|---|---|---|---|---|
| inputLatency | 13.4 | 27.0 | 182.5 → 183.6 | 193.5 | 67 | 5450 | 4 |
| typingBurst | 3.4 | 33.8 | 183.8 → 184.5 | 194.5 | 11 | 2320 | 12 |
| idle | 33.0 | 25.2 | 184.7 → 153.3 | 194.5 | 0 | 301 | 0 |
| scroll | 11.5 | 27.1 | 153.2 → 153.6 | 194.5 | 1 | 184 | 5 |
| animation | 12.5 | 28.9 | 153.4 → 168.0 | 215.0 | 4 | 587 | 1 |
| video | 16.0 | 28.8 | 168.7 → 168.4 | 215.0 | 0 | 149 | 0 |

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

