# RAST E2E scenario matrix — native backend

Generated 2026-11-12T22:29:17.380Z against http://127.0.0.1:8732

| Scenario | Result | Duration | Evidence | Notes |
|---|---|---|---|---|
| c — Same user opens Chrome and another GUI app (shared session) | **PASS** | 4.2 s | `/home/lufria/RAST/tests/e2e/screenshots/native/c/two-apps-one-user.png`<br>`/home/lufria/RAST/tests/e2e/screenshots/native/c/after-closing-chrome.png` | [install google-chrome-stable] already installed: chrome<br>[install RAST Test App (portable)] uploading /home/lufria/RAST/tests/e2e/fixtures/rast-test-app-1.0.tar.gz<br>[install RAST Test App (portable)] install job cf9e3dcc-ffb9-4cf1-88cb-e3373695f597<br>native profile ancestor permissions fixed ( |
| d — Three Chrome windows: taskbar entries, focus, resize, close | **FAIL** | 1m 2s |  | TypeError: Cannot read properties of undefined (reading 'id')
    at openChromeWindowFromInside (file:///home/lufria/RAST/tests/e2e/scenarios/d-chrome-windows.mjs:22:56)
    at async Object.run (file:///home/lufria/RAST/tests/e2e/scenarios/d-chrome-windows.mjs:42:7)
    at async main (file:///home/l |
| f — Concurrent launch requests reuse the intended session | **PASS** | 2.5 s | `/home/lufria/RAST/tests/e2e/screenshots/native/f/concurrent-launch-one-window.png` | [install google-chrome-stable] already installed: chrome<br>native profile ancestor permissions fixed (0711)<br>Chrome app chrome (ready, backend native, build undefined)<br>concurrent requests: 8 ok, 0 rejected<br>native session: {"appId":null,"appName":"Desktop session","apps":[{"appId":"chrome"," |
| g — Browser refresh reconnects without duplicating processes | **PASS** | 4.4 s | `/home/lufria/RAST/tests/e2e/screenshots/native/g/before-refresh.png`<br>`/home/lufria/RAST/tests/e2e/screenshots/native/g/after-refresh.png` | [install google-chrome-stable] already installed: chrome<br>native profile ancestor permissions fixed (0711)<br>Chrome app chrome (ready, backend native, build undefined)<br>native launch ready on attempt 1: run ?, display 100<br>native profile ancestor permissions fixed (0711)<br>session before ref |
| h — Closing one app preserves the other apps | **FAIL** | 26.7 s |  | Error: assertion failed: the fixture app window disappeared when Chrome closed
    at assert (file:///home/lufria/RAST/tests/e2e/scenarios/_helpers.mjs:18:25)
    at Object.run (file:///home/lufria/RAST/tests/e2e/scenarios/h-close-preserves.mjs:50:5)
    at async main (file:///home/lufria/RAST/tests |

Totals: 3 passed, 2 failed, 0 skipped.

