# Phase 1 security review (task 1.6)

- Date: 2026-11-17
- Scope: `git diff 0ab47a6..a13e829` (HEAD; `a13e829` only changes tests
  after `832a650`). Most attention went to task 1.5 (`home_migrate_*`, its
  journal, `rast migrate home`), task 1.5b (backup reader) and task 1.5c
  (native pilot, ADR 0007), which had no independent review yet.
- Not repeated: what the task 1.2, 1.3 and 1.4 reviews fixed (see
  `docs/remake/PROGRESS.md`). F3 reopens an assumption behind a task 1.3
  decision (`80078f0`) because it does not hold on the production host.
- Method: code reading of the helper, the agent, the server, the CLI and the
  runbooks; read-only checks of the host configuration (cron). No root, no
  production access, no code change, no throw-away test: every finding is
  shown by its code path and has a regression test sketch.
- Production exposure, which sets the severity: production runs 0.18.3 with
  `native_enabled = false`. After the `1.0.1-alpha.1` deploy the pilot is
  `native_pilot_prefixes = ["t-"]`: the test accounts `t-admin`, `t-alice`,
  `t-bob` and the test shared folder `t-share`. Real accounts stay legacy
  and outside the pilot. Where a rating would rise once real accounts are
  migrated or the pilot ends, the finding says so.

## Summary

| Id | Severity | Component | Title | Status |
| --- | --- | --- | --- | --- |
| F1 | medium | Restore guide (`docs/OPERATIONS.md`, `api/backup.rs`) | Root restore trusts owners, set-id bits, special files and `preservedTrees` from an archive that `rast` writes | fixed (`native/restore_files.rs`, `backup_reader/archive.rs`, `backup/restore_files.rs`, `src/main.rs`, `api/backup.rs`, `docs/OPERATIONS.md`) |
| F2 | medium | Linux apps (`rast-session` `native_apps.rs`) | A pilot administrator's app actions change data of accounts outside the pilot | open |
| F3 | low | Share groups, HOME migration | Processes started by cron run outside RAST units: they keep revoked share groups and can start during a migration | open |
| F4 | low | Helper `home_migrate_trash` | `ensure_dir_in` chowns a folder swapped in after `mkdirat` | open |
| F5 | low | Helper `home_migrate_rollback` | Rollback leaves the identity write access and identity-owned entries in the legacy HOME | open |
| F6 | low | Backup reader (`walk.rs`) | The reader streams the content of files of any owner | fixed (`backup_reader.rs`, `backup_reader/walk.rs`, `backup/reader.rs`, `src/main.rs`) |
| F7 | low | Backup reader, Backup Center | Sparse files make a `users` backup arbitrarily large | fixed (`backup_reader/walk.rs`, `backup/reader.rs`, `backup/mod.rs`) |
| F8 | low | Helper trash move, `rast migrate home` | Legacy recycle-bin rows go to any container: no layout check in the helper, no access check in the CLI | open |
| F9 | low | Native pilot (share rename) | A shared folder renamed out of the pilot keeps its groups; revocations stop | open |
| F10 | low | `rast migrate home/shares --dry-run` | Dry runs walk real accounts and shared folders as root and print their paths | open |
| F11 | low | Pilot runbook (`docs/OPERATIONS.md`) | The runbook does not tie `native_enabled` to `native_pilot_prefixes` | open |
| F12 | info | Backup reader (ADR 0005 D5, D12) | Any `rast` process can read every one-HOME HOME through the reader | fixed (`backup_reader.rs`, `docs/adr/0005-one-home.md`, `CLAUDE.md`) |
| F13 | info | Native pilot, one HOME | Shrinking the pilot or turning native apps off strands migrated accounts | open |
| F14 | info | Helper trash move | Payload checks compare a fresh `lstat`, not the planned one; no link-count check on files | open |
| F15 | info | Helper `home_migrate_rollback` | Rollback matches entries by inode number, which can be reused | open |
| F16 | info | Legacy Docker display proxy | `/linux-apps/<key>/` has no pilot backstop | open |

When to fix, as proposed by the review:

- Before any root restore of user files: F1 (at least the guide).
- Before Linux-app tests by `t-admin` on production: F2, or a rule that test
  administrators never uninstall with data or migrate apps that real
  accounts use.
- Before real accounts join (end of the pilot): F3 to F9. With real
  accounts in one-HOME mode, F3, F5 and F9 become medium.

## Findings

### F1 (medium) Root restore trusts owners, set-id bits, special files and `preservedTrees` from an archive that `rast` writes

**Description.** Task 1.5b changed the restore guide. Root now extracts the
`users` part of an archive with `tar --numeric-owner --same-owner --acls
--xattrs` straight into `storage_dir`. It chowns to `rast` only the folders
that are *not* listed under `preservedTrees` in the archive's
`manifest.json`. Run as root, GNU tar also restores permission bits
(set-uid and set-gid included) and creates device nodes and FIFOs from the
archive. The archive, its manifest and its checksums are written by
`rast` and live in `/var/lib/rast/backups`, which `rast` owns. The guide
checks the archive with `rast restore --dry-run`, which runs as `rast` and
checks only what `rast` wrote. Before Phase 1 the guide ended with `chown
-R rast:rast` over both roots, which clears set-id bits on regular files.
Device nodes already got through then.

**Impact.** Code execution as `rast` becomes root as soon as the
maintainer restores user files with the documented commands. `rast` is
the unprivileged service that the design keeps away from root.

**Exploit scenario.** An attacker who runs code as `rast` rewrites the
newest `users` archive:

