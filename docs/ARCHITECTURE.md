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

              ┌───────────────────────────┐
              │    rast-native.service    │
              │  User=root                │
              │  `rast native-helper`     │
              │  Unix socket /run/rast/native.sock (0660 root:rast)
              └────────────┬──────────────┘
                           │  systemd transient units in rast-linux.slice
                           ▼
              systemd: rast-xpra-u<id>  +  rast-app-u<id>-<app>-<n>
                           │
                           ▼
              Xpra seamless session (loopback TCP)  ←  native apps (Chrome, …)
                           ▲
                           │  HTTP + WebSocket, same-origin and per-user
                           │  authorized, proxied at /linux-apps/session/<key>/
                           │
                    rast.service (the branch above)

        SQLite + files:                    user files:
        /var/lib/rast                      /srv/rast
```

The web server **never** opens the Docker socket and never runs privileged
commands. The worker accepts a strict JSON protocol with structured arguments
and executes only whitelisted `docker` / `docker compose` operations:
Compose lifecycle and logs for managed apps (always the `rast-<slug>` project
in `<data_dir>/apps/<slug>`), and for the Docker app's engine view
`start`/`stop`/`restart`/`rm --force` (never `-v`) and `logs` by hex container
id, `pull`/`image inspect`/`image rm` (never forced) by a validated image reference,
`image prune`, and `volume rm` (never forced) by a validated volume name.
Arguments that could look like options are refused, and `--` ends option
parsing before every image or volume name.

Container terminals (`container-exec`) are the one long-lived operation. The
worker re-inspects the container, refuses it unless it is running and passes
the terminal policy (not privileged, no host namespaces, devices, dangerous
capabilities, or bind mounts of system folders or the Docker socket), then
runs `docker exec -i -t <full id> sh -c <fixed script>` (bash when the image
has it, else sh) on a pseudo terminal it owns. After the JSON reply the same
connection carries binary frames `[kind u8][length u32 BE][payload]`: 1 data,
2 resize (cols, rows as u16 BE), 3 exit status (i32 BE), 4 close; payloads are
at most 64 KiB. At most 4 terminals run at once, a terminal without keyboard
input for 60 minutes is closed, and closing the connection hangs up the shell.
The web server bridges each session to a WebSocket (see `docs/API.md`).

## Processes and systemd units

| Unit | Command | User / groups | Purpose |
| --- | --- | --- | --- |
| `rast.service` | `/usr/local/bin/rast serve` | `rast:rast` | HTTP API, embedded UI, SQLite, file storage, background jobs, backup scheduling, SSE event stream |
| `rast-worker.service` | `/usr/local/bin/rast worker` | `rast-worker`, primary group `rast`, supplementary group `docker` | Privileged Docker operations for the Applications app and for legacy Docker-backed Linux apps |
| `rast-native.service` | `/usr/local/bin/rast native-helper` | `root` | Privileged native helper: per-user Linux identities, transient systemd units (session + applications, cgroup limits), admin-approved APT transactions, managed profile paths |
| `rast-xpra-u<id>` (transient) | `/usr/bin/xpra start :<display> …` | `rast-u<uid>` (unprivileged identity) | One Xpra seamless graphical session per RAST user, lazily started |
| `rast-app-u<uid>-<app>-<n>` (transient) | the app's launcher argv | `rast-u<uid>` | One native application process group inside that person's session |

The units are installed from [`deploy/`](../deploy):
[`rast.service`](../deploy/rast.service),
[`rast-worker.service`](../deploy/rast-worker.service),
[`rast-native.service`](../deploy/rast-native.service) and
[`rast-linux.slice`](../deploy/rast-linux.slice). The worker is optional and
only installed when Docker is present (or requested explicitly); the native
helper is optional and only installed when Xpra is present (or requested with
`RAST_NATIVE=1`).

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
| `src/worker/` | Privileged worker: Unix-socket JSON-lines server, whitelisted actions; `linux.rs` also builds the legacy app images and exports legacy data volumes for migration |
| `src/native/` | Privileged native helper: JSON-lines protocol with strict validation (`protocol.rs`), client for the web server (`client.rs`), root daemon (`daemon.rs`), per-user Linux identities (`identity.rs`) |
| `src/linux_apps/native_session.rs` | Per-user shared Xpra session manager: lazy start, application launch, idle/session lifecycle, restart recovery, systemd cgroup limits |
| `src/linux_apps/native_proxy.rs` | Same-origin HTTP + WebSocket proxy for `/linux-apps/session/<key>/`, per-user authorization, shim injection |
| `src/linux_apps/native_apps.rs` | Native installation, update, uninstall and Docker→native migration service (APT, portable archives, AppImage, compatibility preview, profiles) |
| `src/linux_apps/native_archive.rs` | Path-safe archive extraction, desktop-entry parsing, launcher/icon detection for portable applications |
| `src/backup/` | Tar.gz backup archives (`db`, `config`, `users`, `apps` scopes), verification, restore, scheduling/retention |
| `src/packages/` | App Center: manifest schema and validation (`manifest.rs`), signed catalog fetch/verify with anti-rollback (`catalog.rs`), origin-checked downloads (`net.rs`), safe tar.gz extraction (`archive.rs`), staged install, checks and atomic activation (`install.rs`), the sandbox for version checks (`sandbox.rs`), detection of the runtimes the operating system already has (`system.rs`), the login-shell PATH entry and runtime permissions (`shell.rs`), GitHub release inspection (`github.rs`), host detection, persistence and the `PackageManager` service (jobs, events, audit) |
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


## Access policy

Group policy lives in `groups`, `group_members`, `group_folder_rules`,
`group_app_rules` plus per-user `user_folder_rules`/`user_app_rules`
(migration `0004`, additive; migration `0005` keys folder rules by the stable
`shares.id` with `ON DELETE CASCADE`, so renames never touch rules). A single evaluator (`src/access/mod.rs`) is used
by API enforcement, storage resolution, SSE filtering, the `/api/auth/me`
launcher permissions and the admin Effective Access view, so the UI always
explains the decision the backend actually makes. Decisions are cached per
account/resource and invalidated by a generation counter after every committed
membership/rule/share change. Legacy `share_members` produces the fallback
level when no new rule grants access, which preserves existing access on
upgrade; share creators get no implicit grant. `src/access/review.rs` builds
the administrator views (paginated Effective Access with conflict
explanations, group member pages, group-deletion impact) on top of the same
evaluator and its pure precedence helpers. Read-only Applications/Backup
grants expose status metadata only (no environment values, container logs,
archive downloads or host paths); Resource Monitor container inventory is
redacted per account and per live sample. See `docs/GROUPS.md` for the full
contract.

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
adds `accent`, migration 0006 adds `widgets`, migration 0007 adds
`widgets_customized` and `taskbar_pins`): mode, accent id, wallpaper source
(bundled scene, private upload or solid colour), fit, position, overlay, the
enabled desktop widgets with their preferred position, whether the account ever
chose its widgets, and its ordered taskbar pins (JSON arrays checked by SQLite,
re-validated and sanitised on read). Wallpapers are validated by content
and stored under `<data_dir>/wallpapers/<user-id>/<uuid>.<ext>`; the owner-only
API serves them as `private, no-cache` with an ETag: every use revalidates with
the current session (304 for the owner), so another account signing in to the
same browser cannot reuse a cached copy. The client keeps only a small
per-user paint cache.

Writes are partial and explicit. `PUT /api/appearance` merges only the fields
it receives (serialised server-side, so concurrent partial saves from two tabs
never drop each other's field). The tray toggle and account menu send only
`{theme}`; the Appearance editors send only the fields that differ from the
saved baseline. Nothing is saved implicitly (no store subscription writes the
theme). The client `AppearanceService` is scoped to the signed-in account: it
tracks whether its value is server-confirmed (`ready`), the account's own paint
cache (`cached`) or unknown (`error`, defaults for painting only), drops any
response that arrives after sign-out or an account switch, and the editors
refuse to open on an unknown state (Control Panel shows Retry instead).

## Desktop widgets

`frontend/src/shell/widgets.ts` renders System Health, CPU & Memory, Storage
and Network cards on the desktop (desktop menu → **Widgets…**). They read the
same data as Resource Monitor: one shared `MonitorFeed` loads
`/api/monitor/snapshot` and `/api/monitor/history`, then follows the existing
`monitor` SSE broadcast, so widgets add no polling and no new endpoint and get
the same per-account access check and container redaction. Feed states come
from the data itself: `loading`, `live`, `stale` (no sample for
`max(15 s, 3 × interval)`; the card dims and shows the age), `unavailable` (the
monitor reports it cannot sample, with its reason), `denied` (no Resource
Monitor access or a `403`; no data is requested) and `error` (retryable).
Positions are stored as fractions of the free desktop area and clamped on every
resize, so cards stay inside the usable area above the taskbar. Layout changes
are saved through the coalesced `saveWidgets` path, which sends only
`{widgets}`. An account that never saved a list (`widgetsCustomized: false`)
sees System Health (top right) and CPU & Memory (bottom right) if it may use
Resource Monitor; nothing is stored until the user changes them, and removing
both is stored as an empty list.

## Taskbar

Pinned applications are part of the account's appearance (`taskbarPins`,
default File Station, Resource Monitor, Control Panel) and follow it to any
browser. `frontend/src/shell/taskbar-order.ts` gives every pinned application a
fixed slot in the user's order: opening it shows its window(s) in that slot
instead of moving it, and windows of unpinned applications follow the pins in
opening order. Pin and unpin from a taskbar button's or desktop icon's menu;
changes are saved through the coalesced `saveTaskbarPins` path (`{taskbarPins}`
only).

## Storage layout

```
/usr/local/bin/rast                  binary (0755 root:root)
/usr/local/lib/rast/rast-<version>   previous binaries kept by the installer (rollback)
/etc/rast/rast.toml                  configuration (0640 root:rast)
/etc/rast/rast.env                   optional env overrides (0600 root:rast)
/var/lib/rast/                       data dir (0750 rast:rast; 0751 with packages_shell_path)
  rast.db  rast.db-wal  rast.db-shm  SQLite database (WAL)
  secret.key                         master key, 0600, generate-on-first-start
  backups/                           backup archives + pre-restore copies
  apps/<slug>/compose.yaml            managed Docker Compose definitions
  apps/<slug>/.env                    encrypted app secrets (rendered on deploy)
  tmp/                               transient backup/restore work dirs
  linux-apps/<id>/                   legacy: Linux app file being built into an image (removed after the build)
  linux-apps/apps/<id>/<version>/    native portable/AppImage installs (managed, versioned; read-only for app users)
  linux-apps/profiles/<app>/<user>/  one native application profile per person (0700, owned by their identity)
  linux-apps/staging/<uuid>/         uploads/downloads being inspected or installed
  linux-apps/migrations/<app>/<user> Docker→native profile exports (retained until cleanup)
  linux-users/<user_id>/             unprivileged Linux identity homes (0700 rast-u<uid>)
