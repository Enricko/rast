# RAST app matrix: native backend, one-home home

Generated 2026-11-17T22:25:44.692Z against https://desktop.lufria.tech as `t-alice` (production, test account). RAST 1.0.1-alpha.2.

Browser: devicePixelRatio 1, viewport 1480x920.

## Checks

| App | Launcher | RAST window | Typing | Saved file in File Station | Closes |
|---|---|---|---|---|---|
| Google Chrome | **PASS** | **PASS** | **PASS** | **PASS** | **PASS** |

XFAIL = expected to fail in this mode (reason below); XPASS = passed although expected to fail.

### Notes

- Google Chrome, typing:  pixels changed while typing; the typed text is verified in the saved file (check 4)
- Google Chrome, saved:  visible after 1596 ms, typed text inside
- Google Chrome, closed:  closed (2 windows)

## Responsiveness

| App | Method | Typing p50 / p95 / max (ms) | Scroll p50 / p95 / max (ms) | Scroll fps | Drag fps | Repaint after drop (ms) | Window | Xpra |
|---|---|---|---|---|---|---|---|---|
| Google Chrome | canvas | 301.3 / 327.3 / 1000 | 732.6 / 1000 / 1000 | 1.2 | 18 | 123.8 | 740x864 | auto, dpi ? |

## Budgets

| App | Budget | Limit | Measured | Result |
|---|---|---|---|---|
| Google Chrome | typing p95, keydown to first changed pixel | <= 150 ms | 327.3 ms | FAIL |
| Google Chrome | scroll p95, wheel to first changed frame | <= 200 ms | 1000 ms | FAIL |
| Google Chrome | changed frames per second while scrolling | >= 15 fps | 1.2 fps | FAIL |
| Google Chrome | RAST window frames per second while dragging | >= 30 fps | 18 fps | FAIL |
| Google Chrome | app content repaint after the drop | <= 250 ms | 123.8 ms | PASS |

Totals: checks 5 pass, 0 fail, 0 expected fail, 0 unexpected pass, 0 skip; budgets 1 pass, 4 fail, 0 unmeasured (report). Overall: OK.

