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
    shared folder is never entered; the tree walks below a container
    (search, size, copy, the removal after a move) compare every entry's
    `st_dev` with the container's and leave other file systems out (a
    removal that meets one stops with `403` and keeps the folder); HOME
    keeps the user's own mounts;
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
| `share_members_set` | `shareId`, `write`, `read` (lists of user ids) | At most 8 000 ids each (task 1.3: `gpasswd -M` takes the list as one argument, and Linux caps one argument at 128 KiB), unique, disjoint. Users without their `rast-u<id>` account yet are left out and returned as `skipped`. An account `rast-u<id>` with another uid than the identity's is never added and is taken out of both groups (less access is always safe); it is returned in `conflicts` (`userId`, `uid`, `expectedUid`), the other users are still set, and `rast doctor` lists it (review follow-up; it used to fail the whole call, so no member of that share changed). `gpasswd -M` per group (full replacement), only when it changes. Returns, per group, the user ids added and removed. |
| `share_dir_prepare` | `shareId`, `container`, `recursive` | `container` passes `shares::validated_container`; must be a real directory on the storage filesystem. Applies owner, mode, ACL and default ACL from section 1; sets project id = rw gid with the inherit flag when project quota is on. A recursive run also removes named-user ACL entries of RAST identities. |
| `share_dir_remove` | `shareId`, `container` | Removes the container tree (no-follow, one device); only a container owned by `rast` or root. |
| `share_dir_release` | `shareId`, `container` | Task 1.3: the container of a deleted shared folder (whose data RAST keeps) goes back to `rast:rast` 0700 without ACLs before its groups are removed, so a later share that gets the freed gid cannot open it. |
| `quota_status` | none | Device and filesystem type of the homes and shared roots; user and project quota each `off`, `accounting` or `enforced`; grace times. |
| `quota_set` | `userId`, `softBytes`, `hardBytes` | 0 = no limit; hard ≥ soft; both ≤ filesystem size. `quotactl(Q_SETQUOTA, USRQUOTA)` through `libc` (already a dependency). |
| `share_quota_set` | `shareId`, `softBytes`, `hardBytes` | Project quota (project id = rw gid) when available, otherwise group quota on the rw gid if task 1.3 accepts it (D3). Returns the backend that enforces the limit. Task 1.4: group quota only; project quota is deferred until a host supports it (section 11). |
| `quota_report` | `userIds`, `shareIds` | At most 10 000 ids; used bytes and limits from `Q_GETQUOTA`. |
| `home_migrate_scan`, `home_migrate_apply`, `home_migrate_rollback` | `userId`, `username` | Section 8 and 13. The scan is read-only with bounded output (counts, plus the first 50 paths per category). |
| `home_migrate_trash` | `userId`, `username`, `entries` (1 to 256: `id` lowercase UUID, `deletedAt` ≥ 0, `path` relative with valid components, optional `container`) | Task 1.5 (section 13): moves one batch of the legacy recycle bin `<storage>/.trash/<userId>/<id>` into XDG Trash of the HOME or of the container, after the apply. No path to the payload is taken; it is built from the ids. |
| `shares_migrate_scan`, `shares_migrate_apply`, `shares_migrate_rollback` | `shareId`, `container` | Same pattern for shared folders (task 1.3). |
| `share_dir_repair` | `shareId`, `container` | Daily repair pass (D3 decision, 2026-11-16): the recursive `share_dir_prepare` of a container that already has the layout (owned by root); a legacy container is left for `rast migrate shares`. Keeps the container's deny entries. |
| `share_trash_usage` | `shareId`, `container` | Task 1.4 review (2026-11-17): read-only. The share must have its groups and the container must be a real folder of `rast` or root. Sums the apparent size of the regular files below the folders named like recycle bins at the top of the container (`.Trash`, `.Trash-*`), which the service cannot read; no-follow, one device and mount, at most 1 000 000 names (`truncated` when it stops early), 60 s deadline, not serialised with long-running work. For RAST's check of shared folder limits (section 11). |
| `share_deny_set` | `shareId`, `container`, `userIds` (at most 256) | D7 decision (2026-11-16): the full set of `u:rast-u<id>:---` entries on the access ACL of a prepared container, the stopgap while a full revocation waits for the user's session restart. Entries of users not listed are removed; the default ACL is not touched. |
| `share_session_groups` | `userIds` (1 to 200) | Read-only: per user, how many processes run as the identity in one of the user's units and which gids of the share range they carry (`Groups:` of `/proc/<pid>/status`), with the names of those that are share groups. A process counts only when its real and effective uid are the identity's and the first non-slice component of its cgroup (`/proc/<pid>/cgroup`) is `rast-xpra-u<id>`, `rast-files-u<uid>`, `rast-app-u<uid>-…` or `rast-term-u<uid>-…` (`.service`); both files are read through one descriptor of `/proc/<pid>`, which stays bound to that process. Lets the server find sessions that still hold a revoked group after a server restart. Phase 2 (user units under `user@<uid>.service`) must extend the unit rule. |
| `backup_reader_start` | none | Starts the root reader unit `rast-backup-reader` (socket-activated, like `files_agent_start`; task 1.5b, section 14). It only streams tar archives of one HOME owned by an identity or one migrated container, directly below the fixed homes and shared roots (section 7). Returns `{unit, socket, token}`. |

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
- A `.Trash-<uid>` that another member made first (a link, a file, or a
  folder of another owner) is never used, as the specification asks: that
  member's deletions and restores in the share answer `403` with "this
  recycle bin folder belongs to another account; ask an administrator to
  remove it". RAST does not change or remove it (task 1.4 review decision,
  2026-11-17); the daily repair pass logs it and `rast doctor` lists it,
  and an administrator removes it (`docs/OPERATIONS.md`).
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
  `ownersPreserved`. User files are restored with `rast restore-files` as
  root (review F1; it replaced the `tar --numeric-owner --same-owner` guide
  of task 1.5b), **without** `chown -R rast:rast` (which would break one
  HOME), followed by `rast doctor`, which re-applies the share groups.
  Implemented in task 1.5b (section 14).
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
   sudo setquota -t -u 604800 604800 /   # D4: 7-day grace for users
   sudo setquota -t -g 604800 604800 /   # and for groups (shared folders)
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
  and other file systems and mounts are left unchanged and reported. (The
  owner check came with the review of 2026-11-16: with
  `fs.protected_hardlinks = 0` a member can hard-link any file it can
  reach into a share, and once the original name is gone that file has a
  single link.) Mounts are told apart by mount id (`statx` with
  `STATX_MNT_ID`, Linux 5.8, which every supported distribution has), not
  only by `st_dev`, so a bind mount on the same file system is never
  entered, changed or emptied either; a container that is itself a mount
  point is refused, and `share_dir_remove` stops at a mount point inside
  (review follow-up). Operations on one share's container (prepare,
  repair, remove, release, `share_deny_set`, migration apply and rollback)
  hold that share's lock in the helper and fail with `share_busy` after
  10 seconds of waiting (the server retries on its next pass; a deny set
  no longer races the repair pass's rewrite of the container ACL). Apply
  and rollback also hold a `flock` on
  `<data_dir>/migrations/shares/<shareId>/lock`, so a rollback can never
  run beside an apply, even when the CLI that started the apply was
  killed or a second helper process runs.
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
  `Groups:` line of `/proc/<pid>/status`, only for processes in the
  user's own units with real and effective uid of the identity; review
  follow-up). A session that holds a group its
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

