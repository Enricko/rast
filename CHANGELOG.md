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

## [0.1.6] - 2026-11-11

A coherence and live-updates release: one control system, ten accent presets and
automatic synchronization for every window.

### Added

- **Ten accent presets** (Ocean Blue, Forest Green, Teal, Sage, Amber,
  Terracotta, Rose, Plum, Indigo, Graphite) with coordinated light/dark values,
  readable foregrounds, hover/pressed/focus/selection tokens and a small live
  preview. Available in Control Panel → Appearance and the desktop Personalize
  dialog; Ocean Blue is the default and settings persist per user.
- **Shared realtime layer**: one authenticated SSE connection per browser tab
  with sequence numbers, gap detection, heartbeats, exponential backoff with
  jitter, online/visibility/wake handling and `resync` snapshots. A
  subtle Live/Reconnecting/Offline indicator shows the real state and last
  successful update.
- **Live data everywhere**: File Station (own operations and external filesystem
  changes via a bounded watcher, quota/usage), Applications (deployment
  progress, container state, bounded live log stream), Resource Monitor,
  Backup Center (queued/running/completed/failed jobs and history), Activity
  Logs (new audit events), Control Panel (users, settings, shares, components)
  and the desktop (notifications, appearance).
- **Cross-session appearance sync**: changes saved in one session are applied in
  the user's other sessions; a session editing appearance gets a conflict
  notice instead of losing its draft.
- Server-side event authorization: scoped events (`user`, `admin`, `users`,
  broadcast), per-connection session revalidation (expiry, revocation,
  deactivation, role change), and no credentials in event URLs.

### Changed

- **One control system** (`styles/controls.css` + `styles/tokens.css`): shared
  default/hover/pressed/selected/focus-visible/disabled/loading states for
  buttons, icon buttons, window controls, segmented controls, inputs, selects,
  checkboxes, radios, switches, uploads, tabs, menus, context menus, tables,
  dialogs, toasts, tooltips, scrollbars and chart accents.
- **Neutral semantic light/dark palette** with solid application surfaces,
  readable text over any wallpaper, thin borders, restrained accents and soft
  shadows. Main surfaces stay neutral; status colours keep their meaning.
- **Appearance save workflow**: Save is disabled with “All changes saved” when
  the form is clean, enabled when dirty, shows Saving… and Saved states, inline
  validation and a retry path on request failure. Preview applies instantly to
  every open window; Cancel restores the saved appearance.
- Routine Refresh buttons were removed where realtime synchronization replaced
  them (File Station, Backup Center, Activity Logs, storage/components tabs);
  contextual Retry/Reconnect remains for genuine failures.
- Resource Monitor continues to use one shared backend sampler (default ~5 s);
  live updates add one SSE connection per tab instead of polling loops per menu.

### Fixed

- **Theme mode buttons no longer use browser-default styling.** The old
  hand-built Light/Dark/System buttons lacked the shared `.segment` class, so
  the browser painted its default `2px outset` border and grey fill. The mode
  and accent selectors now use the shared accessible segmented control and
  swatch components; a defensive reset prevents any unclassed button in a
  segmented group from leaking UA styles.
- Unselected theme/accent options are no longer styled as disabled; selected
  state is communicated by accent surface plus `aria-checked`.
- Legacy pastel/blue-tinted hardcoded control colours were replaced with
  semantic tokens across shell and applications, including Control Panel
  navigation, menus, tables, toasts and overlays.
- `appearance.changed` events update the user's other sessions, and audit
  events are published for authorized subscribers only.

## [0.1.5] - 2026-11-10

Appearance polish found while verifying v0.1.4 on a live deployment.

### Fixed

- **A freshly uploaded wallpaper did not appear in the Personalize dialog until
  it was reopened.** The editor refreshed its draft copy of the wallpaper list
  separately from the shared appearance service, so only the built-in gallery
  updated. The upload list now merges the service result immediately, so the new
  thumbnail is selectable (and previewed) at once.

## [0.1.4] - 2026-11-10

A visual and personalisation release: the desktop now follows a calm,
NAS-inspired visual language and every account can choose its own wallpaper and
theme, stored on the server.

### Added

- **Neutral NAS-inspired visual theme.** White windows and panels, `#F5F6F8`
  toolbars, `#DDE1E6` borders, dark neutral text (`#27313D` / `#657180`) and a
  single blue accent (`#2785D8`). The charcoal taskbar (`#303740`) keeps the
  desktop readable on any wallpaper; the light theme is now the default and the
  dark theme uses matching neutral charcoal surfaces. Oversized glows,
  decorative gradients, heavy blur and the background grid pattern are gone.
