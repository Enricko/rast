# RAST E2E scenario matrix — native backend

Generated 2026-11-12T23:01:37.588Z against http://127.0.0.1:8732

| Scenario | Result | Duration | Evidence | Notes |
|---|---|---|---|---|
| d — Three Chrome windows: taskbar entries, focus, resize, close | **FAIL** | 34.5 s | `/home/lufria/RAST/tests/e2e/screenshots/native/d/three-chrome-windows.png` | locator.click: Timeout 30000ms exceeded.
Call log:
  - waiting for locator('.window[data-window-id="win-4"]').locator('.win-titlebar')
    - locator resolved to <header class="win-titlebar">…</header>
  - attempting click action
    2 × waiting for element to be visible, enabled and stable
      - e |
| e — Two users use Chrome concurrently with separate data | **PASS** | 12.8 s | `/home/lufria/RAST/tests/e2e/screenshots/native/e/alice-chrome.png`<br>`/home/lufria/RAST/tests/e2e/screenshots/native/e/bob-chrome.png` | [install google-chrome-stable] already installed: chrome<br>native profile ancestor permissions fixed (0711)<br>Chrome app chrome (ready, backend native, build undefined)<br>native launch ready on attempt 1: run ?, display 100<br>native profile ancestor permissions fixed (0711)<br>native launch read |
| h — Closing one app preserves the other apps | **PASS** | 35.5 s | `/home/lufria/RAST/tests/e2e/screenshots/native/h/remaining-app-before-typing.png`<br>`/home/lufria/RAST/tests/e2e/screenshots/native/h/remaining-app-after-typing.png` | [install RAST Test App (portable)] already installed: rast-test-app<br>[install google-chrome-stable] already installed: chrome<br>native profile ancestor permissions fixed (0711)<br>Portable app rast-test-app (ready, backend native, build undefined)<br>native profile ancestor permissions fixed (071 |
| i — Idle cleanup stops the session after use | **FAIL** | 4.8 s |  | Error: assertion failed: session stopped after 2760 ms, before the 120000 ms grace
    at assert (file:///home/lufria/RAST/tests/e2e/scenarios/_helpers.mjs:18:25)
    at Object.run (file:///home/lufria/RAST/tests/e2e/scenarios/i-idle-cleanup.mjs:45:5)
    at process.processTicksAndRejections (node:i |
| j — Service restart recovery | **PASS** | 5.1 s | `/home/lufria/RAST/tests/e2e/screenshots/native/j/after-service-restart.png` | [install RAST Test App (portable)] already installed: rast-test-app<br>native profile ancestor permissions fixed (0711)<br>Portable app rast-test-app (ready, backend native, build undefined)<br>native launch ready on attempt 1: run ?, display 100<br>native profile ancestor permissions fixed (0711)<b |
| k — Failed install and update leave recoverable state | **PASS** | 15.4 s | `/home/lufria/RAST/tests/e2e/screenshots/native/k/healthy-app-after-failures.png` | [install google-chrome-stable] already installed: chrome<br>native profile ancestor permissions fixed (0711)<br>Chrome app chrome (ready, backend native, build undefined)<br>invalid-corrupt.deb: install started ({"installing":true,"jobId":"08a5f09f-9b66-4cc3-85e1-c0d4ebb4590a"})<br>failure state for |
| l — Uninstall respects shared dependencies and user data choice | **PASS** | 12.6 s | `/home/lufria/RAST/tests/e2e/screenshots/native/l/after-uninstall.png` | [install google-chrome-stable] already installed: chrome<br>native profile ancestor permissions fixed (0711)<br>Chrome app chrome (ready, backend native, build undefined)<br>[install RAST Test App (portable)] already installed: rast-test-app<br>native profile ancestor permissions fixed (0711)<br>Por |
| m — Docker app data migrates or reports a precise compatibility failure | **SKIP** | 286 ms |  | [install google-chrome-stable] already installed: chrome<br>native profile ancestor permissions fixed (0711)<br>Chrome app chrome (ready, backend native, build undefined)<br>skipped: the app backend is 'native', expected a docker-backed app to migrate |

Totals: 5 passed, 2 failed, 1 skipped.

