# Quick backend performance — docker

Generated 2026-11-12T22:37:58.132Z; app chrome; server http://127.0.0.1:8731.

| Measurement | Value |
|---|---|
| Cold launch — launch API response | 0.02 s |
| Cold launch — backend ready (stream URL available) | 14.82 s |
| Cold launch — first painted frame after opening the stream page | 0.35 s |
| Warm launch — launch API response | 0.01 s |
| Warm launch — backend ready | 0.01 s |
| Warm launch kept the same session | true |
| Idle memory after 60 s | 464.4 MiB |
| Active memory during 10 s interaction | 501.5 MiB |
| CPU during 10 s interaction | 76.1 % of one core |
| Input: key press → first page pixel | 104.0 ms (samples [111,104,86]) |
| Two-user combined memory | 1236.4 MiB |
| Two-user CPU during interaction | 326.6 % |
| Two-user open (both ready) | 49.34 s |

Method: see tests/e2e/results/README.md (quick backend variant: API launch + streamed page directly, desktop shell excluded).
