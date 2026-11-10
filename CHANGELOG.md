# Changelog

All notable changes to RAST are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Release notes published on GitHub are generated from the matching section of
this file by `scripts/changelog-section.sh`.

## [Unreleased]

### Added

- Nothing yet.

### Planned

- Published release signatures (minisign/cosign) and installer enforcement via
  `RAST_REQUIRE_SIGNATURE`; the installer already supports fail-closed
  verification once signatures are published.
- Automated restore drills and a documented JSON schema for backup manifests.
- Static musl builds are **not** planned for 0.1.x (see
  [docs/COMPATIBILITY.md](docs/COMPATIBILITY.md)).

## [0.1.0] - 2026-11-10

Initial public release: a single Rust binary that replaces the collection of
ad-hoc tools used to administer a home or small-office Linux server.

### Added

- **Single binary distribution.** Axum HTTP server, SQLite metadata store and
  the built frontend (Vite/TypeScript, embedded via `rust-embed`) in one
  `rast` executable for Linux x86_64 and aarch64. No runtime Node.js, database
  server or external web server required.
- **CLI**: `rast serve [--bind]`, `rast status [--json]`, `rast doctor`,
  `rast user create-admin`, `rast user list`, `rast user reset-password`,
  `rast setup-token`, `rast backup`, `rast restore`, `rast update`,
  `rast version`, plus the internal `rast worker`.
- **Authentication and accounts**: Argon2id password hashing (19 MiB, 2
  iterations), password policy (minimum 10 characters), server-side sessions
  whose tokens are stored only as SHA-256 hashes, sliding expiry with a 30-day
  absolute cap, per-session revocation, CSRF tokens bound to the session,
  `HttpOnly`/`SameSite=Lax`/auto-`Secure` cookies, login rate limiting and
  per-account lockout. No default credentials and no public registration.
- **First-run setup**: create the first administrator from the CLI, or use a
  one-shot `rast setup-token` token (60 minutes, single use) with
  `setup_token_required` enforced by the API.
- **Layered configuration**: built-in defaults → `/etc/rast/rast.toml` →
  `RAST_*` environment variables → CLI flags, with validation, warnings
  surfaced by `rast doctor`, and a fully documented
  [deploy/rast.toml.example](deploy/rast.toml.example).
- **Audit log**: append-only record of who did what, to which object, with
  result and client IP; retention pruning (90 days), Activity Logs app and
  export endpoint.
- **Background jobs**: persistent queue with bounded concurrency, progress
  reporting over Server-Sent Events (SSE) and cooperative cancellation; stale
  jobs recovered on startup.
- **Events**: `/api/events` SSE stream backed by a single in-process broadcast
  bus (one monitor sampler regardless of open tabs).
- **Backups**: self-contained `.tar.gz` archives with a checksummed manifest,
  `db` (consistent `VACUUM INTO` snapshot), `config` (including `secret.key`),
  `users` and `apps` scopes; archive verification; integrity-checked restore
  that refuses to run while the service is active and keeps a pre-restore copy
  of the database; daily/hourly/weekly schedules in server-local time with
  configurable retention (default schedule disabled, 03:00, keep 7).
- **Self-update**: `rast update [--version X] [--check]` verifies
  `SHA256SUMS`, checks the downloaded binary's version, takes a database
  backup, replaces the binary atomically, restarts `rast.service` and rolls the
  binary back if the health check fails.
- **Service management**: hardened systemd units for `rast.service` and the
  separate privileged Docker worker `rast-worker.service` (unprivileged web
  process, Unix-socket JSON protocol with an operation whitelist, no shell
  execution).
- **Installer and uninstaller**: POSIX-sh, `curl | sudo sh` capable, checksum
  verification before installation, architecture/OS/systemd detection,
  idempotent upgrades that preserve data, optional Docker worker setup and a
  safe uninstaller that preserves configuration/data unless `--purge` is
  confirmed.
- **Web desktop** (single-page app): desktop shell, login/first-run screens and
  app modules for File Station, Applications, Resource Monitor, Control Panel,
  Backup Center and Activity Logs. Live data is delivered over SSE.
- **Docker administration foundation**: compose validation, app deployment
  metadata and the worker IPC client/protocol live in
  `src/docker/`; the worker executes only `docker`/`docker compose` commands
  with structured arguments.
- **Documentation**: architecture, configuration, installation (including
  nginx/Caddy/Cloudflare Tunnel examples), operations/backup/restore/update
  runbooks, compatibility matrix, security policy, contributing guide and
  third-party license inventory.
- **CI/CD**: format, strict clippy, tests, frontend build (Node.js + npm),
  native x86_64/aarch64 release builds, ShellCheck/YAML linting for packaging,
  installer asset sync check, secret scanning, non-blocking dependency audit,
  and a tag-driven release workflow that publishes `rast-linux-x86_64`,
  `rast-linux-aarch64`, `SHA256SUMS`, `install.sh`, `uninstall.sh` and
  `rast.toml.example`.

### Security

- Unprivileged HTTP server; all Docker/root operations are delegated to a
  separate worker unit over a Unix socket (mode 0660, group `rast`).
- Strict systemd sandboxing for both units (`ProtectSystem=strict`,
  `ProtectHome`, `PrivateTmp`, `PrivateDevices`, empty capability sets,
  `NoNewPrivileges`, syscall filtering, memory caps).
- Release archives are verified with SHA-256 checksums by both the installer
  and `rast update`; fail-closed signature verification hooks are available for
  operators who manage their own signing keys.
- Path resolution for file operations canonicalises every path under the user's
  root and rejects traversal/symlink escapes; previews of active content are
  served with a sandboxing CSP.
- Strict Content-Security-Policy for the embedded UI, cache-busting ETags and
  no-store headers for health endpoints.
- Master key (`secret.key`, 0600) with derived keys for CSRF tokens and
  AES-256-GCM encryption of application secrets at rest.

### Known limitations

- Release signing is not enabled yet (see [Unreleased](#unreleased)).
- The aarch64 release is **build-only**: it is compiled on a native arm64
  runner, but runtime testing is pending (see
  [docs/COMPATIBILITY.md](docs/COMPATIBILITY.md)).
- Only glibc-based systemd distributions are supported; musl/static builds and
  non-systemd init systems are not provided in 0.1.x.
- File Station, Applications and Resource Monitor screens are delivered
  incrementally during the 0.1.x series; the backend status of each subsystem is
  tracked in the repository issues and will be reflected here and in the README
  as it stabilises.
