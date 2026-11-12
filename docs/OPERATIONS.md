# Operations

Day-2 procedures for a RAST installation: service management, monitoring,
backup, restore, updates and rollback. Architecture and security background is
in [ARCHITECTURE.md](ARCHITECTURE.md); configuration keys are in
[CONFIGURATION.md](CONFIGURATION.md).

Run every command as root (`sudo`) unless stated otherwise. Commands that
access the database should run as the service user so file ownership stays
consistent:

```bash
sudo -u rast rast <command>
```

## Service management

```bash
sudo systemctl status rast.service          # state, recent log lines
sudo systemctl start|stop|restart rast.service
sudo journalctl -u rast.service -f          # follow logs (stderr/journald)
sudo journalctl -u rast-worker.service -n 100 --no-pager

sudo rast status                            # version, paths, service, /healthz
sudo rast status --json                     # machine-readable
sudo -u rast rast doctor                    # config, permissions, DB, bind, Docker
curl -fsS http://127.0.0.1:8720/healthz     # liveness probe ("ok"/"degraded")
```

`rast.service` shuts down gracefully on `SIGTERM`: in-flight requests finish,
then the SQLite WAL is checkpointed. `systemctl stop` waits up to 30 seconds
(`TimeoutStopSec`). There is no configuration reload signal; restart the unit
after editing `/etc/rast/rast.toml` or `/etc/rast/rast.env`.

### Logs and rotation

RAST logs to stderr, which journald captures. Tune retention in
`/etc/systemd/journald.conf` (`SystemMaxUse=`, `MaxRetentionSec=`) and reload
with `systemctl restart systemd-journald`. Increase log detail with
`RAST_LOG_LEVEL=debug` (or `RUST_LOG=rast=debug,tower_http=info`) in
`/etc/rast/rast.env`, then restart the service. Debug logs can include client
IPs and paths — do not leave them on permanently.

## Monitoring and alerting

Recommended minimum:

* HTTP probe `GET /healthz` from an internal monitoring host (expects `200 ok`;
  returns `503 degraded` when the database is unreachable).
* `systemctl is-active rast.service` and `rast-worker.service` (when Docker is
  used).
* Disk space on `/var/lib/rast` (database, backups) and `/srv/rast` (user data).
* Certificate/tunnel expiry (proxy side), not RAST itself.

`rast status --json` exposes version, bind address, paths, service state and the
health probe result; `/api/system/info` and `/api/system/components` (admin
session required) expose host and component details for dashboards.

Watch these growth factors:

| Location | Grows with | Control |
| --- | --- | --- |
| `/srv/rast/homes`, `shared` | user uploads | quotas (`RAST_DEFAULT_QUOTA_BYTES`, per-user quotas), trash cleanup |
| `/srv/rast/.trash` | deleted files not yet purged | empty trash in the UI |
| `/var/lib/rast/backups` | backups | schedule retention (UI), manual deletion via Backup Center/API |
| `rast.db` | users, sessions, jobs, audit logs | automatic retention: audit 90 days, finished jobs 30 days, login attempts 7 days |
| journald | service logs | journald limits |

## Backup

### What is in a backup archive

`rast backup` produces a self-contained `tar.gz` with a `manifest.json` (format
version, schema version, scope, per-file SHA-256) and, depending on `--scope`:

| Scope | Contents |
| --- | --- |
| `db` | Consistent SQLite snapshot (`VACUUM INTO`, includes committed WAL data) |
| `config` | `/etc/rast/rast.toml`, `/etc/rast/rast.env` (if present) and `secret.key` |
| `users` | Copy of user homes and shared folders |
| `apps` | Managed application definitions (compose files + encrypted env metadata) |

Default scopes: `db,config`. Default output directory: `/var/lib/rast/backups`.

```bash
# create a backup (add users/apps for a full backup)
sudo -u rast rast backup --scope db,config --out /var/lib/rast/backups --note "before upgrade"

# verify an archive without restoring it
sudo -u rast rast restore --from /var/lib/rast/backups/rast-...tar.gz --dry-run
```

> **Sensitive material:** `config`-scope backups include `secret.key`, and
> full backups include user files. Treat archives like production credentials:
> store them off the server (pull them with a backup agent or `rsync`), encrypt
> them at rest, and restrict access. Losing `secret.key` invalidates sessions
> and makes encrypted app secrets unrecoverable; leaking it compromises those
> same secrets.

