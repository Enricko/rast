# RAST E2E scenario matrix — docker backend

Generated 2026-11-12T21:36:12.219Z against http://127.0.0.1:8731

| Scenario | Result | Duration | Evidence | Notes |
|---|---|---|---|---|
| s0 — Smoke: API health, browser login and desktop shell | **PASS** | 2.4 s | `/home/lufria/RAST/tests/e2e/screenshots/docker/s0/desktop-after-login.png`<br>`/home/lufria/RAST/tests/e2e/screenshots/docker/s0/app-center.png` | logged in through the form as admin (windows: 0)<br>App Center opened<br>bootstrap: RAST 0.12.0 (authenticated=false) |
| a — Admin installs a compatible .deb and launches it | **FAIL** | 19.1 s | `/home/lufria/RAST/tests/e2e/screenshots/docker/a/app-center-lists-app.png` | page.waitForTimeout: Target page, context or browser has been closed
    at Module.waitForWindows (/home/lufria/RAST/tests/e2e/lib/browser.mjs:190:16)
    at async Object.run (/home/lufria/RAST/tests/e2e/scenarios/a-install-deb.mjs:46:21)
    at async main (/home/lufria/RAST/tests/e2e/run.mjs:227:7) |
| b — Admin installs a portable archive and launches it | **FAIL** | 6 ms |  | browser.newContext: Target page, context or browser has been closed
    at async newDesktopSession (/home/lufria/RAST/tests/e2e/lib/browser.mjs:45:19)
    at async Module.openDesktop (/home/lufria/RAST/tests/e2e/lib/browser.mjs:98:19)
    at async Object.run (/home/lufria/RAST/tests/e2e/scenarios/b- |
| c — Same user opens Chrome and another GUI app (shared session) | **FAIL** | 6 ms |  | browser.newContext: Target page, context or browser has been closed
    at async newDesktopSession (/home/lufria/RAST/tests/e2e/lib/browser.mjs:45:19)
    at async Module.openDesktop (/home/lufria/RAST/tests/e2e/lib/browser.mjs:98:19)
    at async Object.run (/home/lufria/RAST/tests/e2e/scenarios/c- |
| d — Three Chrome windows: taskbar entries, focus, resize, close | **FAIL** | 3 ms |  | browser.newContext: Target page, context or browser has been closed
    at async newDesktopSession (/home/lufria/RAST/tests/e2e/lib/browser.mjs:45:19)
    at async Module.openDesktop (/home/lufria/RAST/tests/e2e/lib/browser.mjs:98:19)
    at async Object.run (/home/lufria/RAST/tests/e2e/scenarios/d- |
| e — Two users use Chrome concurrently with separate data | **FAIL** | 3 ms |  | browser.newContext: Target page, context or browser has been closed
    at async newDesktopSession (/home/lufria/RAST/tests/e2e/lib/browser.mjs:45:19)
    at async Module.openDesktop (/home/lufria/RAST/tests/e2e/lib/browser.mjs:98:19)
    at async Object.run (/home/lufria/RAST/tests/e2e/scenarios/e- |
| f — Concurrent launch requests reuse the intended session | **FAIL** | 13.1 s |  | browser.newContext: Target page, context or browser has been closed
    at async newDesktopSession (/home/lufria/RAST/tests/e2e/lib/browser.mjs:45:19)
    at async Module.openDesktop (/home/lufria/RAST/tests/e2e/lib/browser.mjs:98:19)
    at async Object.run (/home/lufria/RAST/tests/e2e/scenarios/f- |
| g — Browser refresh reconnects without duplicating processes | **FAIL** | 4 ms |  | browser.newContext: Target page, context or browser has been closed
    at async newDesktopSession (/home/lufria/RAST/tests/e2e/lib/browser.mjs:45:19)
    at async Module.openDesktop (/home/lufria/RAST/tests/e2e/lib/browser.mjs:98:19)
    at async Object.run (/home/lufria/RAST/tests/e2e/scenarios/g- |
| h — Closing one app preserves the other apps | **FAIL** | 5 ms |  | browser.newContext: Target page, context or browser has been closed
    at async newDesktopSession (/home/lufria/RAST/tests/e2e/lib/browser.mjs:45:19)
    at async Module.openDesktop (/home/lufria/RAST/tests/e2e/lib/browser.mjs:98:19)
    at async Object.run (/home/lufria/RAST/tests/e2e/scenarios/h- |
| i — Idle cleanup stops the session after use | **FAIL** | 3 ms |  | browser.newContext: Target page, context or browser has been closed
    at async newDesktopSession (/home/lufria/RAST/tests/e2e/lib/browser.mjs:45:19)
    at async Module.openDesktop (/home/lufria/RAST/tests/e2e/lib/browser.mjs:98:19)
    at async Object.run (/home/lufria/RAST/tests/e2e/scenarios/i- |
| j — Service restart recovery | **FAIL** | 3 ms |  | browser.newContext: Target page, context or browser has been closed
    at async newDesktopSession (/home/lufria/RAST/tests/e2e/lib/browser.mjs:45:19)
    at async Module.openDesktop (/home/lufria/RAST/tests/e2e/lib/browser.mjs:98:19)
    at async Object.run (/home/lufria/RAST/tests/e2e/scenarios/j- |
| k — Failed install and update leave recoverable state | **FAIL** | 1m 57s |  | browser.newContext: Target page, context or browser has been closed
    at async newDesktopSession (/home/lufria/RAST/tests/e2e/lib/browser.mjs:45:19)
    at async Module.openDesktop (/home/lufria/RAST/tests/e2e/lib/browser.mjs:98:19)
    at async Object.run (/home/lufria/RAST/tests/e2e/scenarios/k- |
| l — Uninstall respects shared dependencies and user data choice | **FAIL** | 8 ms |  | browser.newContext: Target page, context or browser has been closed
    at async newDesktopSession (/home/lufria/RAST/tests/e2e/lib/browser.mjs:45:19)
    at async Module.openDesktop (/home/lufria/RAST/tests/e2e/lib/browser.mjs:98:19)
    at async Object.run (/home/lufria/RAST/tests/e2e/scenarios/l- |
| m — Docker app data migrates or reports a precise compatibility failure | **FAIL** | 8 ms |  | browser.newContext: Target page, context or browser has been closed
    at async newDesktopSession (/home/lufria/RAST/tests/e2e/lib/browser.mjs:45:19)
    at async Module.openDesktop (/home/lufria/RAST/tests/e2e/lib/browser.mjs:98:19)
    at async Object.run (/home/lufria/RAST/tests/e2e/scenarios/m- |

Totals: 1 passed, 13 failed, 0 skipped.

