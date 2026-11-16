# 0005. One HOME per user

- Status: accepted (decisions D1–D14 taken by the maintainer on 2026-11-16)
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

### Production host (checked 2026-11-16, read-only; root checks run by the maintainer)

| Check | Result |
| --- | --- |
| `findmnt -T /srv/rast` | `/` on `/dev/mapper/ubuntu--vg-ubuntu--lv`, **ext4**, `rw,relatime`; 754 G, 241 G free (64 % used) |
| `/proc/fs/ext4/dm-0/options` | `acl`, `user_xattr`, **`noquota`** |
| On the same filesystem | `/var/lib/rast` (production database), the OS, `/var/tmp` |
| `/tmp` | tmpfs with `usrquota` (set by systemd) |
| Kernel 7.0 | `CONFIG_QUOTA=y`, `CONFIG_QFMT_V2=m`, `CONFIG_QUOTACTL=y` |
| Tools | `quota` not installed (4.09 in the archive); `xfs_quota`, `tune2fs`, `setfacl` installed |
| `lsattr -p` on `/srv/rast/*` | project id 0 everywhere |
| ext4 `quota` / `project` features (`sudo dumpe2fs -h /dev/mapper/ubuntu--vg-ubuntu--lv \| grep -iE 'features\|inode size'`) | **neither `quota` nor `project`** in the feature list; inode size **256** |
| Free space in `ubuntu-vg` (`sudo vgs ubuntu-vg`) | 2 PV, 2 LV, VSize <1.14t, **VFree 0** |
| Quota kernel modules | `quota_v2` and `quota_tree` in `linux-modules-7.0.0-38-generic` (installed, not loaded) |
| systemd quota units | `systemd-quotacheck-root.service` and `quotaon-root.service` (`quotaon -ug /`) ship with systemd 259 |
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
    `ubuntu-vg`, a data copy and a short downtime. **Not possible on this
    host without an extra disk: `ubuntu-vg` has VFree 0.**
- **Project quota** is not possible with Q-a: ext4 project quota needs both
  the `project` and the `quota` features, so only Q-b or Q-c. The inode size
  of 256 would allow it (project ids live in the extended inode), but Q-b
  needs the root filesystem offline and Q-c a new disk, so **production will
  not have project quota** (D1).
- **Risk today:** HOME, the production database and the OS share one
  filesystem. An app that fills a HOME stops production RAST.

## Decision

### 1. Ownership model

