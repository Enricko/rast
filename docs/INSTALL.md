# Installation

RAST ships as a single Linux binary for `x86_64` and `aarch64`, plus systemd
units and a POSIX-sh installer. The installed server does **not** need Node.js,
npm, Rust, a database server or a separate web server — only systemd and a
reverse proxy if you expose it beyond loopback.

* Supported platforms and testing status: [COMPATIBILITY.md](COMPATIBILITY.md)
* Configuration reference: [CONFIGURATION.md](CONFIGURATION.md)
* Day-2 operations: [OPERATIONS.md](OPERATIONS.md)

## Requirements

| Requirement | Details |
| --- | --- |
| Linux with systemd | The installer manages `rast.service` / `rast-worker.service` |
| CPU | `x86_64` or `aarch64` |
| libc | glibc (see the compatibility matrix for minimum versions) |
| Network | HTTPS access to `github.com` for the release download (or a mirror) |
| Tools | `curl` or `wget`, `sha256sum`/`shasum`, `useradd`, `install`, `systemctl` |
| Docker (optional) | Docker Engine + Compose v2 plugin for the Applications app |

The installer creates the `rast` service account (and `rast-worker` when Docker
is configured), `/etc/rast/`, `/var/lib/rast/`, `/srv/rast/`, installs the
binary and units, then starts the service bound to `127.0.0.1:8720`.

## Install with the one-liner

```bash
curl -fsSL https://github.com/Enricko/rast/releases/latest/download/install.sh | sudo sh
```

This is a **prebuilt, checksum-verified** binary — not a `curl | bash` build
script. If you prefer to inspect before running (recommended for production):

```bash
curl -fsSLO https://github.com/Enricko/rast/releases/latest/download/install.sh
less install.sh
sudo sh install.sh
```

Pin an exact release:

```bash
curl -fsSL https://github.com/Enricko/rast/releases/download/v0.1.0/install.sh \
  | sudo sh -s -- --version 0.1.0
```

Preview without changing the system (downloads the binary and verifies its
checksum, prints the plan, then exits):

```bash
curl -fsSL https://github.com/Enricko/rast/releases/latest/download/install.sh \
  | sh -s -- --dry-run
```

### Installer options

| Option | Environment variable | Default | Meaning |
| --- | --- | --- | --- |
| `--version X.Y.Z` | `RAST_VERSION` | latest | Release to install (`v` prefix optional) |
| `--bind ADDR` | `RAST_BIND` | `127.0.0.1:8720` | Listen address |
| `--public-url URL` | `RAST_PUBLIC_URL` | unset | Canonical public URL (`https://…`) |
| `--data-dir DIR` | `RAST_DATA_DIR` | `/var/lib/rast` | Database/backups/apps directory |
| `--storage-dir DIR` | `RAST_STORAGE_DIR` | `/srv/rast` | User files directory |
| `--trusted-proxies LIST` | `RAST_TRUSTED_PROXIES` | unset | Comma-separated proxy CIDRs |
| `--docker` / `--no-docker` | `RAST_DOCKER_ENABLED` | auto | Enable/disable Docker integration |
| `--enable-worker` / `--no-worker` | `RAST_ENABLE_WORKER` | auto | Install/start the worker unit |
| `--no-start` | `RAST_NO_START=1` | off | Install files only, do not enable/start services |
| `--dry-run` | `RAST_DRY_RUN=1` | off | Detect, download, verify, print plan, change nothing |
| `--help` | — | — | Usage |
| — | `RAST_RELEASE_BASE_URL` | GitHub Releases | Mirror/air-gap download base URL |
| — | `RAST_REQUIRE_SIGNATURE=minisign\|cosign` | unset | Fail unless a release signature verifies (see below) |
| — | `RAST_MINISIGN_PUBKEY` / `RAST_COSIGN_PUBKEY` | unset | Local public key for signature verification |

Notes:

* The installer requires root; when started as a normal user it uses `sudo` for
  the privileged steps only, and never pipes a download into a shell.
* It never edits the firewall, DNS, TLS certificates or reverse proxy config.
* It always verifies `SHA256SUMS`; a mismatch aborts before anything is
  installed.
* `--no-start` is useful in images/automation: run
  `systemctl enable --now rast.service` yourself afterwards.
* If the downloaded binary cannot run (glibc or CPU mismatch) the installer
  fails **before** replacing anything; build from source instead.

### What the installer creates