- **Original bundled nature wallpapers.** Alpine Dawn, Forest Mist, Coastal
  Calm and Desert Dusk (1920×1080 JPEG, CC0-1.0, generated for the project; see
  [docs/WALLPAPERS.md](docs/WALLPAPERS.md)).
- **Per-user appearance settings stored in SQLite.** Wallpaper source (bundled
  scene, solid colour or private upload), fit (cover/contain/center), position,
  darkening overlay and theme are saved per account and survive restarts,
  sign-out/sign-in and browser changes. See `GET/PUT /api/appearance`.
- **Personalize dialog** (right-click the desktop → *Personalize…*, start menu,
  account menu or Control Panel → **Appearance**): live preview, Apply, Cancel
  and Reset to default, with the previous look restored when cancelling.
- **Private wallpaper uploads.** JPEG/PNG/WebP up to 10 MiB, validated by
  content (magic bytes + dimensions) and served only to the owner with
  immutable private caching; server-generated identifiers, per-user storage and
  ownership checks on read/delete. Limits: 20 images / 100 MiB per account.
- **Coloured application icons** with one consistent icon language: amber folder
  for File Station, blue grid for Applications, teal monitor, warm orange for
  Backup Center, slate gear for Control Panel and a document for Activity Logs.
- Documentation screenshots under `docs/screenshots/`.

### Changed

- Theme and wallpaper settings are consolidated into one server-backed
  appearance system; the previous browser-only wallpaper picker and the
  duplicate Appearance card in Profile are gone.
- Desktop icon labels wrap between words instead of truncating mid-word, and
  the "No applications open" card is smaller, calmer and no longer dominates
  the wallpaper.
- `GET /api/bootstrap` now reports whether the session cookie is valid, so a
  signed-out boot no longer probes a protected endpoint (and no longer logs a
  spurious 401 in the browser console).
- Additive database migration `0002_appearance.sql` (new tables only); existing
  installations upgrade automatically on start.

### Fixed

- Personalisation cache is cleared on sign-out and session expiry so a previous
  account's wallpaper never appears for the next user on a shared browser.

## [0.1.3] - 2026-11-10

Self-update fix found by running the real update flow on a live installation.

### Fixed

- **`rast update` failed with `downloaded binary is not executable`.** The
  release asset is downloaded to a temporary directory with mode `0644`; the
  version self-check and the atomic installation step then tried to execute it.
  The artifact is now made executable (`0755`) immediately after its checksum is
  verified, before it is probed or installed.

## [0.1.2] - 2026-11-10

Deployment fix found while verifying managed application deployment on a real
installation. No schema or API changes.

### Fixed

- **Managed applications could not be deployed when the privileged worker ran as
  its own service account.** The Applications API wrote `compose.yaml`/`.env`
  with owner-only permissions (`0600`) and the app directory `0700`, but the
  Docker worker runs as `rast-worker` (group `rast`) and therefore could not read
  the definitions; validation failed with `application '<slug>' has no
  compose.yaml`. Definitions are now written `0640` inside a `0750` directory so
  the worker can validate and deploy them without granting write access.
  (Deploying on the same account as the web server — the earlier verification
  setup — accidentally hid this.)

## [0.1.1] - 2026-11-10

Maintenance release: two production-deployment fixes found while verifying the
v0.1.0 installation on a real host. No schema, API or data changes.

### Fixed

- **`rast update` ownership of pre-update backups.** Because the update command
  runs as root, the database snapshot (and any SQLite sidecar files it touched)
  could end up owned by root inside the service data directory. They are now
  handed back to the account that owns the data directory, so retention pruning
  and later writes by `rast.service` keep working.
- **`rast doctor` false alarms on a healthy installation.** The listen-address
  check reported a failure when the running service legitimately held the port,
  and the Docker check probed the unprivileged Docker CLI instead of the
  privileged `rast-worker.service`/socket that the web server actually uses.
  Both checks now describe the real state (and `doctor` exits 0 on a healthy
  deployment).

## [0.1.0] - 2026-11-10

Initial public release. RAST replaces a pile of ad-hoc scripts and web tools
with one self-hosted binary: a desktop-like web UI, a REST API, a CLI and
hardened systemd units for a home or small-office Linux server. Includes the
File Station, Applications (Docker Compose), Resource Monitor, Control Panel,
Backup Center and Activity Logs apps, all backed by the same SQLite database
and embedded frontend — no runtime Node.js, database server or external web
server required.

### Added

