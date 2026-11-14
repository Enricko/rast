# Quick backend performance — native

Generated 2026-11-14T09:54:59.305Z; app chrome; server http://127.0.0.1:8742.

| Measurement | Value |
|---|---|
| Cold launch — launch API response | 9.15 s |
| Cold launch — backend ready (stream URL available) | 9.15 s |
| Cold launch — first painted frame after opening the stream page | n/a |
| Warm launch — launch API response | 0.01 s |
| Warm launch — backend ready | 0.01 s |
| Warm launch kept the same session | true |
| Idle memory after 60 s | 194.8 MiB |
| Active memory during 10 s interaction | 197.7 MiB |
| CPU during 10 s interaction | 15.9 % of one core |
| Input: key press → first page pixel | 108.0 ms (samples [122,108,82]) |
| Two-user combined memory | 394.1 MiB |
| Two-user CPU during interaction | 21.2 % |
| Two-user open (both ready) | 8.95 s |
| Session unit memory (systemctl MemoryCurrent / MemoryPeak) | 194.8 MiB / 270.6 MiB |

Method: see tests/e2e/results/README.md (quick backend variant: API launch + streamed page directly, desktop shell excluded).