| Path | Mode | Owner | Notes |
| --- | --- | --- | --- |
| `/usr/local/bin/rast` | `0755` | `root:root` | Binary; the previous version is kept under `/usr/local/lib/rast/` |
| `/etc/rast/rast.toml` | `0640` | `root:rast` | Created from the packaged template **only if missing** |
| `/var/lib/rast/` | `0750` | `rast:rast` | Database, `secret.key`, backups, app definitions |
| `/srv/rast/` | `0755` | `rast:rast` | Storage root |
| `/srv/rast/homes`, `shared` | `0755` | `rast:rast` | Created in advance; per-user subdirectories enforce `0700` |
| `/srv/rast/.trash` | `0700` | `rast:rast` | Recycle bin, private to the service account |
| `/etc/systemd/system/rast.service` | `0644` | `root:root` | Always installed |
| `/etc/systemd/system/rast-worker.service` | `0644` | `root:root` | Only when the worker is enabled |
| `/etc/systemd/system/rast.service.d/10-rast-paths.conf` | `0644` | `root:root` | Only for custom data/storage paths (`ReadWritePaths=`) |
| user/group `rast` | — | — | System account, shell `nologin`, home `/nonexistent` |
| user `rast-worker` | — | — | When the worker is enabled; primary group `rast`, supplementary `docker` |

Re-running the installer is the supported upgrade path: it replaces the binary
and units, keeps `/etc/rast/rast.toml` (and prints a warning if you passed
configuration flags that were not applied) and never touches existing data.

### Release signature verification

Official releases can be verified with `minisign` or `cosign`. Signature
verification is **fail-closed**: if `RAST_REQUIRE_SIGNATURE` is set and the
tool, public key or signature is missing, the installer aborts. Release
signing is not yet enabled; leave the variable unset until a signing
announcement is made in the release notes.

```bash
# example once signatures are published
RAST_REQUIRE_SIGNATURE=minisign \
RAST_MINISIGN_PUBKEY=/etc/rast/minisign.pub \
  sh install.sh --version 0.1.0
```

### Air-gapped or mirrored installation

The installer downloads from the GitHub release base by default. For an
internal mirror, replicate the release layout and point the installer at it:

```text
/srv/rast-mirror/v0.1.0/rast-linux-x86_64
/srv/rast-mirror/v0.1.0/rast-linux-aarch64
/srv/rast-mirror/v0.1.0/SHA256SUMS
/srv/rast-mirror/v0.1.0/install.sh
/srv/rast-mirror/v0.1.0/uninstall.sh
/srv/rast-mirror/v0.1.0/rast.toml.example
```

```bash
# explicit version is required with a custom base
RAST_VERSION=0.1.0 \
RAST_RELEASE_BASE_URL=https://mirror.example.com/rast-mirror \
  sudo sh install.sh
```

The installer appends `/v<version>/` to the base URL and still verifies
`SHA256SUMS`; `file://` bases work for offline media. If your mirror also
serves a newest-release pointer, keep the default GitHub URLs unchanged and
mirror the release assets under the standard `v<version>/` layout.

## Post-install steps

### 1. Create the first administrator

There is no default account or password. Either use the CLI:

```bash
sudo -u rast rast user create-admin admin --password-stdin
```

…or use the web first-run page with a one-shot token (recommended when
`setup_token_required = true`):

```bash
sudo -u rast rast setup-token
```

Open the URL shown by the installer (`http://127.0.0.1:8720` or your
`public_url`) and sign in.

### 2. Diagnose

```bash
sudo -u rast rast doctor     # config, permissions, DB, bind address, Docker
sudo rast status [--json]    # version, paths, service state, /healthz probe
```

### 3. Put it behind TLS

