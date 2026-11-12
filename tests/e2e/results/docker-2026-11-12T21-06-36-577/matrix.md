# RAST E2E scenario matrix — docker backend

Generated 2026-11-12T21:06:58.103Z against http://127.0.0.1:8731

| Scenario | Result | Duration | Evidence | Notes |
|---|---|---|---|---|
| s0 — Smoke: API health, browser login and desktop shell | **FAIL** | 20.1 s |  | page.waitForSelector: Timeout 20000ms exceeded.
Call log:
  - waiting for locator('input[name="username"], .auth-form') to be visible

    at Module.loginViaForm (/home/lufria/RAST/tests/e2e/lib/browser.mjs:88:14)
    at async Object.run (/home/lufria/RAST/tests/e2e/scenarios/s0-smoke.mjs:19:5)
     |

Totals: 0 passed, 1 failed, 0 skipped.

