# RAST

**Resource Administration & Storage Toolkit** — a self-hosted server
administration toolkit with a desktop-like web UI, distributed as a single Rust
binary.

RAST bundles the day-to-day server admin tools into one web desktop:

| App | What it does |
| --- | --- |
| **File Station** | Browse, upload, download, rename, copy, move, delete, preview and restore files, with per-user homes, shared folders and quotas. |
| **Applications** | Deploy and manage Docker Compose applications through a restricted administrative worker. |
| **Resource Monitor** | Live CPU, memory, swap, disk, network and container metrics. |
| **Control Panel** | Profile, password, users, roles, quotas, shared folders, storage locations and component health. |
| **Backup Center** | Consistent SQLite + configuration backups with schedules, retention and restore guidance. |
| **Activity Logs** | Auditable record of every action: who, what, when, result. |

[![CI](https://github.com/Enricko/rast/actions/workflows/ci.yml/badge.svg)](https://github.com/Enricko/rast/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/Enricko/rast?include_prereleases)](https://github.com/Enricko/rast/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Platforms](https://img.shields.io/badge/platforms-linux%20x86__64%20%7C%20aarch64-informational)

> **Status:** v0.1.0 — first public release (2026-11-10). APIs and data formats
> may still change before v1.0; see [CHANGELOG.md](CHANGELOG.md) for the exact
> feature set and known limitations.

## Quick start (installed server)

```bash
# 1. Install (Linux x86_64 / arm64, systemd)
curl -fsSL https://github.com/Enricko/rast/releases/latest/download/install.sh | sudo sh

# 2. Create the first administrator (CLI only — there is no default password)
sudo -u rast rast user create-admin admin --password-stdin

# 3. Open the web UI at https://your-host/ and sign in
```

The installer creates the `rast` service account, `/etc/rast/`, `/var/lib/rast/`
and `/srv/rast/`, installs a `rast.service` systemd unit and starts it bound to
`127.0.0.1:8720`. Put it behind your reverse proxy with TLS.

## Build from source

Requirements: Rust stable (pinned in `rust-toolchain.toml`), Node.js 22+ and
npm **for the frontend build only**. The installed server never needs Node.js.

```bash
git clone https://github.com/Enricko/rast.git
cd rast
npm --prefix frontend ci
npm --prefix frontend run build
cargo build --release
./target/release/rast version
```

## Configuration

See [docs/CONFIGURATION.md](docs/CONFIGURATION.md). Defaults:

| Item | Path |
| --- | --- |
| Configuration | `/etc/rast/rast.toml` |
| Database & backups | `/var/lib/rast/` |
| User files | `/srv/rast/` |
| Binary | `/usr/local/bin/rast` |
| Service | `rast.service` (worker: `rast-worker.service`) |

Environment variables use the `RAST_` prefix (see `.env.example`).

## Security model

* Argon2id password hashing (19 MiB, 2 iterations, 1 lane).
* Server-side sessions stored as SHA-256 hashes; `HttpOnly`, `Secure`,
  `SameSite=Lax` cookies; CSRF tokens required for every mutating request.
* Login rate limiting and per-account lockout; no public registration; the
  first administrator can only be created from the CLI or with a one-shot
  setup token.
* Per-user filesystem isolation with path-traversal and symlink-escape
  protection enforced on the server for every endpoint.
* Docker operations run through a separate worker process with a strict
  message protocol — the web server never talks to the Docker socket.

See [SECURITY.md](SECURITY.md) for reporting vulnerabilities.

## Documentation

* [Architecture](docs/ARCHITECTURE.md)
* [Configuration](docs/CONFIGURATION.md)
* [Installation](docs/INSTALL.md)
* [Operations: backup, restore, update, rollback](docs/OPERATIONS.md)
* [API reference](docs/API.md)

## License

MIT — see [LICENSE](LICENSE). Dependency licenses are listed in
[docs/THIRD-PARTY-LICENSES.md](docs/THIRD-PARTY-LICENSES.md) (generated with
`cargo license`).