### 11. Task 1.4: implementation notes

**XDG Trash** (`storage::xdg_trash`, `homefs::agent::trash`).

- The agent writes what GIO writes: `info/<name>.trashinfo` reserved with
  `O_EXCL` (a taken name gets `.2`, `.3`, …), the payload moved with
  `RENAME_NOREPLACE` (on `EXDEV` copied, then removed), `Path` absolute and
  percent-encoded for the home trash, relative to the container for a
  share's `.Trash-<uid>`, `DeletionDate` in local time, and a
  `directorysizes` line for folders. A fixture written by `gio trash` is
  parsed in the tests.
- **`.trashinfo` is untrusted** (apps and the user write it). `Path` must
  decode to UTF-8 without control characters; it is mapped to a restore
  target only when it names plain segments below HOME (relative paths are
  below `~/.local/share`, as the specification says), below the container
  of a shared folder the server named in the request (write access needed
  for a restore), or `~/Shared/<name>/…` of such a folder. Anything else
  (`/etc/passwd`, `..`, another container, a path into a trash folder, a
  first segment `shared` or `@root`) is restored to the top of the root
  that holds the trash, under the entry's name. Folders on the way are made
  through the descriptor layer (`openat2`, no symlinks), so a link that an
  app put in the way is refused (`403`) and nothing is written where it
  points; a payload that is itself a link moves as a link.