- **Single binary distribution.** Axum HTTP server, SQLite metadata store and
  the built frontend (TypeScript + Vite, embedded via `rust-embed` with a strict
  CSP) in one `rast` executable for Linux x86_64 and aarch64, plus hardened
  `rast.service` and `rast-worker.service` systemd units.
- **CLI**: `rast serve [--bind]`, `rast status [--json]`, `rast doctor`,
  `rast user create-admin`, `rast user list`, `rast user reset-password`,
  `rast setup-token`, `rast backup [--scope …] [--out DIR] [--note TEXT]`,
  `rast restore --from FILE [--dry-run] [--yes]`, `rast update [--version X]
  [--check]`, `rast version`, and the internal `rast worker`.
- **Authentication and accounts**: Argon2id password hashing (19 MiB, 2
  iterations, 1 lane), password policy (minimum 10 characters), opaque
  server-side sessions stored only as SHA-256 hashes with sliding expiry and a
  30-day absolute cap, per-session revocation and "sign out everywhere", CSRF
  tokens bound to the session, `HttpOnly`/`SameSite=Lax` cookies with automatic
  `Secure` when `public_url` is HTTPS, login rate limiting (per IP and per
  account), persistent lockout after repeated failures, no default credentials
  and no public registration. First administrator via CLI or a one-shot
  `rast setup-token` (60 minutes, single use).
- **File Station**: home and shared-folder roots with breadcrumbs, list/grid
  views, sorting, pagination and bounded search; streaming uploads (single
  request and resumable chunked upload via init/chunk/complete) and streaming
  downloads with `Range` support; basic operations (mkdir, rename, copy, move,
  delete); sandboxed previews for images, text and PDF with a restrictive CSP;
  transactional quota accounting (`used`/`reserved` bytes per user, `0` =
  unlimited); recycle bin with restore, permanent delete and empty; shared
  folders with per-user `read`/`write`/`admin` ACLs; usage statistics; every
  mutation audited.
- **Applications (Docker Compose)**: deploy, start, stop, restart, redeploy,
  inspect logs and deployment history for managed apps; compose validation
  (bind mounts restricted to the app data directory, host namespaces, devices
  and privileged mode refused) and encrypted per-app environment secrets that
  are masked in API responses; detection of unmanaged containers with an
  explicit import flow (never silently adopted); delete with a deliberate
  keep-data vs remove-volumes choice. All Docker access goes through
  `rast-worker.service` over a Unix socket; the web server never touches
  `/var/run/docker.sock` and the worker runs only whitelisted
  `docker`/`docker compose` operations with structured arguments, no shell, and
  per-operation timeouts.
- **Resource Monitor**: one process-wide sampler for CPU/load, memory/swap,
  uptime, disks, network throughput and (when Docker is available) container
  stats; bounded in-memory history; live updates over Server-Sent Events with
  stale/unavailable states instead of falling over.
- **Backup Center**: self-contained `.tar.gz` archives with a checksummed
  manifest and `db` (consistent `VACUUM INTO` snapshot), `config` (including
  `secret.key`), `users` and `apps` scopes; verification, download, deletion;
  hourly/daily/weekly schedules evaluated in the server's local timezone with
  configurable retention (default schedule disabled: daily 03:00, keep 7);
  integrity-checked restore that refuses to run while the service is active and
  keeps a pre-restore copy of the database.
- **Control Panel and Activity Logs**: profile, password change, active
  sessions, theme; user management with roles and quotas (with safety checks
  around the last administrator); shared-folder management; storage and
  component health; audit-log browsing with filters, pagination, row details
  and CSV export.
- **Web desktop**: window manager, taskbar, start menu, desktop icons, light
  and dark themes, keyboard navigation and accessibility labels, responsive
  mobile layout, toasts, session-expiry handling that preserves context;
  real-time updates over SSE without unbounded DOM growth.
- **Operations**: `rast update` verifies release `SHA256SUMS`, checks the
  downloaded binary version, takes a database backup, replaces the binary
  atomically, restarts the service and rolls the binary back if health checks
  fail; migrations are compiled into the binary and applied at startup
  (forward-only), with the schema version reported by `rast status`/`doctor`.
- **Packaging and deployment**: POSIX-sh installer usable as
  `curl … | sudo sh` (architecture/OS/systemd detection, SHA-256 verification,
  optional fail-closed minisign/cosign hook, idempotent upgrades preserving
  `/etc/rast`, `/var/lib/rast` and `/srv/rast`, optional Docker worker setup,
  peer-uid restriction on the worker socket) and a safe uninstaller
  (data-preserving by default, typed confirmation for `--purge`); fully
  documented configuration template; nginx, Caddy and Cloudflare Tunnel
  examples; compatibility matrix; security policy; third-party license
  inventory; `rust-toolchain.toml` pinning the build toolchain.
