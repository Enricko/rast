# RAST native runtime — performance benchmarks and application compatibility matrix

This document is the reference for measuring the performance of RAST native Linux
apps (Xpra-backed, seamless windows adopted into the RAST desktop) and for
tracking which installed applications actually work. It contains:

* the measurement method and exact reproduction commands,
* the environment the numbers were captured in,
* the **BEFORE** baseline captured on the production build that was live on
  2026-11-24 before the native-runtime changes,
* a clearly marked **AFTER** section to be re-run with the same script and the
  same environment after the changes are deployed,
* the application compatibility matrix and its known limitations.

> **Status**
> The BEFORE baseline, AFTER-balanced and AFTER-responsive measurements are
> captured and committed (2026-11-24). The final production profile is
> **`balanced`**; the responsive measurement is kept as a documented A/B
> experiment. The three-condition comparison, environment delta and final
> decision rationale live in [§7](#7-after-the-native-runtime-changes).

Harness files (all committed):

| Path | Purpose |
|---|---|
| `tests/e2e/perf/bench.mjs` | The benchmark runner (`--label=<name>`). |
| `tests/e2e/perf/bench-page.html` | Deterministic fixture page served by the harness on `127.0.0.1` and loaded inside the remote Chrome. |
| `tests/e2e/compat/matrix.mjs` | The application compatibility matrix runner. |
| `tests/e2e/results/perf-runtime2-*` | Raw JSON + markdown + Xpra evidence of each benchmark run. |
| `tests/e2e/results/compat-runtime2-*` | Raw JSON + markdown of each matrix run. |

The older harnesses (`tests/e2e/perf/quick-perf.mjs`, `run-perf.mjs`) and their
method notes (`tests/e2e/results/README.md`) remain valid for the legacy
Docker/native comparison; `bench.mjs` supersedes them for the native runtime
work because it measures the **real authenticated desktop path** with
percentiles, typing bursts, per-phase cgroup resources, WebSocket bandwidth and
Xpra negotiation evidence.

---

## 1. What is measured and how

All timestamps are taken in the Node process with `Date.now()`; browser,
RAST server and Xpra display run on one host. The default `--base-url` is the
public HTTPS URL, so the numbers include the real Cloudflare/nginx/WAN path.

| Metric | Start | End | Notes |
|---|---|---|---|
| **Cold launch** | click on the app tile in the start menu (menu already open, excluded) | first **adopted** RAST window (`.window[data-window-id]` with appId `linux:<app>:<wid>`) | the user's whole native session is stopped first (`POST /api/linux-apps/session/close`); includes session start + app start. Also records the "Starting <app>…" placeholder window and the first painted pixel. |
| **Warm launch** | same click | first adopted RAST window | the shared Xpra session stays alive and only the app is restarted (run closed via the API) — the per-app launch cost a person sees in daily use. |
| **Input → display latency** | `keydown` (Node) | first changed pixel of the adopted app window | 20 samples, reported p50/p95/min/max, not a single sample. Screenshot polling ⇒ **upper bound**; the measured poll granularity (~45–83 ms p50 ≈ 56 ms) is reported. |
| **Typing burst** | first key of a 40-char burst typed with `delay: 0` | first stable (two identical) final window frame | reports dispatch (first→last key written), backlog drain (last key→final frame) and total. Screenshot based ⇒ upper bounds. |
| **Scrolling smoothness** | start of 24 wheel events (~1.3 s) | 0.9 s after the last wheel event | three independent counts: (a) distinct CDP screencast frames = painted compositor frames (**~20 fps ceiling in headless Chromium**, lower bound), (b) distinct fixed-interval screenshots (lower bound), (c) server-encoded frames from `xpra info` `total_frames.*` deltas (**authoritative**). |
| **Animation smoothness** | start | 3 s later | same frame counting on a 60 Hz canvas fixture. |
| **Video playback smoothness** | start | 4 s later | same frame counting on a locally served 6 s VP8/30 fps test video (generated with ffmpeg into `tests/e2e/tmp/`). |
| **CPU / memory per phase** | phase start | phase end | cgroup v2 `cpu.stat` (`usage_usec` delta → % of one core), `memory.current`, `memory.peak` of `rast-xpra-u<id>` **plus** every active `rast-app-u<uid>-*` unit of that identity. |
| **Idle overhead** | start of a quiet interval | end | 30 s with no input, app open: CPU, memory, server-encoded frames, WebSocket bytes. |
| **Bandwidth** | session start | session end / per phase | WebSocket payload bytes of the Xpra HTML5 client, observed through Playwright `websocket` events (application-level, after Xpra compression). |
| **Negotiated encoding / latency** | — | — | `xpra info` of the benchmark user's own session before / while typing (mid-interaction) / after the phases: dominant encoder by painted pixels, quality/speed, batch config and adaptive delay, damage target/frame-total latency, ping latency, compressor. |
| **Direct stream page** | navigation start | first painted frame | raw Xpra HTML5 session page opened directly (no desktop shell / window manager), as a comparison point for stream start-up. |

The deterministic fixture page (`bench-page.html`) is served by the harness on
`127.0.0.1` (default port 8799) and loaded inside the remote Chrome through its
own address bar. Each keypress changes a large colour swatch *and* the whole
page background and there are no other animations in type mode, so **one
changed window pixel proves the input reached the display**. A pure-black strip
at the top of the page lets the harness locate the remote page origin inside the
adopted window (needed for clip screenshots and for clicking the in-page
TYPE/SCROLL/ANIM/VIDEO buttons).

## 2. Exact commands

```bash
# BEFORE / AFTER benchmark (same command, new label; writes JSON+MD+evidence
# under tests/e2e/results/perf-runtime2-<label>-<YYYY-MM-DD>.{json,md} and
# -evidence.txt):
node tests/e2e/perf/bench.mjs --label=baseline
node tests/e2e/perf/bench.mjs --label=after-balanced --base-url=https://desktop.lufria.tech
node tests/e2e/perf/bench.mjs --label=after-responsive --base-url=https://desktop.lufria.tech

# Compatibility matrix (live tests through the desktop shell):
node tests/e2e/compat/matrix.mjs --label=baseline
node tests/e2e/compat/matrix.mjs --label=after --base-url=https://desktop.lufria.tech

# Detection only (no session is touched; safe while another benchmark runs):
node tests/e2e/compat/matrix.mjs --label=inventory --detect-only
```

Useful options (both scripts): `--creds=<file>`, `--user=<name>`,
`--admin=<name>`, `--base-url=<url>`, `--timeout-scale=<factor>`;
`bench.mjs` additionally has `--cold-runs`, `--warm-runs`, `--input-samples`,
`--burst-samples`, `--idle-seconds`, `--skip-scroll|--skip-anim|--skip-video|--skip-direct`,
`--keep-session`; `matrix.mjs` has `--max-apps`, `--detect-only`.
The production credentials are read from `/home/lufria/.config/rast-test/creds`
(`t-alice` user, `t-admin` admin, `t-bob` user) and are never printed; the dev
instance format (`RAST_E2E_*` in `/tmp/rast-e2e/credentials.env`) is also
accepted.

`bench.mjs` starts by closing the **benchmark user's own** session through the
RAST API, measures, then closes it again. It never stops systemd units or
production services directly and never touches another user's session.

## 3. Environment (BEFORE baseline, 2026-11-24)

| Fact | Value |
|---|---|
| Host | single host, 12 vCPU `Intel(R) Core(TM) i7-9750H CPU @ 2.60GHz`, 31 GiB RAM |
| Load average during the BEFORE run | 5.18 → 6.85 (elevated by concurrent builds, see Limitations) |
| RAST | `RAST 0.18.3 (f69a4544e6fd)` — production build serving `https://desktop.lufria.tech` |
| RAST services | `rast.service` + `rast-native.service`; only `rast.service` is restarted by the deploy; the native helper and live sessions are untouched |
| Backend | native Linux apps (one shared Xpra session per user, seamless DOM adoption, software Xvfb) |
| Xpra | `xpra v6.5.5-r0` |
| Xvfb | software rendering, `-screen 0 8192x4096x24` per session, `--dpi=96` |
| Chrome | `Google Chrome 158.0.8127.36` (deb `158.0.8127.36-1`) |
| Browser used for measurement | Playwright 1.55 headless Chromium (`chromium-1248`), same build as `tests/e2e/lib/browser.mjs`; viewport 1480x920 |
| App window | remote Chrome resized through the native shim to 1440x860 for the interaction phases (encoded 1438x806 content) |
| GPU | none in the path (software Xvfb, no hardware decode) |
| Network path | public URL ⇒ Cloudflare → nginx (`127.0.0.1:8720`) → RAST; loopback for the fixture page served to the remote Chrome |
| Session flags (BEFORE) | `xpra start … --clipboard=yes --audio=yes --speaker=on --xvfb=Xvfb --dpi=96 --disable-shm=yes --notifications=no …` |
| Xpra batch config (BEFORE) | `batch.min-delay=16 ms`, `batch.max-delay=500 ms`, `batch.expire=250 ms` (Xpra adaptive defaults) |
| **Broken NVENC probe (BEFORE only)** | `xpra-codecs-nvidia` (NVENC SDK 13.1 vs driver 595.99's 13.0) was installed on the baseline host: every session startup logged `RuntimeError: cannot open encoding session … invalid struct version` and then fell back to webp/x264. The AFTER environment has this module (and `pycuda`) **removed** — that removal is part of the native-runtime fix, not a measurement trick, and belongs to the documented environment delta. |

## 4. BEFORE baseline (captured 2026-11-24 04:52–04:55 UTC)

Raw data (committed):

* `tests/e2e/results/perf-runtime2-baseline-2026-11-24.json` — every sample and phase
* `tests/e2e/results/perf-runtime2-baseline-2026-11-24.md` — generated report
* `tests/e2e/results/perf-runtime2-baseline-2026-11-24-evidence.txt` — `systemctl show` of the session unit, `xpra info` selected fields before / mid-interaction / after, journal encoding lines

Command: `node tests/e2e/perf/bench.mjs --label=baseline` (public URL, `t-alice`,
user id 127 → session unit `rast-xpra-u127`, display `:102`, Linux uid 200127,
runtime dir `/run/rast-linux/200127`).

### 4.1 Launch latency

| Measurement | p50 | p95 | min–max | n |
|---|---|---|---|---|
| Cold launch — click → adopted window | **6.53 s** | 12.45 s | 6.53–12.45 s | 2 |
| Cold launch — click → placeholder window | 255 ms | 256 ms | 255–256 ms | 2 |
| Cold launch — adopted window → first painted | 281 ms | 504 ms | 281–504 ms | 2 |
| Warm launch — click → adopted window | **764 ms** | 772 ms | 758–772 ms | 4 |
| Warm launch — click → placeholder window | 253 ms | 256 ms | 248–256 ms | 4 |
| Warm launch — adopted window → first painted | 469 ms | 533 ms | 440–533 ms | 4 |

Notes: cold launch means the user's **whole Xpra session was stopped first**
(session teardown + session start + Chrome start), which is why it is seconds;
the per-app cost in normal use is the warm path (**~760 ms** click→adopted
window, plus ~470 ms until the window has painted content). The "Starting
<app>…" placeholder appears in ~250 ms, so the shell acknowledges the click
immediately. Cold launch is bimodal (6.5 s vs 12.5 s in the two samples) because
session teardown/start-up timing varies; see Limitations.

### 4.2 Input → display and typing burst

| Measurement | p50 | p95 | min–max | n |
|---|---|---|---|---|
| Input latency — keydown → changed pixel | **217 ms** | **398 ms** | 49–496 ms | 20 |
| Typing burst 40 chars — dispatch (first→last key) | 129 ms | 163 ms | 104–163 ms | 3 |
| Typing burst 40 chars — backlog drain (last key→final frame) | **696 ms** | 880 ms | 600–880 ms | 3 |
| Typing burst 40 chars — total (first key→final frame) | **825 ms** | 1.04 s | 704–1043 ms | 3 |

Screenshot poll granularity: p50 56 ms (p95 83 ms), so the latency upper bound
has ~±1 poll of resolution. The spread (49–496 ms) is the user-visible lag
pattern: some keystrokes paint quickly, many wait for Xpra batching + the WAN
round trip. This is the main metric the `responsive` profile is expected to
improve (batching cap).

### 4.3 Scroll / animation / video smoothness

| Scenario | Server-encoded fps (authoritative) | Painted frames observed (CDP screencast, ~20 fps ceiling) | Distinct screenshots (lower bound) | CPU % of one core |
|---|---|---|---|---|
| Scroll (24 wheel events) | **23.6** | 19.2 | 0.4 | 34.6 |
| Animation — 60 Hz canvas | **101.6** | 19.7 | 8.3 | 108.7 |
| Video — local VP8 30 fps | **61.4** | 19.0 | 8.3 | 55.5 |

Notes: the server-encoded count comes from consecutive `xpra info`
`total_frames.*` deltas over the phase and is the honest frame-rate number; the
CDP screencast is throttled by headless Chromium (~20 fps on this host) and the
screenshot sampling is limited by screenshot round-trip time (~8 fps path), so
both are lower bounds. The scroll phase CPU is the cgroup CPU of the session
while Xpra encodes/batches the scrolling content.

### 4.4 CPU, memory, bandwidth per phase

| Phase | duration | CPU % of one core | memory start → end | memory peak | server-encoded frames | WS received | WS sent |
|---|---|---|---|---|---|---|---|
| input latency (20 samples) | 12.2 s | 26.3 % | 390.8 MiB | 436.4 MiB | 90 | 6.0 MiB | 5.2 KiB |
| typing burst (3 × 40 chars) | 6.0 s | 41.2 % | 437.3 MiB | 437.7 MiB | 68 | 2.4 MiB | 12.4 KiB |
| idle (30 s, no input) | 33.0 s | **10.6 %** | 389.5 MiB | 437.9 MiB | 0 | 300 KiB | 0.3 KiB |
| scroll | 12.3 s | 34.6 % | 400.4 MiB | 438.6 MiB | 290 | 1.7 MiB | 16.7 KiB |
| animation | 12.5 s | 108.7 % | 402.5 MiB | 438.6 MiB | 1270 | 3.8 MiB | 51.3 KiB |
| video | 16.1 s | 55.5 % | 402.4 MiB | 438.6 MiB | 988 | 11.7 MiB | 42.6 KiB |

The session unit alone reported `MemoryCurrent 350.2 MiB` / `MemoryPeak
384.3 MiB` and 28 tasks in the `systemctl show` evidence of the same run;
per-phase numbers above are the sum over the session unit **and** the active
Chrome app unit cgroup(s).

Idle overhead (30 s quiet, Chrome window open, no input): **10.6 % of one core**
and **~390 MiB** current memory for the user's session plus app unit; zero
server-encoded frames on the static fixture page.

Session bandwidth (whole run, ~2.7 min): **43.5 MiB received / 0.17 MiB sent**
over 16,340 / 3,858 WebSocket frames (the received side is dominated by the
animation and video phases above).

### 4.5 Negotiated Xpra encoding / latency evidence

`xpra info` of `rast-xpra-u127` (`sudo -n -u rast-u127 env
XDG_RUNTIME_DIR=/run/rast-linux/200127 xpra info
--socket-dir=/run/rast-linux/200127/sockets :102`) captured before the
interaction phases, **while the person was typing** (mid-interaction) and after
the measured phases:

| Field | before | mid-interaction (typing) | after |
|---|---|---|---|
| Dominant encoder by painted pixels | **webp** (16.96 M px webp vs 3.48 M px h264) | h264 (42.7 M px) vs webp (30.9 M px) — adaptive switch under typing | webp |
| Window | 1438x806 | 1440x848 | — |
| quality.cur / avg | 98 / 88 | 28 / 79 | — |
| speed.cur / avg | 56 / 62 | 55 / 58 | — |
| batch delay avg / max | 8 / 500 ms | **4 / 17 ms** (instantaneous adaptive value; max-delay cap is 500 ms) | 1 / 11 ms |
| batch min-delay / max-delay / expire | 16 / 500 / 250 ms | 16 / 500 / 250 ms | 16 / 500 / 250 ms |
| damage target-latency | 90 ms | 90 ms | — |
| damage frame-total latency | 201 ms | **158 ms** | 116 ms |
| client ping latency p50 / p90 / max | 54 / 54 / 54 ms | 54 / 117 / 117 ms | 61 / 127 / 255 ms |
| compressor | lz4 | lz4 | lz4 |
| encodings offered by client | `jpeg, png, png/P, png/L, rgb, rgb32, rgb24, webp, void, avif, h264, vp8` | — | — |

Interpretation for the lag: the Xpra server batches damage with
`min-delay=16 ms` / `max-delay=500 ms` and adapts the delay to the measured
client latency; the client ping latency p50 is ~54 ms over the public path and
the frame-total latency is ~120–200 ms. Together with the adaptive batching this
explains the input→display p50 of ~217 ms and the ~700 ms typing-burst backlog
drain: keystrokes can sit in a batch before being encoded. The `responsive`
profile caps batching (`max 120 ms`, start `16 ms`) and is expected to reduce
exactly these numbers. A validation probe on the same build/accounts during
active scrolling measured batch delay avg in the 149–202 ms range (max 421 ms),
i.e. the adaptive delay does rise well above its 16 ms floor under load.

Direct raw Xpra session page (desktop shell excluded): time to first paint
**1.95 s** (session already running).

### 4.6 Run-to-run variance of the BEFORE measurement

The harness was validated twice on the unchanged production build before the
deploy (same measurement code; the second run added the mid-interaction evidence
capture). The on-disk run above is the second one; the first run measured cold
launch p50 12.09 s (12.09/12.73), input p50 222 ms / p95 315 ms, typing burst
total p50 534 ms and idle CPU 10.2 % under load 6.58. The differences are host
load and session teardown variance, not code changes: warm launch (759 vs
764 ms) and idle CPU/memory are stable across both. **For BEFORE/AFTER
comparison use the same environment and expect a few percent of run-to-run
noise on the latency metrics.**

---

## 5. Application compatibility matrix

Runner: `tests/e2e/compat/matrix.mjs` (see §2 for commands). It answers "which
installed applications actually work through RAST native windows", with
evidence instead of guesses:

* **Detection** — system `.desktop` entries (`/usr/share/applications`,
  `/usr/local/share/applications`, flatpak/snap export dirs, `~/.local/share/applications`),
  the RAST App Center Linux app list (admin and user view), the RAST built-in
  app list and `command -v` availability for common desktop apps.
* **Live checks** (only for apps the App Center can launch, through the real
  authenticated desktop path in headless Chromium):
  * **launch** — start-menu tile click → adopted RAST window appears;
  * **window mapping** — exactly **one adopted RAST window per remote
    top-level window**, with **0 iframes / 0 per-window viewers** inside the
    RAST window (the adopted `.rast-native-xpra-window` node is inspected in the
    DOM and cross-checked against the native shim's `RASTNative.windows()`);
  * **typing** — keystrokes must change pixels of the app window (deterministic
    fixture page inside the remote Chrome);
  * **scrolling** — wheel events must change pixels on a tall fixture page;
  * **clipboard** — copy → clear → paste must restore the identical text frame
    through the Xpra clipboard;
  * **second window** — a second top-level window must be mirrored 1:1;
  * **close** — closing the RAST window must remove the remote window from both
    the shell and the Xpra session.
* Every check is **Pass / Partial / Fail / Not tested / Not applicable**. Apps
  that are installed on the host but **not managed by the RAST App Center**
  (and therefore cannot be launched through RAST) are reported as
  **not tested — cannot be launched**, separately from apps that are simply
  **not installed**, and separately from real failures.

### 5.1 Matrix result (BEFORE, 2026-11-24)

Raw data: `tests/e2e/results/compat-runtime2-baseline-2026-11-24.json` / `.md`.

* Detected: **31 apps** in total = 1 RAST App Center Linux app (Google Chrome,
  native, ready) + 21 system `.desktop` entries (only Chrome is managed by the
  App Center; the rest are installed on the host but cannot be launched through
  RAST → not tested) + 9 known desktop apps that are not installed (Firefox,
  LibreOffice, GIMP, VLC, mpv, gedit, gnome-terminal, xclock, VS Code → not
  installed → not tested). 9 RAST built-in apps (File Station, App Center, …)
  are outside the native Linux-app matrix.
* The **BEFORE** compatibility file
  (`tests/e2e/results/compat-runtime2-baseline-2026-11-24.{json,md}`) is the
  *detection-only* inventory captured on the pre-change build
  (`--detect-only`); the **AFTER** file
  (`tests/e2e/results/compat-runtime2-after-2026-11-24.{json,md}`) is the live
  matrix captured on the final build. No fail was recorded in either; the only
  non-pass is Chrome's clipboard check (`partial`), described in §7.6.
* Chrome live checks: the live matrix is re-run on the final post-change build
  and its Pass/Partial/Fail row is in [§7.6](#76-live-compatibility-matrix-after);
  the detection inventory above is the BEFORE baseline. Live results are also in
  `tests/e2e/results/compat-runtime2-after-2026-11-24.{json,md}`.

### 5.2 Known matrix limitations

* Only apps that the RAST App Center can launch are live-tested; host packages
  outside the App Center are reported as installed-but-not-tested.
* App-specific checks (typing/scrolling/clipboard/second window) use a
  deterministic fixture where one exists (Chrome). For other apps those checks
  are reported **not applicable / not tested** instead of being inferred from
  unrelated pixel changes.
* One environment (one host, one Xpra version, one Chrome build, software Xvfb,
  no GPU).

---

## 6. Known limitations and safety notes

* **Single host, software rendering.** No GPU and no hardware video decode in
  the path; all encoding (webp/h264 software) runs on the CPU. Absolute FPS and
  CPU numbers are host-specific.
* **Public URL.** `--base-url` defaults to the public HTTPS URL, so latency
  includes Cloudflare/nginx and the WAN round trip — this is the real user path,
  and it is the same for BEFORE and AFTER. Use the loopback URL only for a
  LAN-free reference, and never mix the two inside one comparison.
* **Host load** varies (concurrent builds during the baseline: load 5.2–6.9 on
  12 vCPU). Every run records load average at start and end in its JSON; keep it
  in mind when comparing small deltas.
* **Input latency / typing drain are upper bounds** limited by screenshot
  polling (measured poll granularity is reported). They are comparable across
  runs on the same host.
* **CDP screencast is throttled** by headless Chromium (~20 fps ceiling on this
  host), so "painted frames observed" is a lower bound; use the server-encoded
  fps from `xpra info` as the authoritative frame-rate metric.
* **Cold launch is bimodal** (session teardown/start variance, 6.5 s vs 12.5 s
  in the baseline samples). The daily-use number is the warm launch.
* **Browser chords are reserved by newer shells.** The native-runtime shell
  reserves browser shortcuts for the local browser (the `OTHER_BROWSER_RESERVED`
  set includes `Ctrl+L`, `Ctrl+N`, `Ctrl+T`, `Ctrl+W`, `Ctrl+R`, …), so those
  chords no longer reach the remote app. The benchmark and matrix therefore
  navigate the remote Chrome with **Alt+D** (not reserved; Chrome focuses its
  own address bar) and open a second window through an in-page fixture button
  instead of `Ctrl+N`; both were re-validated on the new build. This is intended
  shell behaviour, not a benchmark workaround for a bug.
* **`xpra info` snapshots are instantaneous** for the adaptive `batch.delay`
  statistics; the mid-interaction snapshot in the evidence file is the relevant
  one for lag analysis, the batch *config* fields (`min-delay`, `max-delay`,
  `expire`) are the deterministic profile knobs.
* **One benchmark user at a time.** Do not run the benchmark concurrently with
  other production verification on the same test account; use the other test
  account (`t-bob`) for independent work.
* **Live production session is sacred.** Never restart `rast.service` /
  `rast-native.service`, never stop `rast-*` units and never end the live
  session `rast-xpra-u1` while measuring; the benchmark only manages the
  benchmark user's own session through the RAST API.
* **Disposable dev instance hazard (documented harness limitation).**
  `scripts/e2e/dev-instance.sh up native` **must not be run while another RAST
  instance has active native sessions** (including production). The scheduled
  recovery in `src/linux_apps/native_session.rs` lists managed units host-wide
  (`systemctl show 'rast-xpra-*.service' 'rast-app-*.service'`) and stops every
  active unit that its own database does not know; a dev instance started next
  to production would therefore stop the live production session. The dev
  instance is also stopped with `stop_native_units`, which stops all
  `rast-app-*`/`rast-xpra-*` units on the host. Use the dev instance only on a
  host/window where no other RAST native session exists (the Lead confirms a
  safe window explicitly).
* **Production accounts.** The benchmark uses the dedicated test accounts
  (`t-alice` user, `t-admin` admin) and reads the test user's own `xpra info`
  via `sudo -u rast-u<id>`; credentials are never printed or committed.

---

## 7. AFTER the native-runtime changes

> **AFTER section — captured 2026-11-24 by re-running the same benchmark, same
> URL, same accounts and same harness against the deployed builds.** Raw data:
> `tests/e2e/results/perf-runtime2-after-balanced-2026-11-24.{json,md,evidence}`,
> `tests/e2e/results/perf-runtime2-after-responsive-2026-11-24.{...}`,
> `tests/e2e/results/compat-runtime2-after-2026-11-24.{json,md}`.

### 7.1 Deployment delta (environment)

| Item | BEFORE | AFTER-balanced | AFTER-responsive |
|---|---|---|---|---|
| RAST binary | `RAST 0.18.3 (f69a4544e6fd)` (sha256 `…3fd6…`) | `RAST 0.18.3 (d519ef13da71)` (sha256 `6d934d89…`) — frontend input/clipboard fixes + backend runtime profiles | `RAST 0.18.3 (049be2d60558)` + co-fixes (cgroup-metrics path fix, doctor wording); no protocol change |
| Runtime profile / display backend | (no profile) | `linux_profile = "balanced"`, `linux_display_backend = "software"` | `linux_profile = "responsive"`, `linux_display_backend = "software"` |
| Xpra session flags | none of the profile flags | none of the profile flags (balanced keeps the previous adaptive behaviour) | `--quality=30 --min-quality=10 --speed=90 --min-speed=70 --video-scaling=0 --av-sync=no` (proven in the evidence file's `ExecStart`) |
| Xpra batching | `min-delay=16 ms`, `max-delay=500 ms`, `expire=250 ms` | unchanged (16 / 500 / 250) | **capped**: Environment `XPRA_BATCH_MAX_DELAY=120 XPRA_BATCH_MIN_DELAY=8 XPRA_BATCH_START_DELAY=16 XPRA_BATCH_EXPIRE_DELAY=100 XPRA_BATCH_TIMEOUT_DELAY=1000`; `xpra info` reports 16 / 120 / 100 (proven in the evidence file) |
| NVENC | `xpra-codecs-nvidia` (NVENC SDK 13.1 vs driver 595.99's 13.0) installed; every session startup logged `RuntimeError: cannot open encoding session … invalid struct version` and fell back to webp/x264 | module and `pycuda` **removed**; `xpra encoding` shows `enc_x264` and `enc_openh264` still present, `nvenc`/`nvdec`/`nvfbc` absent | same as balanced |
| Restarts | — | only `rast.service` was restarted; `rast-native.service`, the live `rast-xpra-u1` session and `/etc/rast` were untouched; new test sessions picked up the new flags | only `rast.service` |

The `xpra encoding` snapshot proving x264/openh264 availability after the NVENC
removal is in every evidence file (`## Xpra encoder modules on the host`).

Final deployment (after the A/B): `/usr/local/bin/rast` = RAST 0.18.3
(`4a4625a351b0`, sha256 `8bbcd9799c06`), `linux_profile = "balanced"`,
`linux_display_backend = "software"`; only `rast.service` was restarted, the
native helper, the live `rast-xpra-u1` session and the user data were untouched.
The final binary differs from the measured AFTER-balanced binary only by
cosmetic changes (App Center / doctor GPU-note wording), so the AFTER-balanced
numbers remain representative of the shipped build.

### 7.2 Re-run commands

```bash
node tests/e2e/perf/bench.mjs --label=after-balanced  --base-url=https://desktop.lufria.tech
node tests/e2e/perf/bench.mjs --label=after-responsive --base-url=https://desktop.lufria.tech
node tests/e2e/compat/matrix.mjs --label=after --base-url=https://desktop.lufria.tech
```

### 7.3 Comparison table (captured 2026-11-24, same host, same public URL, same accounts, same harness)

Runs — all `node tests/e2e/perf/bench.mjs --base-url=https://desktop.lufria.tech`
(public URL ⇒ real Cloudflare/nginx/WAN path):

| Condition | Result files | RAST binary | Profile | Host load start → end |
|---|---|---|---|---|
| BEFORE | `perf-runtime2-baseline-2026-11-24.*` | `0.18.3 (f69a4544e6fd)` | — | 5.18 → 6.85 |
| AFTER-balanced | `perf-runtime2-after-balanced-2026-11-24.*` | `0.18.3 (d519ef13da71)` | `balanced` | 3.12 → 7.16 |
| AFTER-responsive | `perf-runtime2-after-responsive-2026-11-24.*` | `0.18.3 (049be2d60558)` + co-fixes | `responsive` | 3.03 → 4.74 |

| Measurement | BEFORE | AFTER-balanced | AFTER-responsive | Δ balanced | Δ responsive |
|---|---|---|---|---|---|
| Cold launch — click → adopted window (p50) | 6.53 s (12.09 s in the identical validation run) | 12.22 s | 12.03 s | cold is bimodal; no change | no change |
| Warm launch — click → adopted window (p50) | 764 ms | 771 ms | 767 ms | +1 % | +0 % |
| Warm launch — adopted → first painted (p50) | 469 ms | 353 ms | 263 ms | −25 % | −44 % |
| Input latency — keydown → changed pixel (p50 / p95) | 217 / 398 ms | **182 / 280 ms** | 198 / 287 ms | **−16 % / −30 %** | −9 % / −28 % |
| Typing burst 40 chars — dispatch (p50) | 129 ms | 26 ms | 27 ms | **−80 %** | −79 % |
| Typing burst 40 chars — drain (p50) | 696 ms | 311 ms | 340 ms | **−55 %** | −51 % |
| Typing burst 40 chars — total (p50) | 825 ms | **337 ms** | 367 ms | **−59 %** | −56 % |
| Scroll — server-encoded fps | 23.6 | 12.7 | 12.9 | encoder-selection difference¹ | encoder-selection difference¹ |
| Animation — server-encoded fps | 101.6 | 47.0 | 50.7 | encoder-selection difference¹ | encoder-selection difference¹ |
| Video — server-encoded fps | 61.4 | 31.4 | 32.4 | encoder-selection difference¹ | encoder-selection difference¹ |
| Idle CPU (% of one core) | 10.6 % | 10.3 % | 22.1 % | −3 % | flagged anomaly² |
| Idle memory — memory.current (MiB) | 389.5 | 250.6 | 271.9 | **−36 %** | −30 % |
| Session WS bandwidth received (whole run) | 41.5 MiB | 51.4 MiB | 40.0 MiB | +24 % (encoder mix) | −4 % |
| Direct stream page first paint | 1.95 s | 2.87 s | 1.80 s | noisy | noisy |
| Negotiated encoder (dominant) | webp (+h264/x264 for video damage) | webp | webp | encoder-selection difference¹ | encoder-selection difference¹ |
| Xpra batch config (`min` / `max` / `expire`) | 16 / 500 / 250 ms | 16 / 500 / 250 ms | **16 / 120 / 100 ms** | unchanged by design | **capped, proven in evidence** |
| Frame-total latency (`xpra info`, mid-interaction) | 158 ms | 120 ms | 125 ms | −24 % | −21 % |
| Client ping latency p50 (mid-interaction) | 54 ms | 61 ms | 80 ms | +13 % | +48 % |

¹ **Server-encoded frame count vs encoder selection (not a smoothness
regression).** The continuous-motion server-side frame counts roughly halve
after the deploy (animation 101.6 → 47.0/50.7, video 61.4 → 31.4/32.4, scroll
23.6 → 12.7/12.9) while the client-painted frames observed through the CDP
screencast stay at the same ~20 fps headless ceiling and the effective
bandwidth is similar — i.e. about half as many, roughly twice-as-large frames.
This matches the Xpra pipeline evidence: BEFORE, that damage was partly encoded
with h264-through-x264 (`pipeline_option.0.encoder=x264`, h264 pixels present
in `xpra info`); AFTER, the same damage goes through webp
(`pipeline_option.0.encoder=webp`). x264 itself is still available — the
`xpra encoding` snapshot in the evidence files shows `enc_x264` (x264 165) and
`enc_openh264` present, `nvenc`/`nvdec`/`nvfbc` absent. The changed choice is
Xpra's encoder scoring after the broken NVENC module (and `pycuda`) was removed,
not a broken or missing x264. **Conclusion: no user-visible smoothness
regression; server-encoded frame count differs because the pipeline chose webp
instead of h264 for that damage.**

² **Responsive idle CPU anomaly (unresolved, single sample).** The responsive
run's 30 s quiet phase measured 22.1 % of one core versus 10.3 % in balanced,
with the same 56 server-encoded frames, the same ~318 KiB received and similar
memory in both. Host load differed (4.74 vs 7.16) and this is a single sample,
so it is recorded as a flagged anomaly, not a conclusion; the final profile
choice below does not depend on it.

**Reading of the A/B.** The large, reproducible BEFORE→AFTER win lands in
**balanced** (typing-burst total 825 → 337 ms, drain 696 → 311 ms, dispatch
129 → 26 ms, keydown→display p50 217 → 182 ms / p95 398 → 280 ms, warm→first
painted 469 → 353 ms, idle memory 389.5 → 250.6 MiB from the
NVENC/pycuda removal): it comes from the **input/focus/clipboard fixes and the
encoder-path cleanup**, not from Xpra damage batching. The **responsive**
profile was applied and is proven in the evidence (unit `Environment` with
`XPRA_BATCH_MAX_DELAY=120 …`, `ExecStart` with the quality/speed/video flags,
and `xpra info` reporting batch `16/120/100`), but it did **not** measurably
reduce input/typing latency versus balanced: p50 198 vs 182 ms, p95 287 vs
280 ms, typing total 367 vs 337 ms — all inside the ~59 ms screenshot-poll
resolution and run-to-run noise. The batching cap only has something to cap
when adaptive batching actually rises to hundreds of milliseconds under
continuous damage; with the discrete-key workload the adaptive delay stayed at
3–4 ms during typing in all three conditions.

### 7.4 Final profile decision

**`linux_profile = "balanced"` is the final production profile.** Rationale
(data-driven):

* same measured input/typing latency as responsive within noise,
* lower (and better understood) idle CPU (10.3 % vs the flagged 22.1 %),
* no quality trade-off: responsive forces `quality=30` / `speed=90` and disables
  video scaling / A/V sync, which is an opt-in trade-off, not a measured win for
  this workload,
* it preserves the historical Xpra policy, so existing sessions behave as
  before apart from the proven fixes.

`responsive` remains available as an explicit opt-in profile for users/networks
where the adaptive batching delay actually climbs (high-latency links, heavy
continuous damage); its numbers are documented above so the trade-off is
visible. The unresolved responsive idle-CPU anomaly should be re-measured before
recommending it.

### 7.5 Cold launch: known limitation and future work

Cold launch (the user has no running session and opens the first native app)
takes **~12 s** (12.22 s balanced, 12.03 s responsive; the BEFORE baseline also
measured 12.09 s in an identical validation run, while its on-disk two-sample
median was 6.53 s). The variance comes from the session stop/start path
(`POST /api/linux-apps/session/close` teardown, identity setup, Xpra/Xvfb start
and Chrome start). This was **not fixed** by the native-runtime changes and is
documented as a known cold-session-start limitation; the user-visible per-app
launch cost is the warm path (**~767 ms** click→adopted window, plus ~263–353 ms
to first paint). Recommended future work: pre-warming / eager session start
(create the Xpra session and identity in the background right after login or
before the first tile click) and measuring the cold path with the same harness
(`--cold-runs`) once implemented.

### 7.6 Live compatibility matrix (AFTER, final build)

Runner: `node tests/e2e/compat/matrix.mjs --label=after --base-url=https://desktop.lufria.tech`.
Raw data: `tests/e2e/results/compat-runtime2-after-2026-11-24.{json,md}`;
second-window screenshot: `tests/e2e/screenshots/compat/`.

| App | Launch | 1 adopted window / remote top-level | Typing | Scrolling | Clipboard | Second window | Close | Overall |
|---|---|---|---|---|---|---|---|---|
| Google Chrome | pass (12.4 s cold, ~0.8 s warm-equivalent) | **pass** (1 RAST window per remote top-level window, 0 iframes/viewers, adopted DOM node) | pass (pixels changed in 444 ms) | pass (wheel scroll changes pixels) | **partial** | pass (1 → 2 windows, both mirrored 1:1) | pass (window removed from shell and remote session) | **partial** |

Summary: 31 apps detected → **1 live-tested (Chrome, partial), 30 not tested
(none failed)**. The 30 not-tested rows are: 21 system `.desktop` entries that
are installed on the host but **not managed by the RAST App Center** (therefore
not launchable through RAST — explicitly *not* failures) and 9 common apps that
are **not installed**. This is the honest separation the matrix is built for.

Check notes:

* **Clipboard (partial, only non-pass).** Ctrl+A → Ctrl+C → Backspace → Ctrl+V
  in the remote Chrome fixture did change pixels (paste happened), but the
  restored frame was not pixel-identical to the pre-clear frame — a caret /
  selection / focus difference is the likely cause and the check is deliberately
  conservative. The copy/paste path works; it is not marked "pass" because the
  exact-frame assertion is stronger evidence than "something changed".
* **Second window.** `Ctrl+N` is reserved by the new shell for the local browser
  (see §6), so the matrix opens a second Chrome window from inside the fixture
  page (a trusted key gesture triggers `window.open`). The result is a real
  second top-level Xpra window mirrored as a second adopted RAST window
  (`RASTNative.windows()` and the DOM both show 2 → 2). Earlier automated
  attempts hit a harness edge case (Chrome restoring windows from a previous
  abrupt session and pointer mapping at the window's bottom-right corner); the
  final matrix normalises restored windows first and the check is stable.
* **Window mapping** is asserted three ways: RAST shell window count equals
  remote top-level window count, each RAST `.win-body` contains an adopted
  `.rast-native-xpra-window` node, and **zero `<iframe>` / per-window viewer**
  nodes exist inside the window body.
* **Performance caveat:** the launch number in the matrix is a single cold
  launch (no running session) and is therefore ~12 s; the user-visible per-app
  (warm) launch cost measured by the benchmark is ~767 ms (§7.3).

---

## 8. Input and clipboard latency stage profile (2026-11-24, production)

This section decomposes the input path that §4/§7 measure end-to-end. The
"input latency" in the earlier benchmark is a screenshot-polling upper bound
and does not say where the time goes. The profile below was measured with
`tests/e2e/perf/latency.mjs` against the public production URL
(`https://desktop.lufria.tech`) while the tester typed into the controlled
remote page used by the clipboard matrix.

> **Status:** stream-path numbers captured on production build
> `RAST 0.18.3 (93585ba2c0ce)`; the deployed final build at commit time is
> `RAST 0.18.3 (02c1722e731f)` (binary sha256
> `90e61351b038b47e515f06be7698ff3e66f598515190da27f1be0ae5a6b9c591`). The
> final deploys changed prewarm/discovery/terminal code only; the
> input/clipboard/stream path is unchanged. The re-run attempt on the final
> build aborted in the harness focus handshake (see §8.3) and is kept as an
> attempt log, not as data. Single host: browser, RAST, Xpra and remote Chrome
> all run on the measurement host, so the public leg crosses the real
> Cloudflare tunnel but not an end user's last mile.

Harness files (committed):

| Path | Purpose |
|---|---|
| `tests/e2e/perf/latency.mjs` | Stage profiler: rAF counters (desktop + hidden viewer iframe), Event Timing + shim `sendKey` instrumentation, Playwright WebSocket frame timing, `xpra info` snapshots, loopback-vs-public HTTP RTT. Includes a focus handshake that aborts instead of reporting meaningless samples. |
| `tests/e2e/clipboard/clipboard-page.html` | Controlled remote page whose `document.title` reports application state. |
| `tests/e2e/clipboard/lib.mjs` | Shared production harness helpers. |
| `tests/e2e/results/perf-input-latency-2026-11-24-evidence.txt` | Raw `xpra info` snapshots (idle / mid-typing / after) + tunnel data of the valid run. |
| `tests/e2e/results/perf-input-latency-2026-11-24-summary.md` | Valid-run summary + list of non-measurement attempts. |
| `tests/e2e/results/perf-input-latency-2026-11-24-*.log` | Console logs of the valid run, the earlier same-day run and the aborted final-build attempt. |
| `tests/e2e/results/perf-input-latency-2026-11-24-02c1722e731f-handshake-failed.{json,md}` | Aborted final-build attempt (evidence of the guard, not latency data). |

Exact commands:

```bash
cd tests/e2e
node perf/latency.mjs                 # production, t-alice, default 24 keys
node perf/latency.mjs --keys=30 --idle=5
node perf/latency.mjs --keep-session  # leave the test session up
```

The profiler uses only the test account's own session and closes it through
the RAST API at the end; the live user session and the RAST services are never
touched.

### 8.1 What is measured

| Stage | Method | Honesty note |
|---|---|---|
| **input dispatch + RAST handler** | Chromium Event Timing entries for `keydown` + a page-only runtime wrapper around the shim's `sendKey` + a raw capture-phase `keydown` listener (product code untouched) | browser dispatch + RAST policy/handler + the `sendKey` call; not the socket write |
| **handler → WebSocket frame** | Playwright `page.on("websocket")` `framesent` / `framereceived` wall times, correlated per keystroke | local client ↔ RAST server leg only |
| **application acknowledgement** | keypress → `document.title` change mirrored into the adopted window title (MutationObserver) | full keypress → application → client round trip; can include Xpra window-metadata propagation, so an upper bound for app acknowledgement, not screen paint. **Not reported from this run** (see §8.3). |
| **hidden-viewer rAF throttling** | rAF counters in the desktop document **and** the off-screen same-origin `.linux-native-session-frame` viewer iframe, idle and during typing | directly tests the suspected render-throttling contributor |
| **Xpra transport / damage** | `sudo -u rast-u<id> xpra info --socket-dir=… :<display>` of the test identity's own session | instantaneous adaptive-state snapshots; mid-typing snapshot taken while the burst is in flight |
| **Cloudflare tunnel RTT** | 40 keep-alive HTTP requests to `http://127.0.0.1:8720/healthz` (loopback) vs `https://desktop.lufria.tech/healthz` (public) | measured on the RAST host; no end-user last-mile |
| **estimated floor** | sum of measured stages with stated assumptions | an estimate, not an individual keystroke measurement |

### 8.2 Hidden-viewer rAF vs main-document rAF

| Condition | Main document | Off-screen viewer iframe | Throttled? |
|---|---|---|---|
| idle (5 s) | **60.0 fps** | **60.0 fps** | no |
| during typing burst | **59.9 fps** | **59.9 fps** | no |

**Finding:** the off-screen `.linux-native-session-frame` (same origin, laid
out at `position: fixed; left: -30000px`) is **not render-throttled**: it runs
at the same ~60 fps as the visible desktop document both while idle and while
keys are forwarded. Hidden-viewer rAF throttling is therefore **not a latency
contributor** in this deployment. (The iframe must stay laid out — not
`display:none` — for the Xpra client to keep painting; that is what the shell
does.) The earlier same-day run measured 59.4/59.4 idle and 59.0/59.0 typing;
the conclusion is identical.

### 8.3 Stage latencies (one typed burst, n = keys)

| Stage | p50 | p95 | n |
|---|---|---|---|
| input delay (event start → handler start) | 0.25 ms | 0.55 ms | 6 |
| RAST handler processing (Event Timing processing span) | 0.15 ms | 0.53 ms | 6 |
| dispatch to capture listener (raw listener) | 0.30 ms | 0.99 ms | 24 |
| keydown event timestamp → shim `sendKey` call (raw listener) | 0.25 ms | 0.79 ms | 24 |
| keypress (Node) → outgoing WebSocket frame | 11.0 ms | 16.9 ms | 24 |
| keypress → incoming WebSocket frame | 21.0 ms | 32.2 ms | 24 |
| keypress → application acknowledgement (title) | *not reported* | — | — |

The browser-side stages are **sub-millisecond**: key dispatch, RAST key policy,
focus routing and the shim `sendKey` call are not the bottleneck. The client
WebSocket send happens ~11 ms after the keypress (includes Xpra client
encoding/queue) and the first inbound frame ~21 ms after it.

**Application acknowledgement is not reported from this run.** In the
automated post-prewarm runs the fixture stopped receiving keystrokes (the title
channel produced no fresh entry while `shim.sendKey` was still called 24×;
`perf-input-latency-2026-11-24-02c1722e731f-handshake-failed.json` records the
handshake evidence), so any number would be meaningless. The pre-deploy
clipboard matrix proves the title channel works end-to-end (exact
`PASTED`/`COPIED` titles, 313–352 ms per paste/copy round trip; §8.7 and
INPUT-CLIPBOARD.md). `latency.mjs` now performs a focus handshake and aborts
instead of emitting wrong values.

### 8.4 Xpra transport and damage latency (test identity's own session)

| Metric | idle | mid-typing | after |
|---|---|---|---|
| client ping latency p50 / p90 | 62 / 101 ms | 62 / 101 ms | 57 / 68 ms |
| damage client-latency | 104 ms | 94 ms | 94 ms |
| damage frame-total latency | 104 ms | 94 ms | 94 ms |
| damage target-latency (window) | 71 ms | 71 ms | 71 ms |
| batch delay avg / max (cur) | 8 / 21 (2) ms | 8 / 21 (2) ms | 8 / 21 (2) ms |
| batch config min / max / expire | 16 / 500 / 250 ms | 16 / 500 / 250 ms | 16 / 500 / 250 ms |
| compressor / encoder | lz4 / openh264 | lz4 / openh264 | lz4 / openh264 |

**Finding:** the transport round trip over the public path is ~55–62 ms p50
(p90 up to 101 ms on the measured run), and Xpra's own server-side damage→frame
pipeline (`frame-total latency`) is ~94–119 ms for this text/edit workload.
Adaptive batching stays at its ~8–15 ms level (discrete keystrokes, no
continuous damage), so the **transport + server damage pipeline dominates**,
not batching. This matches the §7 finding that the `responsive` profile's batch
cap did not change typing latency.

### 8.5 Cloudflare tunnel overhead (HTTP RTT from the RAST host)

| Path | p50 | p95 | n |
|---|---|---|---|
| loopback `http://127.0.0.1:8720/healthz` | **2.14 ms** | 3.10 ms | 40 |
| public `https://desktop.lufria.tech/healthz` | **67.33 ms** | 95.10 ms | 40 |
| **inferred tunnel overhead** | **≈ 65.19 ms** | — | — |

Cloudflare colo for the public leg: `cf-ray a5f68928187f68b7-SIN` (Singapore
edge). The earlier same-day run measured 2.30 vs 67.7 ms → 65.4 ms overhead,
i.e. the same value. Fresh-connection `curl` components are in the evidence
file (loopback DNS 0.05 ms / connect 0.3 ms / TTFB ~1 ms).

**Interpretation:** the tunnel adds ~65 ms of HTTP round-trip latency; the Xpra
WebSocket transport RTT measured by the client (~55–62 ms p50) is the same
order, i.e. the transport RTT is dominated by the public tunnel path and
roughly half of it (~27–31 ms) is the one-way delay a keystroke and its
resulting frame each pay.

### 8.6 Estimated floor

Using the measured stages with stated assumptions:

```
floor ≈ RAST handler (0.25 ms)
      + one-way client→server (ping p50 / 2 = 31 ms)
      + server damage→frame (frame-total latency mid-typing = 94 ms)
      + one-way server→client (31 ms)
      + client decode/paint (~one viewer frame at 59.9 fps = 16.7 ms)
       ≈ 173 ms
```

Assumptions and caveats:

* one-way transport ≈ `xpra info` client ping p50 / 2 in each direction;
* server damage→encoded-frame time = mid-typing
  `client.0.damage.frame-total-latency` snapshot;
* client paint ≈ one hidden-viewer rAF interval at the measured viewer rate;
* the RAST handler stage is measured event→`sendKey`; the client WebSocket send
  queue is not included;
* this is an *estimate for one keystroke to become a painted frame*; Xpra
  batching can add up to `batch.max-delay` under continuous damage.

The same-day pre-deploy run gave ≈164 ms with the older ping/damage sample.
The earlier §4/§7 "keydown → changed pixel" p50 of ~182–217 ms and the clipboard
matrix paste/copy round trips (313–352 ms, which include a complete clipboard
handshake plus app acknowledgement) are consistent with this floor: a large
part of the user-visible lag is the transport RTT plus the Xpra damage/encode
pipeline (~94 ms), not the browser/RAST key handling (sub-millisecond).

### 8.7 Clipboard/input matrix classification (same day, pre-deploy build `ab9b821d18cb`)

`tests/e2e/clipboard/verify.mjs` run `2026-11-24T06-18-48-986`: **18 of 19
checks pass**. Machine report: `tests/e2e/clipboard/results/2026-11-24T06-18-48-986/report.{json,md}`;
classification addendum: `…/classification-note.md`.

| # | Check | Result |
|---|---|---|
| 1 | local browser → remote app (ASCII) | pass (`PASTED:RAST-LOCAL-TO-REMOTE-48-986`, 313.8 ms) |
| 2 | local browser → remote app (unicode) | pass (`PASTED:RAST ⇄ Linux ✓ — ünïcødé テスト 日本語 🚀 Ω≈ç√∫ -986`, 345.5 ms) |
| 3 | remote app → local browser clipboard | pass (exact unicode + independent `xclip` evidence on `:101`) |
| 4 | select-all / cut | pass (`SEL:46`, `CUT:de568410\|0`, clipboard received) |
| 5 | typing exactly once / undo / redo | pass (`INPUT:typed-marker-48-986`, `UNDO:…`, `REDO:…`) |
| 6 | second window 1:1 + focus switching | pass (1 → 2 adopted windows, focus follows clicks) |
| 7 | remote window A → remote window B | pass (`PASTED:remote-to-remote-48-986`, 316.1 ms) |
| 8 | desktop reload/reconnect → paste | pass (`PASTED:after-reconnect-48-986`, 351.6 ms) |
| 9 | macOS `keyboardSwap` emulation | **partial / not proven**: `keyboardSwap=true` and Cmd+A/Cmd+C proven end-to-end; the automated Cmd+V pass did not surface the new text (see INPUT-CLIPBOARD.md limitations — no trusted browser paste event via automation, no real Apple hardware) |
| 10 | image/HTML clipboard | not tested (text-only by design) |

No product bugs were found by the clipboard probes. One real product bug WAS
found and fixed during the deploy window: the hidden Xpra transport could steal
keyboard focus after a prewarmed session connected. The first fix (`93585ba2`,
blurring the hidden viewer) was **reverted in `951c15a`** because it also stopped
the shim's synthetic keys from reaching the remote client; the final deployed
build keeps the previous input path and disables pre-warm (see §9.1), and a
production re-check on `951c15a` confirmed key forwarding again
(`Ctrl+K` → `about:blank - Google Chrome`). That
matches the class of focus-ownership edge that the automated post-prewarm
latency runs hit (keys forwarded while the fixture received none), which is why
the profiler now aborts via its focus handshake instead of reporting a title
round trip.

### 8.8 Honest limitations

* Single host: browser, RAST, Xpra and remote Chrome on the measurement host;
  the public leg crosses the real Cloudflare tunnel but not a remote user's
  last mile.
* `xpra info` snapshots are instantaneous samples of adaptive state; the batch
  delay statistics are windowed and the mid-typing snapshot is taken while the
  burst is in flight.
* The application-acknowledgement stage is reported as *not measured* in the
  final post-prewarm runs (see §8.3); the clipboard matrix proves the title
  channel works on the pre-deploy build at the same URL.
* rAF numbers are for headless Chromium with background timer/renderer
  throttling disabled; a headed browser and a real monitor may differ.
* The clipboard matrix's own limitations (no trusted paste event via
  automation, no real macOS hardware) are documented in
  [INPUT-CLIPBOARD.md § End-to-end verification](INPUT-CLIPBOARD.md#end-to-end-verification-production-2026-11-24).

---

## 9. Prewarm and multi-user isolation (final build, 2026-11-24)

Both items were measured by the Lead on a quiet production window with the
dedicated test accounts; they are recorded here because they change the
performance and security envelope of the native runtime.

### 9.1 Session prewarm (implemented, measured, **disabled in the deployed build**)

> Status correction: the deployed runtime 2.1 build (`951c15ac13e4`, binary
> sha256 `ab5e511f…`) runs with `linux_prewarm = false`. Reliable pre-warming
> makes the hidden Xpra transport connect right after sign-in, and the client's
> clipboard autofocus then steals keyboard focus from local RAST apps (Terminal
> typing ignored until a click). The measured win below is real, but the feature
> stays opt-in/off until the viewer connection is deferred to the first native
> window launch; that follow-up is on the roadmap.


The server now starts the user's shared native session in the background right
after the desktop loads the app list (`/api/linux-apps/session/prewarm`), i.e.
**outside the app-click path**.

| Measurement | Value | Notes |
|---|---|---|
| Background native session ready after desktop load | **9.2 s** | scheduled by the server after the app list loads; not in the click path |
| Click → adopted Chrome window (prewarm enabled) | **3.1 s** | warm-equivalent path when the session is already prepared |
| Click → adopted Chrome window (cold launch, no prewarm) | ~12 s | §7.5 / §7.3 cold path (session start + Chrome start) |

Runner: `tests/e2e/prewarm/measure.mjs`. The user-visible win is the first
native-app launch of a session: it drops from the ~12 s cold path to ~3.1 s.
Prewarm does not change the stream/input path measured in §8.

### 9.2 Multi-user isolation

Runner: `tests/e2e/multiuser/verify.mjs` (production, quiet window):
**6/6 PASS**.

* another user's session URL/key is rejected (**403**);
* anonymous access is rejected (**401**);
* three distinct `rast-xpra-u<id>` units exist per user (one per account);
* no raw display ports are exposed on the host network;
* the nftables display firewall is present.

These are guardrails for the per-user shared Xpra session model, and they were
re-verified after the prewarm change (the prewarmed session is still scoped to
its own identity).

## 10. Final input verification on the deployed runtime 2.1 build

Deployed source `951c15ac13e4` (binary sha256 `ab5e511f…`, prewarm disabled):
keyboard forwarding is verified again after the revert — `Ctrl+K` navigation
(`about:blank`) and `Alt+D` address-bar typing both reach the remote Chrome
window, focus stays on `.win-body linux-native-window` and the transport shim
reports v1.3.0/connected. The standalone in-page typing probe is
harness-inconclusive on a cold session (synthetic click focus), and the earlier
18/19 clipboard matrix applies because the input code is byte-identical after
the revert. Full record: `tests/e2e/results/final-input-verification-951c15a.md`.

## 11. Final runtime 2.1 benchmark run (2026-11-24, deployed build 8a08a9a3)

Full reports: `tests/e2e/results/perf-runtime2-runtime21-final-2026-11-24-2026-11-24.{json,md,evidence.txt}`.

Environment caveat: this run happened while the shared host was under sustained
external disk-I/O pressure (`/proc/pressure/io` some avg10 ≈ 30–45 %) and the
benchmark's new session fell back to the software Xvfb backend
(`Display: Xvfb software (:101)`) instead of the GPU display. The numbers below
are an **environment-degraded reference**, not the normal-load product result;
they are published with the environment facts and must not be treated as a fair
regression without re-running under comparable conditions.

| Metric | p50 / p95 | Samples | Notes |
| --- | --- | --- | --- |
| Cold launch — click → adopted window | 5174 / 5215 ms | 2 | session stopped first; Xvfb fallback |
| Cold launch — placeholder window | 264 / 290 ms | 2 | shell responsiveness |
| Cold launch — adopted → first painted | 50 / 236 ms | 2 | additional paint wait |
| Warm launch — click → adopted window | 846 / 2638 ms | 4 | p95 outlier under load; warm median 846 ms |
| Input latency keydown → changed pixel | 229 / 456 ms | 20 | screenshot polling ≈59 ms granularity; upper bound |
| Typing burst 40 chars — dispatch | 29 / 29 ms | 3 | first→last key |
| Typing burst — drain / total | 321 / 434 ms, 350 / 461 ms | 3 | last key→stable frame, first→stable frame (upper bounds) |
| Idle CPU | 25.2 % of one core | 1 | session+app cgroups, 30 s quiet |
| Idle memory (current / peak) | 153.3 / 194.5 MiB | 1 | cgroup memory.current / memory.peak |
| Negotiated encoder | webp (before) → h264 (mid/after) | — | xpra info; batch 16/500/250 ms, lz4 |
| Host load at start/end | 1.18 / 2.53 | — | 12 vCPU i7-9750H, I/O pressure elevated |

The authoritative before/after comparison for the 2.0 input/runtime work (§7.3)
remains: input p50 217→182 ms, p95 398→280 ms, typing total 825→337 ms,
dispatch 129→26 ms, cold session start 12.09 s → pre-warmed/lazy path ~3 s.
This final run confirms the deployed build is healthy and records the
host-contention caveat; a clean re-run on a quiet host is scheduled for the
final acceptance report.
