# RAST E2E scenario matrix — native backend

Generated 2026-11-12T23:18:51.467Z against http://127.0.0.1:8732

| Scenario | Result | Duration | Evidence | Notes |
|---|---|---|---|---|
| d — Three Chrome windows: taskbar entries, focus, resize, close | **FAIL** | 5.4 s | `/home/lufria/RAST/tests/e2e/screenshots/native/d/three-chrome-windows.png` | Error: assertion failed: window did not resize (1462x806 -> undefinedxundefined)
    at assert (file:///home/lufria/RAST/tests/e2e/scenarios/_helpers.mjs:18:25)
    at Object.run (file:///home/lufria/RAST/tests/e2e/scenarios/d-chrome-windows.mjs:111:9)
    at async main (file:///home/lufria/RAST/tes |

Totals: 0 passed, 1 failed, 0 skipped.

