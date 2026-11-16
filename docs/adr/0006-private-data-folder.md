# 0006. A private folder for the database and the master key

- Status: accepted (maintainer decision of 2026-11-16, task 1.3 security
  review)
- Date: 2026-11-16

## Context

The data directory (`/var/lib/rast`) is `0751` whenever native Linux apps or
the Package Center shell PATH are on: the app identities need to traverse it
to reach `linux-apps/profiles` and `linux-users`, and package users need it
for the runtimes. "Others execute" lets any local account *name* every file
directly in it, although it cannot list or read them.

`rast.db` (with `rast.db-wal` and `rast.db-shm`) and `secret.key` were
directly in that directory. With `fs.protected_hardlinks = 0` (the kernel
default; systemd sets `1` on most distributions) an account that can name a
file can hard-link it into a folder it owns, for example a shared folder.
The link alone gives nothing (the file keeps its owner and mode), but the
shared folder repair pass, run as root, gives the share's group to files of
the `rast` service once the file has a single link again (after the service
replaced the original name, as a restore does). The review asked to check
whether the traverse bit is still needed and, if so, to keep the sensitive
files in a `0700` subfolder.

The traverse bit is still needed (the 0.18.x layout of app profiles and
identity homes, and the package runtimes), so `0750` is not an option.

## Decision

- The database and the master key live in `<data_dir>/private` (`0700`,
  owned by the service). `backups/` is `0700` as well, and `tmp/` loses any
  access for others. `Config::db_path()` and `secret_path()` point into
  `private/`; only while an older release's file still sits at its old
  place (not moved yet) do they point there.
- `rast serve` moves the files of an older release in after it holds the
  instance lock and before anything opens them
  (`db::move_into_private`): each file is hard-linked into `private/`, then
  its old name is replaced by a symlink in one rename, so every file is
  reachable under both names at every moment and an interrupted move
  resumes. WAL files go first and keep no link (SQLite resolves the
  database link and opens them next to the real file).
- The old names stay as links (`rast.db -> private/rast.db`,
  `secret.key -> private/secret.key`), created by every command when free,
  so an older release after a downgrade and existing scripts keep working.
  Following them needs `private/`.
- Two different files at both places (an older release restored a backup
  over the link) stop the server, and `rast doctor` reports a failure:
  which one is right is the administrator's call.
- `rast doctor` reports the folder, files that wait for the move, and warns
  when `fs.protected_hardlinks` is not `1`.
- Independently, the helper walk never changes a file with more than one
  link (ADR 0005, section 10), and only files of `rast` and the RAST
  identities.

## Consequences

- On-disk layout change (approved by the maintainer). The upgrade needs no
  action; a downgrade works through the links. A restore by an older
  release after a downgrade leaves a conflict that must be resolved by hand
  (documented in `docs/OPERATIONS.md`, "The private data folder").
- A CLI run as root (`sudo rast update`) gives the folders and links it
  creates to the owner of the data directory, like the database files.
- Manual SQLite commands in the runbook use `private/rast.db`; the old path
  still works through the link.
- Still open: files the service keeps elsewhere in the data directory that
  are readable only by it (`apps/<slug>/.env`, encrypted) sit in `0750`
  folders, which others cannot traverse.
