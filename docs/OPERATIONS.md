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
| `users` | Copy of user homes and shared folders (HOMEs in one-HOME mode are skipped, see below) |
| `apps` | Managed application definitions (compose files + encrypted env metadata) |

Native Linux app installs and per-person profiles (`<data_dir>/linux-apps`) are
**not** part of any backup scope: back them up separately when native apps are
in use (stop `rast-linux.slice` first, then archive the directory; see
[Upgrade and rollback of native apps](#upgrade-and-rollback-of-native-apps)).

A HOME in one-HOME mode (`rast migrate home`) belongs to the account's Linux
user and the service cannot read it, so the `users` scope **skips** it: the
archive's `manifest.json` and the backup result list it under `warnings`
(`users/homes/<name>: skipped, …`), and Backup Center shows the warning
after the backup. Folders that cannot be read are listed the same way. Until
Backup Center reads HOMEs through the helper (task 1.5b), back such HOMEs up
as root, keeping owners and ACLs, for example
`sudo tar --numeric-owner --acls -czf home-<name>.tar.gz -C /srv/rast/homes <name>`.

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
sudo -u rast sqlite3 /var/lib/rast/private/rast.db \
  ".backup '/var/lib/rast/backups/manual-$(date +%Y%m%d-%H%M%S).db'"

# alternative: stop the service, then copy the database and sidecar files
sudo systemctl stop rast.service
sudo cp -a /var/lib/rast/private/rast.db{,-wal,-shm} /path/to/backup/dir/ 2>/dev/null || true
sudo systemctl start rast.service
```

Also back up `/etc/rast/` and `/var/lib/rast/private/secret.key` (copying the WAL
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
# Give what you restored to the service, folder by folder (see below):
sudo chown -R rast:rast /srv/rast/homes/<username>     # each restored home
sudo chown -R rast:rast /srv/rast/shared/<folder>      # each restored shared folder
sudo chown root /srv/rast/shared/<folder>              # only if it was migrated (rast migrate shares)
# Per-user homes stay private; shared folders follow their share policy.
sudo chmod 0700 /srv/rast/homes/<username>

# 4. Start and verify.
sudo systemctl start rast.service
sudo -u rast rast doctor
```

Never run `chown -R rast:rast` over all of `/srv/rast/homes` or
`/srv/rast/shared`: a HOME in one-HOME mode belongs to its account's Linux
user (`rast-u<id>`) and would stop working for its apps, terminals and file
agent, and a migrated shared folder's container must stay owned by root
(the server's repair pass gives its entries the group layout back when it
starts). HOMEs in one-HOME mode are not in the archive anyway; restore them
from your root copy with `sudo tar --numeric-owner --same-owner --acls -xzf
home-<name>.tar.gz -C /srv/rast/homes`.

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
3. Restore `/etc/rast/` (config + env), `/var/lib/rast/private/secret.key` and the
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

### Native Linux apps after an upgrade (0.12 → 0.13)

0.13 adds the native backend and the new `rast-native.service`. The old
`rast-worker.service` and all Docker-backed apps keep working untouched.
`rast update` refuses to leave an enabled native backend without its helper: it
detects missing native service assets (`rast-native.service`, the tmpfiles
config, `<data_dir>/linux-apps`) and runs the release installer to add them,
then restarts the helper with the new binary (it needs curl/wget and the
standard `/etc/rast/rast.toml` for that). Running the full installer is still
the recommended path after the upgrade because it also installs the dependency
packages (`nftables` and the Xpra audio prerequisites):

```bash
sudo sh install.sh          # detects Xpra, installs rast-native.service, rast-linux.slice and nftables
sudo rast native-helper --check
sudo -u rast rast doctor    # the "native Linux apps" section must be clean
```

Without Xpra the installer leaves native execution disabled and the app center
keeps installing legacy Docker-backed apps; `native_enabled` defaults to
"Xpra is installed". Until the helper runs, app installs fail with an
actionable error (they never fall back silently). Existing Docker-backed apps
show `backend: docker` in App Center and can be migrated one by one.

#### Upgrade and rollback of native apps

The native feature adds durable per-person state outside the database:
installed app trees (`<data_dir>/linux-apps/apps`) and per-app per-person
profiles (`<data_dir>/linux-apps/profiles`). The database schema is **v14**
(migration `0013_native_linux_apps.sql`: `linux_identities`,
`linux_sessions`, `linux_runs`; migration `0014_super_admin.sql`: the
`users.super_admin` flag, set for every administrator that existed at the
upgrade).

Upgrade:

1. Take a full backup before upgrading:
   `sudo -u rast rast backup --scope db,config` (the database + configuration).
   Also copy the native app trees, because `--scope db,config` does not include
   them:
   `sudo systemctl stop rast-linux.slice` (ends every native session and
   application; save work first), then
   `sudo tar -C /var/lib/rast -czf /var/lib/rast/backups/native-apps-pre-upgrade.tar.gz linux-apps`.
   `linux-apps` contains the installs, profiles and migration backups; it can
   be large.
2. Install the new release with the installer (recommended for native apps) so
   the systemd units, `nftables` package and Xpra prerequisites are refreshed:
   `sudo sh install.sh`. `rast update` also replaces the binary and restarts
   `rast.service`, `rast-worker.service` and `rast-native.service` (and installs
   missing native service assets through the release installer), but it does
   not refresh existing unit files or the native dependency plan.
3. Restarting `rast-native.service` during the upgrade does **not** kill
   running sessions; they are separate systemd units. Old audio-disabled
   sessions are recreated once on the next start. If the upgrade changes
   session flags or the helper protocol incompatibly, finish running work
   first: `sudo systemctl stop rast-linux.slice`.
4. Verify: `sudo rast native-helper --check` (Xpra 6.5+, display firewall),
   `sudo -u rast rast doctor`, then launch one app per test account.

Rollback:

* Migrations are forward-only. To go back to a release whose binary predates
  schema v13, restore the **matching pre-upgrade database backup** first (the
  old binary refuses a newer schema), then install the old binary; see
  [Rollback](#rollback). The native app trees are compatible across releases
  (restore the `linux-apps` tar if they were damaged or the old release
  expects its previous layout), but profiles written by the newer release may
  not be readable by older app versions.
* Rolling back only the binary while leaving schema v13 in place is supported
  only between releases that share schema v13; check `rast doctor` (`schema
  vN`) before swapping binaries.
* Apps installed natively are host packages/processes: rolling RAST back does
  not uninstall `.deb` packages or delete profiles. Uninstall or clean up
  through App Center *before* the rollback if the old release cannot manage
  them.

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
   `rast-worker.service` if it is running (`systemctl try-restart`) and
   `rast-native.service` if it is running, refreshing the native tmpfiles. Both
   helpers run the same binary; left on the old one they refuse operations
   added since. Missing native service assets are installed through the release
   installer before the new binary starts (the update aborts and keeps the old
   binary when that is not possible). Updates *from* releases before 0.9.1 run
   the old updater, which skips restarting the helpers: restart them yourself
   with `sudo systemctl restart rast-worker.service rast-native.service`.
   `rast doctor` warns when a helper runs a different version.
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
   sudo -u rast sqlite3 /var/lib/rast/private/rast.db \
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

### Roles

| Role | What it is for |
| --- | --- |
| **Super administrator** | The server owner: everything an administrator does, plus the server itself: File Station's **Root file system**, the root (or `terminal_admin_account`) shell in Terminal, and creating, changing, disabling or resetting **administrator** accounts. |
| **Administrator** | Runs RAST for its people: users, groups, shared folders, apps, backups, settings. Cannot manage administrator accounts or change roles, has no root file system, and gets the unprivileged identity in Terminal. |
| **User** | Their own Home, the shared folders and applications they were given. |

The first account (web setup) and accounts made with `rast user create-admin`
are super administrators. Upgrading to 0.18.0 makes every existing
administrator a super administrator, so nobody loses access; demote the ones
who should only run RAST in Control Panel → Users. The last active super
administrator cannot be demoted or disabled.

An administrator is trusted with the applications of the server (Docker,
App Center, Linux apps), which run with wide privileges: the role keeps
day-to-day administration away from the server's own files and shells, it is
not a sandbox against a hostile administrator.

### Root file system in File Station

Super administrators see a third location in File Station, after Home and
Shared folders: **Root file system**, the server's own files from `/`, with
full access as root (browse, preview, download, upload, new folder, rename,
copy, move, delete). There is no quota and **no recycle bin**: a delete is
permanent, and File Station asks before every change. New files and folders
get the owner of the folder they are created in; a replaced file keeps its
owner and mode. Copy and move work inside the root file system, not between
it and RAST storage. Every change and download is in the activity log.

The hardened service cannot read system files, so these requests are served
by a root process in its own unit, `rast-rootfs`, that the helper starts on
first use and that stops itself after 15 idle minutes
(`journalctl -u rast-rootfs`). `files_root_enabled = false` removes the
location for everyone.

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
AppImage or a portable archive from File Station, drag and drop or an HTTPS
link). Since v0.13 the **native backend** is the default: the app is installed
on the server and runs natively under the person's own unprivileged Linux
identity, inside one lazily started **Xpra seamless graphical session per RAST
user**. Every application that person opens — and every window Chrome opens —
shares that session and its connection; each normal top-level window is a RAST
desktop window with its own taskbar entry, while dialogs and popups are attached
to their parent window and never get one. Legacy Docker-backed apps keep
working unchanged (they show `backend: docker` in App Center) until an
administrator migrates them. The user-facing behaviour, security model and
limitations are in [NATIVE-LINUX-APPS.md](NATIVE-LINUX-APPS.md).

Installed apps are shared with every signed-in user by default, so they appear
on every account's desktop and in the Applications folder. Everyone launches
their own processes with their own profile: separate Linux identity, separate
home, separate browser settings/sign-ins, separate display and sockets. An
administrator can restrict an app to administrators: App Center → the app →
More actions → **Who can use it…**. This per-app visibility decides access;
Linux app windows are outside the group policy's "applications" (Docker) rule.

```bash
systemctl status rast-native            # privileged helper (root)
systemctl status 'rast-xpra-*'          # per-person graphical sessions
systemctl status 'rast-app-*'           # native application processes
systemd-cgls /rast-linux.slice          # who runs what, with cgroup limits
ls /var/lib/rast/linux-apps/apps        # managed portable/AppImage installs
ls /var/lib/rast/linux-apps/profiles    # per-app per-person profiles (0700)
getfacl /srv/rast/homes/<user>          # read+traverse ACL for their app identity
xpra info :<display>                    # display/encoding/limits of one session (see `systemctl status`)
journalctl -u rast-native -n 100        # helper log (no credentials are logged)
```

* Per-user limits are set by `linux_session_memory_mb/cpus/tasks` and
  `linux_app_memory_mb/cpus/tasks` (`MemoryMax`, `CPUQuota`, `TasksMax` on the
  transient units, all inside `rast-linux.slice`). Check live usage with
  `systemd-cgtop /rast-linux.slice`.
* A session starts on the first launch and is reclaimed after the last
  application exits: an empty, viewer-less session first gets a short reload
  grace (60 s) and then `linux_idle_minutes` extra minutes (`0` = reclaim right
  after the grace). Never while an app runs or a viewer is attached. A page
  reload within that window reconnects to the same session.
* Running apps and windows survive a **restart of the web server**
  (`systemctl restart rast.service`): the units keep running and the new
  process adopts them from `linux_sessions`/`linux_runs`. The same applies
  after a crash. **Restarting `rast-native.service` does not stop running
  sessions or applications** — they are independent transient systemd units,
  not helper children, and their display streams keep working because the proxy
  connects directly to the Xpra port. Only helper-backed actions (launch,
  close, install) fail while it is down. On start the helper reconciles the
  nftables display-port set against the live `rast-xpra-*` units. To stop all
  native sessions on purpose (e.g. before a maintenance window or before
  stopping RAST for good), run `sudo systemctl stop rast-linux.slice`;
  orphaned units from an earlier server are also stopped automatically on the
  next start.
* The helper installs one nftables rule at startup (table `inet rast_native`)
  that rejects connections to the loopback Xpra display ports from every uid
  except the `rast` service account; each session port is added before the
  session is usable and removed when it stops. This needs the `nftables`
  package (the installer adds it to the native dependency plan); `rast doctor`
  reports the firewall state and warns when `nft` is missing.
* New applications are admitted based on observed memory pressure: free memory
  must cover the app's limit plus `linux_admission_reserve_mb`; otherwise the
  launch fails with a 503 that shows both numbers.
* Closing a RAST window sends the app's normal close request, so unsaved-work
  dialogs appear inside the session as a dialog window. To terminate a stuck
  application, use App Center → the app → **End application** (or
  `POST /api/linux-apps/session/runs/{run}/close`; the shorter
  `POST /api/linux-apps/session/runs/{run}` is kept as a compatibility alias);
  that stops the whole process group and leaves other windows/apps running.
  There is **no "End session" action in the App Center UI**: the whole-session
  API is `POST /api/linux-apps/session/close` (also reached by closing the
  fallback "Native desktop" whole-session window), and the operator-level
  equivalent is `systemctl stop rast-linux.slice`.
* An explicit sign-out or a revoked browser session closes a live display
  stream within ~5 s (the proxy revalidates the login every 5 s) but does not
  kill the person's applications; they keep running until they exit, are ended
  individually, or are stopped with the session/account. Deactivating an
  account stops its native session and applications.
* Running apps and windows survive a restart of the web server and of
  `rast-native.service` (see above). A reboot ends every process; profiles and
  installs stay on disk.
* Chromium browsers keep their sandbox enabled (the person's identity is
  unprivileged; `/opt/google/chrome/chrome-sandbox` is setuid root and the
  renderer processes run with `Seccomp: 2`, `NoNewPrivs: 1`). Never add
  `--no-sandbox`.
* Uninstall removes a `.deb` package only when RAST added it and nothing else
  depends on it (the App Center preview warns before a transaction that would
  remove or downgrade unrelated packages); **Also delete its settings**
  controls the per-person profiles only, never files in RAST homes.
* Updates come from the app's official source (apt for `.deb` packages that
  install a repository, GitHub releases or zsync for AppImages) or from an
  updated file/link for archives without an update source. Portable updates
  stage the new version, validate it and switch atomically; the previous
  directory stays for recovery (`previousPath` in the API) until the next
  update replaces it.
* **Migration of Docker-backed apps** is an explicit administrator action
  (App Center → app → **Migrate to native**) and runs as a job with per-app
  results. It keeps the app id and shortcuts, installs the native app (asking
  for the original file if it is no longer staged), copies every person's
  Docker settings volume into their native profile while the app is stopped,
  and retains the Docker image and volumes. Use **Roll back** to return to the
  Docker backend, and **Clean up old Docker data** only after native operation
  has been verified. The audit log records `linuxapp.migrate`,
  `linuxapp.rollback` and `linuxapp.cleanup`.
* Reverse proxies must pass WebSockets on `/linux-apps/` (the session stream
  and input). For nginx, inside the `location`: `proxy_set_header Upgrade
  $http_upgrade;` and `proxy_set_header Connection $connection_upgrade;` with
  `map $http_upgrade $connection_upgrade { default upgrade; "" ""; }` at the
  `http` level. Turn the access log off for that location: the session address
  contains a secret key. Without the headers the windows stay black with
  "disconnected".
* Legacy Docker apps (only while `backend = "docker"`): `docker ps --filter
  label=rast.managed=linux-app`, `docker volume ls --filter
  label=rast.managed=linux-app`. The shared base image is about 3.7 GB and
  each app image about 1 GB. Docker's build cache is not removed by RAST;
  reclaim it with `docker builder prune`. Copies left running by a restart are
  stopped when RAST starts again.
* Native apps and their session URLs require an authenticated RAST session;
  the Xpra port binds to loopback only and is never exposed. Session keys are
  not written to the access log or to application logs.

## Shared folders as Linux groups (`rast migrate shares`)

With the native backend, every shared folder gets two Linux groups
(`rast-s<h>-rw`, `rast-s<h>-ro`) whose members follow Effective Access (ADR
0005, section 4). New shared folders get the group layout when they are
created. Existing ones are moved with `rast migrate shares`, before
`rast migrate home`:

```bash
sudo -u rast rast migrate shares --share <name> --dry-run   # read-only report; the server may run
sudo systemctl stop rast
sudo -u rast rast migrate shares --share <name>             # apply one shared folder
sudo systemctl start rast
```

Every mode needs `--share <name>` (one shared folder) or `--all` (every
one); without either the command stops with an error, so a run never
covers every shared folder by default.

- The apply first writes a journal of every entry's group, mode and ACLs to
  `<data_dir>/migrations/shares/<shareId>/<stamp>/journal.jsonl` (root,
  0700/0600) and syncs it, then gives the container `root:rast-s<h>-rw`
  2770 and every folder and file the group, group write and the ACL.
  Set-uid bits are dropped. Symlinks, files with several hard links,
  root-owned entries, entries owned by any account other than `rast` and
  the RAST identities (`otherOwner`), special files, other file systems
  and bind mounts (`otherDevice`) are left unchanged and listed in the report; the repair pass skips the
  same entries. An apply that stopped half way
  resumes with its journal when run again.
- The apply **replaces the whole ACL** of every entry with the RAST
  layout. Read the dry run first: it lists every entry whose ACL holds
  named entries that RAST does not manage (anything other than `u:rast`,
  the read-only group and old per-user entries of RAST accounts), with the
  entries themselves, for example `docs/plan.txt: u:1001:rw-, default
  g:2000:r-x`. They are removed by the apply, and again by the daily
  repair pass (below), which logs each one it removes. The journal keeps them for a rollback; to keep a copy of
  your own, save them first with `sudo getfacl -R -p <path>`.
- `sudo -u rast rast migrate shares --share <name> --rollback` (server stopped) restores
  the journal: only entries that are still the recorded inode with the
  recorded owner; entries changed since are reported and left. The groups
  stay; they give nothing without the container layout. Set-uid bits, and
  set-gid bits of regular files, are never restored (set-gid on folders
  is); the rollback output lists every entry that lost a bit, and the full
  list is kept next to the journal in
  `<stamp>.rolled-back/dropped-bits.txt` (`<recorded> <restored> <path>`).
- `--json` prints a machine-readable report; the exit code is 1 when any
  shared folder failed (the others are still done).
- An apply or rollback that is still running in the helper (for example
  after its terminal was closed) keeps the share locked: a second run of
  either fails for that share with `share_busy` ("still running") until
  the first one ends. Wait for `journalctl -u rast-native` to log
  "shared folder migrated" or "rolled back", then run it again.
- **Repair pass.** The server applies the group layout again to every
  migrated shared folder when it starts (so right after every
  `rast migrate shares`, whose apply needs it stopped) and then once a day
  (helper operation `share_dir_repair`; legacy, not yet migrated folders
  are left alone). It gives the rw group, group write and the ACL back to
  entries that lost them: files renamed in from HOME, `chgrp` or `cp -p`
  by a member, and files that the legacy in-process File Station (`rast`,
  accounts not yet on one HOME) moved or restored into a migrated folder
  (review finding F4). It also replaces ACL entries that RAST does not
  manage, and logs each share where it changed entries
  (`journalctl -u rast | grep "shared folder repair"`). Permission changes
  that members make inside a shared folder (`chmod 600`, `chgrp`,
  `setfacl`) are therefore undone by the next pass: tell users to keep
  private files in their HOME. Under group quota
  (Q-a) a share's limit is therefore **"filesystem, best effort"**: files
  with a foreign group escape it until the next pass, and a member can
  charge files outside the share to it (ADR 0005, D3).
- **Identity conflicts.** An account named `rast-u<id>` whose uid is not
  `linux_uid_base + <id>` (created by hand, or left from another
  `linux_uid_base`) is never added to a shared folder group and is taken
  out of the groups it is in; the other members are still updated. The
  server logs it and `rast doctor` lists it. RAST never changes or removes
  that account: find out where it comes from, then fix or remove it.
- Running apps and terminals keep the groups they started with. A
  revocation restarts the person's apps and terminals after a 60-second
  warning; a grant is offered as *Restart session now*. When the person
  lost a shared folder entirely (not a move from write to read), its
  container also gets the entry `u:rast-u<id>:---` during the warning, so
  new paths into it fail at once; the entry goes after the restart, or
  when access is granted again. At start-up and every five minutes the
  server checks which share groups the processes in each person's units
  carry (`/proc/<pid>/status` and `/proc/<pid>/cgroup`, through the
  helper; a process of the identity outside its units, such as a set-uid
  program run by someone else, does not count): a session that still holds a
  group its user lost, for example because the server restarted during a
  warning, is warned and restarted again. Leftover deny entries of an
  earlier run are removed by the first check.

## Moving accounts to one HOME (`rast migrate home`)

`rast migrate home` moves a legacy account into one-HOME mode (ADR 0005,
section 8): its HOME, its app data and its recycle bin become its Linux
account's (`rast-u<id>`), so File Station, terminals and every app share
one HOME. Run it after `rast migrate shares`, one account at a time:

```bash
sudo -u rast rast migrate home --user <name> --dry-run     # read-only report; the server may run
sudo systemctl stop rast
sudo -u rast rast migrate home --user <name>               # apply
sudo systemctl start rast
sudo -u rast rast migrate home --user <name> --rollback    # undo (server stopped)
```

`--all` instead of `--user <name>` covers every account; one of the two is
required. `--json` prints a machine-readable report; the exit code is 1
when any account failed (the others are still done). Back up `/etc/rast`,
`/var/lib/rast` and `/srv/rast` first.

- **Dry run.** Per account: the size of the HOME (and on disk), the
  entries that change owner (`rast` → `rast-u<id>`), the ACL entries of
  RAST identities that are removed (the 0.18.3 read grant `u:rast-u<id>:rx`
  and `d:u:rast-u<id>:rx`, and terminal grants), other named ACL entries
  (kept, listed with their entries), files with several hard links, links,
  root-owned entries, entries of other accounts and other mounts (all left
  as they are), the app data that is merged into the HOME per app with its
  conflicts (first 50 paths), the legacy recycle bin and the entries that
  stay in it, the projected usage against the quota (the HOME, the merged
  app data and the recycle bin), `~/Shared` names that collide with shared
  folders, a stray `homes/rast-u<id>` folder (D13; RAST never deletes it),
  running units and processes, and the shared folders that still need
  `rast migrate shares`.
- **Before the apply.** Every shared folder the account can open must be
  migrated (`rast migrate shares --share <name>`); the apply refuses and
  names them otherwise. Administrators can open every shared folder, so an
  administrator account can only be migrated after all of them. The apply
  stops the account's session, apps, terminals and file agent, and refuses
  while any process of `rast-u<id>` still runs (for example
  `sudo pkill -u rast-u<id>`, then run it again).
- **Apply.** The helper writes a journal of every entry's owner, group,
  mode and ACLs and the plan of every move to
  `<data_dir>/migrations/home/<userId>/<stamp>/` (`journal.jsonl`,
  `plan.jsonl`; root, 0700/0600, synced) before it changes anything. It
  then gives the HOME's entries to `rast-u<id>` and removes the identity
  ACL entries, merges the legacy identity home
  (`<data_dir>/linux-users/<id>`) and every app profile
  (`<data_dir>/linux-apps/profiles/<app>/<id>/`) into the HOME **per
  file** — `config` → `~/.config`, `data` → `~/.local/share`, `state` →
  `~/.local/state`, `cache` → `~/.cache`, `home` → `~/` — sets the passwd
  home, and finally gives the HOME itself to the account (0700, no ACL)
  and writes `applied`. An entry that the HOME already has (a file, or a
  file where a folder is) is a conflict: it stays in the old profile and
  is listed. `.Xauthority` is not moved (the session makes it again), nor
  is a profile's `tmp`. Data on another file system than the HOME is not
  moved.
- **Recycle bin.** The legacy recycle bin
  (`<storage_dir>/.trash/<userId>`) moves into XDG Trash
  (`~/.local/share/Trash`, or the shared folder's `.Trash-<uid>` for
  entries deleted from one), in batches of 256. Its database rows are saved
  to `<data_dir>/migrate-home/<userId>/trash-rows-<time>.jsonl` (0600)
  before they are deleted. Entries whose shared folder no longer exists,
  and entries whose data is missing, stay in the legacy recycle bin and
  are listed.
- An apply that stopped half way resumes with its journal when run again;
  an account that is done is left as it is (its session keeps running).
  An apply or rollback still running in the helper keeps the account
  locked (`share_busy`, "still running") until it ends.
- **Rollback** (server stopped) moves the recycle bin entries and the
  merged app data back, restores the recorded owners, groups, modes and
  ACLs (the 0.18.3 grant included), gives entries made since the migration
  to the service, sets the passwd home back and puts the recycle bin rows
  back; the journal folder is renamed `<stamp>.rolled-back`. Entries that
  are another file now, or that the HOME holds in place of a moved entry,
  are listed and left as they are. Set-uid bits are never restored.
- **Afterwards** the account's quota counts its app data, caches and
  recycle bin (ADR 0005, D2), and Backup Center skips its HOME with a
  warning (see "What is in a backup archive"). The legacy folders that the
  migration emptied, `.trash/<userId>` and a stray `homes/rast-u<id>` are
  not deleted: cleaning them up is a separate step.

## Recycle bin and quota of one-HOME accounts

One-HOME accounts (ADR 0005; none on production until `rast migrate home`)
use the freedesktop XDG Trash, shared with their Linux apps:
`~/.local/share/Trash` in HOME and `<container>/.Trash-<uid>` (mode `0700`,
owned by the account) at the top of each shared folder they delete from.
The service account cannot read either; the retention setting
(`trashRetentionDays`) is applied when the account opens its recycle bin.
A `.Trash-<uid>` folder in a shared folder is private to its owner: it is
hidden in File Station, the repair pass leaves it alone, and it holds that
member's deleted files until they empty it. Its bytes count towards the
shared folder's size limit.

**A `.Trash-<uid>` that is not its owner's.** A member with write access
can create the folder name of another member first (a link, a file, or a
folder they own), from an app or a terminal. That member's file agent then
never uses it (the Trash specification's check): deleting and restoring in
that shared folder fail with "this recycle bin folder belongs to another
account; ask an administrator to remove it". RAST never changes or removes
it. The daily repair pass logs it ("recycle bin folders made by someone
else") and `rast doctor` lists it with the owner uid. To fix it, look at
what it holds first, then remove it as root, for example:

```bash
sudo ls -la /srv/rast/storage/shared/<container>/.Trash-<uid>
sudo rm -r /srv/rast/storage/shared/<container>/.Trash-<uid>   # after checking
```

The member's agent creates a new, private one at the next deletion. A
`.Trash-<uid>` of an account that left the shared folder, or was deleted,
is not removed either; it keeps counting towards the folder's size limit
until an administrator removes it the same way.

**Which quota applies.** `rast doctor` (section "storage quota") and Control
Panel → Storage say it:

- **filesystem** (kernel user quota enforced on the homes filesystem): the
  limit of `rast-u<id>` is soft = the account's quota, hard = soft + 5 %
  (at least 1 GiB) with the filesystem's grace time, and every app,
  Terminal and File Station are limited. Usage is everything the identity
  owns on that filesystem: apps, caches, the recycle bin and its files in
  shared folders.
- **RAST** (no kernel quota; production until the maintainer enables it,
  runbook in ADR 0005 section 9): File Station and App Center refuse writes
  over quota; a scan of HOME (every ten minutes when the account lists
  files, and when it opens the usage view) refreshes the usage; apps,
  Terminal and launchers are **not** limited.

RAST never edits `/etc/fstab`, turns quota on or changes grace times. With
user quota only accounting (not enforcing), `rast doctor` warns.

**Grace times.** Between the soft and the hard limit, the kernel allows
writes only for the filesystem's block grace time; after it, the soft limit
acts as the hard one until usage drops below it. RAST's limits (D4) assume
7 days. Grace times are global for each filesystem and quota type, so they
would affect accounts outside RAST too: RAST only reads them. `rast doctor`
prints them ("storage quota") and warns when the user block grace is not 7
days. Set them as root when quota is turned on (runbook in ADR 0005,
section 9):

```bash
sudo setquota -t -u 604800 604800 /   # user quota: 7 days for blocks and inodes
sudo setquota -t -g 604800 604800 /   # group quota (shared folder limits)
sudo repquota -u / | head -3          # shows the grace times
```

**Shared-folder size limits** (Control Panel → Shared folders, "Size limit"):
RAST checks them for File Station uploads, copies, moves into the folder
and restores from a recycle bin ("RAST only"). The usage it compares with
the limit is the folder's files plus the members' recycle bins at its top
(measured by the native helper, kept for a minute), plus the uploads still
running. Apps and terminals of members are not limited by it. With group
quota on the shared filesystem, the folder's read-write group also gets a
kernel limit ("filesystem, best effort": files moved in with another
group, `chgrp` or group-preserving copies are not counted against the
folder, only against their owner).

## The private data folder

Since 1.0.1 the database (`rast.db` with its `-wal` and `-shm` files) and
the master key (`secret.key`) live in `/var/lib/rast/private/` (`0700
rast:rast`, ADR 0006), and `backups/` is `0700`. The data directory itself
must stay traversable (`0751`) for native app identities and Package Center
users, so any account can *name* a file directly in it; with
`fs.protected_hardlinks = 0` it could then hard-link the database or the key
into a folder it owns (a shared folder, for example). In `private/` it
cannot.

- The first `rast serve` of 1.0.1 moves the files of an older release in,
  under the instance lock (hard link into `private/`, then the old name is
  replaced by a link in one rename), and logs what it moved. `rast doctor`
  lists files that still wait for the move.
- The old names stay as links (`rast.db -> private/rast.db`,
  `secret.key -> private/secret.key`), so scripts and an older release after
  a downgrade find the same files. Following a link needs `private/`, so
  the links give nobody else anything.
- If a file exists at both places as two different files (an older release
  restored a backup over the link after a downgrade), the server refuses to
  start and `rast doctor` reports a failure. Keep the file you need in
  `private/`, move the other one away, then start RAST.
- `rast doctor` warns when `fs.protected_hardlinks` is not `1` (the kernel
  default is `0`; systemd's `50-default.conf` sets `1` on most
  distributions). Set it with `sysctl -w
  fs.protected_hardlinks=1` and a file in `/etc/sysctl.d/`.

## Secrets and key rotation

`/var/lib/rast/private/secret.key` (0600) is the master key for CSRF
derivation and application-secret encryption. Rotating it invalidates all sessions and makes
existing encrypted app secrets unreadable. Procedure if rotation is required:

1. Back up the database and config, and take note of managed app secrets.
2. Stop the service, move `private/secret.key` aside (and remove the
   `secret.key` link next to `private/`), start the service (a new key is
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
| Move shared folders to Linux groups | [Shared folders as Linux groups](#shared-folders-as-linux-groups-rast-migrate-shares) |
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
| Native apps unavailable | `systemctl status rast-native`, `rast doctor` (Xpra binary/version, display firewall, helper socket, slice, uid range), `journalctl -u rast-native -n 100`; `native_enabled=true`; the installer disables native apps in auto mode (or fails with `RAST_NATIVE=1`) when Xpra is below 6.5 |
| Native app shows no window / black | `journalctl -u rast-native`; check the session unit (`systemctl status 'rast-xpra-*'`), the Xpra log under `/run/rast-linux/<uid>/`, and that the reverse proxy forwards WebSockets on `/linux-apps/` |
| Display firewall warning in `rast doctor` | Install the `nftables` package (`apt-get install nftables`) and restart `rast-native.service`; the helper then restricts the session display ports to the `rast` uid. The native backend works without it, but other local accounts could reach the loopback Xpra ports |
| App exits with `Authorization required`, `Invalid MIT-MAGIC-COOKIE-1` or `Missing X server or $DISPLAY` | The app got the wrong X11 auth file. The helper derives it from the running Xvfb (`-auth` argument) on every launch, so this means the session is stale/broken: `systemctl stop rast-linux.slice` (or close the session in the UI) and let the next launch start a fresh session. If it persists, check the identity home ownership (`ls -ld /var/lib/rast/linux-users/<id>` must be `700 rast-u<uid>`) and re-run `rast doctor` |
| Native app cannot open files in the RAST home | `getfacl /srv/rast/homes/<user>` must include the user's `rast-u<id>` identity with `r-x`; re-run `rast doctor`, re-launch the app to repair the ACL. The ACL is read-only by design — the app cannot write to the RAST home, and shared folders are not mapped |
| Migration fails | The app page shows the exact reason (usually: the original install file is gone — upload it via *Migrate* → provide file). Nothing is deleted; the Docker app keeps working |
| SQLite `database is locked` | Data dir on NFS/SMB (unsupported) or an external process holding the DB; move to local storage |
| High memory | Large file operations + many jobs; lower `RAST_JOB_CONCURRENCY`, check systemd `MemoryMax`, inspect `/api/system/info` |

For security incidents see [SECURITY.md](../SECURITY.md); for bugs, open an
issue with version (`rast version`), `rast doctor` output and relevant journal
lines (remove secrets and personal paths first).