- they add `users/homes/t-alice/` with mode 0755;
- they add `users/homes/t-alice/.cache/x` with uid 0 and mode 04755, an
  ELF of their choice;
- they keep `users/homes/t-alice` in `preservedTrees`, where it already is
  for a one-HOME pilot account.

After an incident, the maintainer follows "Restoring user files from an
archive". The dry run passes. Step 3 extracts as root and the folder is
not chowned because it is preserved, so `x` is now a set-uid root program
that `rast` can reach. A character or block device entry works the same
way, for example the system disk with mode 0666, and it also worked with
the pre-Phase-1 guide. `--xattrs` is not needed for archives RAST writes:
the reader writes only `SCHILY.acl.*`, and the server's parser refuses any
other PAX key (`archive.rs:869`). So `--xattrs` only widens what a
tampered archive can set.

**Where.**

- `docs/OPERATIONS.md:211-275`: the guide, with tar at 237-244 and the
  `preservedTrees` rule after it.
- `crates/rast-server/src/api/backup.rs:487-499`: the same commands in
  `GET /api/backups/restore-guide`, with its test at `:660`.
- `crates/rast-server/src/backup/mod.rs:61-69`, `:637`, `:720`, `:805`:
  `ownersPreserved` and `preservedTrees` come from the archive writer.
- `crates/rast-server/src/backup/reader.rs:222-227`: the reader's uid, gid
  and mode go into the archive unchanged, which is correct for a stream
  nobody tampered with.

**Suggested fix.** Never extract a rast-writable archive with preserved
owners directly into `storage_dir`.

Short term, in the docs and the guide route:

1. Extract into a root-only scratch folder.
2. Stop if `find "$scratch" \( -type b -o -type c -o -type p -o -type s \)`
   prints anything.
3. Clear set-id bits:
   `find "$scratch" -type f -perm /6000 -exec chmod ug-s {} +`.
4. Check owners. Below `homes/<name>` only `rast` and that account's uid
   (`linux_uid_base + id`) are allowed. Below a container, only root for
   the container itself, plus identities and `rast`.
5. Decide which folders get `chown -R rast:rast` from the numeric owner of
   each top folder in the archive, not from `preservedTrees`.
6. Drop `--xattrs`, then move the checked tree into place.

Longer term: a root command, for example `rast restore-files --from
<archive> --dry-run`. It reads the `users` part with the strict codec of
`backup_reader::archive`, refuses the cases above and writes through
descriptors. This is a new root path, so it needs the maintainer's
approval.

**Regression test.** For the root command, feed it an archive with:

- a uid 0, mode 04755 file below `users/homes/t-x/`;
- a character device;
- a FIFO;
- a manifest that lists a tree whose top folder belongs to `rast`.

It refuses each of these entries and writes nothing. For the guide, change
the assertion at `api/backup.rs:660`: the user-files step extracts into a
scratch folder and runs the checks above.

**Resolution.** Decision A6: the root command `rast restore-files
--from <archive>` (`--dry-run`, `--tree`, `--target`, `--to-storage`,
`--json`) replaces `tar` as root in `docs/OPERATIONS.md` and in
`GET /api/backups/restore-guide`. It reads the archive with the strict
codec (`archive::read_backup_entry`, which adds the GNU headers the
backup's own writer uses), so devices, FIFOs, sockets, unknown headers and
unsafe paths refuse the run. A tree keeps owners only when its folder
entry comes first and matches the layout, checked against the database
and the host's `/etc/passwd` and `/etc/group`: a HOME owned by its
account's identity with entries of that identity and `rast`, or a
container owned by root and its share's rw group with entries of `rast`
and RAST identities. `preservedTrees` must list exactly those trees. Other
trees come back as `rast:rast`. Set-id bits never land on a file (set-gid
stays on folders of a container), foreign groups and ACL entries are
replaced or dropped and counted. Everything is written through
descriptors below a new root-only folder; one refusal removes what was
written. `--to-storage` takes the service's lock and never replaces a
folder. Tests: `restore_files::tests` (the four cases above, plus links,
hard links across trees, host-account conflicts, the storage mode and the
codec) and two end-to-end tests on real backups in `backup::reader::tests`;
the guide test now asserts `restore-files` and no `tar` or `chown`.

### F2 (medium) A pilot administrator's app actions change data of accounts outside the pilot

**Description.** The pilot gate decides *who* may manage Linux apps, not
*whose data* the action touches. A pilot administrator (`t-admin` on
production) passes `pilot_gate` for every management route:
`DELETE /api/linux-apps/{id}`, `/migrate`, `/migration/rollback`,
`/migration/cleanup` and `/update`. The service layer then loops over
every account:

- Uninstall with "Also delete settings" removes the native profile of every
  account that `profile_users` returns. That list includes every row of
  `linux_identities` and `linux_runs`.
- "Migrate to native" (`migrate_legacy`) works through each user in turn.
  It stops the user's legacy Docker container, exports the volume and runs
  `identity_ensure`. Then it moves the data into native profiles. With the
  pilot on, accounts outside it cannot open those profiles.
- The migration cleanup removes the recorded Docker volumes and native
  profiles of every recorded user.

`native_apps.rs` has no pilot check.

**Impact.** A test account can make real accounts lose data or lose
access to it. This contradicts ADR 0007: "outside the pilot,
administrators included", nothing native happens, and no helper call
names anything outside the pilot.

Real accounts have native profiles only if they used native apps before
the backend was quarantined. They have Docker app volumes if they use
legacy Docker Linux apps. The e2e scenario `l-uninstall` removes only a
fixture app, so the risk is a manual test: for example, uninstalling
Chrome with data, or trying "Migrate to native" on an app that real
accounts use.

**Failure case.** While the pilot is on, `t-admin` uninstalls a native
app with "Also delete settings". `path_remove` then runs on
`<data_dir>/linux-apps/profiles/<app>/<id>` for every real account that
has a profile there.

**Where.**

- `crates/rast-server/src/api/linux_apps.rs:116-157`: `MANAGEMENT_ROUTES`
  and `pilot_gate`; pilot administrators pass.
- `crates/rast-session/src/linux_apps/native_apps.rs`:
  - `:2313-2326`: remove with data
  - `:2346-2372`: `profile_users`
  - `:2873`: `migrate_legacy`
  - `:3082`: `stop_container`
  - `:3176`: `identity_ensure`
  - `:3418-3470`: `cleanup_migration`

**Suggested fix.** While the pilot is active, per-user work covers pilot
accounts only:

- `profile_users` and the migration's volume list keep only accounts whose
  username the pilot allows. Data of the other accounts is kept and
  reported (`keptOutsidePilot`).
- `migrate_legacy` and its cleanup answer 409 while any account outside
  the pilot has a volume or a profile for the app. The message gives the
  number of accounts.
- Removing packages stays app-wide, because it cannot be done per account.
  The dialog says so.

**Regression test.** Set up a fake helper and a database with a pilot
administrator, one pilot account and one account outside the pilot. Each
account has a profile folder.

- Uninstall with `removeData` removes only the pilot profile and reports
  the other one.
- `migrate`, with a volume of the outside account, answers 409 and makes
  no `stop_container` or `identity_ensure` call for that account.

### F3 (low) Processes started by cron run outside RAST units: they keep revoked share groups and can start during a migration

**Description.** Two Phase 1 checks assume one of two things: every
process of an identity lives in a RAST unit, or a check at one moment is
enough.

- `share_session_groups` counts only processes whose first non-slice
  cgroup is one of the user's units (`80078f0`, the task 1.3 review's
  decision). After a revocation, the server restarts those units and lifts
  the deny entry `u:rast-u<id>:---` (`after_restart`). Later passes ask
  `share_session_groups` again.