/run/rast-linux/<uid>/               native session runtime dir (0700; Xpra sockets, logs, Xauthority target is the home)
/run/rast/native.sock                native helper socket (0660 root:rast)
  packages/                          App Center (0751 with packages_shell_path)
    runtimes/<id>/<version>/         one directory per installed version (readable by all
                                     users with packages_shell_path; 0750 otherwise)
    runtimes/<id>/current            symlink to the RAST default version
    bin/<executable>                 shims that follow `current`; first on every login
                                     shell's PATH via /etc/profile.d/rast-packages.sh
    catalog/catalog.json(.sig)       last verified catalog (used when offline)
    .staging/  .trash/               install staging and removal, cleaned at start
                                     (a replaced version tree waits in .trash/displaced.*
                                     until the new one is recorded; restored after a crash)
/srv/rast/                           storage dir (0755 rast:rast)
  homes/<username>/                  per-user home, 0700
  shared/<name>/                     shared folders (ACL in DB)
  .trash/<user_id>/<uuid>/           recycle bin (root 0700, per-user 0700)
/etc/profile.d/rast-packages.sh      App Center bin/ first on PATH (0644 root, written by
                                     `rast packages path sync`; absent when packages_shell_path = false)
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
| `linux_apps`, `linux_identities`, `linux_sessions`, `linux_runs` | Installed desktop apps (backend, native metadata, package ownership and migration state), one unprivileged identity per user, the running shared session and its application runs |
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
| RAST ↔ worker | Unix socket, mode 0660, group `rast`, optional peer-uid allowlist (`RAST_WORKER_ALLOWED_UID`, installer sets it), JSON-lines protocol, action whitelist, strict argument validation, timeouts; terminal sessions switch the connection to bounded binary frames |
| Browser ↔ terminal | WebSocket, admin only, same-origin `Origin` required, CSRF token in the first message, session re-checked every minute, terminal policy checked by the API and again by the worker, every session audited |
| Worker ↔ Docker | Worker is in the `docker` group — practically root-equivalent; it runs no shell and only whitelisted operations |
| RAST ↔ filesystem | Per-user roots resolved and canonicalised under `storage_dir`; traversal and symlink escapes rejected on every operation |
| RAST ↔ data dir | Contains the SQLite DB and master key; 0750 `rast:rast`, master key 0600 |
| RAST ↔ package sources | Official catalog accepted only with a valid Ed25519 signature from a trusted key and a sequence not lower than the last accepted one; artifacts downloaded over HTTPS from the catalog's allowed hosts (redirects re-checked), size and SHA-256 verified before extraction; extraction refuses absolute paths, `..`, devices and links leaving the version directory; extension records over 64 KiB and archives that would leave less than 512 MiB free are refused. Declared version checks execute the package's own files as the `rast` user, without a shell, in a sandbox: own process group (killed on timeout and after exit; `setsid`/`setpgid` denied), seccomp (no sockets, io_uring or ptrace), Landlock (read/execute only in the package and system library directories, write only to a scratch home, TCP denied), no core dumps, 64 MiB file limit, output to files. GitHub imports are third-party code: Landlock is mandatory for them, they are pinned to the inspected release and digests, bound to one repository and publisher (re-checked at install time and in the same transaction that records the install), and can never take an official package id or command name. Expired catalogs drive no installs |
| Browser ↔ Linux apps | Managing apps is administrator-only; every signed-in user only reaches their own runs. **Native backend:** each RAST user maps to a distinct unprivileged host identity; one Xpra session per user; the Xpra port is bound to `127.0.0.1` with Xpra authentication disabled and is never exposed; RAST proxies HTTP+WebSocket under `/linux-apps/session/<128-bit key>/` and checks the RAST session cookie on every request (another user's key → 403, expired session → 401, same-origin `Origin` required for WebSockets), injecting its window-integration shim into same-origin pages. Applications inside one person's session share a trust boundary; filesystem isolation between users rests on distinct uids, 0700 profiles and no cross-user access (the person's RAST home is shared with *their* identity through an ACL). Chromium runs unprivileged with its sandbox enabled. The privileged helper is the only root component: socket 0660 root:rast, peer-uid allowlist, strict per-op validation, no shell, arguments/commands from validated fields only, bounded timeouts, APT transactions serialized and admin-authorized. Session keys and credentials are never logged. **Legacy Docker backend:** each person's copy is a container with its own settings volume and only their RAST home mounted; no privileged mode, `no-new-privileges`, capabilities dropped, memory/CPU/PID limits, loopback-only port; the frame is served under `/linux-apps/<128-bit key>/` with a sandbox CSP |
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
| Native Linux apps | one shared session per user: 4 GiB memory / quarter of the CPUs (2–8) / 2048 tasks by default; per application: 2 GiB / 2 CPUs / 1024 tasks (App Center → the app's page → Memory and CPU); admission requires free memory ≥ the app's limit + 1 GiB reserve | `linux_session_*`, `linux_app_*`, `linux_admission_reserve_mb` |
| Legacy Docker Linux apps | per copy: 4 GiB memory and 6 CPUs by default (per app, adjustable), 4096 PIDs; one copy per person per app | App Center → the app's page → Memory and CPU |
| Binary size | release profile: `opt-level=z`, thin LTO, stripped symbols | `Cargo.toml` release profile |

Disk growth comes from user files (`storage_dir`), backup archives
(`data_dir/backups`), the trash directory, native app installs and profiles
(`data_dir/linux-apps`) and — while legacy Docker apps remain — their images
(the shared base is about 3.7 GB, downloaded once; each app adds roughly
1 GB; plus Docker's build cache). Backups are compressed tar.gz
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
