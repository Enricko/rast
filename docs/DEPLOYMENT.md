# Deployment

## Revamp preview

`https://desktop-revamp.lufria.tech/` serves the Rune browser client. Cloudflare
Tunnel forwards it to the dedicated nginx site on `127.0.0.1:8722` with document
root `/srv/rast-static/revamp`. The preview has device unlock, **not** server
authentication or access to production files and accounts.

From a checkout of `revamp/remake`, build and publish:

```sh
scripts/web-build.sh
deploy/install-revamp.sh --web-only
```

The web-only step requires write access to the static directory, not root. It
does not reload nginx or Cloudflare Tunnel. Use `--dry-run` to preview it.

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
python3 scripts/serve-web.py
```

Open `http://127.0.0.1:8765/` in a WebGPU-capable browser. The local server sends
the same content security policy as the live nginx site.

## Existing production

`https://desktop.lufria.tech/` is the previous RAST release, not the revamp.
Its nginx and tunnel routes are separate and the installer does not modify them.
Do not cut over that domain until the rewrite has a server with authentication,
sessions, and access controls, and its operations have been tested on staging.