- `rast migrate home` runs `refuse_running` only once, at the start of
  apply, of the trash move and of rollback.

On this host `cron` is active, and there is neither `/etc/cron.allow` nor
`/etc/cron.deny` (checked read-only). On Ubuntu that means every user,
`rast-u<id>` included, can install a crontab from Terminal. cron starts
each job with the user's supplementary groups at that moment
(`initgroups`), in `cron.service`'s cgroup, outside RAST units. The job's
children keep running after it ends. A deactivated account's cron jobs
also keep running, but that predates Phase 1.

**Impact.**

- A member removed from a shared folder keeps access through a process
  that cron started before the revocation. RAST never sees that process.
- During a migration step, a job can start after the check and change the
  HOME while the helper walks it. F4 and F5 describe what that allows.

**Exploit scenario.** `t-bob` is a read-only member of `t-share`.

1. From Terminal, he adds a crontab entry that keeps one long-running
   process alive.
2. The administrator removes his access. The groups change, his units
   restart and the deny entry is lifted.
3. The process cron started still holds `rast-s<h>-ro`. It reads
   `/srv/rast/shared/<container>` for as long as it runs.

The server's later passes see no unit process with that group.

**Where.**

- `crates/rast-helper/src/native/shares.rs`:
  - `:2193-2201`: `share_session_groups` keeps only unit processes.
  - `:2099-2121`: `parse_proc_cgroup` and `is_user_unit`.
- `crates/rast-server/src/share_groups.rs`:
  - `:977-1000`: `after_restart` lifts the deny entries.
  - `:22-40`: the design comment.
- `crates/rast-helper/src/native/home_migrate.rs:892-931`:
  `identity_processes` and `refuse_running`, called once each at `:2004`,
  `:2592` and `:3066`.

**Suggested fix.**

1. Lift a deny entry only when no process of the identity, in any cgroup,
   carries a group of that share. Read `Uid:` (real, effective or saved)
   and `Groups:` from `/proc/<pid>/status`. Counting processes outside
   units only keeps access denied; it never triggers a restart. So the
   reason for `80078f0`, that a process must not pass for a unit, still
   holds. Report such processes to the administrator.
2. Keep cron and at away from identities. Either `identity_ensure` adds
   `rast-u<id>` to `/etc/cron.deny` and `/etc/at.deny`, or the installer
   adds a `pam_access` rule for cron. Both are new root behaviour on a
   system file and need the maintainer's approval.
3. Run `refuse_running` again before `finish`, before the HOME step of a
   rollback and at the end of a trash batch. If a new process shows up,
   stop and leave the journal for a resume.

**Regression test.**

- Helper: give a fake `/proc` a process of the identity whose cgroup is
  `/system.slice/cron.service` and whose `Groups:` hold the revoked gid.
  The new list of holders outside units names it.
- Server: with that reply, the deny entry stays after the restart.
- Migration: a test hook adds an identity process after the first check.
  Apply stops before `finish`, and a second run resumes.

### F4 (low) `ensure_dir_in` chowns a folder swapped in after `mkdirat`

**Description.** `home_migrate_trash` creates the XDG Trash folders with
`ensure_dir_in`. These are `.local/share/Trash/{files,info}` in the HOME
and `.Trash-<uid>/{files,info}` at the top of a shared folder's container.
`ensure_dir_in` works in four steps:

