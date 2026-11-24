# Rollback acceptance test — 2026-11-24

Production build under test: `RAST 0.18.3 (8a08a9a39c7d)`, binary sha256
`7ce2b4c9993db5e73d393b405fdb826506875c501e357cf04a86f8db7653e1cb`.
Previous deployed release used for rollback: `RAST 0.18.3 (3d0823fa261d)`,
binary sha256 `55182bbbf0ceb090ca4f4de228706521a2faa4f2f3d4625520aa83272adcc4ae`.

| Step | Result |
| --- | --- |
| Pre-check health / services / sessions | 200; all active; live user session `rast-xpra-u1` running |
| Switch `/usr/local/bin/rast` to previous release, restart rast-native + rast-worker + rast | version reports `3d0823fa261d` (sha `55182bbb…`) |
| Health after rollback | **200**, all three services active |
| Running user session after rollback | **preserved** (`rast-xpra-u1`) |
| Terminal smoke on rolled-back build | **PASS** (real browser Terminal opened, command executed) |
| Switch back to current release, restart all three services | version reports `8a08a9a39c7d` (sha `7ce2b4c9…`) |
| Health after restore | **200**, all three services active |
| Running user session after restore | **preserved** |
| Terminal smoke on restored build | **PASS** |
| Final cleanup | test-admin session closed; only `rast-xpra-u1` remains |
| Final health / version | 200 / `RAST 0.18.3 (8a08a9a39c7d)` |

Raw log: `/tmp/rollback-test.log` (kept on the host). This both exercises the
documented manual rollback commands and proves that a rollback/upgrade cycle
preserves live native sessions and keeps Terminal working.
