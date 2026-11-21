# Deployment

## Revamp preview

`https://desktop-revamp.lufria.tech/` serves the Rune browser client. Cloudflare
Tunnel forwards it to the dedicated nginx site on `127.0.0.1:8722` with document
root `/srv/rast-static/revamp`; nginx forwards `/api/` to the account server
(`rast-server`, `rast-revamp.service`) on `127.0.0.1:8723`. Sign-in uses the
server's own accounts (Super Admin, Admin, User) with an `HttpOnly` session
cookie. The preview has no access to production files or accounts.

From a checkout of `revamp/remake`, build and publish:

```sh
scripts/web-build.sh
deploy/install-revamp.sh --web-only
```

The web-only step requires write access to the static directory, not root. It
does not touch the account server, nginx or Cloudflare Tunnel. Use `--dry-run`
to preview it.

### Account server

After a change to `crates/rast-server` (or the nginx site), build as yourself
and run the full installer:

```sh
cargo build --release -p rast-server
scripts/web-build.sh
sudo deploy/install-revamp.sh
```

It installs `/opt/rast-revamp/bin/rast-server`, `rast-revamp.service`
(`DynamicUser`, `StateDirectory=rast-revamp`, systemd sandboxing, loopback only)
and the nginx site, then checks the API locally. The database stays in
`/var/lib/rast-revamp/rast.db` (owner-only) across upgrades. Back it up with
`sqlite3 /var/lib/rast-revamp/rast.db ".backup …"`, not by copying the file of a
running server.

**First run:** with no accounts the server writes a one-time setup token. Open
the site; it shows *Set up RAST*. Read the token on the host with
`sudo cat /var/lib/rast-revamp/setup-token` and create the Super Admin. The
token file is deleted once used. After that there is no public sign-up: Super
Admins and Admins create accounts in Control Panel → Users. Super Admins set
up groups and their app rules under Control Panel → Groups.

Logs: `journalctl -u rast-revamp`. Failed sign-ins are throttled per username
and per visitor IP (Cloudflare's `CF-Connecting-IP`, passed as `X-Client-IP`),
and 10 wrong passwords within 30 minutes lock the account for 15 minutes; an
administrator can unlock it on the account's page. The installer upgrades the
database schema in place when the server starts. Dump it first
(`sudo sqlite3 -readonly /var/lib/rast-revamp/rast.db .dump > backup.sql`, kept
owner-only: it holds password hashes) so an upgrade can be undone.

### Caching, updates and rollback

The build puts the bootstrap, generated JavaScript, WebAssembly, and supporting
files under `assets/<content digest>/`. JavaScript resolves the Wasm and all
other dependencies relative to its own release directory. The installer stages
and verifies assets before replacing `index.html` atomically. It retains previous
asset directories so cached pages and open tabs can still finish loading. The
HTML entry point is revalidated; it is not part of the long-lived asset cache.

Deploy with the installer, **not** by overwriting individual assets or syncing
with `--delete`. Fixed filenames can make an edge cache serve old JavaScript
alongside new Wasm, causing an import `LinkError`. To roll back, publish a saved
build of the previous release with the same installer. Keep old release assets
as long as pages may refer to them.

### First-time host setup

If nginx and the tunnel have not yet been set up, build the bundle and run
`sudo deploy/install-revamp.sh` on the host. This installs the dedicated nginx
site, validates and updates the tunnel ingress when necessary, and checks the
local and public site. DNS must be a proxied CNAME to the relevant Cloudflare
Tunnel (`<tunnel-id>.cfargotunnel.com`). The script header describes how to
remove the host setup.

### Local preview

```sh
scripts/web-build.sh
cargo run -p rast-server -- --data /tmp/rast-dev   # account API on 127.0.0.1:8723
python3 scripts/serve-web.py
```

Open `http://127.0.0.1:8765/` in a WebGPU-capable browser. The local server sends
the same content security policy as the live nginx site and forwards `/api/` to
the account server (`RAST_API` to point elsewhere). The setup token is in
`/tmp/rast-dev/setup-token`.

## Existing production

`https://desktop.lufria.tech/` is the previous RAST release, not the revamp.
Its nginx and tunnel routes are separate and the installer does not modify them.
Do not cut over that domain until the rewrite's server covers what the old one
does (files, apps, groups and permissions), not only accounts and sessions, and
its operations have been tested on staging.