1. `mkdirat`
2. `open_at(O_NOFOLLOW)` by name
3. because it created the folder, `fchown` to the identity, and in a
   container also removal of the access and default ACLs
4. `fchmod` to 0700, plus set-gid in a container

Nothing checks that the folder it opens is the one it just made. A share
container is 2770 without the sticky bit, so any read-write member can
rename entries at its top. This is the bug class that `1a04a46` fixed in
`home_ensure` (task 1.2 review). `home_migrate.rs` has its own `mkdir_at`
and does not use `homes::open_created_dir`, and `Owners::chown` changes
any owner.

**Impact.** Another member picks a directory: any top-level folder of the
share, with its contents. Root gives it to the account being migrated,
sets mode 2700 and strips its ACLs. The other members lose it, and the
daily repair pass never puts it back, because it skips `.Trash-*` at the
top. In the account's own HOME, a process of the account (cron, see F3)
can swap in a directory it is allowed to rename, such as a top-level
folder of a share it can write, and get it chowned to itself. In neither
case can the attacker read anything they could not read before.

**Exploit scenario.** The maintainer runs `rast migrate home --user
t-alice`, which stops only `t-alice`'s units. Meanwhile `t-bob`, a
read-write member of `t-share`, loops on two renames:
`rename(".Trash-<uidA>", "y")` and `rename("Projects", ".Trash-<uidA>")`.
When the swap lands between `mkdirat` and `open_at`:

- `Projects` becomes `.Trash-<uidA>`;
- it is owned by `t-alice`'s identity, with mode 2700 and no ACL;
- `t-alice`'s recycle-bin payloads are then moved into it.

**Where.**

- `crates/rast-helper/src/native/home_migrate.rs`:
  - `:241-255`: `mkdir_at`
  - `:266-306`: `ensure_dir_in`, with the chown at 288-290, the ACL
    removal at 291-296 and the chmod at 297
  - `:2343-2380`: `ensure_trash`
  - `:147-158`: `Owners::chown`
- Compare `crates/rast-helper/src/native/homes.rs:164-171` (`mkdir_at`
  with the test hook) and `:247-259` (`open_created_dir`).

**Suggested fix.** In `ensure_dir_in`, use `homes::mkdir_at` and
`homes::open_created_dir`, which checks that the new folder is owned by
the effective uid and empty. If the check fails, leave the entry in the
legacy recycle bin with a reason and change nothing.

**Regression test.** Like the `1a04a46` test: the `AFTER_MKDIR` hook
renames a non-empty folder owned by another uid into the place of
`.Trash-<uid>`. The trash batch reports the entry as left, and the
swapped folder keeps its owner, mode and ACLs.

### F5 (low) Rollback leaves the identity write access and identity-owned entries in the legacy HOME

**Description.** `home_migrate_rollback` restores the journalled entries
exactly. Entries made after the migration ("returned") are only chowned to
`rast`: their mode and ACLs stay as the identity set them. The walk visits
only directories and regular files with one link. So symlinks, FIFOs,
sockets and files with several links that the identity made stay owned by
the identity. The report lists them under `tree`, but nothing changes
them. After the rollback the identity again has `u:rast-u<id>:rx` on the
HOME (the 0.18.3 state), so it can reach those entries.

**Impact.** Legacy mode gives the identity read-only access to its File
Station HOME. The reason: the in-process File Station runs as `rast`,
which can read every legacy HOME, and its safe-path layer checks paths by
name (`verify_no_symlinks`, then an open by path).

A rolled-back account that kept a writable directory can create entries
there from Terminal or an app, and race those checks toward another
account's legacy HOME. On production only test accounts are migrated and
rolled back. But the files such a race can reach are the real accounts'
legacy HOMEs.

The race is the one task 1.2 documented for the legacy layer. This
finding is about the write access that makes it reachable.

**Failure case.** While in one-HOME mode, `t-alice` runs:

- `mkdir ~/drop; chmod 0777 ~/drop`, or instead
  `setfacl -m u:rast-u<id>:rwx,d:u:rast-u<id>:rwx ~/drop`;
- `ln -s /srv/rast/homes ~/drop/l`.

Then `rast migrate home --rollback --user t-alice` runs. Afterwards `drop`
belongs to `rast` but keeps mode 0777 and the named write entry, and `l`
still belongs to the identity. Her Terminal can create, rename and swap
entries in `drop` while File Station serves it.

**Where.**

- `crates/rast-helper/src/native/home_migrate.rs`:
  - `:3092-3116`: `rollback_tree`; unmatched entries of the identity are
    chowned to `rast` and nothing else changes.
  - `:414-560`: `walk_tree`, which decides what is visited.
  - `:2896-2912`: the report.
- `crates/rast-base/src/storage/safe_path.rs:374-408`.

**Suggested fix.**

- For every entry the rollback does not restore from the journal: remove
  the named ACL entries and the default ACL, and set the modes the legacy
  File Station would have created (0644 or 0755, no group or other write).
- Entries the walk does not visit that belong to the identity (links,
  FIFOs, sockets, files with several links) stop the rollback before the
  HOME step, with a list. The maintainer moves them out; RAST never deletes
  without asking. After that, `--rollback` continues.
- Run `refuse_running` again before the HOME step (see F3).

**Regression test.** Use the owner overlay of the `home_migrate` tests.
After apply, add a 0777 directory with a named `rwx` entry for the
identity, and a symlink owned by the identity.