- Ids: `h:<hex name>` and `s:<shareId>:<hex name>`. A restore checks every
  conflict before it moves anything (`409`). The list purges entries older
  than `trashRetentionDays` (default 30) when it is read; entries without a
  payload are not listed, payloads without an info file are removed by
  "Empty".
- **Share trash:** `<container>/.Trash-<uid>` is made by the agent, `0700`
  plus the container's set-gid bit (so its files keep the rw group and stay
  in the share's group quota), without inherited ACL entries. It is hidden
  in listings and searches, never served as a path (`404`), and names
  starting with `.Trash` cannot be created at the top of a shared folder,
  by the agent or in process. The helper's tree walk (prepare, repair,
  migration) leaves a top-level `.Trash-<uid>` owned by that identity
  alone, so the daily repair pass does not open it to the group.
- The legacy recycle bin of an account is moved into the XDG Trash by
  `rast migrate home` (task 1.5, section 8 step 6); task 1.4 does not move
  it.

**Quota.**

- Helper: `quota_status`, `quota_set`, `share_quota_set` and
  `quota_report` use `quotactl_fd(2)` (Linux 5.14+) on the homes and shared
  roots opened with `O_NOFOLLOW` (`Q_XGETQSTATV` for the state per type,
  `Q_GETINFO` for grace times, `Q_GETQUOTA`, and `Q_SETQUOTA` with
  `QIF_BLIMITS` only). They take ids only. `quota_report` takes at most
  10 000 ids, of which at most 1 000 shares (one group lookup each), and
  replies with compact rows `[id, used, soft, hard, graceEnds]`, so 10 000
  rows fit in one 1 MiB reply.
- The server checks the backend at start, every ten minutes and after a
  quota, role or share-limit change. With user quota **on** (accounting or
  enforced), each one-HOME account gets the D4 limits (set only when they
  differ, capped at the filesystem's size) and its used bytes come from
  the kernel; the usage view shows `quotaBackend: "filesystem"` only while
  user quota is **enforced**, and `byLocation.shared` is the kernel's usage
  beyond what HOME holds. Without user quota (production, D1), a HOME scan
  is the usage: on demand in the usage view, and started by listings at
  most every ten minutes. The weekly in-process recalculation skips homes
  the service does not own (it used to reset them to zero).
- Shared-folder limits on the kernel side: group quota on the rw gid
  (`share_quota_set`, the same D4 margin), shown as "filesystem, best
  effort". RAST checks every limit for File Station writes (single and
  chunked uploads, copies and moves from outside the share, restores from
  a recycle bin; the whole batch must fit), against a measurement of the
  container that is kept for a minute: a walk by the service without the
  recycle bin folders at its top, plus `share_trash_usage` for those.
  Uploads reserve their bytes while they run (as the user quota does), so
  two uploads that start together cannot both use the same free space; a
  cancelled, failed or expired upload gives them back.
