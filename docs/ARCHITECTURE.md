# Architecture

RAST is a self-hosted server administration toolkit shipped as **one Rust
binary** with an embedded web desktop. This document describes how the pieces
fit together, where the security boundaries are, what lives on disk and which
resource limits apply. For configuration keys see
[CONFIGURATION.md](CONFIGURATION.md); for deployment procedures see
[INSTALL.md](INSTALL.md) and [OPERATIONS.md](OPERATIONS.md).

> Feature status and known limitations are tracked per release in
> [CHANGELOG.md](../CHANGELOG.md). 0.1.0 ships the complete 0.1 feature set:
> authentication, users, audit, jobs, events, backups, self-update, the File
> Station, Applications/Docker worker, Resource Monitor, Control Panel and
> Activity Logs.

## Design constraints

* **Single binary, no runtime dependencies.** The frontend is built with
  Node.js + npm at release time and embedded into the executable with
  `rust-embed`; the installed server needs neither Node.js nor a separate static
  file server.
* **No default credentials, no privileged web process.** The HTTP server runs
  as the unprivileged `rast` user. Docker operations are delegated to a
  separate worker unit that holds the `docker` group.
* **Boring storage.** Metadata in SQLite (WAL mode) on local storage, user files
  on a normal filesystem. No object store, no external database, no message
  broker.
* **One administrator-controlled deployment.** RAST is not multi-tenant SaaS:
  users are trusted to the extent the operator chooses, and the filesystem
  permissions of the service user are the final backstop.

## Runtime topology

```
                    Internet / LAN
                          │
                  TLS termination
             (Cloudflare Tunnel and/or nginx / Caddy)
                          │  HTTP, loopback only
                          ▼
              ┌───────────────────────────┐
              │        rast.service       │
              │  User=rast  Group=rast    │
              │  /usr/local/bin/rast serve│
              │  ┌─────────────────────┐  │
              │  │  Axum HTTP server   │  │
              │  │  (API + embedded SPA)│ │
              │  └─────────┬───────────┘  │
              └────────────┼──────────────┘
                           │  Unix socket /run/rast/worker.sock (0660)
                           ▼
              ┌───────────────────────────┐
              │     rast-worker.service   │
              │  User=rast-worker         │
              │  Group=rast               │
              │  SupplementaryGroups=docker│
              │  `rast worker`            │
              └────────────┬──────────────┘
                           │  /var/run/docker.sock (root:docker)
                           ▼
                    Docker Engine

        SQLite + files:                    user files:
        /var/lib/rast                      /srv/rast
```

The web server **never** opens the Docker socket and never runs privileged
commands. The worker accepts a strict JSON protocol with structured arguments
and executes only whitelisted `docker` / `docker compose` operations.

## Processes and systemd units

| Unit | Command | User / groups | Purpose |
| --- | --- | --- | --- |
| `rast.service` | `/usr/local/bin/rast serve` | `rast:rast` | HTTP API, embedded UI, SQLite, file storage, background jobs, backup scheduling, SSE event stream |
| `rast-worker.service` | `/usr/local/bin/rast worker` | `rast-worker`, primary group `rast`, supplementary group `docker` | Privileged Docker operations for the Applications app |

Both units are installed from [`deploy/`](../deploy):
[`rast.service`](../deploy/rast.service) and
[`rast-worker.service`](../deploy/rast-worker.service). The worker is optional
and only installed when Docker is present (or requested explicitly).

`rast.service` is the only unit with `[Install] WantedBy=multi-user.target` in
the default profile; the worker has `Wants=docker.service` (not `Requires`) so
boot does not fail when Docker is absent, and a `ConditionPathExists` guard
keeps it inactive in that case.

## Code map