- Rollback reports the symlink and stops before the HOME step.
- After the symlink is removed, rollback leaves the directory owned by
  `rast`, mode 0755, without named entries.

### F6 (low) The backup reader streams the content of files of any owner

**Description.** The reader checks the owner of the tree's root only.
`open_tree` requires a HOME owned by an identity, or a container owned by
root with a share group. Below the root, `Walk::file` uses
`CAP_DAC_READ_SEARCH` to stream every regular file on the same mount,
whatever its owner. With `fs.protected_hardlinks = 0`, a user can
hard-link any file on the same file system into the HOME or a shared
folder. The task 1.3 review fixed the same class in the share walk
(`898421f`: only entries of `rast` and identities are changed). The
reader came after that fix.

**Impact.** Root-only files can end up in an archive that `rast` owns and
that administrators can download (`GET /api/backups/{id}/download`). This
covers `/etc/shadow` or the SSH host keys if `/srv/rast` is on their file
system. Production has `fs.protected_hardlinks = 1`, and `rast doctor`
warns otherwise, so this needs a host that changed the default.

**Exploit scenario.** On a host with `protected_hardlinks = 0`, `t-alice`
runs `ln /etc/shadow ~/s`. The next `users` backup contains that file as
`users/homes/t-alice/s`.

**Where.** `crates/rast-helper/src/native/backup_reader/walk.rs:357-452`
(`file`) and `crates/rast-helper/src/native/backup_reader.rs:413-505`
(`open_tree`).

**Suggested fix.** Stream content only for files with an allowed owner:

- in a HOME, the tree's identity or `rast`;
- in a container, any identity or `rast`.

Any other file gets no entry. It gets an `otherOwner` problem and is
counted in the trailer, as in the share walk.

**Regression test.** Run a real walk in a tempdir with the allowed owners
as a parameter, leaving out the test uid for one file. That file is absent
from the stream and listed as `otherOwner`. With the uid allowed, it is
present.

**Resolution.** `OwnerRules` now carries the service uid (the reader's
peer, `--peer-uid`) and `entry_owners` gives the owners a tree may hold:
in a HOME its identity and the service, in a container the identity range
and the service. The walk checks every folder, file and symlink (files
and folders again on the opened descriptor) and leaves any other entry
out as `otherOwner`, counted in the trailer; the server turns the count
into a warning. Test: `entries_of_other_owners_are_left_out`.

### F7 (low) Sparse files make a `users` backup arbitrarily large

**Description.**

- The reader writes each file at its apparent size (`st_size`) and pads
  with zeros when a read comes up short.
- The server copies the declared size and pads a broken stream up to it.
- Nothing compares a file's size with the blocks it uses.
- Backup Center has no byte budget and no free-space floor for the `users`
  scope.

Kernel quota counts allocated blocks, so a sparse file costs the user
nothing. Legacy HOMEs cannot get holes, because uploads write chunks in
order. So this is new with one-HOME HOMEs and migrated shared folders.

**Impact.** A pilot account can make every `users` backup run for hours.
It can also fill the backup folder's file system: `/var/lib/rast/backups`
is on the root file system on a default install. deflate compresses zeros
about 1:1000, so a 16 TiB file (the ext4 limit with 4 KiB blocks) gives
about 16 GiB of archive. On XFS the limit is 8 EiB.

**Exploit scenario.** `t-alice` runs `truncate -s 16T ~/big` in Terminal.
The next `users` backup, scheduled or manual, streams 16 TiB of zeros
through the reader and gzip.

**Where.**

- `crates/rast-helper/src/native/backup_reader/walk.rs:396-436`
- `crates/rast-server/src/backup/reader.rs:147-180` (`copy_data`)
- `crates/rast-server/src/backup/mod.rs`: no budget for the `users` scope
- Compare `crates/rast-base/src/storage/chunked.rs:9-10` and `:358-361`.

**Suggested fix.**

- In the reader: leave out a file whose apparent size exceeds its
  allocation (`st_blocks * 512`) by more than a margin, with a `sparse`
  problem. Or write PAX sparse entries using `SEEK_DATA` and `SEEK_HOLE`.
- In the server: give each tree a byte budget, for example the account's
  or share's limit plus slack. Check free space in `backup_dir` before and
  during the backup, and stop padding a broken stream at the budget.

**Regression test.**

- Reader: a tempdir file with `set_len(1 << 40)` and no data is left out
  with a `sparse` problem, and the stream stays small.
- Server: a fake reader announces a 1 TiB file and closes. The tree is
  marked incomplete, and nothing beyond the budget is written.

**Resolution.** The reader measures each file's data with
`SEEK_DATA`/`SEEK_HOLE` and leaves a file out (`sparse` in the trailer)
once the tree's holes pass 1 GiB (`MAX_TREE_HOLES`). The server gives each
tree a budget of its quota plus max(10 %, 2 GiB) (`tree_budget`, from
`users.quota_bytes` and `shares.quota_bytes`): a file header that would
pass it ends the tree with an `incomplete` warning before any of its data
is written. A `users` backup checks that the backup folder keeps 1 GiB
free (`SpaceGuard`, `MIN_FREE_BYTES`) before it starts and every 64 MiB
written, and stops with an error. Tests:
`sparse_files_beyond_the_hole_budget_are_left_out`,
`data_bytes_counts_only_what_a_file_holds`,
`a_file_beyond_the_budget_is_never_written`,
`the_budget_comes_from_the_quota_and_ends_the_tree_with_a_warning`,
`a_users_backup_stops_before_the_disk_is_full`.