- **CI/CD and testing**: `cargo fmt`, strict `cargo clippy -D warnings` and the
  full test suite (199 unit/integration tests covering safe-path escapes,
  quota races, ACL enforcement, trash restore, worker protocol validation,
  authentication flows and API end-to-end paths), frontend type-check/build
  with Node.js + npm, native x86_64/aarch64 release builds, ShellCheck/YAML and
  installer-asset sync checks, secret scanning, dependency audit reporting, and
  a tag-driven release workflow that publishes `rast-linux-x86_64`,
  `rast-linux-aarch64`, `SHA256SUMS`, `install.sh`, `uninstall.sh` and
  `rast.toml.example` with release notes extracted from this file. A
  clean-environment installer test (`scripts/test-installer-clean.sh`, 64
  checks) installs, upgrades and uninstalls RAST inside a disposable systemd
  container built from the official Ubuntu 24.04 image.

### Security

- Unprivileged HTTP server (loopback bind by default, forwarded headers trusted
  only from configured proxy CIDRs); Docker/root operations delegated to a
  separate worker account over a Unix socket (mode `0660`, group `rast`,
  optional peer-uid allowlist).
- Strict systemd sandboxing for both units: `ProtectSystem=strict` with explicit
  `ReadWritePaths`, `ProtectHome`, `PrivateTmp`, `PrivateDevices`, empty
  capability bounding sets, `NoNewPrivileges`, syscall-group filtering, memory
  limits and `UMask=0027`.
- Per-user filesystem isolation: every path resolved and canonicalised under
  the caller's root, with traversal and symlink-escape rejection before any
  filesystem call; previews of user-supplied active content are served with a
  sandboxing CSP and never execute in the application origin.
- Secrets: master key at `<data_dir>/secret.key` (0600) with derived keys for
  CSRF and AES-256-GCM encryption of application secrets at rest; session
  tokens hashed in the database; release archives verified against
  `SHA256SUMS` by both the installer and `rast update`.
- Installer and uninstaller never modify firewall, DNS, TLS or reverse-proxy
  configuration, and require explicit confirmation before deleting any data.

### Known limitations

- **Release signing is not enabled yet.** Integrity relies on HTTPS plus the
  published `SHA256SUMS`; the installer's fail-closed `RAST_REQUIRE_SIGNATURE`
  (minisign/cosign) path exists for operators who manage their own keys.
- **aarch64 is build-only.** The official `rast-linux-aarch64` binary is built
  on a native arm64 runner, but runtime testing on arm64 hardware is still
  pending; x86_64 is the tested platform. The arm64 build also needs glibc
  >= 2.39 (built on Ubuntu 24.04), while x86_64 needs glibc >= 2.35.
- **glibc + systemd only.** No musl/static build is published, and the
  installer/service units assume systemd. Other init systems are unsupported
  (the binary can run under a custom supervisor, but that is not a supported
  configuration).
- **Docker app path is not exercised against a live daemon in CI.** The worker
  protocol, path/slug/compose validation and Docker-manager error paths are
  unit-tested with a fake worker socket; a live-Docker end-to-end test is not
  part of CI (the clean-environment installer test installs the worker only
  when Docker is present).
- **Proxy request-body limits still apply.** The File Station supports chunked
  resumable uploads for environments with per-request limits (e.g. Cloudflare
  plans cap request bodies), but a proxy's per-request limit is outside RAST's
  control and the chunk size must be configured below it; direct large uploads
  are also bounded by `RAST_MAX_UPLOAD_BYTES`.
- **Not implemented yet:** two-factor authentication, passkey/WebAuthn login,
  remote audit-log forwarding/SIEM integration, and cryptographically chained
  (tamper-evident) audit records. Audit logs are append-only by convention and
  can be altered by an administrator with database access.
- **Known dependency advisory.** `RUSTSEC-2026-0459` (`libsqlite3-sys` 0.37.0,
  CVE-2026-119375) affects the bundled SQLite WAL implementation; the patched
  `>=0.39.0` is blocked by `sqlx 0.9.0`'s dependency constraint and needs an
  upstream update. CI ignores exactly this advisory (any other advisory fails
  the build) and the exposure/limits are documented in
  [SECURITY.md](SECURITY.md#known-dependency-advisory-01).
- Frontend build requires Node.js >= 22 and npm (build-time only); the
  installed server needs neither.

