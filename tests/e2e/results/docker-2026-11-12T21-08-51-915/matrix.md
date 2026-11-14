# RAST E2E scenario matrix — docker backend

Generated 2026-11-12T21:09:13.734Z against http://127.0.0.1:8731

| Scenario | Result | Duration | Evidence | Notes |
|---|---|---|---|---|
| a — Admin installs a compatible .deb and launches it | **FAIL** | 20.4 s |  | page.waitForSelector: Timeout 20000ms exceeded.
Call log:
  - waiting for locator('input[name="username"], .auth-form') to be visible

    at loginViaForm (/home/lufria/RAST/tests/e2e/lib/browser.mjs:88:14)
    at async Module.openDesktop (/home/lufria/RAST/tests/e2e/lib/browser.mjs:103:5)
    at as |

Totals: 0 passed, 1 failed, 0 skipped.