| Path | Owner and mode | Notes |
| --- | --- | --- |
| `<storage>/homes/` | `rast:rast` 0755 (unchanged) | the server can still `stat` each HOME |
| `<storage>/homes/<username>` | `rast-u<id>:rast-u<id>` **0700** | the one HOME and the passwd home of `rast-u<id>`; `rast` has no access |
| `~/.local/share/Trash/{files,info}` | user, 0700 | XDG Trash (section 5) |
| `~/Shared/<share name>` | symlinks to `<storage>/shared/<container>` | for apps only (file dialogs, terminals); the agent never follows or lists them (section 2). Task 1.3 decides who maintains them; whoever does adds and removes only symlinks that point into the shared root and never touches other files there |
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
  today) and the home namespace (`/` is HOME; `/shared/<name>/…` is the
  share's container, see "Shared folders" below).
- **Shared folders.** The agent never follows `~/Shared/<name>`: checking a
  link and then following it leaves a race, and a link pointed at another
  share would bypass a share size limit that only RAST enforces (fallback
  Q-a). Instead, after its access checks the server sends the share id,
  display name and container from its database for every `/shared/<name>`
  that a request names, in the header `x-rast-shares` (hex-encoded JSON).
  Only the server sets it: the header from the browser is removed, and the
  agent parses it strictly (unknown fields, an invalid container or a
  non-UUID id refuse the whole request with 400). The agent opens the
  container directly below its descriptor of the shared root, as the root
  of that request's namespace (a link there is refused), then checks that it
  is a folder owned by root or by the owner of the shared root, whose group
  is the shared root's (legacy containers) or the share's read-write group
  `rast-s<h>-rw` (otherwise 403). A share that the header does not name is
  404. The `~/Shared` links are for apps only; File Station lists shared
  folders from `/api/files/roots`, as before.
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
  unit. Further hardening (`ProtectKernelTunables`, `ProtectKernelModules`,
  `ProtectKernelLogs`, `ProtectControlGroups`, `RestrictNamespaces`,
  `RestrictSUIDSGID`, `LockPersonality`, `SystemCallFilter=@system-service`
  and similar) is added once it has been verified under real systemd on the
  test host. Deactivating the account stops the agent (section 7).
- **Server.** A new `api/homefs.rs` next to `api/rootfs.rs` puts a dispatch
  middleware in front of `/api/files/*`, including trash and usage. For a
  one-HOME user the server still does every check it does today (session and
  CSRF, `LogicalPath::parse`, Effective Access for `/shared/<name>` with 404
  for hidden and 403 for read-only, audit, background jobs for copy, move and
  delete), then forwards path, query and body unchanged and streams the
  response back (client generalised from `RootFs::send`). `/@root` still goes
  to the root agent; legacy users stay in-process. Upload ids of the agent
  start with `usr-` (the root agent uses `sys-`). Agent replies are untrusted
  input (see the invariant below). When the helper is down a one-HOME user
  gets 503; the server never falls back to in-process access. When an agent
  refuses the token (its socket unit was restarted with a new one), the
  server asks the helper again and sends the request once more; a request
  with a streamed body cannot be replayed and gets 503.
- **Invariant: agent output is untrusted.** The agent runs with the same uid
  as every app and terminal of its user, so any of them may have taken it
  over. The server therefore treats everything an agent sends as untrusted
  input: JSON replies are read with a size limit and parsed strictly
  (entries must carry valid logical paths, upload ids the `usr-` prefix);
  error statuses keep their meaning but static messages come from the
  server's own list and other messages are bounded; streamed replies pass
  only file headers, the server sets `nosniff` and the sandboxing CSP
  itself, and a 401 or redirect becomes 502. What the agent must not decide
  travels the other way, in headers that only the server sets: the free
  quota and the share containers.
- **Same JSON shapes:** `EntryDto`, `ListingDto`, `QuotaDto`, `UsageDto`,
  `TrashEntryDto`. `writable` comes from `access(W_OK)` as the user. A kernel
  `EDQUOT` becomes the existing 413 "storage quota exceeded".
- **Security.** The agent runs with the uid of every app and terminal of its
  user, so that uid is no boundary between the agent and the user's own
  files. A path-based agent would be a confused deputy: when another member
  of a shared folder swaps an entry of the share for a symlink between the
  agent's check and its use, the agent would copy the user's `~/.ssh` into
  the share or write into `~/.config/autostart`. Every file operation of the
  agent therefore goes through one directory-descriptor layer
  (`homefs::fsat`):
  - a path is opened below a descriptor of its namespace root (HOME or a
    share container) with `openat2` and `RESOLVE_BENEATH |
    RESOLVE_NO_MAGICLINKS | RESOLVE_NO_SYMLINKS` (on kernels without
    `openat2`, a walk of `openat` calls with `O_NOFOLLOW`); no symlink is
    followed anywhere, also not one that stays inside the root; below a
    share container also `RESOLVE_NO_XDEV` (the walk compares devices),
    and an entry on another file system is refused, so a mount inside a
    shared folder is never entered; HOME keeps the user's own mounts;
  - the operation then works with `*at` calls, by single names, on folders
    it holds open, and compares what it opens with the inode it checked;
  - new files are created with `O_CREAT | O_EXCL | O_NOFOLLOW`; a new folder
    is opened after `mkdirat` and must be empty and owned by the agent;
    renames use `RENAME_NOREPLACE`;
  - copy, move and upload work through descriptors: tree walks open each
    folder with `O_NOFOLLOW` below the folder that held it, an upload keeps
    its temporary file and its destination folder open from init to
    completion, a failed copy removes only what it created, and a move
    removes its source only while it is still the entry that was copied.

  An entry swapped for a link while the agent works is refused (403); a
  folder that is moved away keeps receiving the operation it was checked
  for, wherever it now is, and never redirects it. The safe-path layer still
  validates every name first. Tests swap entries for links at points in the
  middle of a copy and an upload, on the source and the destination side.
  Listings keep skipping symlinks, `~/Shared/*` included.
- **Realtime.** The agent watches the visible tree of HOME (not dot
  directories) and streams logical paths to the server, which keeps the
  existing debounce and audience logic. The server's own watcher skips
  one-HOME homes and keeps watching the shared root, so changes in shared
  folders reach their members as before.
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
| `share_group_ensure` | `shareId`, `container` (optional) | UUID v4 only. Groups `rast-s<16 hex>-rw` and `-ro` (25 characters) created with `groupadd -K GID_MIN=… -K GID_MAX=…` in `[linux_uid_base + 1 000 001, linux_uid_base + 1 999 999]`. Reuses the gid of the container's group (and of its named read-only ACL group) when it is free and in the range (restore on a new host); that is what the optional `container` is for. A group with that name outside the range is a conflict. |
| `share_group_remove` | `shareId` | `groupdel`, groups in the RAST range only. |
| `share_members_set` | `shareId`, `write`, `read` (lists of user ids) | At most 8 000 ids each (task 1.3: `gpasswd -M` takes the list as one argument, and Linux caps one argument at 128 KiB), unique, disjoint. Users without their `rast-u<id>` account yet are left out and returned as `skipped`; an account with another uid is a conflict. `gpasswd -M` per group (full replacement), only when it changes. Returns, per group, the user ids added and removed. |
| `share_dir_prepare` | `shareId`, `container`, `recursive` | `container` passes `shares::validated_container`; must be a real directory on the storage filesystem. Applies owner, mode, ACL and default ACL from section 1; sets project id = rw gid with the inherit flag when project quota is on. A recursive run also removes named-user ACL entries of RAST identities. |
| `share_dir_remove` | `shareId`, `container` | Removes the container tree (no-follow, one device); only a container owned by `rast` or root. |
| `share_dir_release` | `shareId`, `container` | Task 1.3: the container of a deleted shared folder (whose data RAST keeps) goes back to `rast:rast` 0700 without ACLs before its groups are removed, so a later share that gets the freed gid cannot open it. |
| `quota_status` | none | Device and filesystem type of the homes and shared roots; user and project quota each `off`, `accounting` or `enforced`; grace times. |
| `quota_set` | `userId`, `softBytes`, `hardBytes` | 0 = no limit; hard ≥ soft; both ≤ filesystem size. `quotactl(Q_SETQUOTA, USRQUOTA)` through `libc` (already a dependency). |
| `share_quota_set` | `shareId`, `softBytes`, `hardBytes` | Project quota (project id = rw gid) when available, otherwise group quota on the rw gid if task 1.3 accepts it (D3). Returns the backend that enforces the limit. |
| `quota_report` | `userIds`, `shareIds` | At most 10 000 ids; used bytes and limits from `Q_GETQUOTA`. |
| `home_migrate_scan`, `home_migrate_apply`, `home_migrate_rollback` | `userId`, `username` | Section 8. The scan is read-only with bounded output (counts, plus the first 50 paths per category). |
| `shares_migrate_scan`, `shares_migrate_apply`, `shares_migrate_rollback` | `shareId`, `container` | Same pattern for shared folders (task 1.3). |
| `share_dir_repair` | `shareId`, `container` | Daily repair pass (D3 decision, 2026-11-16): the recursive `share_dir_prepare` of a container that already has the layout (owned by root); a legacy container is left for `rast migrate shares`. Keeps the container's deny entries. |
| `share_deny_set` | `shareId`, `container`, `userIds` (at most 256) | D7 decision (2026-11-16): the full set of `u:rast-u<id>:---` entries on the access ACL of a prepared container, the stopgap while a full revocation waits for the user's session restart. Entries of users not listed are removed; the default ACL is not touched. |
| `share_session_groups` | `userIds` (1 to 200) | Read-only: per user, how many processes run with the identity's effective uid and which gids of the share range they carry (`Groups:` of `/proc/<pid>/status`), with the names of those that are share groups. Lets the server find sessions that still hold a revoked group after a server restart. |
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
- **Consequence of D6:** every app and terminal of an administrator account
  can open every shared folder with the kernel's permission, not only through
  File Station. A compromised or misbehaving app in an administrator session
  reaches all shared data. Administrators should therefore use a non-admin
  account for everyday app use. Task 1.3 writes this into
  `docs/NATIVE-LINUX-APPS.md`.
- Running processes keep the groups they started with. The agent is
  restarted at once, so File Station is right immediately (the server's
  Effective Access check is the first gate anyway). A **revocation** restarts
  the user's session (apps and terminals) after a 60-second warning; a
  **grant** applies at the next session start, and the warning offers
  "Restart session now" (D7, accepted for now).
- **Alternative to the forced restart, evaluated in task 1.3.** On a
  revocation the helper adds a named-user ACL entry `u:rast-u<id>:---` to the
  share's container directory only (not as a default ACL). POSIX ACLs check
  named-user entries before any group entry, so path lookups through the
  container fail with `EACCES` at once, although the process still carries
  the old group. The session then restarts when the user is ready, and the
  helper removes the entry after the restart (or when access is granted
  again). Known gaps, to be measured in task 1.3:
  - a process whose working directory is inside the share keeps resolving
    relative paths, because lookups from the cwd do not pass the container;
  - file and directory descriptors opened before the revocation stay usable
    (including `openat` below an open directory);
  - hard links that the user made from share files into HOME before the
    revocation (allowed for rw members by `protected_hardlinks`) keep the
    inode reachable;
  - the extra entries need ACL space on the container inode (bounded by the
    number of pending restarts).
  If the gaps are acceptable, task 1.3 replaces the 60-second forced restart
  with this entry plus a "Restart session" prompt; the decision stays with
  the maintainer. **Decided (2026-11-16, section 10):** the forced restart
  stays, and the entry is added as a stopgap during the warning.
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
  Shared-folder limits use project quota when it is available, otherwise
  possibly group quota on the rw group (D3, below).
- **RAST fallback** (no kernel quota; **this production host until the
  maintainer enables Q-a**, D1): the
  server keeps reserving bytes for File Station writes (as today), and the
  agent scans HOME (allocated bytes, each hard-linked inode once, at most
  every 10 minutes and on demand) to refresh `storage_usage.used_bytes`. File
  Station and App Center refuse writes over quota; **apps, Terminal and
  launchers are not limited**. `rast doctor` and Control Panel say so. A
  shared-folder limit (D3) is checked by RAST for File Station writes only.
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
- **Production host (D1, decided):** Phase 1 ships with detection and the
  fallback. Q-c is not possible without an extra disk (`ubuntu-vg` has
  VFree 0), so production runs on the RAST fallback until just before Phase 3
  reaches production. Then the maintainer enables **Q-a** (journaled user and
  group quota on `/`, runbook in section 9). Q-c only if a new disk is added
  later. RAST, its installer and `rast doctor` never change `/etc/fstab` or
  turn quota on.

**Shared-folder limits (D3, decided).** Migration **0015** adds the column
`shares.quota_bytes`, nullable; `NULL` = no limit (all existing shares).
The limit is applied by the strongest backend available, and the UI (shared
folder settings and the usage view) shows which one enforces it:

| Backend | Enforced by | What is limited |
| --- | --- | --- |
| Project quota (Q-b, Q-c) | filesystem | every write into the share, by any process |
| Group quota on the share's rw group (Q-a), if task 1.3 finds it sufficient | filesystem | files whose group is the rw group |
| None (fallback) | RAST only | File Station writes into the share |

Production will not have project quota, so **task 1.3 evaluates whether
group quota on the rw group is enough** under Q-a. The container is setgid
(2770), so new files and folders inherit the rw group and count against its
group quota. Known gaps:

- a file moved into the share with `mv` on the same filesystem keeps its old
  group (for example the user's primary group `rast-u<id>`) and is not
  counted against the share; a file moved out keeps the rw group and is
  still counted against the share while it lives in HOME;
- a member can `chgrp` a file they own to another of their groups (their
  primary group, another share's rw group);
- `cp -p`, `rsync -a` and `tar` extraction keep the source group;
- these files always stay in the **user quota** of their owner, so nothing
  escapes all accounting; only the share's own limit is bypassed.

Task 1.3 also measures a repair pass (the helper or the agent resets the
group of foreign-group entries below a container to its rw group, bounded and
no-follow) and reports whether "filesystem" may be shown for group quota or
only "filesystem, best effort". `share_quota_set` sets the project or the
group limit accordingly.

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

### 9. Runbook: enable Q-a on the production host (maintainer only)

Done by the maintainer as root, just before Phase 3 reaches production, in a
quiet hour. RAST never runs these steps. Journaled quota (`jqfmt=vfsv1`) needs
no `quotacheck` after a crash.

Needs: package `quota` (4.09 in the Ubuntu archive: `quotacheck`, `quotaon`,
`setquota`, `repquota`); kernel modules `quota_v2` and `quota_tree` (in
`linux-modules-$(uname -r)`, already installed; `CONFIG_QUOTA=y`,
`CONFIG_QUOTACTL=y` are built in). The systemd units
`systemd-quotacheck-root.service` and `quotaon-root.service` turn quota on at
boot once `/etc/fstab` has the options.

1. Preparation: back up `/etc/fstab`; make a RAST backup; check that
   `/` has some free space for the two quota files.

   ```bash
   sudo cp -a /etc/fstab /etc/fstab.before-quota
   sudo apt install quota
   sudo modprobe quota_v2 quota_tree
   printf 'quota_v2\nquota_tree\n' | sudo tee /etc/modules-load.d/quota.conf
   ```

2. Add the options to the `/` line in `/etc/fstab` (keep the device as it is):

   ```text
   ... / ext4 defaults,usrjquota=aquota.user,grpjquota=aquota.group,jqfmt=vfsv1 0 1
   ```

   Check with `sudo findmnt --verify` before going on.

3. Apply without a reboot, create the quota files and turn quota on:

   ```bash
   sudo mount -o remount /
   sudo quotacheck -ugcm /     # first scan of all files; -m: no read-only remount
   sudo quotaon -ug /
   sudo quotaon -pug /         # both must say "is on"
   ```

   `quotacheck -m` on a live filesystem can be slightly off for files that
   change during the scan; run it while the RAST services are idle. Journaled
   quota stays correct from then on.

4. Verify: `sudo repquota -ug /` lists the `rast-u<id>` users;
   `sudo -u rast rast doctor` reports user quota `enforced` (from
   `quota_status`, task 1.4). Reboot once in the maintenance window and check
   `systemctl status quotaon-root.service` and `quotaon -pug /` again.

5. Limits are set by RAST (`quota_set`, `share_quota_set`), not by hand.

Rollback: `sudo quotaoff -ug /`, restore `/etc/fstab.before-quota`,
`sudo mount -o remount /`, remove `/aquota.user` and `/aquota.group`; RAST
falls back on its next `quota_status` check.

### 10. Task 1.3: implementation notes and evaluations

**Implemented** (helper, server, agent, CLI, desktop):

- Helper: the operations of section 3 for shared folders, plus
  `share_dir_release`. Trees are walked through no-follow directory
  descriptors and changed with `fchown`, `fchmod` and `fsetxattr` on the
  checked descriptor (a small POSIX ACL codec, `native::acl`); symlinks,
  special files, files with more than one hard link, root-owned entries,
  entries owned by any account other than `rast` and the RAST identities,
  and other file systems are left unchanged and reported. (The owner check
  came with the review of 2026-11-16: with `fs.protected_hardlinks = 0` a
  member can hard-link any file it can reach into a share, and once the
  original name is gone that file has a single link.)
- Server: a syncer recomputes membership on every `access::bump_generation`
  (now also on role and status changes of accounts) and every five minutes,
  for the active accounts that have a Linux identity, and calls
  `share_members_set` (a shared folder whose groups cannot be set is logged
  and skipped; the pass goes on with the others and acts on their changes,
  and only an unreachable helper ends it early). A change restarts the file agent at once and updates
  the `~/Shared` links; a revocation (any group lost, write → read
  included; since the follow-up, read → write is not one) restarts apps
  and terminals after 60 seconds, a grant offers
  the restart (`GET`/`POST /api/session/restart`, events
  `session.restart` and `session.restarted`). Creating a share runs
  `share_group_ensure` and `share_dir_prepare`; deleting one runs
  `share_dir_release`, then `share_group_remove`.
- `~/Shared/<name>` links are kept by the **user's own file agent**
  (internal route `/internal/shared-links`, the shares in the usual
  `x-rast-shares` header). It only adds or removes symlinks whose target is
  a direct child of the shared root; a file, a folder or a foreign link
  with a share's name is a reported conflict, left alone.
- `rast migrate shares` as in section 8.

**Deviations from this ADR** (answered by the maintainer on 2026-11-16):

- At most 8 000 members per group instead of 10 000 (see the table).
  **Accepted.**
- The shares journal is plain JSON lines, not gzip: the helper needs no new
  crate. One line per entry: relative path, type, device, inode, uid, gid,
  mode, access and default ACL (hex of the xattr). The home journal of
  task 1.5 may follow the same format. **Accepted.**
- The recursive `share_dir_prepare` replaces the whole access ACL of every
  entry (and the default ACL of folders) with the layout of section 1, so
  every other named entry goes, not only those of RAST identities.
  **Accepted, on condition** that the dry run shows what is lost: it lists
  every entry whose ACL holds named entries other than `u:rast`, the
  read-only group and RAST identities, with those entries
  (`foreignAclEntries`), and the runbook in `docs/OPERATIONS.md` says so.
  Entries with identity entries are still listed separately.
- Rollback drops set-uid bits (and set-gid on files) instead of refusing a
  journal that records them: legacy shares may hold such files, and a
  rollback must not fail for them. **Accepted, on condition** that only
  set-uid, and set-gid on regular files, are dropped (set-gid on folders is
  restored) and every such entry is reported: `droppedBits` in the reply
  (the first 2 000, with `droppedBitsCount`) and the full list in
  `<stamp>.rolled-back/dropped-bits.txt` (root, 0600); the CLI prints them.
- ~~Pending restarts live in the server's memory; a server restart during
  the warning forgets the restart.~~ **Not accepted.** Pending restarts
  still live in memory, but every pass of the syncer (start-up, every
  change, every five minutes) asks the helper which share groups the
  running processes of every identity carry (`share_session_groups`, the
  `Groups:` line of `/proc/<pid>/status`). A session that holds a group its
  user is no longer in (rw of a share it may now only read, or any group of
  a share it lost; deactivated accounts included) gets the warning and the
  forced restart again, and its agent is restarted. Sessions restarted less
  than 30 seconds ago are skipped while their processes exit. A gid of the
  range without a share group (a deleted share, its container closed)
  counts as nothing; a gid that a new share reused does count.
- The first sync after an upgrade adds every member at once: every account
  with an identity gets its agent restarted and a "Restart session now"
  offer. **Accepted.**
- New, not in the deviations above: a move from the read-only to the
  read-write group is no longer a revocation (the read-only group gives
  nothing the read-write group does not), so it only offers the restart.

**Evaluation (a): group quota on the rw group for `shares.quota_bytes`
under Q-a (D3).** Decided by the maintainer on 2026-11-16: group quota may
be used, and the UI shows it as **"filesystem, best effort"** (task 1.4).
A repair pass (`share_dir_repair`, the recursive prepare of a migrated
container) runs when the server starts, which follows every
`rast migrate shares`, and then daily; it also covers review finding F4.
The evaluation:

- What works: the container and every folder below it are set-gid, so every
  new file and folder, whoever creates it (agent, app, terminal, legacy
  File Station as `rast`), gets the rw gid and counts against the rw
  group's quota. File Station moves between HOME and a share are now copy
  plus delete (fix of 2026-11-16), so they are counted too.
- Gaps, all reachable by any rw member without root:
  - `rename(2)` into a share from HOME on the same filesystem (terminal
    `mv`, apps that write a temporary file in HOME and rename it into
    place) keeps the old gid: not counted against the share.
  - `chgrp` by the owner to any group they belong to: their primary group
    (escape the share limit) or **another share's rw group** (move usage
    onto another share).
  - `cp -p`, `rsync -a`, `tar -p` keep the source gid when the user is in
    that group.
  - The limit follows the gid, not the place: a member can `chgrp` files in
    HOME to the rw group and fill the share's quota from outside (a
    denial of service against the other members); files moved out of the
    share stay counted.
  - Hard links from the share into HOME keep one inode and one gid.
  - Everything stays in the owner's uid quota (D2), so nothing escapes all
    accounting; only the share's own limit is bypassed.
- Repair pass, measured: the walker of `share_dir_prepare` (no-follow,
  one device, descriptor-based) over 100 000 files in 200 folders takes
  **0.85 s when nothing changes and 1.4 s when every entry changes**
  (release build, warm cache, NVMe, this host, 2026-11-16; a cold cache is
  slower). A repair that only resets foreign gids below a container to its
  rw gid would cost about the same: affordable every few minutes for
  shares up to a few hundred thousand entries, and bounded by
  `MAX_TREE_ENTRIES` (1 000 000). It closes the `mv`, `chgrp` and `cp -p`
  gaps inside the share between passes, but not the gid of files outside
  the share (the denial of service above).
- Assessment: group quota limits normal use (apps and File Station saving
  new files), but a member who wants to can bypass it or misuse it. If it
  is used, the UI should say **"filesystem, best effort"**, not
  "filesystem", and the repair pass should run after every agent session
  and on a timer.

**Evaluation (b): a per-user deny ACL entry `u:rast-u<id>:---` on the
container instead of the forced restart (D7).** Decided by the maintainer
on 2026-11-16: the forced restart stays. When access is revoked entirely
(not on a move from write to read), the container gets the entry for the
60-second warning (`share_deny_set`); it goes after the user's session
restarts, or at once when the share is granted again. Every prepare and
repair keeps the entries; the first pass after a server start sets every
share, which removes leftovers. The verification is on the test host
list. The evaluation:

- What works: the kernel checks named-user entries before any group entry,
  so every new path lookup through the container fails with `EACCES` at
  once, although the process still carries the old group.
- Gaps:
  - A process whose working directory is inside the share keeps resolving
    relative paths (common: a terminal after `cd ~/Shared/Team`, an IDE
    with a project there).
  - Open file and directory descriptors stay usable, including `openat`
    below an open directory and inotify watches.
  - **`/proc/<pid>/cwd` and `/proc/<pid>/fd/<n>`**: every process of the
    same uid can follow these links of another of its processes, so one
    process with its cwd or a directory open in the share reopens the whole
    share to all of the user's processes, bypassing the container.
  - Hard links made from share files into HOME before the revocation.
  - **A downgrade (write → read) cannot be expressed** by an entry on the
    container: `u:<id>:r-x` lets lookups through, and the files below still
    grant write to the stale rw gid. Only `---` works, which also removes
    read access until the restart.
  - It needs its own reconciliation: the entries must go after the restart
    or when access is granted again (a lost entry blocks a user who has
    access), and the recursive `share_dir_prepare` must keep them, since it
    replaces the container ACL. One entry per pending restart on the
    container inode (ext4: about 500 entries in a 4 KiB block).
  - Grants still need a restart.
- Assessment: it closes the common path (new opens by path) instantly but
  leaves several practical ways in, and does not cover downgrades. Options
  for the maintainer: keep the forced restart (current implementation), or
  add the entry as an immediate stopgap **during** the 60-second warning,
  together with the restart.
- Not measured: this host has no second uid to test with and no unprivileged
  user namespaces (`kernel.apparmor_restrict_unprivileged_userns = 1`, no
  `newuidmap`). On the test host, as root: create a share with one member,
  have the member `cd` into it and open a file in an app, add the entry,
  then check a new `ls ~/Shared/Team` (expect `EACCES`), the open shell
  (`ls .`, expected to work), `ls /proc/<shell pid>/cwd` from a second
  process (expected to work), and the open file.

### Decisions (taken by the maintainer on 2026-11-16)

| # | Question | Decision |
| --- | --- | --- |
| D1 | How production gets kernel quota | Q-c is not possible without an extra disk (`ubuntu-vg` VFree 0). Production uses the RAST fallback until just before Phase 3 reaches production; then the maintainer enables **Q-a**, journaled user and group quota on `/` (runbook in section 9). Q-c only if a new disk is added later. |
| D2 | Kernel uid quota counts everything the user owns on that filesystem (apps, Flatpak, caches, trash, the user's files in shared folders) | Accepted, as on a NAS. |
| D3 | Shared-folder size limits | Accepted: **migration 0015** adds `shares.quota_bytes`, nullable (`NULL` = no limit). The UI shows whether the limit is enforced by the filesystem or only by RAST. Task 1.3 evaluates group quota on the rw group under Q-a, with its gaps (section 6). **Decided after task 1.3 (2026-11-16):** group quota is shown as "filesystem, best effort"; a repair pass runs at server start (after every `rast migrate shares`) and daily (section 10). |
| D4 | Soft = `quota_bytes`, hard = soft + max(5 %, 1 GiB), 7-day grace; usage = allocated bytes | Accepted. The 20 GiB default stays and is reviewed again in task 3.2. |
| D5 | `rast` keeps `rwx` on shared folders during the transition, no access to any HOME | Accepted. |
| D6 | Administrators are in every share's rw group | Accepted, with the consequence in section 4: administrator apps and terminals open every share, so administrators should use a non-admin account for everyday app use (documented in `docs/NATIVE-LINUX-APPS.md` in task 1.3). |
| D7 | A revocation restarts the session after 60 seconds; a grant applies at the next session start | Accepted for now. Task 1.3 evaluates the per-user deny ACL entry on the container as a restart-free alternative, with its gaps (section 4). **Decided after task 1.3 (2026-11-16):** the forced restart stays; on a full revocation the container also gets `u:rast-u<id>:---` during the warning, removed after the restart (section 10). |
| D8 | On-disk layout of section 1; new accounts get one HOME when the helper is available | Accepted. |
| D9 | `rast migrate home` and `rast migrate shares` need `rast.service` stopped for apply and rollback | Accepted. |
| D10 | Separate app settings opt-in through `linux_apps.launcher` JSON; "Also delete its settings" removes isolated profiles only | Accepted. |
| D11 | Share group gids from a range derived from `linux_uid_base`; no new configuration key | Accepted. |
| D12 | Backup `users` scope through a root reader, keeping owners, modes and ACLs; the restore guide changes | Accepted. |
| D13 | The `start_run` / Docker → native `rast-u<id>` home bug | Fixed separately on 0.18.x (0.18.3, branch `fix/native-home-username`), together with refusing new usernames of the form `rast-u<digits>`. The maintainer removes the stray `homes/rast-u*` folders on production by hand after the fix is installed; RAST does not delete them. |
| D14 | Additive `UsageDto.byLocation` and `UsageDto.quotaBackend`; opaque XDG-based trash ids | Accepted. |

## Consequences

- **Task 1.2:** `rast file-agent`, `api/homefs.rs`, `home_ensure`,
  `files_agent_start`/`files_agent_stop`, realtime events from the agent,
  the hidden-files toggle. No user is switched yet.
- **Task 1.3 (done 2026-11-16, section 10):** share groups, the `share_*`
  operations, membership sync, the session restart warning,
  `rast migrate shares`; the evaluations of group quota for share limits
  under Q-a (D3) and of the deny ACL entry instead of a forced restart (D7);
  the administrator note in `docs/NATIVE-LINUX-APPS.md` (D6).
- **Task 1.4:** XDG Trash, quota backends, the `quota_*` operations, the
  usage breakdown, migration 0015 (`shares.quota_bytes`) with the
  "enforced by" indicator in the UI.
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
  - The fallback does not protect the OS filesystem from apps (D1). On
    production this lasts until the maintainer enables Q-a.
  - Under Q-a, share limits are at best group quota, which `mv`, `chgrp` and
    group-preserving copies can bypass (D3).
  - Administrator accounts reach every share from their apps (D6).
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
  and in the last component; shares only through the server's
  `x-rast-shares` header (never `~/Shared` links), container owner and group
  checks; peer uid and token checks;
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
