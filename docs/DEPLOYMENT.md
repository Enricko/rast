# Deployment

This page is the honest state of deploying the rewrite. Read it before touching
`desktop.lufria.tech`.

## Current production is not this code

`desktop.lufria.tech` still runs the **last released RAST 1.0.1-alpha.2** from
branch `Remake`. The rewrite on `revamp/remake` does not replace it, and this
repository does not contain a server yet — only the client shell.

Probed read-only on 2026-11-21: `https://desktop.lufria.tech/` answers `HTTP 200`
and the installed `rast` service is active.

## Why the rewrite cannot be deployed yet

1. **There is nothing to serve.** The rewrite has no server, no auth, no API, no
   session handling and no desktop/apps. Deploying it to the production domain
   would replace a working product with a connection-status window.
2. **Deploying is a root operation on the host**, not something this repository's
   sandbox can do: installing a release, `systemctl`, nginx vhosts, `/etc/rast`,
   `/var/lib/rast` and `/srv/rast` all live outside the workspace and need the
   maintainer.
3. **Production doubles as the test host.** A mistake there is visible to real
   users immediately, and there is no separate VM to fall back to.

## What exists today for deployment

- `dist` CI job: release binaries for Linux and Windows (`.github/workflows/ci.yml`,
  `workflow_dispatch` or a tag). macOS builds are tested but not packaged yet.
- `scripts/web-build.sh`: the browser build, a static folder.
- No server package, no systemd unit, no nginx site for the rewrite yet.

## Safe plan for the first real deployment (staging first)

Do not touch `desktop.lufria.tech` until the rewrite has a server that can at
least authenticate. The sequence:

1. **Staging vhost**: add a separate hostname or port
   (e.g. `revamp.desktop.lufria.tech`) pointing at the rewrite's server, while
   the old instance keeps serving `desktop.lufria.tech`.
2. **Test accounts only** on staging; never real users, never real shared
   folders.
3. Health check: sign in, open a window, verify a file round-trips through the
   new server, check logs for errors.
4. **Cutover in a maintenance window**: keep the old release installed, note its
   version for rollback, back up `/etc/rast`, `/var/lib/rast`, `/srv/rast`.
5. **Rollback**: stop the new units, restore the vhost to the old backend, and
   reinstall the previous release with
   `sudo rast update --version <previous version>`.

## Until then

- Production: unchanged; do not run `rast update` for the rewrite.
- Rewrite: build it in CI, run the client locally against any address (default
  `desktop.lufria.tech:443` only tells you the host is up), and use the
  client's Settings screen to point test machines at whatever test server the
  server phase produces.
