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
  SameSite=Strict`, 7 days). The database keeps only the SHA-256 of the token,
  plus a random public id, the device's user agent and IP address, and when it
  was last used. *Profile → Sessions* lists your devices (*Chrome on Windows ·
  203.0.113.5 · 2 h ago*) and ends any of them but the one you are using, or
  all the others at once. An account's page shows administrators its sessions
  and has *Sign out everywhere*. Signing out deletes the session; disabling an
  account, setting its password or requiring a new one ends all of its
  sessions; changing your own password ends your other sessions. The token
  hash never leaves the server: lists and the API name sessions by public id.
- **Temporary passwords.** A password an administrator sets, when creating an
  account or later, is temporary by default (`mustChangePassword`). Until the
  account chooses its own, its session can only read `/api/auth/state`, change
  the password and sign out; everything else answers `403`. The browser sign-in
  asks for the new password right after the temporary one and opens the desktop
  only then. The new password must differ from the current one.
- **CSRF.** Every request except `GET`/`HEAD` must send `X-RAST: 1`. Browsers
  only send custom headers cross-site after a CORS preflight, which the server
  never approves.
- **Throttling and lockout.** After 5 failed sign-ins per username or visitor
  IP, the next attempt waits 30 s, doubling up to 15 min (in memory). In the
  database, 10 wrong passwords within 30 minutes lock the account for 15 minutes,
  which survives a restart. A locked account answers like the throttle, `429`,
  even with the right password, so a guess never learns it was right, and each
  try still counts towards the throttle, so a lock that outlives a restart
  cannot be hammered. A successful sign-in clears the count; administrators see
  *Locked until 14:05* on the account's page and can *Unlock* it early.
- **First run.** With no accounts the server writes a random setup token to
  `<data>/setup-token` (`0600`). Creating the first account, always a Super
  Admin, needs it, so whoever opens a fresh public site first cannot claim it.
- **Audit log.** Sign-ins, failed and locked sign-ins, lockouts and unlocks,
  setup, account, profile, group and access changes, ended sessions, and refused
  app requests are recorded in `audit_log` with the visitor's IP. nginx sets that
  IP from Cloudflare's `CF-Connecting-IP`; the server accepts only a valid
  address there and never trusts `X-Forwarded-For`.
- **Database settings.** Every connection turns on `foreign_keys`, writes with
  `synchronous=FULL` in WAL mode (a committed change survives a power cut),
  `secure_delete` (deleted rows, such as ended sessions and old password
  hashes, are overwritten instead of left in free pages) and
  `trusted_schema=OFF`. Schema version 3 adds the lockout, temporary-password
  and session-detail columns in place; sessions signed in before it stay signed
  in.

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
`PATCH /api/auth/profile` (your display name), `GET|DELETE /api/auth/sessions`
(yours; `DELETE` ends all but this one), `DELETE /api/auth/sessions/{id}`,
`GET|POST /api/users`, `PATCH|DELETE /api/users/{id}`,
`GET|DELETE /api/users/{id}/sessions`, `GET|PATCH /api/users/{id}/access`,
`GET|POST /api/groups`, `PATCH|DELETE /api/groups/{id}` and
`GET /api/access/me`. A user `PATCH` takes any of `displayName`, `role`,
`disabled`, `password`, `mustChangePassword` (defaults to `true` with a new
`password`) and `unlock`; nobody sets their own password there (Profile asks
for the current one). In an access `PATCH`, an app set to `"inherit"` removes
its rule. Errors are `{"error": "…"}` with a sentence meant for the user.

Next: folder permissions per group and account, with the same deny-wins rule.

### Is SQLite enough for an enterprise?

For one server, yes. SQLite is an embedded library, not a network service:
there is no database port, user or password to attack. The data is one file in
an owner-only directory of a sandboxed `DynamicUser` service, and SQLite itself
is among the most tested code in use. A security review of RAST is mostly a
review of this server's code, not of the database.

What SQLite does not give, and what to add when a customer needs it:

- **Encryption at rest.** The file is plain on disk. Encrypt the disk (LUKS, or
  the cloud provider's volume encryption), or switch to SQLCipher if the file
  must be encrypted on its own. Without it, anyone with the disk can read the
  password hashes (Argon2id, so slow to crack) and the audit log.
- **Backups.** `sqlite3 rast.db .backup` or `.dump` while the server runs,
  encrypted, kept off the machine, and restored in a drill. Never copy the
  folder of a running database.
- **High availability.** One server writes at a time. That is plenty for
  accounts, but a second node needs streaming replication (Litestream) or a
  move to PostgreSQL. The queries are plain SQL behind `db.rs`, so that move
  stays contained.
- **Enterprise identity.** Two-factor sign-in, SSO (OIDC/SAML/LDAP), a
  tamper-evident audit log exported to a SIEM, and password and session
  policies are planned in Control Panel; they don't depend on the database.

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
