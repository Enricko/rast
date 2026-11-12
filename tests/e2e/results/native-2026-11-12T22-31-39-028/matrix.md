# RAST E2E scenario matrix — native backend

Generated 2026-11-12T22:34:40.146Z against http://127.0.0.1:8732

| Scenario | Result | Duration | Evidence | Notes |
|---|---|---|---|---|
| d — Three Chrome windows: taskbar entries, focus, resize, close | **FAIL** | 2m 1s |  | Error: timed out waiting for a mirrored Chrome window; windows now: []
    at Module.waitForWindows (file:///home/lufria/RAST/tests/e2e/lib/browser.mjs:186:13)
    at async openChromeWindowFromInside (file:///home/lufria/RAST/tests/e2e/scenarios/d-chrome-windows.mjs:22:19)
    at async Object.run (f |
| e — Two users use Chrome concurrently with separate data | **FAIL** | 58.4 s |  | TypeError: fetch failed |
| h — Closing one app preserves the other apps | **FAIL** | 3 ms |  | TypeError: fetch failed |
| i — Idle cleanup stops the session after use | **FAIL** | 2 ms |  | TypeError: fetch failed |
| j — Service restart recovery | **FAIL** | 1 ms |  | TypeError: fetch failed |
| k — Failed install and update leave recoverable state | **FAIL** | 2 ms |  | TypeError: fetch failed |
| l — Uninstall respects shared dependencies and user data choice | **FAIL** | 1 ms |  | TypeError: fetch failed |
| m — Docker app data migrates or reports a precise compatibility failure | **FAIL** | 1 ms |  | TypeError: fetch failed |

Totals: 0 passed, 8 failed, 0 skipped.

