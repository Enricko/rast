# RAST E2E scenario matrix — native backend

Generated 2026-11-12T23:14:45.284Z against http://127.0.0.1:8732

| Scenario | Result | Duration | Evidence | Notes |
|---|---|---|---|---|
| d — Three Chrome windows: taskbar entries, focus, resize, close | **FAIL** | 2m 10s |  | Error: timed out waiting for a mirrored Chrome window; windows now: []
    at Module.waitForWindows (file:///home/lufria/RAST/tests/e2e/lib/browser.mjs:186:13)
    at async openChromeWindowFromInside (file:///home/lufria/RAST/tests/e2e/scenarios/d-chrome-windows.mjs:22:19)
    at async Object.run (f |
| i — Idle cleanup stops the session after use | **PASS** | 54.9 s | `/home/lufria/RAST/tests/e2e/screenshots/native/i/after-idle-cleanup.png` | [install RAST Test App (portable)] uploading /home/lufria/RAST/tests/e2e/fixtures/rast-test-app-1.0.tar.gz<br>[install RAST Test App (portable)] install job 3dbf48a2-bac7-4e64-bfc9-5cd40e78e5d7<br>[install RAST Test App (portable)] job 3dbf48a2-bac7-4e64-bfc9-5cd40e78e5d7: done Installed<br>native p |

Totals: 1 passed, 1 failed, 0 skipped.

