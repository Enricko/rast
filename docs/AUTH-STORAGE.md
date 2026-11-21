# Where the login lives: database or files?

The question this answers: *the login page should be saved on the device — is a
normal database better than a file-based store like games and OSes use?*

Short version:

| Data | Where | Why |
| --- | --- | --- |
| Server accounts, password hashes, sessions, audit trail | **SQLite on the server** (`rast-server`, later) | Many rows, concurrent writers, queries, migrations, one-file backup. A real (embedded) relational database is the right "normal DB". |
| This device's unlock password | **One small file**, `0600`, Argon2id hash only (native) / `localStorage` (browser) | Exactly one record, read once at startup, written once. A database would be a dependency and a corruption surface for no benefit. |
| Server session token / refresh token (later) | **OS secret store** — Keychain, Credential Manager, Secret Service | It is a bearer credential; it must not sit in a plain file or in `localStorage`. |
| Cached app data (window layouts, recents, offline lists, later) | **SQLite** (native) / IndexedDB (browser) — only when it exists | When data becomes relational, queried or concurrently mutated, hand-rolled files stop being worth it. |

So: **normal DB on the server, file on the device, OS keychain for secrets.** A
custom "file DB like a game save" is not the right tool for a login record; a
file is, and that is what is implemented today.

## What is implemented now

`crates/rast-ui/src/auth.rs` — the *device login*:

- first launch: username + password (8+ characters) → `Argon2id` PHC string
  (crate defaults: m=19456 KiB, t=2, p=1), stored at
  `<config dir>/rast/device-account`, mode `0600` on Unix, written atomically
  (temp file + rename);
- later launches: the shell opens on the login screen and unlocks only when the
  password verifies; wrong password is rejected with an error, the account is
  not deleted;
- sign out returns to the login screen; the device account stays;
- the plaintext password is never stored, never logged and never leaves the
  process. Password buffers are dropped when the shell moves on;
- in the browser build the same record goes to this browser's `localStorage`
  (one "device" per browser profile, and no file permissions to protect it).
  That is fine for the preview, but a real deployment should move to the
  server's `HttpOnly` session cookie — a browser origin is not a trust boundary.

Default locations (`RAST_ACCOUNT` overrides the file path):

| OS | Device account file |
| --- | --- |
| Linux | `$XDG_CONFIG_HOME/rast/device-account` (or `~/.config/rast/device-account`) |
| macOS | `~/Library/Application Support/rast/device-account` |
| Windows | `%APPDATA%\rast\device-account` |
| Browser | this browser's `localStorage` (`rast.device-account`); a real deployment should use the server's HttpOnly session cookie |

File format — deliberately readable, with nothing secret in it beyond the hash:

```ini
# RAST device account. Argon2id hash only: the password is never stored.
username = admin
hash = $argon2id$v=19$m=19456,t=2,p=1$…$…
```

## Why this split

**Why not SQLite on the device today?** The device login is one row read once.
SQLite would add a native-only dependency (the browser build cannot open files),
a second migration story and a larger attack surface for zero query benefit.
`rusqlite` also does not fit the wasm build, so we would have to build two
storage backends before there is data that needs one.

**Why not "file DB like games"?** Games rewrite one save blob because the entire
state is theirs and single-writer. That breaks down with concurrency, partial
writes and queries — exactly the properties a *server* account table needs, which
is why the server side should be SQLite rather than a directory of files. On the
device the "blob" really is a single immutable record, so a plain file with
atomic replace is the honest version of that pattern, with the OS login database
(`/etc/shadow`) as the precedent: a file holding hashed passwords, not a DB.

**Why OS keychain for tokens?** A hash can be public; a session token is a
password-equivalent. Once the server exists, the client should keep only a
refresh token in the OS secret store (Keychain / Credential Manager / Secret
Service) and send it over HTTPS, so a copied config directory cannot impersonate
the user. The browser build must use an `HttpOnly`, `Secure`, `SameSite` cookie
set by the server; never `localStorage`.

## Security rules this code follows (and the next phase must keep)

1. Local login is **not** server authentication. It unlocks the shell on this
   device; the server will authenticate users independently with its own
   accounts. A local account must never grant server powers.
2. Never store the plaintext, a reversible encoding, or a fast unsalted hash.
   Only `Argon2id` with a random salt (already the case; tests assert the file
   does not contain the password).
3. Keep the device account unreadable to other users (`0600`) and write it
   atomically, so a crash cannot leave a half-written hash.
4. No password in logs, errors, analytics or crash reports.
5. When server auth lands: rate-limit login attempts server-side, keep Argon2id
   server-side, add a "trust this device" toggle that stores only a revocable
   refresh token in the OS store, and let the server revoke sessions.
6. Later, optional: encrypt local cached data with a key held in the OS secret
   store; do not invent a file encryption format.

## When the device file should become a database

Switch the device side to SQLite only when there is real relational/queried data
on the device, for example: multiple device profiles, an offline cache with
indexes, pending uploads queue, or a local audit log. At that point put it in one
SQLite file (`rast-client.db`) with migrations — not a directory of custom files
and not one file per concern.
