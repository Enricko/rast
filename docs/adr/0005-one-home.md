# 0005. One HOME per user

- Status: proposed (decisions D1–D14 need the maintainer's approval)
- Date: 2026-11-16

## Context

Phase 1 of the remake (`CLAUDE.md`) makes every RAST user one Linux account
(`rast-u<id>`) with **one HOME**, shared by File Station, Terminal and every
app. Today a person's data is spread over four places with different owners,
so a file saved by Chrome never appears in File Station. This ADR is the
design for tasks 1.2–1.5 (`docs/remake/PROMPTS.md`). It builds on ADR 0004:
apps that launchers install, and per-user Flatpak in Phase 3, live in HOME.

### What the code does today

- **Homes.** `users::create` (`crates/rast-base/src/users.rs`) creates
  `<storage>/homes/<username>` owned by `rast`, mode 0700. File Station serves
  it in-process (`Storage::resolve_root`, `crates/rast-base/src/storage/mod.rs`)
  through the safe-path layer (`storage/safe_path.rs`): no traversal, every
  symlink refused, symlinks skipped in listings.
- **Identity.** `rast-u<id>` (uid = gid = `linux_uid_base` + id, default base
  200000, user ids up to 1 000 000) has its own home
  `<data_dir>/linux-users/<id>` (`crates/rast-helper/src/native/identity.rs`),
  stored in `linux_identities.home`. Terminals use it as HOME, so shell
  history, `CARGO_HOME` and other caches live there.
- **Apps.** `start_run` (`crates/rast-session/src/linux_apps/native_session.rs`)
  calls `profile_prepare`, which creates a per-app profile
  `<data_dir>/linux-apps/profiles/<app>/<user>/{home,config,data,cache,state,tmp}`;
  HOME, `XDG_*_HOME` and `TMPDIR` point into it. On every launch it then calls
  `path_symlink` (a `RAST` link in the profile) and a recursive
  `path_acl … rx` on the RAST home.
- **Bug found while designing this.** `start_run` builds the RAST home from
  `identity.username`, which is the Linux account name `rast-u<id>`, not the
  RAST username. It links and ACL-grants `<homes>/rast-u<id>`, an empty folder
  it creates itself, so native apps never see the person's files. The
  Docker → native migration in `native_apps.rs` does the same. On the
  production host this created `homes/rast-u1`, `rast-u97` … `rast-u104`
  (link count 2, with ACLs) and `rast-u999999`.
- **Terminal.** `grant_terminal_acl` (`crates/rast-helper/src/native/daemon.rs`)
  adds `u:<uid>:rwX` or `u:<uid>:rX` recursively on every folder opened in a
  terminal. The grant is never revoked when RAST access changes.
- **Quota.** Application-level only (`storage/quota.rs`): `storage_usage`
  reserves bytes for File Station writes in one atomic `UPDATE`; `used_bytes`
  is the apparent size of the home; deleted files (recycle bin) and writes
  into shared folders do not count; administrators and `quota_bytes = 0` are
  unlimited; new accounts get `default_quota_bytes` (20 GiB). Shared folders
  have no limit.
- **Recycle bin.** `<storage>/.trash/<user_id>/<uuid>` plus the `trash` table
  (`storage/trash.rs`), for deletions in the home and in shared folders.
- **Other code that reads homes as `rast`.** Backup Center walks homes and
  shared folders and skips unreadable files with a warning
  (`crates/rast-server/src/backup/mod.rs`); the realtime watcher
  (`crates/rast-base/src/realtime/watch.rs`); the storage settings size walk
  (`crates/rast-server/src/api/settings.rs`). The legacy Docker Linux backend
  runs its container as the home's owner uid
  (`crates/rast-docker/src/worker/linux.rs`, `person_home`).
- **Template.** The root file agent (`crates/rast-agent/src/rootfs/`) already
  serves the `/api/files` JSON shapes over a Unix socket. The helper starts it
  (`rootfs_start`) and `crates/rast-server/src/api/rootfs.rs` (`dispatch`)
  forwards File Station requests to it.

### Production host (checked 2026-11-16, read-only)

| Check | Result |
| --- | --- |
| `findmnt -T /srv/rast` | `/` on `/dev/mapper/ubuntu--vg-ubuntu--lv`, **ext4**, `rw,relatime`; 754 G, 241 G free (64 % used) |
| `/proc/fs/ext4/dm-0/options` | `acl`, `user_xattr`, **`noquota`** |
| On the same filesystem | `/var/lib/rast` (production database), the OS, `/var/tmp` |
| `/tmp` | tmpfs with `usrquota` (set by systemd) |
| Kernel 7.0 | `CONFIG_QUOTA=y`, `CONFIG_QFMT_V2=m`, `CONFIG_QUOTACTL=y` |
| Tools | `quota` not installed (4.09 in the archive); `xfs_quota`, `tune2fs`, `setfacl` installed |
| `lsattr -p` on `/srv/rast/*` | project id 0 everywhere |
| ext4 `quota` / `project` features | **unconfirmed**: needs root. Ubuntu's `/etc/mke2fs.conf` does not enable them for ext4, so they are almost certainly off. Confirm with `sudo dumpe2fs -h /dev/mapper/ubuntu--vg-ubuntu--lv \| grep -iE 'features\|inode size'` |
| Free space in `ubuntu-vg` | **unconfirmed**: `sudo vgs ubuntu-vg` |
| systemd | 259 (Ubuntu 26.04.1 LTS); `systemd-run --socket-property` available |

What this means:

- **User quota** is possible on this ext4 filesystem but **not active**. It
  can be turned on in three ways, each a host change made by the maintainer
  (never by RAST or its installer):
  - **Q-a, journaled quota files on `/`:** add
    `usrjquota=aquota.user,jqfmt=vfsv1` to `/` in `/etc/fstab`, install
    `quota`, run `quotacheck -cum /` and `quotaon /`. Works online; user (and
    group) quota only.
  - **Q-b, ext4 `quota` and `project` features:** `tune2fs -O quota,project`
    needs the filesystem unmounted, so for `/` only from a rescue system.
    Downtime and risk on the production root filesystem.
  - **Q-c, a dedicated LV for `/srv/rast`:** ext4 created with
    `-O quota,project`, or XFS mounted with `uquota,pquota`. User and project
    quota, and user data can no longer fill `/`. Needs free extents in
    `ubuntu-vg`, a data copy and a short downtime.
- **Project quota** is not possible with Q-a: ext4 project quota needs both
  the `project` and the `quota` features, so only Q-b or Q-c.
- **Risk today:** HOME, the production database and the OS share one
  filesystem. An app that fills a HOME stops production RAST.

## Decision

### 1. Ownership model

| Path | Owner and mode | Notes |
| --- | --- | --- |
| `<storage>/homes/` | `rast:rast` 0755 (unchanged) | the server can still `stat` each HOME |
| `<storage>/homes/<username>` | `rast-u<id>:rast-u<id>` **0700** | the one HOME and the passwd home of `rast-u<id>`; `rast` has no access |
| `~/.local/share/Trash/{files,info}` | user, 0700 | XDG Trash (section 5) |
| `~/Shared/<share name>` | symlinks to `<storage>/shared/<container>` | maintained by the agent; it adds and removes only symlinks that point into the shared root and never touches other files there |
| `~/.local/bin` | user | on the session `PATH` (ADR 0004) |
| `<storage>/shared/<container>` | `root:rast-s<h>-rw` **2770**; ACL `g:rast-s<h>-ro:r-x`, `u:rast:rwx`; the same entries as default ACLs | `<h>` = first 16 hex digits of the share UUID |
| `<storage>/shared/<container>/.Trash-<uid>` | user, 0700 | recycle bin of a share (Trash spec "top directory" layout) |
| `<storage>/.trash/<user_id>` | `rast` 0700 | legacy only; emptied by the migration |
| `<data_dir>/linux-users/<id>`, `<data_dir>/linux-apps/profiles/<app>/<user>` | identity | legacy only; merged into HOME by the migration |

**A user's mode is derived, not stored.** HOME owned by the identity uid
means one-HOME mode (served by the user's file agent); HOME owned by `rast`
means legacy mode (served in-process, as today). No database column is
needed. Installs with some users in each mode work. Hosts without the helper
(`native_enabled = false`, non-Linux builds, tests) stay in legacy mode.

### 2. Per-user file agent and request forwarding

- **Agent.** A hidden subcommand `rast file-agent`, generalised from
  `rootfs/agent.rs` with a namespace trait: the root namespace (`/@root`, as
  today) and the home namespace (`/` is HOME; `/shared/<name>/…` resolves
  through the target of `~/Shared/<name>`, which must be a direct child of the
  shared root).
- **Unit.** The agent runs as `rast-u<id>`, with its supplementary groups
  from `/etc/group`, in a socket-activated transient system unit
  `rast-files-u<uid>` (Phase 1: in `rast-linux.slice`; Phase 2 moves it into
  the per-user slice or user manager). systemd creates the socket
  `/run/rast/files/u<uid>.sock` (`--socket-property`: `SocketUser=root`,
  `SocketGroup=rast`, `SocketMode=0660`) in a `root:rast 0750` directory, so
  the user's own processes can neither reach nor replace it. The agent also
  checks `SO_PEERCRED` (the `rast` uid) and a token, like the root agent.
  Hardening: `NoNewPrivileges=yes`, `ProtectSystem=strict`,
  `ReadWritePaths=<HOME> <shared root>`, `PrivateTmp=yes`,
  `PrivateNetwork=yes`, `RestrictAddressFamilies=AF_UNIX`, an empty
  capability set, `MemoryMax=512M`, `TasksMax=64`. It exits after 900 s
  without requests, but never while the user has a session, app or terminal
  unit.
- **Server.** A new `api/homefs.rs` next to `api/rootfs.rs` puts a dispatch
  middleware in front of `/api/files/*`, including trash and usage. For a
  one-HOME user the server still does every check it does today (session and
  CSRF, `LogicalPath::parse`, Effective Access for `/shared/<name>` with 404
  for hidden and 403 for read-only, audit, background jobs for copy, move and
  delete), then forwards path, query and body unchanged and streams the
  response back (client generalised from `RootFs::send`). `/@root` still goes
  to the root agent; legacy users stay in-process. Upload ids of the agent
  start with `usr-` (the root agent uses `sys-`). Agent replies are size
  bounded and treated as untrusted input. When the helper is down a one-HOME
  user gets 503; the server never falls back to in-process access.
- **Same JSON shapes:** `EntryDto`, `ListingDto`, `QuotaDto`, `UsageDto`,
  `TrashEntryDto`. `writable` comes from `access(W_OK)` as the user. A kernel
  `EDQUOT` becomes the existing 413 "storage quota exceeded".
- **Security.** The agent's uid is the hard boundary: a bug or a symlink race
  in the agent can only reach what `rast-u<id>` can reach. The safe-path layer
  stays (no traversal, symlinks never followed, `O_NOFOLLOW` on the last
  component). Listings keep skipping symlinks, except `~/Shared/*`, which are
  shown as folders whose `path` is `/shared/<name>`.
- **Realtime.** The agent watches the visible tree of HOME (not dot
  directories) and `~/Shared/*`, and streams logical paths to the server,
  which keeps the existing debounce and audience logic. The server's own
  watcher skips one-HOME homes.
- **UI.** File Station hides dot files by default and gets a "Show hidden
  files" toggle.

### 3. New helper operations

Rules for all of them: strict JSON (`deny_unknown_fields`); **no operation
takes a raw filesystem path** (the helper builds every path from its
configuration and validated ids or names); tree walks use `openat`,
`O_NOFOLLOW` and `AT_SYMLINK_NOFOLLOW` and never cross a device; commands run
with an argument vector and no shell; bounded timeouts and output; every
operation is idempotent.

| Operation | Fields | Validation and behaviour |
| --- | --- | --- |
| `home_ensure` | `userId`, `username` | Identity from `identity_ids`. `username` must pass `users::validate_username`, be lowercase, one path component and not reserved. Missing HOME: create it 0700 with the skeleton. Owned by the identity: repair mode and skeleton. Owned by `rast`: error `legacy_home` (needs the migration). Any other owner, or a symlink: `identity_conflict`. Sets the passwd home with `usermod --home` (never `--move-home`). |
| `files_agent_start`, `files_agent_stop` | `userId` | HOME must be owned by the identity. Start returns `{unit, socket, token}`. |
| `share_group_ensure` | `shareId` | UUID v4 only. Groups `rast-s<16 hex>-rw` and `-ro` (25 characters) created with `groupadd -K GID_MIN=… -K GID_MAX=…` in `[linux_uid_base + 1 000 001, linux_uid_base + 1 999 999]`. Reuses the gid of the container's group when it is free (restore on a new host). A group with that name outside the range is a conflict. |
| `share_group_remove` | `shareId` | `groupdel`, groups in the RAST range only. |
| `share_members_set` | `shareId`, `write`, `read` (lists of user ids) | At most 10 000 ids each, unique, disjoint, each with an existing identity. `gpasswd -M` per group (full replacement). Returns the user ids whose membership changed. |
| `share_dir_prepare` | `shareId`, `container`, `recursive` | `container` passes `shares::validated_container`; must be a real directory on the storage filesystem. Applies owner, mode, ACL and default ACL from section 1; sets project id = rw gid with the inherit flag when project quota is on. A recursive run also removes named-user ACL entries of RAST identities. |
| `share_dir_remove` | `shareId`, `container` | Removes the container tree (no-follow, one device). |
| `quota_status` | none | Device and filesystem type of the homes and shared roots; user and project quota each `off`, `accounting` or `enforced`; grace times. |
| `quota_set` | `userId`, `softBytes`, `hardBytes` | 0 = no limit; hard ≥ soft; both ≤ filesystem size. `quotactl(Q_SETQUOTA, USRQUOTA)` through `libc` (already a dependency). |
| `share_quota_set` | `shareId`, `softBytes`, `hardBytes` | Project id from the rw gid. Only with D3. |
| `quota_report` | `userIds`, `shareIds` | At most 10 000 ids; used bytes and limits from `Q_GETQUOTA`. |
| `home_migrate_scan`, `home_migrate_apply`, `home_migrate_rollback` | `userId`, `username` | Section 8. The scan is read-only with bounded output (counts, plus the first 50 paths per category). |
| `shares_migrate_scan`, `shares_migrate_apply`, `shares_migrate_rollback` | `shareId`, `container` | Same pattern for shared folders (task 1.3). |
| `backup_reader_start` | none | Starts the root reader unit `rast-backup-reader` (same pattern as `rootfs_start`). It only streams tar archives of the fixed homes and shared roots (section 7). |

**Trust assumption.** The helper cannot check the `userId` ↔ `username`
mapping of a legacy home, because it lives in the database. During the
migration it trusts the service, which already owns those files. After the
migration the owner uid is the binding: a HOME owned by identity X is never
served to anyone else, or chowned for anyone else.

### 4. Shared folders as Linux groups

- One read-write and one read-only group per shared folder. Membership
  follows the current Effective Access (`access::folder_access_by_id`):
  write or admin → rw group, read → ro group, explicit deny or no access →
  neither, deactivated account → neither. **Administrators are in every rw
  group (D6)**, which keeps today's administrator bypass in File Station. The
  server recomputes membership on every rule, group, share or account change
  and calls `share_members_set`.
- Running processes keep the groups they started with. The agent is
  restarted at once, so File Station is right immediately (the server's
  Effective Access check is the first gate anyway). A **revocation** restarts
  the user's session (apps and terminals) after a 60-second warning; a
  **grant** applies at the next session start, and the warning offers
  "Restart session now" (D7).
- Known limits: an app that creates a file with mode 0600 or 0644 narrows the
  ACL mask, so the group loses write access (as on any Linux file server);
  files keep their creator as owner; with project quota a move between HOME
  and a share becomes copy plus delete (`EXDEV`).

### 5. XDG Trash

- **Home:** `~/.local/share/Trash` per the freedesktop Trash specification
  1.0 (`files/`, `info/<name>.trashinfo` with a percent-encoded absolute
  `Path` and a local-time `DeletionDate`, `directorysizes`), managed by the
  agent. Linux apps and File Station share one trash.
- **Shared folders:** `<container>/.Trash-<uid>` (the specification's top
  directory layout, mode 0700). A deletion stays inside the share (same
  project id, no `EXDEV`) and only the person who deleted it can restore it,
  as today. `.Trash*` entries are hidden in listings. Entries that GIO moved
  from a share into the home trash are shown with their `/shared/…` path.
- `TrashEntryDto` is unchanged. Ids stay opaque strings: `h:` or
  `s:<shareId>:` followed by the encoded trash name. The agent purges entries
  by `DeletionDate` with the existing retention setting.

### 6. Quota

**Backends**, detected by the helper (`quota_status`):

- **Filesystem quota** (user quota enforced on the homes filesystem): the
  limit is the kernel limit of uid `rast-u<id>` and usage is the kernel's
  `curspace`. File Station, Terminal and every app are limited the same way.
  Shared folders get project quota when it is available (D3).
- **RAST fallback** (no kernel quota; **this production host today**): the
  server keeps reserving bytes for File Station writes (as today), and the
  agent scans HOME (allocated bytes, each hard-linked inode once, at most
  every 10 minutes and on demand) to refresh `storage_usage.used_bytes`. File
  Station and App Center refuse writes over quota; **apps, Terminal and
  launchers are not limited**. `rast doctor` and Control Panel say so. Shared
  folders have no limit, as today.
- `users.quota_bytes` stays the source of truth and `storage_usage` keeps the
  last reading, so `UserDto` and `QuotaDto` keep their shapes.
  Administrators and `quota_bytes = 0` stay unlimited.

**Limits (D4).** Soft limit = `quota_bytes`: File Station uploads and App
Center installs stop there. Hard limit = soft + max(5 %, 1 GiB) with a 7-day
grace period, so an app can still save work when File Station has reached the
quota. RAST warns at 90 % and at the soft limit.

**Apps in HOME count.** With uid quota, everything `rast-u<id>` owns on that
filesystem counts:

| Counts | Does not count |
| --- | --- |
| documents; `~/.config`, `~/.cache`, `~/.local/state`; XDG Trash | `.deb` installs, `/opt`, system-wide Flatpak (`/var/lib/flatpak`) |
| apps installed by launchers (ADR 0004): `~/.local/share/JetBrains/Toolbox/…`, `~/.local/bin`, AppImages | RAST-managed apps in `<data_dir>/linux-apps/apps` |
| per-user Flatpak (Phase 3): `~/.local/share/flatpak` (runtimes and apps) and `~/.var/app` | `/tmp` (tmpfs with its own `usrquota`) |
| files the user created in shared folders (D2); the user's files in `/var/tmp` on the same filesystem | files owned by `rast` (written by legacy File Station) |

Approximate sizes, to be measured in tasks 3.2 and 3.6: a Flatpak runtime is
0.5–1.5 GB (OSTree deduplicates only inside one installation), an app
0.1–1 GB, a JetBrains IDE 1–3 GB plus its caches, a browser profile
0.3–2 GB. An active user with a few apps can use 5–15 GB before storing a
single document, so the 20 GiB default is tight. Therefore:

- The usage view gets a breakdown by location: Files, Apps (Flatpak,
  `~/.var/app`, launcher trees), Caches, Trash, In shared folders (additive
  fields `UsageDto.byLocation` and `UsageDto.quotaBackend`).
- App Center compares the installed size (`flatpak remote-info`) with the
  free quota before a per-user Flatpak install.
- Task 3.2 evaluates per-user apps on system-wide runtimes (an administrator
  installs common runtimes once), so each user does not keep a copy of every
  runtime.
- The fallback counts all of HOME too (apps, caches, trash), but not the
  user's files in shared folders.
- **Host recommendation (D1):** Phase 1 ships with detection and the
  fallback. The maintainer turns on kernel quota on production before Phase 3
  puts per-user Flatpak and launchers into HOME: Q-c if `ubuntu-vg` has free
  space, otherwise Q-a (user quota now, project quota later).

### 7. App profiles, Terminal, Backup Center and other modules

- **Apps.** HOME is the one HOME, `XDG_*_HOME` keep their defaults, `TMPDIR`
  is not overridden (task 3.5 decides where AppImages extract). The
  per-launch `path_acl -R`, the `RAST` symlink and the stray
  `homes/rast-u<id>` folders go away. **Separate settings are opt-in per app
  (D10):** `"profile": "isolated"` in the `linux_apps.launcher` JSON sets
  `XDG_CONFIG_HOME`, `XDG_DATA_HOME`, `XDG_CACHE_HOME` and `XDG_STATE_HOME`
  under `~/.local/share/rast/profiles/<app>/`; HOME stays the same, so
  documents are still shared. "Also delete its settings" removes only such
  isolated profiles. `profile_clean_locks` moves into the agent. The
  `profile_*` operations stay for legacy users.
- **Terminal.** HOME is the one HOME. One-HOME users get no
  `grant_terminal_acl` (kernel permissions and groups decide). Allowed working
  directories are HOME and the shares the user can open. Terminal caches now
  count in the quota. Administrator root shells do not change.
- **Backup Center (D12).** The `users` scope reads homes and shared folders
  through `backup_reader_start` (root). The archive keeps numeric uid and gid,
  mode and POSIX ACLs (PAX `SCHILY.acl.*`), never follows symlinks, and
  excludes `~/.cache`, the trash folders and `~/.local/share/flatpak`. The
  layout `users/homes/<username>/…` stays; the manifest records
  `ownersPreserved`. The restore guide in `docs/OPERATIONS.md` changes to
  `tar --numeric-owner --same-owner --acls` as root, **without**
  `chown -R rast:rast` (which would break one HOME), followed by `rast doctor`,
  which re-applies the share groups. This must land before or with task 1.5.
- **Other modules.** `users::create` calls `identity_ensure` and `home_ensure`
  when the helper is available. Storage settings use `statvfs` and
  `quota_report` instead of a tree walk. Deactivating an account also stops
  its agent. The legacy Docker Linux backend keeps working until Phase 4: its
  container runs as the home owner, which becomes `rast-u<id>`.

### 8. Migration

Order: (1) `rast migrate shares` (task 1.3: groups and ACLs; `rast` keeps
`rwx`, so legacy users keep working) → (2) `rast migrate home`, user by user
(task 1.5) → (3) cleanup of legacy leftovers, only after verification and
confirmation.

`rast migrate home [--user <name> | --all] [--dry-run | --rollback] [--json]`
runs as `rast` (`sudo -u rast rast migrate home …`): it opens the database
itself and asks the helper for the root steps. Apply and rollback need
`rast.service` stopped (the existing instance lock, D9); the dry-run may run
while the server runs.

- **Dry-run report, per user:** size (apparent and allocated) of the RAST
  home, the identity home, each app profile and the legacy recycle bin;
  entries that change owner (`rast` → `rast-u<id>`); entries owned by root or
  other uids (left unchanged); hard-linked files (skipped); legacy ACL entries
  to remove; app profiles to merge and their conflicts (first 50 paths);
  `~/Shared` name collisions; stray `homes/rast-u<id>` folders; **projected
  usage against the quota**, because profiles, caches and terminal data now
  count.
- **Permission backup first.** Before changing anything the helper writes a
  journal (gzip JSON lines: relative path, type, uid, gid, mode, ACL, device
  and inode; every move it will make; the legacy trash rows) to
  `<data_dir>/migrations/home/<userId>/<stamp>/`, root:root 0700 and 0600, and
  syncs it to disk.
- **Apply, per user, resumable and idempotent:**
  1. Stop the user's session, apps, terminals and agent; refuse if any unit
     stays active.
  2. Chown only `rast`-owned entries to the identity, without following
     symlinks and on one device; skip and report files with more than one
     hard link.
  3. Remove `u:<identity>` ACL entries.
  4. Move the identity home's entries into HOME (rename); leave and report
     conflicts; drop `.Xauthority`.
  5. Merge each app profile all-or-nothing: `config` → `~/.config`, `data` →
     `~/.local/share`, `state` → `~/.local/state`, `cache` → `~/.cache`,
     `home/*` → `~/`. An app with any conflict keeps its profile as an
     isolated profile for that user.
  6. Move legacy recycle-bin payloads into the XDG Trash (home origin) or
     `<share>/.Trash-<uid>` (share origin); the CLI deletes their `trash` rows
     after the helper reports success.
  7. `usermod --home`; update `linux_identities.home` (a data update, no
     schema change); apply the quota.
  8. Chown the HOME directory itself last: this switches the user to
     one-HOME mode.
- **Rollback**, from the journal and with the server stopped: move merged
  items back (only if they still exist with the same type), restore the
  recorded owners and ACLs, chown entries created after the migration to
  `rast`, restore the recycle-bin payloads and rows, set the passwd home back,
  and set HOME back to `rast:rast 0700`. The helper re-validates every journal
  entry: only relative, normalised paths inside the user's roots; only the
  owners `rast` and the identity, never uid 0; set-uid and set-gid bits
  dropped. A tampered journal cannot create a root-owned or set-uid file.
- **Cleanup** is a separate, confirmed step: empty legacy directories,
  `.trash/<user_id>`, stray `homes/rast-u<id>` folders (D13). Nothing is
  deleted without confirmation.

### Decisions that need the maintainer's approval

| # | Decision | Recommendation |
| --- | --- | --- |
| D1 | Production quota: RAST fallback now; the maintainer turns on kernel quota (Q-c dedicated LV, or Q-a journaled user quota on `/`) before Phase 3 reaches production. Needs root, `/etc/fstab` or LVM changes and downtime. | Q-c if `ubuntu-vg` has free space, otherwise Q-a |
| D2 | Kernel uid quota counts everything the user owns on that filesystem, including apps, Flatpak, caches, the trash and the user's files in shared folders. Today the trash and shared-folder writes do not count. | accept (as on a NAS) |
| D3 | Shared-folder size limits need a column `shares.quota_bytes` (**migration 0015**), applied as project quota | approve the migration, or defer limits and use project quota only to show share usage |
| D4 | Soft limit = `quota_bytes`, hard = soft + max(5 %, 1 GiB), 7-day grace; usage = allocated bytes; review the 20 GiB default for new accounts | accept; keep the default unless decided otherwise |
| D5 | `rast` keeps `rwx` on shared folders (ACL) during the transition and has no access to any HOME | accept |
| D6 | Administrators are members of every share's rw group, so their apps see every share too | accept |
| D7 | A revocation restarts the session after a 60-second warning; a grant applies at the next session start | accept |
| D8 | The on-disk layout of section 1 (HOME owned by `rast-u<id>`, passwd home moves, share containers `root:<rw group> 2770`, XDG Trash, legacy `.trash` emptied); new accounts get one HOME when the helper is available | accept |
| D9 | `rast migrate home` and `rast migrate shares` need `rast.service` stopped for apply and rollback | accept |
| D10 | Separate app settings are opt-in through `linux_apps.launcher` JSON (no migration); "Also delete its settings" removes isolated profiles only | accept |
| D11 | Share group gids come from a range derived from `linux_uid_base`; no new configuration key | accept |
| D12 | Backup `users` scope through a root reader, keeping owners, modes and ACLs and excluding `~/.cache`, trash and `~/.local/share/flatpak`; the restore guide changes | accept |
| D13 | Stray `homes/rast-u<id>` folders from the `start_run` bug are removed only after confirmation; whether to fix the bug on 0.18.x separately | fix separately |
| D14 | Additive API fields `UsageDto.byLocation` and `UsageDto.quotaBackend`; opaque XDG-based trash ids | accept |

## Consequences

- **Task 1.2:** `rast file-agent`, `api/homefs.rs`, `home_ensure`,
  `files_agent_start`/`files_agent_stop`, realtime events from the agent,
  the hidden-files toggle. No user is switched yet.
- **Task 1.3:** share groups, the `share_*` operations, membership sync, the
  session restart warning, `rast migrate shares`.
- **Task 1.4:** XDG Trash, quota backends, the `quota_*` operations, the
  usage breakdown.
- **Task 1.5:** `rast migrate home`, the journal, removal of the per-launch
  `setfacl -R` and the `RAST` symlink, the backup reader, an Upgrade note.
- **Test host:** one HOME needs the helper and a native instance, and only
  one native RAST may run per host. The Phase 1 exit (a file saved by Chrome
  visible in File Station; dry-run, apply and rollback) therefore runs on the
  separate test host, like task 0.6.
- **Docs, updated with the implementation:** `docs/NATIVE-LINUX-APPS.md`,
  `docs/TERMINAL.md`, `docs/OPERATIONS.md` (restore), `docs/API.md`,
  `docs/ARCHITECTURE.md`, `docs/CONFIGURATION.md`; CHANGELOG Upgrade notes for
  the quota meaning, the trash counting, the layout and the restore steps.
- **Risks:**
  - The fallback does not protect the OS filesystem from apps (D1).
  - Group membership of running processes is stale until the session
    restarts (D7).
  - Apps can narrow the ACL mask of files in shared folders.
  - With project quota, moves into a share become copies (`EXDEV`).
  - Each agent uses inotify watches of its uid; only visible trees are
    watched.
  - While the helper is down, one-HOME users have no File Station.
  - In Phase 2 the agent socket must stay reachable by `rast` when the agent
    moves into the user manager.

### Tests

Unit and integration tests (no root; `tempfile`; fakes as in today's helper
tests):

- **Helper protocol:** every new operation rejects unknown fields, bad ids,
  invalid or reserved usernames, share ids that are not UUIDs, oversized,
  duplicated or overlapping member lists, and limits out of range; no
  operation accepts a path field.
- **Helper daemon** (fake `usermod`, `groupadd`, `gpasswd`, `setfacl` and
  `systemd-run` that log their argv): `home_ensure` creates, repairs and is
  idempotent; it refuses a symlinked HOME, another owner and a legacy owner;
  `groupadd -K GID_MIN/GID_MAX`; `gpasswd -M`; ACL argv; socket properties of
  the agent unit; `quotactl` behind a trait with a fake.
- **Migration planner** on temporary trees (chown behind a trait): conflict
  detection; all-or-nothing profile merge; hard links skipped; foreign owners
  reported; a planted symlink to a file outside is never followed; apply twice
  is a no-op; resume after a crash at every step; rollback restores the
  recorded tree exactly; a tampered journal (uid 0, set-uid, `..`, absolute
  path, symlinked component) is refused.
- **File agent contract:** the same request set against in-process storage
  and against the agent gives the same JSON (apart from ids and times) for
  list, stat, mkdir, rename, upload, chunked upload, ranged download,
  preview, text, search, copy, move and delete; traversal (`..`, `%2e%2e`,
  NUL, absolute paths) is rejected; symlink escape is rejected in the middle
  and in the last component; `~/Shared` mapping; peer uid and token checks;
  `EDQUOT` becomes 413.
- **XDG Trash:** `.trashinfo` encoding and decoding (spaces, Unicode,
  newline, `%`), name collisions, `directorysizes`, restore conflicts,
  retention purge, a fixture written by `gio trash`, share `.Trash-<uid>`.
- **Quota:** backend detection, fallback reservation plus scan (allocated
  bytes, hard links counted once), soft and hard limits, location breakdown.
- **Server routing:** one-HOME user → agent, legacy user → in-process,
  `/@root` → root agent, mixing the root file system with storage refused,
  `usr-` upload ids, 503 when the helper is down.
- **Access → membership:** a deny beats a group allow, administrators are in
  every rw group, deactivated users are removed, revocation and grant follow
  D7.
- **Backup:** owner, mode and ACL in the tar headers, exclusions, no symlink
  traversal.

Cross-user and end-to-end tests (test host, native instance; running them
needs approval):

- User A cannot list, read or write B's HOME: File Station answers 404 or 403,
  A's terminal and A's apps get `EACCES`; the `rast` service gets `EACCES` on
  every HOME.
- A's processes cannot connect to A's or B's agent socket.
- A read-only member can read and gets `EACCES` or 403 on write; a non-member
  gets `EACCES` or 404; after a revocation and the session restart the access
  is gone.
- The migration refuses while the user's units run; a planted symlink to
  `/etc/shadow` and a hard link to a file outside HOME are neither followed
  nor chowned.
- Chrome saves to `~/Downloads` and the file is visible in File Station at
  once; a file uploaded in File Station opens in Chrome (Phase 1 exit).
- `rast migrate home --dry-run`, apply and `--rollback` on the disposable
  instance; the tree, owners and ACLs are equal before and after the
  rollback.