### Scheduled backups

The Backup Center exposes one schedule row (created disabled by default:
daily at **03:00 server-local time**, retention 7 archives, scopes `db,config`).
Enable it in the UI or via the API (`GET/PUT /api/backups/schedule`). Frequency
is `hourly`, `daily` or `weekly`; `retention_count` keeps the newest N archives
and deletes older ones (rows and files).

Schedules use the **server's local timezone** (`chrono::Local`). A container
with `TZ=UTC` will run "03:00" in UTC — set the host timezone deliberately.

### Backup best practice

1. Enable the scheduled backup for `db,config`.
2. Add an off-server copy job (pull `/var/lib/rast/backups` with restic/borg/
   `rsync` from another host) and keep at least one backup off-host.
3. Encrypt off-site copies; never commit archives to a Git repository.
4. Restore-test quarterly on a scratch host (see below).
5. Before **every** upgrade: `sudo -u rast rast backup --scope db,config --note "pre-upgrade"`.

### Manual SQLite safety (when the built-in backup is unavailable)

Never copy a live `rast.db` while the service is running: SQLite WAL means
`rast.db` alone may be missing committed transactions. Safe options:

```bash
# preferred: online consistent snapshot (SQLite >= 3.27)
sudo -u rast sqlite3 /var/lib/rast/rast.db \
  ".backup '/var/lib/rast/backups/manual-$(date +%Y%m%d-%H%M%S).db'"

# alternative: stop the service, then copy the database and sidecar files
sudo systemctl stop rast.service
sudo cp -a /var/lib/rast/rast.db{,,-wal,-shm} /path/to/backup/dir/ 2>/dev/null || true
sudo systemctl start rast.service
```

Also back up `/etc/rast/` and `/var/lib/rast/secret.key` (copying the WAL
sidecars while the service is stopped is important; `cp` errors for missing
`-shm` files are expected).

## Restore

RAST restore is deliberately offline and conservative:

1. It **refuses to run while `rast.service` is active**.
2. It extracts the database from the archive, runs `PRAGMA integrity_check` and
   rejects a corrupt/foreign snapshot.
3. It keeps a **pre-restore copy** of the current database under
   `/var/lib/rast/backups/pre-restore-<timestamp>/`.
4. It replaces the live database atomically (same directory rename) and prints
   a JSON summary (`restored`, `database`, `preRestoreBackup`, `integrity`).

Procedure:

```bash
# 1. Choose and verify the archive
sudo -u rast rast restore --from /var/lib/rast/backups/rast-20261110-030000-db-config.tar.gz --dry-run

# 2. Stop the service (restore refuses to run while it is active)
sudo systemctl stop rast.service

# 3. Restore the database
sudo -u rast rast restore --from /var/lib/rast/backups/rast-20261110-030000-db-config.tar.gz --yes

# 4. Restore configuration/secret material if needed (from the same archive or
#    your config backup); ensure ownership root:rast 0640 for rast.toml and
#    rast:rast 0600 for secret.key, then:

# 5. Start and verify
sudo systemctl start rast.service
sudo rast status
sudo -u rast rast doctor
# sign in through the browser and check audit logs / files
```

### Restoring user files from an archive

`rast restore` replaces the **database** only. A `users`-scope archive also
contains the raw user files, stored as `users/homes/<username>/...` and
`users/shared/<name>/...`. They must be extracted into the configured
`storage_dir` subdirectories — never into `/`.

With the default `storage_dir = /srv/rast`:

```bash
archive=/var/lib/rast/backups/rast-...-db-config-users.tar.gz
sudo systemctl stop rast.service        # do not extract while the server runs

# 1. Inspect the archive (no writes).
tar -tzf "$archive" | head -n 30

# 2. Test on an isolated copy first: extract to a scratch directory and compare.
scratch=$(mktemp -d)
mkdir -p "$scratch/homes" "$scratch/shared"
tar -xzf "$archive" -C "$scratch/homes" --strip-components=2 users/homes
tar -xzf "$archive" -C "$scratch/shared" --strip-components=2 users/shared
diff -r "$scratch/homes/<username>" /srv/rast/homes/<username> | head

# 3. Extract into storage_dir (NOT into /).
sudo tar -xzf "$archive" -C /srv/rast/homes --strip-components=2 users/homes
sudo tar -xzf "$archive" -C /srv/rast/shared --strip-components=2 users/shared
sudo chown -R rast:rast /srv/rast/homes /srv/rast/shared
# Per-user homes stay private; shared folders follow their share policy.
sudo chmod 0700 /srv/rast/homes/*

# 4. Start and verify.
sudo systemctl start rast.service
sudo -u rast rast doctor
```

