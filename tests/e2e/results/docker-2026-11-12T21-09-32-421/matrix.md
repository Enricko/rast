# RAST E2E scenario matrix — docker backend

Generated 2026-11-12T21:10:36.056Z against http://127.0.0.1:8731

| Scenario | Result | Duration | Evidence | Notes |
|---|---|---|---|---|
| a — Admin installs a compatible .deb and launches it | **PASS** | 1m 2s | `/home/lufria/RAST/tests/e2e/screenshots/docker/a/app-center-lists-app.png`<br>`/home/lufria/RAST/tests/e2e/screenshots/docker/a/app-window-opened.png`<br>`/home/lufria/RAST/tests/e2e/screenshots/docker/a/app-stream-visible.png`<br>`/home/lufria/RAST/tests/e2e/screenshots/docker/a/app-usable.png` | [install google-chrome-stable] already installed: chrome<br>Chrome app chrome (ready, backend docker, build undefined)<br>app after install: {"id":"chrome","name":"Google Chrome","status":"ready","backend":"docker","visibility":"users","version":"157.0.8103.78-1"}<br>app is visible to a non-admin (s |

Totals: 1 passed, 0 failed, 0 skipped.

