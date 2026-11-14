# Docker vs native — performance comparison

Generated 2026-11-12 from `perf-quick-docker-latest.json` (22:37 UTC) and
`perf-quick-native-latest.json` (23:07 UTC). Same host, loopback browser,
Chrome 157.0.8103.78, app `chrome`, quick backend harness (API launch + streamed
page directly, desktop shell excluded). Method in `README.md`.

| Measurement | Docker (container) | Native (shared Xpra session) | Notes |
|---|---|---|---|
| Cold launch — API response | 0.02 s | 8.89 s | native cold start includes session + first app start (identity already provisioned; a first-ever identity adds 8–17 s once) |
| Cold launch — backend ready (stream URL) | 14.82 s | 8.89 s | docker = container create/start + Selkies ready |
| Cold launch — first painted frame on the stream page | 0.35 s | n/a* | *native poll did not observe a page-pixel change within the window (Xpra page painting/stream restart); needs a re-run with the app-window fix |
| Warm launch — API response | 0.01 s | 0.00 s | |
| Warm launch — backend ready | 0.01 s | 0.00 s | both keep the session/copy alive |
| Idle memory after 60 s | 464.4 MiB | 189.6 MiB | native = session cgroup (`rast-xpra-u2`; `MemoryCurrent`/`MemoryPeak` 189.6/271.6 MiB); docker = container cgroup |
| Active memory, 10 s typing/scroll | 501.5 MiB | 199.5 MiB | |
| CPU, 10 s typing/scroll | 76.1 % of one core | 10.3 % of one core | same `cpu.stat usage_usec` delta method both sides |
| Input: keypress → first streamed pixel | 104 ms (111/104/86) | 187 ms (108/266) | upper bound (Playwright + screenshot polling) |
| Two-user combined memory | 1236.4 MiB | 392.5 MiB | |
| Two-user CPU during interaction | 326.6 % | 20.7 % | docker = 2 × ~1.6 cores; native = Xpra encode + apps |
| Two-user open (both ready) | 49.34 s | 9.89 s | |
| Session unit memory (systemctl) | n/a | 189.6 MiB current / 271.6 MiB peak | |

Caveats:
* Host load was high (other agents' builds/Xpra sessions) during the Docker
  measurements; absolute Docker numbers are conservative and the native run was
  taken ~30 min later, so treat the delta as indicative, not a controlled
  benchmark.
* Native Chrome crashed ~11 s after start in the build used for the native perf
  run (an Xpra/X-auth issue being fixed by the backend owners). The native idle
  memory therefore reflects an idle shared session with the app exited
  (session-level overhead), not a session with Chrome alive; the Docker number
  includes a live Chrome. The native number is an optimistic lower bound for
  "session + app"; re-run after the fix for the app-inclusive figure.
* Input latency on the native side used the Xpra HTML5 page: key dispatch →
  first changed page pixel; the sample set is smaller.
