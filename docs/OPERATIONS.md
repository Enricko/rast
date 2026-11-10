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

### Full disaster recovery (lost host)

On a fresh host, in this order:

1. Install the **same or newer** RAST version (`install.sh --version X.Y.Z`).
2. Stop the service.
3. Restore `/etc/rast/` (config + env), `/var/lib/rast/secret.key` and the
   database (`rast restore --from … --yes`).
4. Restore `/srv/rast/` (user files) with your file-level backups. Match
   ownership: `rast:rast`; per-user homes are `0700`.
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
6. Replaces the binary atomically and restarts `rast.service`.
7. Polls `/healthz`; if the service does not become healthy, restores the
   previous binary and restarts again.
8. Reports the resulting database schema version. Database migrations are
   **forward-only**; the tool does not automatically roll the database back.

`rast update` must run as root (it writes `/usr/local/bin` and drives systemd).
`--check` is safe for unprivileged monitoring.

### Upgrade checklist

1. Read the release notes and `CHANGELOG.md` section for the target version.
2. Take a backup: `sudo -u rast rast backup --scope db,config --note "pre-upgrade"`.
3. Ensure enough free space for the binary (a few tens of MiB) and the backup.
4. Run the installer or `rast update`; keep the terminal session open.
5. Verify: `sudo rast status`, `sudo -u rast rast doctor`, sign in, open a file,
   check `/healthz` and the service logs (`journalctl -u rast.service -n 100`).

## Rollback

### Binary-only rollback (same or older compatible schema)

If the **new version was already running** and you only need to go back one
step, restore the previous binary and restart:

```bash
sudo systemctl stop rast.service
# option A: rast update's rollback copy
ls -l /usr/local/bin/rast.rollback-*
sudo cp -a /usr/local/bin/rast /usr/local/bin/rast.failed-$(date +%Y%m%d)
sudo cp -a /usr/local/bin/rast.rollback-<previous-version> /usr/local/bin/rast
# option B: installer's copy
ls -l /usr/local/lib/rast/
sudo install -m 0755 /usr/local/lib/rast/rast-<previous-version> /usr/local/bin/rast
sudo systemctl start rast.service
sudo rast status && sudo -u rast rast doctor
```

**Schema caveat:** migrations are forward-only. If the new binary applied a
migration, the older binary may refuse to open the database ("migration …
missing/version mismatch"). In that case restore the pre-upgrade database
(`rast restore --from <pre-upgrade backup> --yes`) as well as the older binary.
Check the current schema version with `rast doctor` (shows `schema vN`) before
and after the upgrade.

### Full rollback

1. Stop the service.
2. Install the previous release (`install.sh --version X.Y.Z` or restore the
   binary copy).
3. Restore the pre-upgrade backup (`rast restore --from … --yes`).
4. Start the service and verify.

Keep the failed-version logs (`journalctl -u rast.service --since …`) for the
issue report.

## User and session operations

```bash
sudo -u rast rast user list
sudo -u rast rast user create-admin <name> --password-stdin [--display-name NAME] [--quota-bytes N]
sudo -u rast rast user reset-password <name> --password-stdin   # revokes that user's sessions
sudo -u rast rast setup-token                                   # one-shot web setup token, 60 min
```

* Passwords must be at least 10 characters. Use `--password-stdin` (or the
  interactive prompt) instead of `--password`, which is visible in process
  lists. `RAST_PASSWORD` is honoured for automation but must not be used in
  shared environments.
* Resetting a password revokes existing sessions for that user.
* Prefer disabling/deleting accounts in the Control Panel over manual SQL.
* Audit history is available in the Activity Logs app and
  `GET /api/logs`; exports are provided by `GET /api/logs/export`.

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
