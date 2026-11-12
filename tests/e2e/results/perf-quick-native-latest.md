# Quick backend performance — native

Generated 2026-11-12T23:07:20.779Z; app chrome; server http://127.0.0.1:8732.

| Measurement | Value |
|---|---|
| Cold launch — launch API response | 8.89 s |
| Cold launch — backend ready (stream URL available) | 8.89 s |
| Cold launch — first painted frame after opening the stream page | n/a |
| Warm launch — launch API response | 0.00 s |
| Warm launch — backend ready | 0.00 s |
| Warm launch kept the same session | true |
| Idle memory after 60 s | 189.6 MiB |
| Active memory during 10 s interaction | 199.5 MiB |
| CPU during 10 s interaction | 10.3 % of one core |
| Input: key press → first page pixel | 187.0 ms (samples [108,266]) |
| Two-user combined memory | 392.5 MiB |
| Two-user CPU during interaction | 20.7 % |
| Two-user open (both ready) | 9.89 s |
| Session unit memory (systemctl MemoryCurrent / MemoryPeak) | 189.6 MiB / 271.6 MiB |

Method: see tests/e2e/results/README.md (quick backend variant: API launch + streamed page directly, desktop shell excluded).
