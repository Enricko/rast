# Where the login lives: database or files?

The question this answers: *the login page should be saved on the device — is a
normal database better than a file-based store like games and OSes use?*

Short version:

| Data | Where | Why |
| --- | --- | --- |
| Server accounts, password hashes, sessions, audit trail | **SQLite on the server** (`rast-server`, implemented) | Many rows, concurrent writers, queries, migrations, one-file backup. A real (embedded) relational database is the right "normal DB". |
| Browser session | **`HttpOnly; Secure; SameSite=Strict` cookie** set by `rast-server` | JavaScript never sees it, so an injected script cannot copy it. |
| This device's unlock password (native client) | **One small file**, `0600`, Argon2id hash only | Exactly one record, read once at startup, written once. A database would be a dependency and a corruption surface for no benefit. |
| Server session token / refresh token (later) | **OS secret store** — Keychain, Credential Manager, Secret Service | It is a bearer credential; it must not sit in a plain file or in `localStorage`. |
| Cached app data (window layouts, recents, offline lists, later) | **SQLite** (native) / IndexedDB (browser) — only when it exists | When data becomes relational, queried or concurrently mutated, hand-rolled files stop being worth it. |

So: **normal DB on the server, file on the device, OS keychain for secrets.** A
custom "file DB like a game save" is not the right tool for a login record; a
file is, and that is what is implemented today.

## Server accounts (implemented)

`crates/rast-server` serves `/api` behind nginx and keeps `rast.db` (SQLite,
schema versioned with `PRAGMA user_version`) in its data directory, which it
makes owner-only:

- **Roles.** *Super Admin* manages everyone; *Admin* creates and manages
  *Users* only; *User* uses the desktop. Nobody changes their own role or status
  or deletes themselves, and the last active Super Admin cannot be demoted,
  disabled or deleted. The server enforces this; the desktop only hides the
  controls a role cannot use.
- **Passwords.** Argon2id PHC strings (8–1024 bytes); unknown usernames are
  checked against a dummy hash so timing does not reveal which accounts exist,
  and the error never says which part was wrong.
- **Sessions.** 256 random bits in the `rast_session` cookie (`HttpOnly; Secure;
  SameSite=Strict`, 7 days). The database keeps only the SHA-256 of the token.
  Signing out deletes it; disabling an account or resetting its password ends
  all of its sessions; changing your own password ends your other sessions.
- **CSRF.** Every request except `GET`/`HEAD` must send `X-RAST: 1`. Browsers
  only send custom headers cross-site after a CORS preflight, which the server
  never approves.
- **Throttling.** After 5 failed sign-ins per username or visitor IP, the next
  attempt waits 30 s, doubling up to 15 min.
- **First run.** With no accounts the server writes a random setup token to
  `<data>/setup-token` (`0600`). Creating the first account, always a Super
  Admin, needs it, so whoever opens a fresh public site first cannot claim it.
- **Audit log.** Sign-ins, failed sign-ins, setup, account, group and access
  changes, and refused app requests are recorded in `audit_log`.

### Groups and app access

Super Admins create **groups**, add accounts to them, and set each app to
*Inherit*, *Allow* or *Deny* for the group. An account can carry rules of its
own too. What an account may open is decided the same way as in classic RAST
and Synology DSM:

1. Super Admins and Admins open every app.
2. Any **deny** wins, whether it is on the account or comes from any of its groups.
3. Otherwise any **allow** grants the app.
4. Otherwise the app's default applies: File Station, Resource Monitor,
   Activity Logs and Control Panel are on for Users; Docker, Backup Center and
   App Center are off.

`crates/rast-server/src/access.rs` holds the app list and the one evaluator.
Control Panel shows its answer with the reason (*Denied by Interns, which
overrides an allow*) on an account's *Apps & groups* page and under
*Profile → Your apps*. The server enforces the decision itself: every request
under an app's prefix (`/api/files`, `/api/monitor`, `/api/logs`, `/api/docker`,
`/api/backups`, `/api/packages`) needs a session, and a denied account gets
`403` before any handler runs. The desktop only hides what is denied, such as
Control Panel's entries in the Start and account menus.

Admins can read groups and an account's access. Only Super Admins change them.
Deleting a group or an account removes the memberships and rules with it
(`ON DELETE CASCADE`). Schema version 2 adds `groups`, `group_members`,
`group_app_rules` and `user_app_rules` to a version 1 database without
touching its accounts.

API: `GET /api/auth/state`, `POST /api/auth/{setup,login,logout,password}`,
`GET|POST /api/users`, `PATCH|DELETE /api/users/{id}`,
`GET|PATCH /api/users/{id}/access`, `GET|POST /api/groups`,
`PATCH|DELETE /api/groups/{id}` and `GET /api/access/me`. In a `PATCH`, an app
set to `"inherit"` removes its rule. Errors are `{"error": "…"}` with a sentence
meant for the user.

Next: folder permissions per group and account, with the same deny-wins rule.

## Device login (native client)

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
- the browser build no longer uses it: it signs in to `rast-server` instead.
  An old preview's `rast.device-account` entry in `localStorage` is ignored.

Default locations (`RAST_ACCOUNT` overrides the file path):

| OS | Device account file |
| --- | --- |
| Linux | `$XDG_CONFIG_HOME/rast/device-account` (or `~/.config/rast/device-account`) |
| macOS | `~/Library/Application Support/rast/device-account` |
| Windows | `%APPDATA%\rast\device-account` |

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