### F8 (low) Legacy recycle-bin rows go to any container: no layout check in the helper, no access check in the CLI

**Description.** `rast migrate home` maps each legacy recycle-bin row of
a shared folder to the container of the share that has that name *now*,
compared case-insensitively. It does not check that the account can still
open that share. The helper's `trash_base` opens
`<storage>/shared/<container>` with `O_NOFOLLOW` only. Unlike `open_tree`
and the share walks, it does not check for the migrated layout (root
owner, share group) or for a mount point. `check_shares` makes sure that
the shares the account can open are migrated. A row from a share the
account has lost, or from one renamed since, slips past that check.

**Impact.** The payload lands in a `.Trash-<uid>` (0700) of a shared
folder the account cannot open:

- the user loses it;
- `share_trash_usage` counts it toward that share's limit;
- in a legacy container, `.Trash-<uid>` and its payloads get group root,
  because the parent has no set-gid.

No one else can read the payload.

**Failure case.** `t-alice` deleted `/Shared/t-old/report.odt`, so it is
in the legacy recycle bin. Later she lost access to `t-old`.
`rast migrate home --user t-alice` still moves the payload into `t-old`'s
container.

**Where.**

- `crates/rast-server/src/migrate_home.rs`:
  - `:606-644`: `map_row`, with the share lookup at 619-623
  - `:582-601`: `pilot_row`
  - `:743-811`: `move_trash`
  - `:193`: the list of every share
- `crates/rast-helper/src/native/home_migrate.rs`:
  - `:2321-2339`: `trash_base`
  - `:283-290`: the group of a new folder

**Suggested fix.**

- CLI: map a row only to a share the account can open now (taken from
  `accessible`). Otherwise leave the row in the legacy recycle bin, with
  the reason.
- Helper: open the container as `open_tree` does (root owner, share gid,
  not a mount point), and refuse it otherwise.

**Regression test.**

- CLI: the account has lost access to the row's share. The row stays,
  with the reason, and no helper call names that container.
- Helper: a container owned by the service gets `forbidden`, and nothing
  is created.

### F9 (low) A shared folder renamed out of the pilot keeps its groups; revocations stop

**Description.** A share's container never changes. A share stays in the
pilot only while both its name and its container match a prefix, so
renaming a pilot folder takes it out (for example `t-share` → `team`).
ADR 0007 chose to do nothing after that: the groups keep their members.
On a later revocation no member is removed, no deny entry is set and no
session is restarted. Renames are allowed while the pilot is on.

**Impact.** After such a rename, revoking a member's access changes only
what the server allows: File Station refuses. The member's apps and
Terminal keep the Linux group and can still read, or write, the
container. That is more access than the setting gives, against the
"least access" rule the ADR itself names.

**Failure case.** `t-admin` renames `t-share` to `team`, then removes
`t-bob`. `t-bob` stays in `rast-s<h>-ro`, and his Terminal can still list
the container.

**Where.**

- `docs/adr/0007-native-pilot.md`: "Rename across the boundary"
- `crates/rast-server/src/share_groups.rs`:
  - `:154`: the plan
  - `:406`: the repair pass
  - `:1104` and `:1134`: the create and delete hooks
- The rename route has no pilot check.

**Suggested fix.** While the pilot is active, refuse a rename that moves
a share across the boundary, with 409 and the reason. If renames must
stay possible, then when a share leaves the pilot, empty both of its
groups and set deny entries for its members (least access).

**Regression test.** With the pilot on, through the API:

- renaming `t-share` to `team` answers 409 and keeps the name;
- renaming it to `t-team` still works.

### F10 (low) Dry runs walk real accounts and shared folders as root and print their paths

**Description.** ADR 0007 says `--dry-run` reports everything.

- `rast migrate home --all --dry-run`, or `--user` with a real account,
  makes the helper walk the real account's legacy HOME as root. It prints
  sample paths: owners to change, links, special files, other owners and
  so on.
- `rast migrate shares --all --dry-run` does the same for real shared
  folders.

**Impact.** The command changes nothing. But it shows file names of real
accounts to whoever runs it, during the autonomous session the
coordinator. CLAUDE.md says to never read the files of a real account.

**Where.**

- `crates/rast-server/src/migrate_home.rs`:
  - `:163-228`: `run_with`; accounts outside the pilot are scanned at
    207-210.
  - `:315-398`: `dry_run`
- `crates/rast-helper/src/native/home_migrate.rs:318-376`: the path
  samples in the report.
- `crates/rast-server/src/migrate_shares.rs:112-200`: `run_in_pilot`

**Suggested fix.** Outside the pilot, a dry run prints only the counts
and the verdict, with no sample paths. Another option: dry runs skip
accounts and shares outside the pilot unless `--include-outside-pilot` is
given. Either way, update ADR 0007 and the runbook.

**Regression test.** A CLI test with an account outside the pilot and a
fake helper that returns samples. The output has the counts and none of
the sample paths.

### F11 (low) The pilot runbook does not tie `native_enabled` to `native_pilot_prefixes`

**Description.** Production runs with `native_enabled = false`. The
runbook says to update first and add the key afterwards. It does not say
that `native_enabled = true` must never be in effect without the key.

The maintainer might enable native apps in the same restart as the
update, or before adding the key. The next start then gives the native
features to every account:

- groups and members for every shared folder, the real one included;
- kernel limits;
- Linux apps for everyone;
- `--all` covering every account and every shared folder.

**Where.**

