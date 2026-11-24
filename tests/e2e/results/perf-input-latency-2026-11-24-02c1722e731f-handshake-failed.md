# RAST input-latency stage profile — 2026-11-24

- Target: https://desktop.lufria.tech (public HTTPS URL ⇒ real Cloudflare tunnel → nginx → RAST)
- Test user: t-alice; session: `{"id":"native","status":"ready","display":103,"profile":"balanced","displayBackend":"xvfb","startedAt":1795503844931,"apps":[{"appId":"google-chrome","status":"running","runId":"b116ce570466d6b1"}],"viewers":1}`
- Controlled remote page: http://127.0.0.1:8798/clipboard-page.html?run=2026-11-24T07-04-03-261&role=A
- Host: {"release":"RAST 0.18.3 (02c1722e731f)","xpraVersion":"xpra v6.5.5-r0","chromeVersion":"Google Chrome 158.0.8127.36","chromiumExecutable":"/home/lufria/.cache/ms-playwright/chromium-1187/chrome-linux/chrome","cpus":"12","loadAverageStart":"3.97 4.58 4.40 2/1524 127123"}

## Stage latencies (per keystroke, n = keys typed in one burst)

| Stage | p50 | p95 | n |
|---|---|---|---|
| keypress → outgoing WebSocket frame | n/a ms | n/a ms | 0 |
| keypress → incoming WebSocket frame | n/a ms | n/a ms | 0 |
| keypress → application acknowledgement (title) | n/a ms | n/a ms | 0 |

### Focus handshake (input really reaches the fixture)

| click point | activeElement | title after probe key | ok |
|---|---|---|---|
| {} | win-body linux-native-window | "READY:A - Google Chrome" | false |
| {"fy":0.75} | win-body linux-native-window | "READY:A - Google Chrome" | false |
| {"fy":0.25,"fx":0.25} | win-body linux-native-window | "READY:A - Google Chrome" | false |
| {"fy":0.75,"fx":0.75} | win-body linux-native-window | "READY:A - Google Chrome" | false |

## Hidden-viewer vs main-document rAF

| Condition | main document | off-screen viewer iframe | viewer available |
|---|---|---|---|
| idle (5 s) | 59.8 fps | 59.8 fps | true |

## Xpra transport / damage (test identity's own session)

| Metric | idle | mid-typing | after |
|---|---|---|---|
| client ping latency p50 | 57.0 | n/a | n/a |
| client ping latency p90 | 57.0 | n/a | n/a |
| damage client-latency | 73.0 | n/a | n/a |
| damage frame-total latency | 73.0 | n/a | n/a |
| damage target-latency | n/a | n/a | n/a |
| batch delay avg / max (cur) | 4.00 / 9.00 (2.00) | n/a | n/a |
| batch config min / max / expire | 16.0 / 500.0 / 250.0 | n/a | n/a |

## Cloudflare tunnel overhead (HTTP RTT from the RAST host)

| Path | p50 | p95 | n |
|---|---|---|---|
| loopback (http://127.0.0.1:8720/healthz) | n/a ms | n/a ms | 0 |
| public (https://desktop.lufria.tech/healthz) | n/a ms | n/a ms | 0 |
| inferred tunnel overhead | n/a ms | — | — |

Cloudflare colo (cf-ray): n/a

### curl components (new connection; ms)

| Path | DNS | TCP connect | TLS | TTFB | total |
|---|---|---|---|---|---|
| loopback | n/a | n/a | n/a | n/a | n/a |
| public | n/a | n/a | n/a | n/a | n/a |

## Estimated floor (estimate, see assumptions)

n/a (missing xpra ping/damage fields)

- 

## Observations

- focus-handshake: the fixture did not receive a probe key in any attempt; latency samples would be meaningless

## Limitations