`--strip-components=2 users/homes` removes the `users/homes/` prefix, so each
home is restored under its username; the same applies to `users/shared`.
Never use `-C / --strip-components=2 users/` — that extracts every home and
shared folder directly into the filesystem root. If `storage_dir` is custom
(see [CONFIGURATION.md](CONFIGURATION.md)), substitute it above. Always try the
scratch extraction on an isolated copy before touching live data.

### Full disaster recovery (lost host)

On a fresh host, in this order:

1. Install the **same or newer** RAST version (`install.sh --version X.Y.Z`).
2. Stop the service.
3. Restore `/etc/rast/` (config + env), `/var/lib/rast/secret.key` and the
   database (`rast restore --from … --yes`).
4. Restore user files into `storage_dir/homes` and `storage_dir/shared` with
   the commands in
   [Restoring user files from an archive](#restoring-user-files-from-an-archive)
   (never extract `users/` into `/`). Match ownership: `rast:rast`; per-user
   homes are `0700`.
5. `sudo systemctl start rast.service && sudo -u rast rast doctor`.

Without `secret.key`, sessions and encrypted app secrets cannot be recovered;
everything else still works.

## Updates

Two supported update paths, both checksum-verified:

### 1. Installer (recommended for most deployments)

```bash
curl -fsSL https://github.com/Enricko/rast/releases/latest/download/install.sh \
  | sudo sh
```

Re-running the installer replaces the binary and units, keeps `/etc/rast`,
`/var/lib/rast` and `/srv/rast`, restarts the service and verifies `/healthz`.
Choose an exact version with `--version X.Y.Z`. Preview with `--dry-run`.
To also verify the release's build provenance (GitHub CLI 2.49+):

```bash
curl -fsSL https://github.com/Enricko/rast/releases/latest/download/install.sh \
  | sudo sh -s -- --verify-attestation
```

### 2. `rast update` (CLI)

```bash
sudo rast update --check          # JSON: current, available, updateAvailable, releaseUrl
sudo rast update                  # update to the latest release
sudo rast update --version 0.1.0  # update/install a specific release
```

What `rast update` does:

1. Resolves the target release via the GitHub API (`--version` skips "latest").
2. Downloads `SHA256SUMS` and `rast-linux-<arch>` from
   `https://github.com/Enricko/rast/releases/download/v<version>/`.
3. Aborts on checksum mismatch; verifies the downloaded binary runs and reports
   the expected version.
4. Creates a database backup (`db` scope) **before** any migration can run.
5. Saves the previous binary as `/usr/local/bin/rast.rollback-<current>`.
6. Replaces the binary atomically and restarts `rast.service`, then
   `rast-worker.service` if it is running (`systemctl try-restart`). The Docker
   helper runs the same binary; left on the old one it refuses Docker actions
   added since. Updates *from* releases before 0.9.1 run the old updater,
   which skips this step: restart the helper yourself with
   `sudo systemctl restart rast-worker.service`. `rast doctor` warns when the
   helper runs a different version.
7. Polls `/healthz`; if the service does not become healthy, restores the
   previous binary and restarts again.
8. Reports the resulting database schema version. Database migrations are
   **forward-only**; the tool does not automatically roll the database back.

`rast update` must run as root (it writes `/usr/local/bin` and drives systemd).
`--check` is safe for unprivileged monitoring.

**Provenance:** `rast update` verifies the release `SHA256SUMS` (integrity) but
does not query Sigstore attestations yet. Verify provenance manually — the
installed binary is byte-identical to the published asset:

```bash
gh attestation verify /usr/local/bin/rast --repo Enricko/rast
# or, before updating, against the downloaded asset:
gh attestation verify rast-linux-x86_64 --repo Enricko/rast
```

(`gh` 2.49+ is required and must be authenticated with `gh auth login` or
`GH_TOKEN` for the attestation API; the installer's `--verify-attestation`
fails closed with the gh output if this is missing.)

The installer can enforce the same check with `--verify-attestation`; see
[INSTALL.md](INSTALL.md#release-integrity-and-provenance). An attestation proves
the binary was built by this repository's release workflow for a specific
commit — it does not prove the source was reviewed or that the build is
reproducible.

### Upgrade checklist

1. Read the release notes and `CHANGELOG.md` section for the target version.
2. Take a backup: `sudo -u rast rast backup --scope db,config --note "pre-upgrade"`.
3. Ensure enough free space for the binary (a few tens of MiB) and the backup.
4. Run the installer or `rast update`; keep the terminal session open.
5. Verify: `sudo rast status`, `sudo -u rast rast doctor`, sign in, open a file,
   check `/healthz` and the service logs (`journalctl -u rast.service -n 100`).

## Rollback

> **Prefer fixing forward.** Rollback restores a database snapshot taken before
> the upgrade, so everything written after that backup (users, files, audit
> events, settings) is lost. If the new release is usable at all, install the
> next patch release instead.

An update has two independent parts: the **binary** and the **database schema**.
The binary can be replaced at any time, but migrations are **forward-only**: a
binary that does not know every migration recorded in the database refuses to
start. Keep the pre-upgrade database backup created by `rast update` (or your
own `rast backup`) until the new release has proven healthy.

### Why a binary-only rollback usually fails after a schema change

RAST applies and validates migrations at startup. A binary older than the
database schema exits with an error like:

```text
rast: database migration failed: migration 4 was previously applied but is missing in the resolved migrations
```

(reproduced with the published v0.1.8 binary against a schema-v4 database).
The service stays down until a new-enough binary is installed **or** the old
database is restored. The saved binaries (`/usr/local/lib/rast/rast-<version>`
from the installer, `/usr/local/bin/rast.rollback-<version>` from `rast update`)
are only directly reusable when the older binary understands the *current*
schema. Check `rast doctor` (`schema vN`) and the target release's migration
list before swapping the binary.

### Rollback procedure

1. Stop the service: `sudo systemctl stop rast.service`.
2. Preserve evidence before changing anything:

   ```bash
   sudo journalctl -u rast.service --since '1 hour ago' > /tmp/rast-failed.log
   sudo cp -a /usr/local/bin/rast /usr/local/bin/rast.failed-$(date +%Y%m%d)
   sudo -u rast sqlite3 /var/lib/rast/rast.db \
     ".backup '/var/lib/rast/backups/pre-rollback-$(date +%Y%m%d-%H%M%S).db'"
   ```

3. **Schema-compatible rollback** (the old binary understands the current
   schema — typically only between patch releases that added no migration):
   install the saved binary and start the service:

   ```bash
   sudo install -m 0755 /usr/local/lib/rast/rast-<previous-version> /usr/local/bin/rast
   sudo systemctl start rast.service
   sudo rast status && sudo -u rast rast doctor
   ```

4. **Schema rollback** (the old binary predates a migration): restore the
   database **first**, then install the old binary, then start:

   ```bash
   # Restore the pre-upgrade archive (stop the service first; rast restore
   # refuses to run while it is active).
   sudo -u rast rast restore --from /var/lib/rast/backups/<pre-upgrade-backup>.tar.gz --yes
   sudo install -m 0755 /usr/local/lib/rast/rast-<previous-version> /usr/local/bin/rast
   sudo systemctl start rast.service
   sudo rast status && sudo -u rast rast doctor
   ```

5. Verify: sign in, open a file, check Activity Logs and `/healthz`.

### Access-control caveat when rolling back below 0.2.0

0.2.0 introduced NAS-style **user groups** and the enforced access policy
(migration `0004_groups.sql`, schema v4). A pre-0.2.0 binary does **not**
evaluate group folder/app rules at all; it falls back to the legacy
`share_members` ACL and owner rules. Rolling back to 0.1.x therefore **widens
access** for every account whose access came from a group grant and silently
ignores explicit group denies. After such a rollback:

* review Control Panel → Users & Groups and the affected shared folders with an
  administrator;
* have users sign out and back in (sessions are revalidated against the running
  server);
* temporarily disable accounts that relied on group grants;
* return to 0.2.0+ as soon as possible.

Keep the failed-version logs and the preserved database copy for the issue
report.

## User and session operations

```bash
sudo -u rast rast user list
sudo -u rast rast user create-admin <name> --password-stdin [--display-name NAME] [--quota-bytes N]
sudo -u rast rast user reset-password <name> --password-stdin   # revokes that user's sessions
sudo -u rast rast setup-token                                   # one-shot web setup token, 60 min
```

## App Center on the shell PATH

```bash
rast packages path status        # login shells, sudo and services: what runs the defaults?
sudo rast packages path sync     # apply packages_shell_path and packages_system_wide
sudo rast packages path links    # re-link /usr/local/bin now (the watcher does this)
```

`sync` writes the profile script only when it is RAST's own (a file someone
else wrote is never replaced or removed), and sets the permissions that let
other users run the installed runtimes. The installer and `rast update` run it.
New login shells pick it up; an open shell can run
`. /etc/profile.d/rast-packages.sh`.

With `packages_system_wide = true`, `sync` also installs and starts
`rast-packages-links.path`: a root systemd path unit that watches
`<data_dir>/packages/bin` and runs `rast packages path links` after every
change (install, *Make default*, *Use the system version*, uninstall). It
links only the commands of official packages (`php`, `node`, `npm`, `npx`,
`corepack`, `go`, `gofmt`, `python3`, `pip3`, `python3.N`, `pip3.N`) into
`/usr/local/bin`; nothing else there is touched. A command installed by hand
under that name is renamed to `.rast-displaced.<name>` and renamed back when
RAST stops providing it. Turning the setting off and running `sync` again
removes the links and the units and puts everything back; so does the
uninstaller. Check with `systemctl status rast-packages-links.path` and
`journalctl -u rast-packages-links.service`.

* Passwords must be at least 10 characters. Use `--password-stdin` (or the
  interactive prompt) instead of `--password`, which is visible in process
  lists. `RAST_PASSWORD` is honoured for automation but must not be used in
  shared environments.
* Resetting a password revokes existing sessions for that user.
* Prefer disabling/deleting accounts in the Control Panel over manual SQL.
* Audit history is available in the Activity Logs app and
  `GET /api/logs`; exports are provided by `GET /api/logs/export`.

## Linux apps

Administrators install desktop Linux apps from App Center (a `.deb`, an
AppImage or a portable archive from File Station). Each app becomes a Docker
image; each person who opens it gets their own container, which stops when the
window closes or after 30 minutes without input.

Installed apps are shared with every signed-in user by default, so they appear
on every account's desktop and in the Applications folder. Everyone opens their
own copy: own container, own settings volume (settings, sign-ins, bookmarks),
own open windows. An administrator can restrict an app to administrators:
App Center → the app → More actions → **Who can use it…**. This per-app
visibility decides access; Linux app windows are outside the group policy's
"applications" (Docker) rule.

```bash
docker ps --filter label=rast.managed=linux-app           # running copies
docker images 'rast-linux/*'; docker images rast-linux-base  # app images and the base
docker volume ls --filter label=rast.managed=linux-app     # per-person app settings
```

* Reverse proxies must pass WebSockets on `/linux-apps/` (the app's picture
  and input). For nginx, inside the `location`:
  `proxy_set_header Upgrade $http_upgrade;` and
  `proxy_set_header Connection $connection_upgrade;` with
  `map $http_upgrade $connection_upgrade { default upgrade; "" ""; }` at the
  `http` level. Turn the access log off for that location: the address holds
  the copy's secret key. Without the headers the window stays black with
  "WebSocket disconnected".
* Disk: the first app downloads the shared base (about 3.7 GB); each app
  adds roughly 1 GB.
* RAST removes old builds after an update and everything of an app on
  uninstall (settings volumes only when *Also delete its settings* is ticked).
  Docker's build cache is not removed; reclaim it with `docker builder prune`.
* Copies left running by a restart are stopped when RAST starts again.
* Closing the app from inside it (its own close button, for example Chrome's
  ×) closes its RAST window within a few seconds; a page reload reconnects to
  the same copy.
* Chromium browsers' own window bar is turned off in the app image (Chrome's
  *Use system title bar and borders* preference), so the RAST window is the
  only window frame. Apps installed before 0.11.2 get this after *Reinstall*;
  apps that draw their own title bar keep it.