- `docs/OPERATIONS.md:1011-1022`
- `docs/adr/0007-native-pilot.md`: "Upgrade order"
- `docs/remake/PROGRESS.md`: decision A5

**Suggested fix.** In the runbook and the deploy checklist:

1. Update with `native_enabled` still false.
2. Add `native_pilot_prefixes = ["t-"]`, restart, and check that
   `rast doctor` shows the pilot.
3. Only then set `native_enabled = true` and restart.

Also, `rast doctor` warns when native apps are on and no pilot is set:
every account then has the native features.

**Regression test.** A `rast doctor` test with native apps on and no
pilot: the output has a warning line that names the key.

### F12 (info) Any `rast` process can read every one-HOME HOME through the reader

**Description.** The service can call `backup_reader_start`. The reader
serves any HOME owned by an identity and any migrated container; only the
server's plan applies the pilot. So ADR 0005 D5 ("the service has no
access to a one-HOME HOME") holds for direct file access and writes, but
not for reads. Administrators can also download the archives. This is the
design of D12, but neither ADR 0005 section 14 nor the security invariants
state it plainly.

**Where.**

- `crates/rast-helper/src/native/backup_reader.rs:686-739` and `:413-505`
- `crates/rast-server/src/backup/mod.rs:219-325`

**Suggested fix.** State it in ADR 0005 (D5 and section 14) and in the
security invariants of `CLAUDE.md`. Log every tree the reader serves in
the helper's journal, so reads can be audited.

**Regression test.** If logging is added: a reader test where one served
tree gives one log record with the tree's kind and name.

**Resolution.** ADR 0005 states it in D5 (a note on the decision) and in
section 14 ("Who can read what"), and the security invariants in
`CLAUDE.md` name the reader as the service's only access to a one-HOME
HOME. The reader logs every tree it serves (`tracing` target
`rast::backup_reader::audit`, with kind, name and peer uid) in its unit's
journal. Test: `the_reader_answers_one_tree_per_connection` checks one
audit record for one served tree.

### F13 (info) Shrinking the pilot or turning native apps off strands migrated accounts

**Description.** `is_one_home` is false outside the pilot and when native
apps are off. A migrated account then has these problems:

- It falls back to the legacy in-process File Station. That runs as `rast`
  and gets `EACCES` on a HOME that belongs to the identity.
- Backups skip the HOME with a warning.
- The RAST quota applies again.

The data stays intact. The runbook's step 6 presents
`native_pilot_prefixes = []` as turning the features off "without
removing them".

**Where.**

- `crates/rast-agent/src/homefs/client.rs:190-193`
- `crates/rast-server/src/backup/mod.rs:256-262`
- `docs/OPERATIONS.md:1043-1046`

**Suggested fix.** The runbook says to roll back migrated accounts before
removing them from the pilot or turning native apps off. `rast doctor`
lists each one-HOME HOME whose account is outside the pilot, or whose
server has native apps off.

**Regression test.** A `rast doctor` test with a HOME owned by an identity
and the pilot set to `[]`: the output has a warning that names the account.

### F14 (info) Payload checks compare a fresh `lstat`, not the planned one; no link-count check on files

**Description.** `payload_records` checks the legacy payloads (link count
and owner) when the batch is planned. At move time, `convert_payload`
compares the payload only with a fresh `lstat`. It does not check a
file's link count again.

The legacy recycle bin (`<storage>/.trash`) belongs to `rast`, so only
`rast` can swap a payload in between. It could, for example, swap in a
hard link to its own database, which root would then give to the
identity. `rast` already owns those files, so this is not an escalation.
It is a gap in the rule that the helper does not trust the service.

**Where.**

- `crates/rast-helper/src/native/home_migrate.rs:2464-2508` and
  `:2512-2562`
- `crates/rast-base/src/config.rs:977-978`

**Suggested fix.** Before converting, compare device, inode and type with
the planned record, and require one link for a file. Otherwise report
`changed`.

**Regression test.** A hook between planning and the move replaces the
payload with a hard link to another file. The status is `changed`, and
nothing is chowned.

### F15 (info) Rollback matches entries by inode number, which can be reused

**Description.** `rollback_tree` matches a journal record to an entry by
path, device, inode and type. Say a recorded file is deleted after the
migration, and a new file at the same path gets the same inode number.
The old record (owner, ACL, mode) is then applied to the new file. Restored
owners are only ever `rast` or the identity, and set-id bits on files are
never restored (`restored_mode`). So at worst, new content in the user's
own HOME gets an old ACL.

**Where.** `crates/rast-helper/src/native/home_migrate.rs:3092-3100`.

**Suggested fix.** Record the inode generation (`FS_IOC_GETVERSION`) or
the change time in the journal. Treat a mismatch as `changedSince`.

**Regression test.** An overlay test with the same device and inode but a
different generation: the entry is reported as `changedSince` and not
restored.

### F16 (info) The legacy Docker display proxy has no pilot backstop

**Description.** ADR 0007 gates Linux apps of every backend, and adds
backstops in the service layer, the Xpra display proxy included. The
legacy Docker display proxy at `/linux-apps/<key>/` sits outside the
`/api/linux-apps*` route layer, and finds the session by its key only.

It is not reachable today:

- keys come only from the gated `open` route;
- legacy sessions live in memory and are stopped at start.

So after the restart that sets the key, no account outside the pilot
holds one.

**Where.**

- `crates/rast-server/src/api/mod.rs:100-110`
- `crates/rast-session/src/linux_apps/proxy.rs:102-155`
- `crates/rast-session/src/linux_apps/mod.rs:500-502` and `:2241-2243`

