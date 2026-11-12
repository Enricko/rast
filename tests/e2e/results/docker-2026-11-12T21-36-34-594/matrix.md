# RAST E2E scenario matrix — docker backend

Generated 2026-11-12T21:39:37.398Z against http://127.0.0.1:8731

| Scenario | Result | Duration | Evidence | Notes |
|---|---|---|---|---|
| a — Admin installs a compatible .deb and launches it | **FAIL** | 3m 1s | `/home/lufria/RAST/tests/e2e/screenshots/docker/a/app-center-lists-app.png` | Error: timed out waiting for a RAST window for Google Chrome; windows now: []
    at Module.waitForWindows (file:///home/lufria/RAST/tests/e2e/lib/browser.mjs:186:13)
    at async Object.run (file:///home/lufria/RAST/tests/e2e/scenarios/a-install-deb.mjs:46:21)
    at async main (file:///home/lufria |

Totals: 0 passed, 1 failed, 0 skipped.

