# Quick backend performance — native

Generated 2026-11-12T23:18:28.421Z; app chrome; server http://127.0.0.1:8732.

| Measurement | Value |
|---|---|
| Cold launch — launch API response | 8.89 s |
| Cold launch — backend ready (stream URL available) | 8.90 s |
| Cold launch — first painted frame after opening the stream page | 0.70 s |
| Warm launch — launch API response | 0.00 s |
| Warm launch — backend ready | 0.00 s |
| Warm launch kept the same session | true |
| Idle memory after 60 s | 207.7 MiB |
| Active memory during 10 s interaction | 209.5 MiB |
| CPU during 10 s interaction | 5.7 % of one core |
| Input: key press → first page pixel | 112.0 ms (samples [112]) |
| Two-user combined memory | 631.3 MiB |
| Two-user CPU during interaction | 10.6 % |
| Two-user open (both ready) | 8.89 s |
| Session unit memory (systemctl MemoryCurrent / MemoryPeak) | 207.7 MiB / 270.0 MiB |

Method: see tests/e2e/results/README.md (quick backend variant: API launch + streamed page directly, desktop shell excluded).