- A restore through the per-user agent gets the bytes still free in every
  limited shared folder the account may write in the server-only header
  `x-rast-share-free` (`<shareId>=<bytes>` pairs separated by commas,
  strictly parsed, removed from browser requests). The agent refuses the
  restore (`413`, nothing moved) when the entries arriving from another
  recycle bin (the HOME one, where GIO puts files deleted through
  `~/Shared/<name>`) do not fit; entries of the share's own `.Trash-<uid>`
  are counted there already. An agent from before this header ignores it
  until it restarts.

**Deviations and open items.**

- **Project quota is deferred** (maintainer's decision, 2026-11-17) until a
  host supports it; on production that needs a new disk (D1, Q-c).
  `share_dir_prepare` does not set project ids, `share_quota_set` uses
  group quota only, and `quota_status` reports the project state. Project
  ids and project limits come with the first host that has project quota.
- **Grace times are not set by RAST** (accepted, 2026-11-17). They are
  global per filesystem, so changing them would affect accounts outside
  RAST. `rast doctor` reports them and notes when the block grace is not 7
  days; the runbook (section 9) sets them and `docs/OPERATIONS.md`
  explains them.
- The RAST share-limit check ("RAST only", production until Q-a) does not
  see what members' apps write. The three gaps the first version had
  (no reservation for uploads, members' `.Trash-<uid>` not counted,
  restores not checked) are closed (review, 2026-11-17). A
  `.Trash-<uid>` of an account that left the share, or was deleted, stays
  and keeps counting until an administrator removes it.
- Under the fallback a HOME scan that runs while an upload is in progress
  counts its temporary file on top of the reservation until the next scan.
- Not tested here (no kernel quota on this host, no root): `quotactl_fd`
  against real limits. The kernel calls themselves run unprivileged in the
  tests (`Q_XGETQSTATV` answers "off" on a filesystem without quota).
  Pending on the test host: section "Pending on the test host" in
  `docs/remake/PROGRESS.md`.

### 12. Task 1.4: security review (2026-11-17)

An independent review of task 1.4 (commits after `7da51ae`). Fixed, each
with a regression test:

- A restore could target a `.Trash*` name at the top of a shared folder
  (another member's recycle bin, or a name that member's agent then
  refuses); such origins now go to the fallback name.
- An id named twice in one request, in different hex case, was handled
  twice; ids are deduplicated by their canonical form.
- "Empty" read a whole `files/` folder into memory; it now works in
  bounded batches.
- An own share trash with loose bits (an app's `chmod`) or a foreign info
  file stayed as it was; every open closes it again (`0700` + set-gid, no
  ACL), and info files of other owners are not listed.
- The share-limit message from the agent became a generic one; it now
  reaches the browser.
- D4 limits were not whole KiB, so every pass set them again; they are
  rounded up to quota blocks.
- A HOME scan that stopped at its entry limit could lower the usage; a
  partial scan only raises it.
- `quota_set` set limits on uid `linux_uid_base + id` without checking
  that the account exists with that uid; it now checks passwd and refuses
  a missing account (`not_found`) or another uid (`identity_conflict`).
- Squatted `.Trash-<uid>` folders are reported (`foreignTrash` in the tree
  walk, the repair pass log, `rast doctor`), section 5.

Found sound: the quota operations take ids only (`u64` limits refuse
negative values; uids only from the identity range, gids only from the
share range; bounded id lists); `.trashinfo` decoding refuses NUL and
control characters, and relative and absolute paths follow the
specification; a `~/.local/share/Trash` that is a symlink is refused;
large trashes are bounded (20 000 entries listed).

**Open: migration 0015 blocks a rollback to 0.18.x.** The sqlx migrator
of 0.18.x refuses a database with an applied migration it does not know
(`VersionMissing(15)`), so after a 1.0.1 build has opened the database,
`rast update --version 0.18.x` installs a binary that does not start. A
rollback to 0.18.x must restore the database from the backup taken before
the deploy (or a 0.18.x patch release must ignore unknown migrations).
The deploy checklist says so until the maintainer decides.

### 13. Task 1.5: implementation notes (2026-11-17)

Decisions taken by the maintainer for task 1.5, and where the code
deviates from section 8:

- **Selection.** `rast migrate home` takes `--user <name>` or `--all`,
  and `rast migrate shares` now takes `--share <name>` or `--all` in
  every mode; neither runs without a selection (a behaviour change of
  `rast migrate shares`, recorded in the CHANGELOG).
- **Per-file merge (deviation from step 5).** App profiles are merged per
  file, not all-or-nothing: an entry the HOME does not have moves in (a
  folder moves as a whole), a folder both have is merged, and anything
  else (a file the HOME has, a file where a folder is) is a conflict that
  stays in the old profile and is reported as `<app>: <path>`. No isolated
  profiles are created (D10 stays opt-in for later); the rollback moves
  every moved entry back. The legacy identity home is merged first, then
  the apps in name order, so the first source wins a name. `.Xauthority`
  and the profiles' `tmp` are not moved; a source on another file system
  or mount than the HOME is not moved (`otherFileSystem`), nor are entries
  of a source that are not the identity's or are mount points, and a
  profile folder that does not belong to the identity is never merged.
- **Journal.** Plain JSON lines, not gzip: `plan.jsonl` (the folders to
  create and the moves, each with the source's device and inode), then
  `journal.jsonl` (every HOME entry handed to the walk: path, type, owner,
  group, mode, ACL, default ACL, device, inode; the HOME itself first,
  owned by `rast`), then `applied` once the HOME is the identity's. A
  folder without `journal.jsonl` is not a journal. Recycle bin batches get
  `trash-NNNN.jsonl` (the folders they create, then per entry its id, its
  new name and the records of its payload), written before anything
  moves. All of it lives in
  `<data_dir>/migrations/home/<userId>/<stamp>/` (root 0700/0600, `.part`
  then `rename` without replace, synced) and is read back as untrusted
  input: owners only `rast` or the identity (never uid 0 or group 0), no
  mode bits above `07777`, ACLs that decode, relative normalised paths,
  `deny_unknown_fields`. The rollback renames the folder
  `<stamp>.rolled-back`; a new apply starts a new journal.
- **ACL entries.** Every named-user entry of a RAST identity uid is
  removed from the access and default ACLs of the HOME's entries (the
  0.18.3 read grant `u:rast-u<id>:rx` and `d:u:rast-u<id>:rx`, terminal
  grants `rwX`/`rX`, and stray entries of other identities). When no
  named entry is left the ACL is removed and the group bits become the
  owning group's entry again; otherwise the mask is recomputed. Other
  named entries are kept and listed (`foreignAclEntries`). The rollback
  restores the recorded ACLs exactly, the 0.18.3 grant included.
- **Order of apply.** Stop the account's units (session, apps, terminals,
  file agent) and refuse while a unit stays active or any process of the
  identity runs (`/proc`, real or effective uid) → lock
  (`<…>/home/<userId>/lock`, `flock`) → journal → entries of the HOME
  (owner, ACLs) → merge steps (`renameat2(RENAME_NOREPLACE)`; a step whose
  source is no longer the recorded inode but whose target is counts as
  done, so a resumed run continues) → `usermod --home` → the HOME itself
  (owner, `0700`, no ACL) → `applied`. The HOME is given away last, so an
  interrupted run leaves the account in legacy mode. An applied account
  returns `alreadyOneHome` without stopping its session.
- **Recycle bin.** The CLI maps each `trash` row to a batch entry: a home
  origin `/a/b` becomes `a/b` (info file `Path=<HOME>/a/b`), a share origin
  `/shared/<name>/a/b` becomes `a/b` below that share's container
  (`Path=a/b`, `.Trash-<uid>` keeps the container's group and set-gid bit,
  like the agent's). Rows whose share no longer exists, whose payload is
  not `<userId>/<id>` or whose data is missing stay in the legacy recycle
  bin and are listed. Rows are saved to
  `<data_dir>/migrate-home/<userId>/trash-rows-<ms>.jsonl` (rast 0700/0600)
  before the helper moves anything and deleted once it reports `moved` or
  `already`; the rollback puts back the rows of the entries the helper
  moved back and renames the files `.rolled-back`.
- **Shared folders first.** The apply refuses while a shared folder the
  account can open still has a legacy container (owner not root), because
  its file agent and apps could not open it. Administrators can open every
  shared folder (D6), so they come last.
- **Legacy path kept.** The per-launch `setfacl -R` grant, the `RAST` link,
  per-app profiles and `profile_clean_locks` stay for legacy accounts and
  are skipped for one-HOME accounts (detected by the helper: the passwd
  home is the HOME and the HOME belongs to the identity). They are removed
  when no legacy account is left.
- **New accounts (D8)** still start in legacy mode until the migration has
  been verified on production with test accounts.
- **Backup Center (D12)** reading HOMEs through the helper is task 1.5b.
  Until then the `users` scope skips HOMEs that the service does not own
  and lists them in `warnings` (manifest, job result, Backup Center), and
  the restore guide no longer runs `chown -R rast:rast` over every home.
  Done in task 1.5b (section 14).
- **Cleanup** of the emptied legacy folders, `.trash/<userId>`, the journal
  folders and stray `homes/rast-u<id>` stays a separate, confirmed step.

### 14. Task 1.5b: implementation notes (2026-11-17)

Backup Center reads HOMEs in one-HOME mode and migrated shared folders
through a root reader (D12).

- **Helper operation.** `backup_reader_start` takes no field at all, like
  `rootfs_start`. The helper writes a new 64-hex token
  (`<run_dir>/backup-reader/reader.token`, root 0600, folder `root:rast`
  0750, opened without following links) and starts `rast-backup-reader`
  with `systemd-run`. The unit is socket-activated: `reader.sock` is
  `root:rast` 0660 and removed when the unit stops, the token reaches the
  reader as the credential `token` (`LoadCredential=`) and never appears on
  a command line, and the reader accepts only the uid of `rast`
  (`SO_PEERCRED`). A running reader is reused with its token. The reader
  stops after 300 s without a connection; the next backup starts it again.
- **Unit.** The reader runs as root with `CapabilityBoundingSet=
  CAP_DAC_READ_SEARCH` (read any file, write none), `NoNewPrivileges`,
  `ProtectSystem=strict`, `ProtectHome=read-only`, `PrivateTmp`,
  `PrivateDevices`, `PrivateNetwork`, `RestrictAddressFamilies=AF_UNIX`,
  `ProtectKernel*`, `ProtectControlGroups`, `MemoryMax=256M`,
  `TasksMax=16`, `UMask=0077` and no writable path. It is the socket
  activation pattern of `files_agent_start`, not the root file agent's own
  bind, because a read-only unit cannot create its socket.
- **Request.** One JSON line per connection, `deny_unknown_fields`:
  `{token, tree: "home"|"shared", name}`, at most 4 KiB, 10 s to arrive.
  `name` is one username (`validate_home_username`) or one container
  (`validate_share_container`); nothing else is taken, in particular no
  path. The reader opens the folder directly below the fixed root with
  `O_NOFOLLOW` (a link is `forbidden`), checks that it is the same inode as
  the directory entry and on the root's device and mount, and serves it
  only in the new layout: a HOME owned by an identity uid
  (`linux_uid_base + 1 ..= + 1 000 000`), or a container owned by root
  whose group is in the share gid range. A legacy folder is refused
  (`not_one_home`, `legacy_container`): the service reads those itself.
  The helper is pilot-agnostic (ADR 0007); the server decides which trees
  it asks for.
- **Stream.** A reply line (`{ok}` or `{ok:false, error:{code,message}}`),
  then a POSIX tar of the tree (`.` first, then depth first), the two end
  blocks, and one trailer line (counts and at most 20 problems of at most
  300 characters). The tar writer and reader are a small codec in
  `rast-helper` (`native::backup_reader::archive`): ustar headers with
  numeric uid and gid, no user or group names, plus PAX records for long
  or non-UTF-8 paths and link targets, large ids, sizes and times, and
  `SCHILY.acl.access` / `SCHILY.acl.default` in the text form GNU tar
  writes (one entry per line, numeric ids). No new dependency.
- **Walk.** Descriptor based (`openat` with `O_NOFOLLOW`, `fstatat` with
  `AT_SYMLINK_NOFOLLOW`, same-inode checks), at most 256 levels and 3 796
  bytes of path. Symlinks are stored as links and never followed; hard
  links after the first are stored as links (the first 100 000 or 16 MiB
  of names, then as copies); FIFOs, sockets and devices are left out;
  other file systems and mounts are left out and counted. Excluded: in a
  HOME `.cache`, `.local/share/Trash` and `.local/share/flatpak`; in a
  container `.Trash` and `.Trash-<digits>` at the top. A file that shrinks
  while it is read is padded with zeros, as GNU tar does, and reported.
  Memory stays bounded (64 KiB buffer, the capped hard link map).
- **Server.** `backup::create` plans the `users` scope folder by folder: a
  HOME that the service owns is legacy and read in process as before (no
  owners). A HOME of another uid goes to the reader when native execution
  is on, a RAST account has that name and the account is in the native
  pilot; otherwise it is skipped with a warning naming the reason. A
  container owned by root goes to the reader when its shared folder is in
  the pilot, and is read in process otherwise. The server starts the reader
  once per backup and treats its stream as untrusted: it parses it with the
  same strict codec (checksums, magic, entry types `0 5 2 1 x` only, PAX
  keys from a fixed list and at most 64 KiB, relative normalised paths, no
  data on links), requires the folder itself first and the trailer to
  match what arrived, and writes every entry again under
  `users/homes/<name>/` or `users/shared/<container>/` (hard link targets
  too). A stream that breaks becomes a warning (`incomplete`); a file cut
  off in the middle is padded so the archive stays valid and is left out
  of the manifest. A refused or unreachable tree becomes a warning for a
  HOME and an in-process read (with a warning) for a container.
- **Manifest.** Entries written through the reader carry
  `ownersPreserved: true`; the manifest lists their trees in
  `preservedTrees`; `verify` adds both counts to its summary. Additive
  only: older archives read as before.
- **Who can read what (review F12).** The reader serves the `rast`
  service every HOME in one-HOME mode and every migrated container it asks
  for; only the server's plan applies the native pilot. So D5 holds for
  direct access and for writes, not for reads: whoever runs code as `rast`
  can read every one-HOME HOME and migrated shared folder through the
  reader, and administrators can download the archives. This is the price
  of D12. Every tree the reader serves is logged (target
  `rast::backup_reader::audit`, with the kind, the name and the peer uid)
  in the journal of the reader's unit.
- **Owners (review F6).** Below a tree's root the reader streams only
  entries of an allowed owner: in a HOME, the tree's identity and the
  service; in a container, the identity range and the service. Any other
  entry (for example a hard link to a root-only file on a host with
  `fs.protected_hardlinks = 0`) is left out and counted as `otherOwner` in
  the trailer, which the server turns into a warning.
- **Size (review F7).** Kernel quota counts allocated blocks, so a sparse
  file costs its owner nothing. The reader measures each file's data with
  `SEEK_DATA`/`SEEK_HOLE` and leaves out a file once the tree's holes pass
  1 GiB (`sparse` in the trailer). The server gives each tree a budget of
  its quota plus max(10 %, 2 GiB) (`users.quota_bytes`,
  `shares.quota_bytes`; no quota, no budget): the first file that would
  pass it ends the tree with an `incomplete` warning, and nothing beyond
  the budget is written. A `users` backup checks that the backup folder's
  file system keeps 1 GiB free before it starts and every 64 MiB it
  writes, and stops with an error otherwise.
- **Restore (review F1).** `sudo rast restore-files --from <archive>`
  (`--dry-run`, `--tree`, `--target`, `--to-storage`, `--json`). The
  archive, its manifest and its checksums are the service's, so root
  trusts none of it. It reads the archive with this codec
  (`archive::read_backup_entry`: the strict subset, plus the GNU headers
  of the files the service read); only files, folders, symbolic links and
  hard links pass. A tree keeps owners only when its first entry is its
  folder and the layout checks out against the database and the host's
  `/etc/passwd` and `/etc/group`: a HOME owned by its account's identity
  (uid `linux_uid_base + id`, a uid and name nobody else holds), holding
  entries of that identity and the service; or a container owned by root
  and its share's rw group, holding entries of the service and of RAST
  identities (root below the top is refused). `preservedTrees` must list
  exactly those trees. Every other tree comes back as the service's
  (`0700` folders, file modes without set-id bits). Set-id bits never
  land on a file; set-gid stays only on folders of a container; a group
  outside the layout becomes the owner's own; named ACL entries for
  accounts outside the layout are dropped. It writes below a new root-only
  folder (`<storage_dir>/restore-<time>` by default) through descriptors
  (`O_NOFOLLOW` one name at a time, `O_EXCL`, hard links only to files of
  the same tree written before, folder owners and modes last). One refused
  entry refuses the run and removes what was written. `--to-storage`
  needs the service stopped (it takes `rast.lock`, never creating it),
  writes to `<storage_dir>/.restore-<time>` and moves each tree into
  `homes/` or `shared/` with `RENAME_NOREPLACE`. The database is opened
  read-only or immutable, so root leaves no file next to it. Then start
  RAST (the share repair pass) and run `rast doctor`. The identities' uids
  are `linux_uid_base + <id>` on every host; share gids are taken back by
  `share_group_ensure` from the container's group when it is free.
- **Not done here.** The reader is not used for the `apps` scope, and a
  verify of the archive still reads it with `tar` (tar-rs), which cannot
  parse ACL values with newlines in PAX records (GNU tar's own format); it
  only needs the paths and sizes, which come first. Real systemd, the
  capability set, the 0700 HOMEs and the restore are checked on the test
  host (PROGRESS, "Pending on the test host").

### Decisions (taken by the maintainer on 2026-11-16)

| # | Question | Decision |
| --- | --- | --- |
| D1 | How production gets kernel quota | Q-c is not possible without an extra disk (`ubuntu-vg` VFree 0). Production uses the RAST fallback until just before Phase 3 reaches production; then the maintainer enables **Q-a**, journaled user and group quota on `/` (runbook in section 9). Q-c only if a new disk is added later. |
| D2 | Kernel uid quota counts everything the user owns on that filesystem (apps, Flatpak, caches, trash, the user's files in shared folders) | Accepted, as on a NAS. |
| D3 | Shared-folder size limits | Accepted: **migration 0015** adds `shares.quota_bytes`, nullable (`NULL` = no limit). The UI shows whether the limit is enforced by the filesystem or only by RAST. Task 1.3 evaluates group quota on the rw group under Q-a, with its gaps (section 6). **Decided after task 1.3 (2026-11-16):** group quota is shown as "filesystem, best effort"; a repair pass runs at server start (after every `rast migrate shares`) and daily (section 10). |
| D4 | Soft = `quota_bytes`, hard = soft + max(5 %, 1 GiB), 7-day grace; usage = allocated bytes | Accepted. The 20 GiB default stays and is reviewed again in task 3.2. |
| D5 | `rast` keeps `rwx` on shared folders during the transition, no access to any HOME | Accepted. *Note (review F12, 2026-11-17):* no direct access and no writes; reads go through the backup reader (D12), which serves the service every one-HOME HOME and migrated container, and logs each one (section 14). |
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
- **Task 1.4 (done 2026-11-16, section 11):** XDG Trash, quota backends,
  the `quota_*` operations, the usage breakdown, migration 0015
  (`shares.quota_bytes`) with the "enforced by" indicator in the UI.
- **Task 1.5 (done 2026-11-17, section 13):** `rast migrate home`, the
  journal, the per-launch `setfacl -R` and the `RAST` symlink skipped for
  one-HOME accounts, `rast migrate shares` per share, an Upgrade note. The
  backup reader moved to task 1.5b.
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