| Path | Responsibility |
| --- | --- |
| `src/main.rs` | CLI parsing, `serve` bootstrap, `status`, `doctor`, user management, restore entry point |
| `src/config.rs` | Layered configuration (defaults → TOML → `RAST_*` env → CLI flags), validation, warnings |
| `src/state.rs` | Shared `AppState` (config, DB pool, secrets, event bus, jobs, monitor, storage, Docker client), maintenance timer |
| `src/server.rs` | Embedded static assets (`rust-embed`), cache headers, ETag, CSP, SPA fallback |
| `src/api/` | Axum router and handlers; auth, users, files, apps, monitor, jobs, logs, settings, backup, system, events |
| `src/auth/` | Argon2id passwords, opaque server-side sessions, CSRF, rate limiting and lockout, client-IP resolution, middleware |
| `src/storage/` | Safe path resolution, quota accounting, recycle bin, streaming file operations, shared-folder ACLs |
| `src/jobs.rs` | Persistent background job queue with bounded concurrency, progress reporting and cooperative cancellation |
| `src/events.rs` | In-process broadcast bus for Server-Sent Events (`/api/events`) |
| `src/monitor/` | One process-wide resource sampler (CPU, memory, disks, network, containers) |
| `src/docker/` | Worker-socket client, compose validation, deployment metadata |
| `src/worker/` | Privileged worker: Unix-socket JSON-lines server, whitelisted actions |
| `src/backup/` | Tar.gz backup archives (`db`, `config`, `users`, `apps` scopes), verification, restore, scheduling/retention |
| `src/update.rs` | GitHub-Release self-update with checksum verification, DB backup, atomic binary replacement and rollback of the binary |
| `src/audit.rs` | Append-only audit log writer |
| `src/secrets.rs` | Master key at `<data_dir>/secret.key` and purpose-derived keys (CSRF, app-secret encryption) |
| `migrations/` | SQLx migrations compiled into the binary and applied at startup |

## Request flow

