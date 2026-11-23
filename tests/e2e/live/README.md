# Live production verification — native Linux apps

`verify-production.mjs` verifies the native Linux app integration on the real
production instance (`https://desktop.lufria.tech`) through the public UI, as a
signed-in test user — no internal APIs beyond the ones the SPA itself uses.

It is the live counterpart of the disposable-instance matrix in
`tests/e2e/run.mjs --mode=native`: the matrix proves the build, this script
proves the deployed instance (Cloudflare tunnel, real accounts, real systemd
units, real memory usage).

## When to run

Only after the Lead reports that the repaired build is deployed and
`native_enabled` / `linux_apps_enabled` are on in `/etc/rast/rast.toml`
(see task-4 on the shared task board). Running it earlier fails the preflight
check with a clear "native API not available" message; it never changes
production configuration.

## Usage

From `tests/e2e` (where Playwright is installed):

```bash
node live/verify-production.mjs                 # full verification
node live/verify-production.mjs --headless=false   # watch it in a window
node live/verify-production.mjs --preflight-only   # reachability + native API only
node live/verify-production.mjs --skip-host        # skip systemctl/pgrep evidence
node live/verify-production.mjs --keep-windows     # leave the app windows open
```

Environment overrides: `RAST_LIVE_URL`, `RAST_LIVE_CREDS`,
`RAST_LIVE_CHROMIUM`.

Exit code 0 only when every check passed.

## App Center check (admin-only app)

In RAST 0.18.3 App Center is the admin-only `packages` application: ordinary
accounts (t-alice/t-bob) get `403` from `/api/packages` and no App Center tile,
so phase 2 needs an administrator. Pass the dedicated **test** administrator
(never the real `admin` account) via environment variables:

```bash
RAST_LIVE_ADMIN_USER=t-admin \
RAST_LIVE_ADMIN_PASSWORD="$(grep '^t-admin=' /home/lufria/.config/rast-test/creds | cut -d= -f2-)" \
node live/verify-production.mjs
```

Without those variables phase 2 is recorded as an explicit SKIP with the
permission/HTTP evidence instead of failing. The password is only used to fill
the real login form; it is never printed or written to the report.

## Credentials

Read from `/home/lufria/.config/rast-test/creds` (mode 0600), one
`username=password` line per account, for `t-alice` (ordinary user) and
`t-bob` (ordinary user, cross-user isolation check). Passwords are never
printed, logged, or written into the report; session viewer keys in URLs are
redacted to `/linux-apps/session/<redacted>/`.

## What it verifies

1. **One native window** — Applications → Google Chrome becomes exactly one
   RAST window whose body is `.linux-native-window` with an adopted remote
   Xpra surface (`canvas`/`video`), **0 iframes** in the body, no Xpra
   toolbar/decoration/remote desktop inside it.
2. **Hidden transport** — the single `.linux-native-session-frame` iframe
   exists in the DOM, is off-screen (`right <= 0`), is outside every window
   and stays laid out (`display` is not `none`).
3. **App Center** — opening Google Chrome from its App Center tile and closing
   App Center leaves the Chrome window running.
4. **Multi-window** — Ctrl+N inside Chrome creates a second independent RAST
   window with its own taskbar entry; the script drags, resizes,
   minimizes/restores, maximizes, closes one window and checks the other one
   survives with its adopted surface intact.
5. **Refresh** — reloading the desktop re-adopts exactly the same remote
   window ids (no duplication) and the same `rast-xpra-*`/`rast-app-*` units
   and Chrome processes keep running (read-only `systemctl`/`pgrep`).
6. **Cross-user isolation** — `t-bob` and an anonymous browser requesting
   `t-alice`'s session viewer URL get 403 / 401.
7. **Metrics** — click-to-first-window latency, adopted window count, session
   unit `MemoryCurrent`/`TasksCurrent`, app unit memory and Chrome process
   count.

## Artifacts

- Report (JSON + markdown): `tests/e2e/live/results/live-<timestamp>/report.{json,md}`
- Screenshots: `tests/e2e/live/screenshots/live-<timestamp>/*.png`

Both directories are git-ignored; keep the report path with the release
evidence.

## Safety

The script only signs in, drives the desktop UI, opens/closes app windows for
the test accounts and reads host state with `systemctl`/`pgrep`. It never edits
`/etc/rast/rast.toml`, never restarts `rast.service`/`rast-native.service`/
`rast-worker.service`, and never touches nginx/cloudflared. App windows are
closed at the end (`--keep-windows` keeps them for manual inspection); the
native session idles out by itself afterwards.
