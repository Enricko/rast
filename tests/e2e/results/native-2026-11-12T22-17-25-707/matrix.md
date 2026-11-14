# RAST E2E scenario matrix — native backend

Generated 2026-11-12T22:17:43.440Z against http://127.0.0.1:8732

| Scenario | Result | Duration | Evidence | Notes |
|---|---|---|---|---|
| s0 — Smoke: API health, browser login and desktop shell | **PASS** | 3.0 s | `/home/lufria/RAST/tests/e2e/screenshots/native/s0/desktop-after-login.png`<br>`/home/lufria/RAST/tests/e2e/screenshots/native/s0/app-center.png` | logged in through the form as admin (windows: 0)<br>App Center opened<br>bootstrap: RAST 0.12.0 (authenticated=false) |
| a — Admin installs a compatible .deb and launches it | **FAIL** | 13.0 s | `/home/lufria/RAST/tests/e2e/screenshots/native/a/app-center-lists-app.png`<br>`/home/lufria/RAST/tests/e2e/screenshots/native/a/app-window-opened.png` | RastApiError: POST /api/linux-apps/chrome/launch -> HTTP 503: Could not start Google Chrome: directory component /tmp/rast-native-e2e/data (mode 0750 owner 993:980) is not traversable by the native identity rast-u2 (uid 210002); every component of the profile/cwd path needs the execute bit for it —  |

Totals: 1 passed, 1 failed, 0 skipped.

