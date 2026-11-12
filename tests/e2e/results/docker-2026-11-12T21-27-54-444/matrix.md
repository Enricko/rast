# RAST E2E scenario matrix — docker backend

Generated 2026-11-12T21:29:51.267Z against http://127.0.0.1:8732

| Scenario | Result | Duration | Evidence | Notes |
|---|---|---|---|---|
| s0 — Smoke: API health, browser login and desktop shell | **PASS** | 3.3 s | `/home/lufria/RAST/tests/e2e/screenshots/docker/s0/desktop-after-login.png`<br>`/home/lufria/RAST/tests/e2e/screenshots/docker/s0/app-center.png` | logged in through the form as admin (windows: 0)<br>App Center opened<br>bootstrap: RAST 0.12.0 (authenticated=false) |
| a — Admin installs a compatible .deb and launches it | **FAIL** | 13.9 s |  | Error: job 988d2a2f-7f28-4b2f-843e-5d275a92eee3 failed: package path must be inside /tmp/rast-native-e2e/data/linux-apps
    at RastApi.waitJob (file:///home/lufria/RAST/tests/e2e/lib/rast-api.mjs:293:23)
    at process.processTicksAndRejections (node:internal/process/task_queues:104:5)
    at async |
| b — Admin installs a portable archive and launches it | **FAIL** | 1.5 s |  | Error: job 16b97ef0-f430-4a92-a7d0-0fb87d2a6bff failed: cannot prepare the staging folder: Permission denied (os error 13)
    at RastApi.waitJob (file:///home/lufria/RAST/tests/e2e/lib/rast-api.mjs:293:23)
    at process.processTicksAndRejections (node:internal/process/task_queues:104:5)
    at asy |
| c — Same user opens Chrome and another GUI app (shared session) | **FAIL** | 1.5 s |  | Error: job 378fc82a-844e-4276-90f3-1b512727fbea failed: cannot prepare the staging folder: Permission denied (os error 13)
    at RastApi.waitJob (file:///home/lufria/RAST/tests/e2e/lib/rast-api.mjs:293:23)
    at process.processTicksAndRejections (node:internal/process/task_queues:104:5)
    at asy |
| d — Three Chrome windows: taskbar entries, focus, resize, close | **FAIL** | 13.8 s |  | Error: job c53ed289-c81e-4378-a635-81b7003e67d4 failed: package path must be inside /tmp/rast-native-e2e/data/linux-apps
    at RastApi.waitJob (file:///home/lufria/RAST/tests/e2e/lib/rast-api.mjs:293:23)
    at process.processTicksAndRejections (node:internal/process/task_queues:104:5)
    at async |
| e — Two users use Chrome concurrently with separate data | **FAIL** | 13.8 s |  | Error: job d79ae241-8b95-4e12-a0e2-b785930f724c failed: package path must be inside /tmp/rast-native-e2e/data/linux-apps
    at RastApi.waitJob (file:///home/lufria/RAST/tests/e2e/lib/rast-api.mjs:293:23)
    at process.processTicksAndRejections (node:internal/process/task_queues:104:5)
    at async |
| f — Concurrent launch requests reuse the intended session | **FAIL** | 12.3 s |  | Error: job 50d3c503-4164-42bc-8712-f5ad12b2d10c failed: package path must be inside /tmp/rast-native-e2e/data/linux-apps
    at RastApi.waitJob (file:///home/lufria/RAST/tests/e2e/lib/rast-api.mjs:293:23)
    at process.processTicksAndRejections (node:internal/process/task_queues:104:5)
    at async |
| g — Browser refresh reconnects without duplicating processes | **FAIL** | 12.3 s |  | Error: job b52558a1-c369-4601-b6a7-5217d1898a36 failed: package path must be inside /tmp/rast-native-e2e/data/linux-apps
    at RastApi.waitJob (file:///home/lufria/RAST/tests/e2e/lib/rast-api.mjs:293:23)
    at process.processTicksAndRejections (node:internal/process/task_queues:104:5)
    at async |
| h — Closing one app preserves the other apps | **FAIL** | 1.5 s |  | Error: job d8aaefc5-5bcd-4580-881d-cb163eae1615 failed: cannot prepare the staging folder: Permission denied (os error 13)
    at RastApi.waitJob (file:///home/lufria/RAST/tests/e2e/lib/rast-api.mjs:293:23)
    at process.processTicksAndRejections (node:internal/process/task_queues:104:5)
    at asy |
| i — Idle cleanup stops the session after use | **FAIL** | 1.5 s |  | Error: job debdbcdf-8965-4558-85d4-6d3d0ec870d5 failed: cannot prepare the staging folder: Permission denied (os error 13)
    at RastApi.waitJob (file:///home/lufria/RAST/tests/e2e/lib/rast-api.mjs:293:23)
    at process.processTicksAndRejections (node:internal/process/task_queues:104:5)
    at asy |
| j — Service restart recovery | **FAIL** | 1.5 s |  | Error: job 800f3853-48f7-4af7-8193-a089f761cc15 failed: cannot prepare the staging folder: Permission denied (os error 13)
    at RastApi.waitJob (file:///home/lufria/RAST/tests/e2e/lib/rast-api.mjs:293:23)
    at process.processTicksAndRejections (node:internal/process/task_queues:104:5)
    at asy |
| k — Failed install and update leave recoverable state | **FAIL** | 13.8 s |  | Error: job 7439b554-394d-4844-b876-ef375de52a67 failed: package path must be inside /tmp/rast-native-e2e/data/linux-apps
    at RastApi.waitJob (file:///home/lufria/RAST/tests/e2e/lib/rast-api.mjs:293:23)
    at process.processTicksAndRejections (node:internal/process/task_queues:104:5)
    at async |
| l — Uninstall respects shared dependencies and user data choice | **FAIL** | 12.3 s |  | Error: job d5aea258-88bc-44f1-a31a-bff9ec302582 failed: package path must be inside /tmp/rast-native-e2e/data/linux-apps
    at RastApi.waitJob (file:///home/lufria/RAST/tests/e2e/lib/rast-api.mjs:293:23)
    at process.processTicksAndRejections (node:internal/process/task_queues:104:5)
    at async |
| m — Docker app data migrates or reports a precise compatibility failure | **FAIL** | 12.2 s |  | Error: job b0440d66-ad73-4f8e-ad36-a1c46b5a79ae failed: package path must be inside /tmp/rast-native-e2e/data/linux-apps
    at RastApi.waitJob (file:///home/lufria/RAST/tests/e2e/lib/rast-api.mjs:293:23)
    at process.processTicksAndRejections (node:internal/process/task_queues:104:5)
    at async |

Totals: 1 passed, 13 failed, 0 skipped.