**Suggested fix.** In `handle`, answer 404 for a session whose user the
pilot does not allow, as `native_proxy::in_pilot` does.

**Regression test.** A legacy session of an account outside the pilot:
`/linux-apps/<key>/` answers 404.

## Checked and found sound

### Migration helper (task 1.5)

- **Journal folders.** They are per user id below the data folder. Every
  component is owned by root with mode 0700, checked on the open
  descriptor (`shares.rs:2581-2632`).
- **Journal files.** They are written with `.part`, `O_EXCL`,
  `RENAME_NOREPLACE` and fsync (`shares.rs:2718-2776`).
- **Journal records.** They are read back as untrusted (`validate_record`,
  `home_migrate.rs:767-787`):
  - the owner must be `rast` or the identity;
  - the gid must not be 0;
  - the mode must be at most 07777;
  - ACLs must decode;
  - files must have no default ACL.
- **Restoring from the journal.**
  - Set-id bits on files are never restored (`restored_mode`).
  - The first record must be the HOME, owned by `rast` (`read_journal`,
    `:1799-1834`).
  - A HOME that is not the recorded inode is refused (`finish_rollback`).
- **User id and name.**
  - The passwd home must be the identity home or the HOME
    (`Account::passwd`, `:821-841`).
  - There is one journal per user id.
  - A HOME owned by another identity is never taken: only entries of
    `rast` or the identity change.
- **Apply order and crash resume.**
  - The account's units are stopped first.
  - `refuse_running` runs under the lock.
  - The plan and the journal are written before any change.
  - The HOME comes last, then the `applied` marker.
  - The lock is held until `finish`.
  - Apply and rollback need the server stopped (D9, `src/main.rs`).
- **Walks.**
  - `O_NOFOLLOW` everywhere, with same-inode, device and mount id checks.
  - Files with several links are skipped.
  - Only `rast` and the identity are changed.
  - Set-id bits are stripped on conversion (masks 0o3707 and 0o1707).
- **Trash.**
  - Entries are validated (`validate_trash_entry`, `:2254-2280`).
  - Batches are bounded (256).
  - `.trashinfo` is written with `O_EXCL`.
  - The share trash is 0700 plus set-gid, without ACL.
  - Undo checks inodes (`undo_trash`, `restore_payload`).
- **Names.** Usernames are validated (`validate_username`: `.`, `..` and
  `rast-u<digits>` are refused) and never change; containers never change
  either.

### One-HOME sessions and terminals (`85a21b7`)

- `one_home_of` uses a no-follow stat, and the owner must be the identity.
- `ensure_one_home` opens the HOME with `O_NOFOLLOW` and repairs its mode
  through the descriptor.
- A terminal's working directory is checked lexically, and systemd enters
  it as the identity.
- One-HOME accounts get no ACL grant.

### Native pilot (task 1.5c)

- **Route layer** (`pilot_gate`): the list is stripped, management routes
  answer 403, every other route answers 404, and so do new routes.
- **Backstops:** in `LinuxApps`, `NativeSessions::launch`, `ensure_session`
  and the Xpra proxy (`native_proxy.rs:146`, `:330`).
- **One HOME:** `is_one_home` checks the pilot.
- **Share groups** (`share_groups.rs`): the plan (`:154`), `retain_users`
  (`:179`, `:201`), the repair pass (`:406`), and the create and delete
  hooks (`:1104`, `:1134`).
- **Quota:** the quota sync filters by the pilot.
- **CLI:**
  - a name outside the pilot is refused by the typed name before the lock,
    and again by the stored name and container;
  - `--all` is limited to the pilot.
- **Configuration:**
  - prefixes are compared with the lower-cased username;
  - an empty environment variable counts as unset (a config test covers
    it).
- **Terminal:** unchanged.

### Backup reader (task 1.5b)

- **Request:** strict, with a constant-time token check (`check_request`,
  `backup_reader.rs:396-408`).
- **Tree owners:** checked by `open_tree`.
- **Walk:**
  - no link is followed;
  - other mounts and devices are left out;
  - special files are skipped;
  - files are opened `O_NONBLOCK`, with a regular-file check after the
    open;
  - exclusions apply;
  - depth and the link map are limited.
- **Unit hardening** (`reader_args`): only `CAP_DAC_READ_SEARCH`, with
  memory and task limits.
- **Token and socket:** they live below `/run/rast-linux`, which is owned
  by root with mode 0755 (`native/mod.rs:671-676`). So `rast` cannot
  redirect the path-based calls in `prepare_dir`.

### Server side of the reader

- **Parser:** a strict ustar and PAX subset (`archive.rs:531-637`):
  - only entry types 0, 1, 2 and 5;
  - mode at most 07777;
  - relative, normalised paths;
  - unknown PAX keys and oversized headers are refused;
  - ACLs only where valid.
- **Stream checks:**
  - the tree root comes first, and only once;
  - path lengths are limited;
  - the trailer is bounded and its counts must match.
- **Output:**
  - the server writes every header again itself;
  - messages are bounded and stripped of control characters
    (`reader.rs:111-122`);
  - the reply, the socket path and the token are validated
    (`parse_access`).
- **Plan** (`backup/mod.rs:219-325`):
  - legacy trees are read in process;
  - one-HOME HOMEs are read only for existing pilot accounts;
  - migrated containers are read only for pilot shares.

### Agent output

It stays untrusted, as the task 1.2 to 1.4 reviews made it. The Phase 1
commits after the 1.4 review did not change that contract.
