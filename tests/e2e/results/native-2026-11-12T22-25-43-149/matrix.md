# RAST E2E scenario matrix — native backend

Generated 2026-11-12T22:25:49.880Z against http://127.0.0.1:8732

| Scenario | Result | Duration | Evidence | Notes |
|---|---|---|---|---|
| s0 — Smoke: API health, browser login and desktop shell | **PASS** | 2.3 s | `/home/lufria/RAST/tests/e2e/screenshots/native/s0/desktop-after-login.png`<br>`/home/lufria/RAST/tests/e2e/screenshots/native/s0/app-center.png` | logged in through the form as admin (windows: 0)<br>App Center opened<br>bootstrap: RAST 0.13.0 (authenticated=false) |
| a — Admin installs a compatible .deb and launches it | **PASS** | 3.0 s | `/home/lufria/RAST/tests/e2e/screenshots/native/a/app-center-lists-app.png`<br>`/home/lufria/RAST/tests/e2e/screenshots/native/a/app-window-opened.png`<br>`/home/lufria/RAST/tests/e2e/screenshots/native/a/app-usable.png` | [install google-chrome-stable] already installed: chrome<br>native profile ancestor permissions fixed (0711)<br>Chrome app chrome (ready, backend native, build undefined)<br>app after install: {"id":"chrome","name":"Google Chrome","status":"ready","backend":"native","visibility":"users","version":"1 |

Totals: 2 passed, 0 failed, 0 skipped.