See [Reverse proxy and TLS](#reverse-proxy-and-tls). Never expose
`127.0.0.1:8720` directly to the internet: RAST expects the proxy to terminate
TLS and set `public_url` accordingly.

## Reverse proxy and TLS

RAST listens on plain HTTP on loopback. Any TLS terminator works; the essential
settings are:

| Setting | Why |
| --- | --- |
| HTTP/1.1 upstream | SSE and streaming uploads work reliably |
| Clickjacking-free HTTPS with a valid certificate | Secure cookies and browser APIs |
| `public_url = "https://your-host"` in RAST | Correct cookies, redirects, links |
| `trusted_proxies = [...]` for the proxy's source CIDR | Real client IPs for lockout and audit |
| Large request body limit ≥ `max_upload_bytes` | Default RAST upload limit is 10 GiB |
| Response buffering off / `X-Accel-Buffering: no` for `/api/events` | Server-Sent Events would otherwise be delayed |
| Request buffering off for uploads | Keeps large uploads streaming instead of filling proxy temp space |
| Timeouts ≥ 3600 s for `/api/events` (and large uploads) | Long-lived connections |

### nginx

```nginx
# /etc/nginx/sites-available/rast
server {
    listen 443 ssl http2;
    server_name desktop.lufria.tech;

    ssl_certificate     /etc/letsencrypt/live/desktop.lufria.tech/fullchain.pem;
    ssl_certificate_key /etc/letsencrypt/live/desktop.lufria.tech/privkey.pem;
    ssl_protocols TLSv1.2 TLSv1.3;

    # Must be >= RAST max_upload_bytes. 0 disables the limit entirely.
    client_max_body_size 10g;
    client_body_timeout 1h;

    location / {
        proxy_pass http://127.0.0.1:8720;
        proxy_http_version 1.1;
        proxy_set_header Host              $host;
        proxy_set_header X-Real-IP         $remote_addr;
        proxy_set_header X-Forwarded-For   $proxy_add_x_forwarded_for;
        proxy_set_header X-Forwarded-Proto $scheme;
        proxy_set_header X-Forwarded-Host  $host;

        # Stream uploads straight through instead of buffering them on disk.
        proxy_request_buffering off;
        proxy_buffering off;

        proxy_read_timeout 3600s;
        proxy_send_timeout 3600s;
    }

    # Server-Sent Events: disable buffering explicitly and allow long reads.
    location /api/events {
        proxy_pass http://127.0.0.1:8720;
        proxy_http_version 1.1;
        proxy_set_header Host $host;
        proxy_set_header Connection "";
        proxy_buffering off;
        proxy_cache off;
        proxy_read_timeout 24h;
        add_header X-Accel-Buffering no;
    }
}
```

Then configure RAST:

```toml
public_url = "https://desktop.lufria.tech"
trusted_proxies = ["127.0.0.1/32", "::1/128"]
```

If nginx and Cloudflare share the host (`cloudflared` → nginx → RAST), keep
both `cloudflared` and nginx on loopback: RAST then sees nginx as the peer.
Cloudflare's `CF-Connecting-IP` header is forwarded by nginx unchanged and is
honoured by RAST because the nginx peer is trusted.

### Caddy

Caddy obtains and renews certificates automatically:

```caddyfile
desktop.lufria.tech {
    encode zstd gzip
    reverse_proxy 127.0.0.1:8720 {
        flush_interval -1          # disable response buffering (SSE)
        transport http {
            read_timeout 1h
            write_timeout 1h
        }
    }
    request_body {
        max_size 10GB
    }
}
```

```toml
public_url = "https://desktop.lufria.tech"
trusted_proxies = ["127.0.0.1/32", "::1/128"]
```

### Cloudflare Tunnel

`cloudflared` connects out to Cloudflare, so no inbound ports are needed:

```bash
sudo cloudflared tunnel login
sudo cloudflared tunnel create rast
sudo cloudflared tunnel route dns rast desktop.lufria.tech
```

`/etc/cloudflared/config.yml`:

```yaml
tunnel: rast
credentials-file: /etc/cloudflared/<tunnel-id>.json

ingress:
  - hostname: desktop.lufria.tech
    service: http://127.0.0.1:8720
  - service: http_status:404
```

```bash
sudo systemctl enable --now cloudflared
```

RAST configuration:

```toml
public_url = "https://desktop.lufria.tech"
trusted_proxies = ["127.0.0.1/32", "::1/128"]
```

Cloudflare limits the **request body size** by plan (commonly 100 MB on
Free/Pro). RAST's own `max_upload_bytes` must be set to a value your plan can
carry, or large uploads will be rejected by Cloudflare before reaching RAST. If
you need multi-GB uploads, terminate TLS yourself (nginx/Caddy) or use a plan
that supports it.

If you run both, the chain is
`Cloudflare → cloudflared → nginx → rast`, and nginx must forward
`CF-Connecting-IP` (it does by default) — the client IP then survives.

### Other proxies

Any proxy that can preserve `Host`, `X-Forwarded-For`, `X-Forwarded-Proto` and
support streaming works. Set `trusted_proxies` to the exact source addresses of
the last hop; when in doubt, leave it empty (RAST then records the proxy's
address, which is safe but less useful for lockout/audit).

## Docker and the Applications app (optional)

The web server never touches the Docker socket. Docker support is provided by
`rast-worker.service`, which runs as `rast-worker` (primary group `rast`,
supplementary group `docker`) and listens on `/run/rast/worker.sock` with mode
`0660`. `rast.service` runs as `rast:rast` and connects as a group member.

When the installer enables the worker it also writes
`/etc/systemd/system/rast-worker.service.d/20-peer-uid.conf` with
`RAST_WORKER_ALLOWED_UID=<uid of rast>`, so only the web server account may use
the socket (the worker verifies the peer uid with `SO_PEERCRED`). For a manual
installation, add that drop-in yourself or set the variable in
`/etc/rast/rast.env`.

```bash
# 1. Docker Engine + Compose v2 plugin must be installed and running.
sudo systemctl enable --now docker

# 2. Install/enable the worker and Docker integration.
sudo RAST_VERSION=0.1.0 sh install.sh --docker --enable-worker
#    (or edit /etc/rast/rast.toml: docker_enabled = true, then restart)

# 3. Verify.
sudo systemctl status rast-worker
sudo -u rast rast doctor
```

Manual equivalent when adding Docker after a plain install:

```bash
sudo groupadd --system rast 2>/dev/null || true
sudo useradd --system --gid rast --home-dir /nonexistent --no-create-home \
  --shell /usr/sbin/nologin --comment "RAST Docker worker" rast-worker
sudo usermod -aG docker rast-worker
sudo install -m 0644 deploy/rast-worker.service /etc/systemd/system/rast-worker.service
sudo systemctl daemon-reload
sudo systemctl enable --now rast-worker
```

Set `docker_enabled = true` in `/etc/rast/rast.toml` and restart
`rast.service`. If Docker is installed later, restart the worker
(`sudo systemctl restart rast-worker`); its unit has a
`ConditionPathExists=/var/run/docker.sock` guard.

> **Security note:** membership in the `docker` group is effectively
> root-equivalent. The worker is deliberately a separate process with a strict
> command whitelist and no shell access, but a compromised worker is still a
> root-level compromise. See [SECURITY.md](../SECURITY.md).

## Manual installation (no installer)

1. Download the release assets, verify the checksum and inspect the files:

   ```bash
   version=0.1.0
   base="https://github.com/Enricko/rast/releases/download/v${version}"
   curl -fsSLO "$base/rast-linux-x86_64"
   curl -fsSLO "$base/SHA256SUMS"
   sha256sum --check --ignore-missing SHA256SUMS
   ```

2. Create the account, directories and configuration:

   ```bash
   sudo groupadd --system rast
   sudo useradd --system --gid rast --home-dir /nonexistent --no-create-home \
     --shell /usr/sbin/nologin --comment "RAST service account" rast
   sudo install -d -m 0750 -o rast -g rast /var/lib/rast
   sudo install -d -m 0755 -o rast -g rast /srv/rast /srv/rast/homes /srv/rast/shared /srv/rast/.trash
   sudo install -d -m 0750 -o root -g rast /etc/rast
   sudo install -m 0755 -o root -g root rast-linux-x86_64 /usr/local/bin/rast
   sudo install -m 0640 -o root -g rast deploy/rast.toml.example /etc/rast/rast.toml
   sudo install -m 0644 deploy/rast.service /etc/systemd/system/rast.service
   sudo systemctl daemon-reload
   sudo systemctl enable --now rast.service
   ```

3. Edit `/etc/rast/rast.toml` (at minimum `public_url` and
   `trusted_proxies`), then `sudo systemctl restart rast.service`.
4. Continue with [Post-install steps](#post-install-steps).

When installing manually, keep the units and the binary updated from the same
release; `rast update` expects the standard paths.

## Build from source

Only needed for development, unsupported platforms, or if the prebuilt binary's
glibc is too new for your distribution.

```bash
git clone https://github.com/Enricko/rast.git
cd rast

# Frontend assets (Node.js >= 22 + npm; embedded into the binary)
cd frontend && npm ci && npm run build && cd ..

# Rust toolchain is pinned by rust-toolchain.toml
cargo build --release --locked
./target/release/rast version
```

Then install with `scripts/build-release.sh` (stages a complete release set) or
copy `target/release/rast` over `/usr/local/bin/rast` and restart the service.
The build needs a C toolchain (bundled SQLite is compiled in); the resulting
binary has no runtime Node.js or SQLite dependency.

## Upgrade

Re-run the installer of the newer release, or use the CLI:

```bash
sudo rast update --check          # show if a newer release exists
sudo rast update                  # download, verify, back up DB, replace, restart
sudo rast update --version 0.1.0  # install a specific release
```

`rast update` verifies the release checksum, saves a database backup, keeps the
previous binary as `/usr/local/bin/rast.rollback-<old-version>`, restarts the
service and checks `/healthz`; it restores the previous binary if the new one
fails to become healthy. See [OPERATIONS.md](OPERATIONS.md#updates) for the
full procedure and rollback caveats (database migrations are forward-only).

## Uninstall

```bash
# preserve configuration, database and user files (default)
curl -fsSL https://github.com/Enricko/rast/releases/latest/download/uninstall.sh \
  | sudo sh

# also delete configuration and data — requires typing PURGE
sudo sh uninstall.sh --purge

# dry run
sudo sh uninstall.sh --dry-run
```

Default behaviour: stop and disable both units, remove the units, the binary and
the runtime socket directory. `/etc/rast`, `/var/lib/rast` and `/srv/rast` are
preserved; service accounts are kept as well (use `--remove-user` to delete
them, after which preserved files keep their numeric UID — reinstall may not be
able to read them).

## Clean-environment installer test (maintainers)

`scripts/test-installer-clean.sh` proves that `installer/install.sh` works on a
**clean system** (not the machine you develop on) and is the regression test for
installer changes. It never installs anything on the host: all changes happen
inside a throwaway container that is removed on exit.

What it does:

1. Builds release artifacts (`frontend` with npm, `cargo build --release
   --locked`) or reuses a staged directory (`--skip-build --artifacts DIR`),
   then assembles a release-like layout: `rast-linux-<arch>`, `SHA256SUMS`,
   `install.sh`, `uninstall.sh`, `rast.toml.example`.
2. Builds a small systemd-enabled image from the **official `ubuntu:24.04`
   image** (`rast-installer-test:systemd`, cached after first run) and boots it
   as a privileged container with `/sys/fs/cgroup` and the artifacts mounted
   read-only at `/releases`.
3. Runs the installer non-interactively against the local `file:///releases`
   base and verifies: `rast` user/group and permissions of
   `/etc/rast`, `/var/lib/rast`, `/srv/rast`; installed binary and version;
   configuration file (0640 `root:rast`); systemd units installed and enabled;
   `rast.service` active; `/healthz` returns `ok`; admin creation via the CLI;
   login through the JSON API with cookie + CSRF; idempotent reinstall
   preserving configuration and data; uninstall preserving data by default and
   reinstalling cleanly; `--purge` removing configuration/data.
4. Writes an evidence log to `/tmp/rast-installer-clean-evidence.*.log` with
   every PASS/FAIL line and the container diagnostics on failure.

Requirements: Docker (directly, or via passwordless `sudo docker`), ~4 GB free
disk, and network access on the first run to build the base test image.

```bash
# full run: build the release artifacts and test them in a systemd container
scripts/test-installer-clean.sh

# reuse an existing release artifact set (e.g. from scripts/build-release.sh)
scripts/test-installer-clean.sh --skip-build --artifacts dist

# keep the container for debugging on failure
scripts/test-installer-clean.sh --keep-container --verbose
```

Fallback mode: if systemd cannot boot inside a container (some kernels/CI
runtimes forbid it), the harness retries with `--mode stub`, which replaces
`systemctl` with a small recorder that starts/stops the real server so that
install, health, login, reinstall and uninstall can still be verified. The
systemd-native enable/active checks are reported as `SKIP` in that mode; the
summary states which mode was used. `--mode systemd` makes a boot failure fatal
instead of falling back.

The harness refuses `--local` (and therefore can never modify the host's
`/etc`, `/var` or `/usr`); use the documented manual installation if you need
to test on a physical machine.

## Troubleshooting installation

| Symptom | Fix |
| --- | --- |
| `systemd is required but was not detected` | RAST service management needs systemd; use the manual installation in a systemd-enabled host/container (or run the binary directly with a supervisor of your choice) |
| `unsupported CPU architecture` | Only `x86_64` and `aarch64` are built; build from source for anything else |
| `the downloaded binary cannot run on this host` | glibc too old or wrong arch. Check [COMPATIBILITY.md](COMPATIBILITY.md); build from source on the target |
| `SHA-256 checksum mismatch` | Corrupted/partial download or a mirror problem; retry, verify the release page |
| `cannot determine the latest RAST release` | No releases published / GitHub API blocked; pin `RAST_VERSION=X.Y.Z` |
| `address already in use` | Another process (or old RAST instance) uses port 8720: `ss -ltnp \| grep 8720` |
| Service fails with a TOML parse error | `journalctl -u rast.service -n 50`; unknown keys are rejected |
| `502 Bad Gateway` from the proxy | RAST not running or proxy target wrong: check `curl http://127.0.0.1:8720/healthz` |
| SSE updates lag/stop | Proxy buffering enabled; disable `proxy_buffering`/`X-Accel-Buffering` |
| Uploads fail at a fixed size | Proxy body limit < `max_upload_bytes` (nginx `client_max_body_size`, Caddy `request_body`, Cloudflare plan limits) |
| Worker stays inactive | Docker not installed, or `/var/run/docker.sock` missing; unit condition skips it |