* **Separate windows.** Images built with the window service stream every app
  window on its own (x11vnc per window, plus a compositor so a covered window
  still shows its own contents): RAST shows one desktop window per app window,
  each with its own taskbar entry and close/minimize/maximize, and closing one
  closes that window inside the app. Apps installed from older images keep the
  single whole-desktop window until *Reinstall*.
* The app's main window is kept filling the RAST window: if the app restored
  it smaller, or it was dragged inside the picture, it is maximized again
  within a second. Windows the app opens later float on top of it, cascaded,
  at about 70% width / 75% height instead of stacking full-size; dialogs and
  popups keep the size the app gave them. Apps installed before 0.11.4 get
  this after *Reinstall*.
* A copy only starts while the host has 4 GiB of memory free.
* Updates come from the app's official source: its apt repository for a
  `.deb` that installs one, GitHub releases or zsync for AppImages that embed
  update information. Archives without an update source are updated by
  installing a newer file.

## Secrets and key rotation

`/var/lib/rast/secret.key` (0600) is the master key for CSRF derivation and
application-secret encryption. Rotating it invalidates all sessions and makes
existing encrypted app secrets unreadable. Procedure if rotation is required:

1. Back up the database and config, and take note of managed app secrets.
2. Stop the service, move `secret.key` aside, start the service (a new key is
   generated on startup).