1. A browser talks to a TLS terminator (Cloudflare Tunnel, nginx, Caddy).
2. The proxy forwards HTTP to `127.0.0.1:8720` with `X-Forwarded-For` /
   `X-Forwarded-Proto` (and Cloudflare's `CF-Connecting-IP`). RAST only honours
   those headers when the immediate peer belongs to `trusted_proxies`.
3. Axum routes the request:
   * **public**: `GET /healthz`, `GET /api/bootstrap`, `GET /api/setup/status`,
     `POST /api/setup`, `POST /api/auth/login`;
   * **protected**: everything else under `/api`, behind the session + CSRF
     middleware; handlers additionally enforce object-level permissions
     (role, ownership, shared-folder ACL);
   * **static**: every other path serves the embedded SPA (unknown non-file
     paths fall back to `index.html`, so client-side routes survive a refresh).
4. Handlers use the DB pool, storage engine and job queue. Mutating endpoints
   write an audit record. Long operations (copy/move/delete trees, backups,
   deployments) are enqueued as jobs and report progress over SSE.
5. Responses that carry cookies set `Secure` automatically when `public_url` is
   HTTPS; static HTML is served with a strict Content-Security-Policy.

### Background work

* **Jobs** (`src/jobs.rs`): persisted rows in `jobs`, bounded concurrency
  (`RAST_JOB_CONCURRENCY`), progress throttled to one SQLite write per 250 ms,
  cooperative cancellation, stale-job recovery on startup.
* **Monitor** (`src/monitor/`): one sampler per process (not per browser tab),
  interval `RAST_MONITOR_INTERVAL_SECS`, published on the event bus and kept in
  a bounded in-memory ring buffer for `/api/monitor/history`.
* **Maintenance timer** (`src/state.rs`): every ~5 minutes prunes expired
  sessions, old audit rows (90 days), finished jobs (30 days), login attempts
  (7 days) and expired sessions; the backup scheduler evaluates due backup
  schedules using the server's local timezone.
* **Backup scheduler** (`src/backup/schedule.rs`): schedules stored in
  `backup_schedules`, retention count configurable in the UI; runs as
  background jobs so large archives do not block HTTP.

## Data flow: upload and download

* **Upload** is streamed: the request body is copied to the destination file
  while quota is reserved transactionally in `storage_usage`; the limit is
  `RAST_MAX_UPLOAD_BYTES` (and, for proxied deployments, the proxy's body
  limit — see [INSTALL.md](INSTALL.md)). Disconnects roll the reservation back.
* **Download** streams with `Range` support; the server never buffers whole
  files in memory.
* **Preview** is restricted by MIME type and served with `Content-Disposition:
  inline` plus a sandboxing CSP so user-supplied SVG/HTML cannot execute in the
  application origin.
* **Delete** moves the file tree into `.trash/<user_id>/<uuid>` and records it
  in the `trash` table; restore re-checks quota and conflicts.


## Live updates

One process-wide `EventBus` (`src/events.rs`) fans out to every authenticated
SSE connection (`GET /api/events`). Events carry a monotonic sequence number and
an authorization scope; the stream handler filters per principal and revalidates
the session every 20 seconds, so expiry, revocation, deactivation and role
changes close open connections. Clients (one connection per browser tab) detect
gaps, reconnect with backoff and refetch authorized snapshots.

Change sources:

* API mutations publish domain events after their database/filesystem change
  commits (files/quota, users/settings, jobs, backups, applications, appearance,
  audit).
* A bounded `notify` filesystem watcher covers only `storage_dir/homes` and
  `storage_dir/shared`, coalesces bursts and maps paths to authorized accounts
  (homes) or share members (shared); overflow triggers `files.resync`.
* A low-frequency Docker reconciler compares managed application state and
  emits `app.changed`; application logs stream through a bounded admin-only SSE
  endpoint.
* The resource monitor samples once per process (~5 s) and broadcasts snapshots.

The frontend keeps a small shared store (`frontend/src/core/realtime.ts`) with a
query cache; windows subscribe to event kinds or cache keys, debounce bursts and
update only the affected view, preserving scroll, selection, filters and
unsaved forms.

## Appearance storage

Per user in SQLite (`user_appearance` + `appearance_uploads`, migration 0003
adds `accent`): mode, accent id, wallpaper source (bundled scene, private upload
or solid colour), fit, position and overlay. Wallpapers are validated by content
and stored under `<data_dir>/wallpapers/<user-id>/<uuid>.<ext>`; the owner-only
API serves them with immutable private caching. The client keeps only a small
per-user paint cache.

## Storage layout

```
/usr/local/bin/rast                  binary (0755 root:root)
/usr/local/lib/rast/rast-<version>   previous binaries kept by the installer (rollback)
/etc/rast/rast.toml                  configuration (0640 root:rast)
/etc/rast/rast.env                   optional env overrides (0600 root:rast)
/var/lib/rast/                       data dir (0750 rast:rast)
  rast.db  rast.db-wal  rast.db-shm  SQLite database (WAL)
  secret.key                         master key, 0600, generate-on-first-start
  backups/                           backup archives + pre-restore copies
  apps/<slug>/compose.yaml            managed Docker Compose definitions
  apps/<slug>/.env                    encrypted app secrets (rendered on deploy)
  tmp/                               transient backup/restore work dirs
/srv/rast/                           storage dir (0755 rast:rast)
  homes/<username>/                  per-user home, 0700
  shared/<name>/                     shared folders (ACL in DB)
  .trash/<user_id>/<uuid>/           recycle bin (root 0700, per-user 0700)
/run/rast/worker.sock                worker socket (0660 rast-worker:rast)
/run/rast/                           runtime dir (0750 rast-worker:rast)
```

`data_dir` must live on a local filesystem with working SQLite locking; network
filesystems (NFS/SMB) are not supported. `storage_dir` should preferably be a
separate volume. Losing `secret.key` invalidates sessions and makes encrypted
application secrets unrecoverable — it is included in `config`-scope backups.

## Database schema

All timestamps are Unix epoch milliseconds. Migrations are embedded in the
binary (`sqlx::migrate!`) and applied at startup; the current schema version is
reported by `rast status`/`rast doctor` and stored in `_sqlx_migrations`.

| Table | Purpose |
| --- | --- |
| `users` | Accounts: Argon2id hash, role (`admin`/`user`), quota, lockout state |
| `sessions` | Opaque session tokens (SHA-256 hashed), TTL, revocation |
| `login_attempts` | Rate-limit and audit history for logins |
| `audit_logs` | Who did what, to which object, result, IP, detail |
| `settings` | UI-managed settings (registration flags, backup retention, …) |
| `shares`, `share_members` | Shared folders and per-user ACL (`read`/`write`/`admin`) |
| `storage_usage` | Quota accounting: used and reserved bytes per user |
| `trash` | Recycle-bin entries: original path, size, deletion time |
| `jobs` | Persistent background job queue and progress |
| `apps`, `app_deployments` | Managed Docker applications and deployment history |
| `backups`, `backup_schedules` | Backup archive metadata and recurring schedules |

**Migration policy:** migrations are forward-only. A newer binary may migrate an
older database automatically; an older binary refuses a database whose schema is
newer than it understands. Always take a backup before upgrading and read the
release notes (see [OPERATIONS.md](OPERATIONS.md#updates)).

## Security boundaries

| Boundary | Control |
| --- | --- |
| Browser ↔ TLS terminator | TLS terminated by the proxy; RAST only listens on loopback by default |
| Proxy ↔ RAST | Forwarding headers trusted only from `trusted_proxies` CIDRs |
| RAST ↔ worker | Unix socket, mode 0660, group `rast`, optional peer-uid allowlist (`RAST_WORKER_ALLOWED_UID`, installer sets it), JSON-lines protocol, action whitelist, strict argument validation, timeouts |
| Worker ↔ Docker | Worker is in the `docker` group — practically root-equivalent; it runs no shell and only whitelisted operations |
| RAST ↔ filesystem | Per-user roots resolved and canonicalised under `storage_dir`; traversal and symlink escapes rejected on every operation |
| RAST ↔ data dir | Contains the SQLite DB and master key; 0750 `rast:rast`, master key 0600 |
| Secrets at rest | Master key file, encrypted app secrets (AES-256-GCM), session tokens hashed at rest |
| systemd sandbox | `ProtectSystem=strict`, `ProtectHome`, `PrivateTmp`, empty capability sets, syscall blacklist, `NoNewPrivileges`, memory caps (see units) |

The threat model and operator hardening checklist live in
[SECURITY.md](../SECURITY.md).

## Resource budget

Design targets for a small home-server deployment (a 2 vCPU / 2 GiB host is
plenty). Tune with systemd drop-ins and configuration keys.

| Resource | Default | Control |
| --- | --- | --- |
| Web server memory | soft 512 MiB, hard 1 GiB | `MemoryHigh`/`MemoryMax` in `rast.service` |
| Worker memory | soft 256 MiB, hard 1 GiB | unit drop-in |
| Tokio runtime threads | 2–4 (`available_parallelism` clamped) | `src/main.rs::worker_threads` |
| SQLite connections | 8 | `RAST_DB_MAX_CONNECTIONS` (1–64) |
| Background jobs | 2 concurrent | `RAST_JOB_CONCURRENCY` (1–8) |
| Monitor sampling | one sampler per process every 5 s | `RAST_MONITOR_INTERVAL_SECS` (1–300) |
| SSE fan-out | broadcast buffer of 256 events | `src/events.rs::BUS_CAPACITY` |
| Job queue | 128 pending | `src/jobs.rs::QUEUE_CAPACITY` |
| File descriptors | 65536 | `LimitNOFILE` in both units |
| Uploads | streamed, never buffered whole | `RAST_MAX_UPLOAD_BYTES` |
| Binary size | release profile: `opt-level=z`, thin LTO, stripped symbols | `Cargo.toml` release profile |

Disk growth comes from user files (`storage_dir`), backup archives
(`data_dir/backups`) and the trash directory. Backups are compressed tar.gz
archives; retention keeps the newest N archives per the schedule. Monitor the
host with `/api/system/info`, `rast status --json` and `df -h`.

## Failure modes and recovery

| Failure | Behaviour | Recovery |
| --- | --- | --- |
| Process crash / power loss | SQLite WAL replays on next start; interrupted jobs are marked stale and recovered at startup | `systemctl start rast.service`; check `journalctl -u rast.service` |
| Worker down / misconfigured | Docker features return "unavailable"; the rest of the app keeps working | `systemctl status rast-worker`; check `/var/run/docker.sock` and `docker` group membership |
| Disk full | Uploads fail with an explicit error; SQLite writes fail loudly | free space, then `rast doctor` |
| Corrupt config | Service refuses to start with the parse error in the journal | fix `/etc/rast/rast.toml`, `systemctl restart rast` |
| Proxy misconfiguration | 502/504 to the browser; `/healthz` still works from the host | fix proxy; see [INSTALL.md](INSTALL.md#reverse-proxy-and-tls) |
| Database corruption | `rast doctor` / restore integrity checks fail | restore from the newest verified backup ([OPERATIONS.md](OPERATIONS.md#restore)) |

## Build and release pipeline

1. `.github/workflows/ci.yml` builds the frontend once (Node.js 22 + npm),
   runs `cargo fmt`, strict `cargo clippy -D warnings`, `cargo test`, builds
   release binaries for both architectures on native runners (x86_64 on Ubuntu
   22.04, aarch64 on `ubuntu-24.04-arm`), lints the shell scripts and checks
   the embedded installer assets are in sync.
2. `.github/workflows/release.yml` runs on a `v*` tag (or manual dispatch),
   cross-checks the version against `Cargo.toml` and `CHANGELOG.md`, builds the
   binaries with the embedded frontend, publishes:
   `rast-linux-x86_64`, `rast-linux-aarch64`, `SHA256SUMS`, `install.sh`,
   `uninstall.sh` and `rast.toml.example`.
3. The release `install.sh` is pinned to its own tag and verifies the
   `SHA256SUMS` checksum of the downloaded binary before installing anything.

## Non-goals

* No plugin/extension system; the binary is the product.
* No multi-node clustering, shared-nothing replication or external database.
* No support for non-systemd init systems (manual installation is documented,
  but service management assumes systemd).
* No musl/static builds are published in 0.1.x (see
  [COMPATIBILITY.md](COMPATIBILITY.md)).