3. Re-enter affected application secrets in the UI and re-deploy those apps.
4. Delete the old key file securely once verified.

## Housekeeping calendar

| Frequency | Task |
| --- | --- |
| Daily (automatic) | scheduled backups, session/audit/job retention pruning |
| Weekly | check disk usage, review audit log for unexpected admin actions, confirm off-site backups are landing |
| Monthly | apply OS + RAST updates, review accounts/quotas, empty trash if large |
| Quarterly | restore-test a backup on a scratch host, review `rast doctor` output, review Docker applications |

## Runbook index

| Scenario | Section |
| --- | --- |
| Service not starting | [Troubleshooting](#troubleshooting) |
| Restore after accidental deletion | [Restore](#restore) |
| Upgrade / downgrade | [Updates](#updates), [Rollback](#rollback) |
| Disk full | [Monitoring and alerting](#monitoring-and-alerting), then free space and run `rast doctor` |
| Compromised admin account | [User and session operations](#user-and-session-operations), rotate passwords/keys, review audit logs, see [SECURITY.md](../SECURITY.md) |

## Troubleshooting

| Symptom | Checks |
| --- | --- |
| Service fails to start | `journalctl -u rast.service -n 100`; TOML syntax/unknown keys; directory ownership; port conflicts (`ss -ltnp \| grep 8720`) |
| `/healthz` returns `degraded` | Database path/permissions or disk full; run `rast doctor`; check `journalctl` for SQLite errors |
| Web UI loads but API calls fail | Proxy path rules, `/api` routing, CSP/CORS misconfiguration; check browser console and proxy logs |
| SSE (live monitor/jobs) frozen | Proxy buffering (`proxy_buffering off`, `X-Accel-Buffering: no`, Caddy `flush_interval -1`) |
| Login blocked | Rate limiting or per-account lockout (15 min after repeated failures); check `login_attempts`, wait or reset the password |
| Uploads rejected (413) | Proxy body limit vs `RAST_MAX_UPLOAD_BYTES`; Cloudflare plan limits |
| Docker apps unavailable | `systemctl status rast-worker`, `/var/run/docker.sock`, `rast-worker` in `docker` group, `docker_enabled=true`, restart worker |
| SQLite `database is locked` | Data dir on NFS/SMB (unsupported) or an external process holding the DB; move to local storage |
| High memory | Large file operations + many jobs; lower `RAST_JOB_CONCURRENCY`, check systemd `MemoryMax`, inspect `/api/system/info` |

For security incidents see [SECURITY.md](../SECURITY.md); for bugs, open an
issue with version (`rast version`), `rast doctor` output and relevant journal
lines (remove secrets and personal paths first).
