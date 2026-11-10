# Security policy

RAST is a self-hosted administration tool: it stores user files, manages
accounts and (optionally) controls Docker applications. Treat a RAST
installation like any other privileged server service — keep it patched, put it
behind TLS and restrict who can reach it.

## Supported versions

| Version | Supported |
| --- | --- |
| `0.1.x` | ✅ Current release series |
| `< 0.1` | ❌ No public releases exist |

Security fixes are released for the current minor series. Upgrade with the
installer or `rast update`; see [docs/OPERATIONS.md](docs/OPERATIONS.md#updates).

## Reporting a vulnerability

**Do not open a public issue for security problems.**

Use GitHub's private vulnerability reporting on this repository:

<https://github.com/Enricko/rast/security/advisories/new>

That creates a private advisory visible only to the maintainers. Please include:

* affected version(s) and platform (`rast version`, distribution, arch);
* a description of the issue and the impact you believe it has;
* reproduction steps or a proof of concept (a minimal HTTP request, script or
  archive is ideal);
* any suggested fix or mitigation;
* whether you intend to publish a write-up, and when.

What to expect:

* acknowledgement of the report as soon as possible (best-effort, usually
  within a few days);
* an assessment and, when accepted, a fix in a patch release and a coordinated
  advisory with credit (unless you prefer to remain anonymous);
* please give us a reasonable window (typically 90 days) before public
  disclosure; we will coordinate with you if a fix takes longer.

Good-faith research on installations you own or are authorized to test is
welcome. Do not test against other people's instances, do not exfiltrate user
data, and do not run denial-of-service tests against shared infrastructure.

## Threat model

### Assets

* user files and shared folders (`storage_dir`);
* account credentials, sessions and CSRF tokens;
* the SQLite database: users, sessions, audit logs, app definitions;
* `secret.key` (master key for CSRF derivation and encryption of app secrets);
* Docker applications running on the host (via the worker);
* the host itself (Docker group membership is effectively root).

### Trust boundaries

```
untrusted network ──TLS proxy──▶ rast.service (unprivileged)
                                      │ Unix socket (0660, group rast)
                                      ▼
                              rast-worker.service (docker group)
                                      │ /var/run/docker.sock
                                      ▼
                                Docker Engine / host
```

1. **Network → proxy.** TLS is terminated by nginx/Caddy/Cloudflare. RAST binds
   to loopback by default. Anyone who can reach the RAST port without a proxy
   can reach the login page; keep it firewalled.
2. **Proxy → RAST.** `X-Forwarded-For` / `CF-Connecting-IP` are trusted **only**
   when the immediate peer is in `trusted_proxies`; otherwise the TCP peer
   address is used. A wrong configuration cannot be used to spoof audit/log
   identities, but it can make every client look like the proxy.
3. **Browser → RAST.** Every mutating request needs a valid session cookie and
   an HMAC CSRF token bound to that session. The UI is served with a strict CSP.
4. **Authenticated user → other users' data.** Handlers enforce role, ownership
   and shared-folder ACLs; path resolution rejects traversal and symlink
   escapes. Quotas are enforced server-side with transactional reservations.
5. **RAST → worker.** The worker only accepts whitelisted actions over its Unix
   socket (mode 0660, group `rast`); the installer also pins the allowed peer uid
   to the `rast` account (`RAST_WORKER_ALLOWED_UID`). Arguments are validated
   (app slugs, compose paths under `data_dir/apps/<slug>/`), commands run
   without a shell and with timeouts.
6. **Worker → Docker.** The worker is in the `docker` group. **This is
   root-equivalent by design.** A compromise of the worker (or of an app
   definition it deploys) can compromise the host; only administrators can
   manage applications, and compose validation restricts what may be deployed.

### Out of scope / limitations

* An attacker with root or physical access to the host; no user-space service
  can defend against that.
* Malicious Docker images or compose files an administrator deliberately
  deploys — administrators are trusted; the worker limits *how* commands are
  executed, not *what* the chosen containers do.
* Availability attacks that exhaust host resources (bounded by systemd limits,
  but there is no per-user request quota beyond file quotas).
* Supply-chain compromise of crates/npm packages beyond what `Cargo.lock`,
  `package-lock.json`, checksummed releases and dependency auditing detect.
* Users who can read the data directory or `secret.key` from the host: that is
  game over for encrypted secrets and sessions.
* Missing browser security features in very old browsers.

### Not implemented (yet) — do not assume these protections exist

* **Release signing** is not enabled; integrity relies on TLS + SHA-256
  checksums from the GitHub Release. Verify checksums and prefer pinned
  versions.
* **Two-factor authentication** (TOTP/WebAuthn) is not available in 0.1.x.
* Audit logs are append-only in the application but are not cryptographically
  chained or forwarded to a remote SIEM; an admin with database access can
  alter them.
* Per-user network quotas, geo/IP allowlists and WAF features are the
  responsibility of your reverse proxy.

## Security features

### Authentication and sessions

* Argon2id password hashing (19 MiB memory, 2 iterations, 1 lane, 32-byte
  output) on a blocking thread pool; transparent re-hash if parameters are
  upgraded.
* Minimum password length 10, maximum 256 characters; passwords are never
  written to logs or audit records.
* Opaque 256-bit session tokens; only SHA-256 hashes are stored. Sliding
  expiry (`RAST_SESSION_TTL_HOURS`, default 168 h) with a hard 30-day absolute
  cap, per-session revocation and "revoke all sessions".
* Login rate limiting per IP (10/5 min) and per username (10/15 min), plus a
  persistent per-account lockout (15 minutes) after repeated failures.
* No default accounts, no public registration, no password reset by e-mail;
  the first administrator is created via the CLI or a one-shot setup token.
* CSRF: `X-CSRF-Token` header required on all mutating authenticated requests,
  HMAC-derived from the session id (double-submit bound to the session).
* Cookies: `HttpOnly`, `SameSite=Lax`, `Secure` automatically when
  `public_url` is HTTPS.

### Network and web

* Loopback bind by default; clear warnings when binding to a network address
  without trusted proxies.
* Strict CSP for HTML (`default-src 'self'`, no inline scripts, no framing),
  `no-store` on health, cache-busting ETags and long-lived immutable caching for
  hashed assets.
* Streaming uploads/downloads (no full-file buffering), upload size limit and
  quota reservations; previews of user content are served with a sandboxing CSP
  and `nosniff` semantics so active content cannot run in the application
  origin.
* Forwarded headers honoured only from configured proxies.

### Storage and secrets

* Per-user roots (`homes/<user>`, `shared/<name>`) resolved and canonicalised;
  `..`, NUL, backslash and symlink escapes are rejected before any filesystem
  call.
* `storage_dir` stays `0755` with per-user homes `0700`; the data directory is
  `0750 rast:rast`; `secret.key` is `0600`.
* Master key with purpose-specific derived keys (HMAC for CSRF, AES-256-GCM for
  encrypted application secrets). Backups that include `config` contain
  `secret.key` and must be stored encrypted off-server.
* systemd sandboxing for both units: `NoNewPrivileges`, empty capability
  bounding sets, `ProtectSystem=strict` with explicit `ReadWritePaths`,
  `ProtectHome`, `PrivateTmp`, `PrivateDevices`, `RestrictAddressFamilies`,
  `LockPersonality`, memory limits and syscall filtering.

### Releases

* Two native builds per release (`x86_64`, `aarch64`) from a pinned Rust
  toolchain and a committed `Cargo.lock`/`package-lock.json`.
* `SHA256SUMS` is published and verified by the installer and `rast update`
  before anything is replaced; a mismatch aborts.
* The release workflow validates the version against `Cargo.toml`, extracts
  release notes from `CHANGELOG.md` and re-verifies all assets before
  publishing.
* Fail-closed signature verification hooks (`RAST_REQUIRE_SIGNATURE`,
  minisign/cosign) are implemented in the installer for operators who manage
  their own keys; official signatures are planned (see
  [CHANGELOG.md](CHANGELOG.md)).

## Known dependency advisory (0.1.0)

`cargo audit` reports **RUSTSEC-2026-0459** (`libsqlite3-sys` 0.37.0,
CVE-2026-119375): the bundled SQLite version has an information-disclosure
issue in WAL handling (CVSS 4.0 local attack vector). The fixed release is
`libsqlite3-sys >= 0.39.0`, but `sqlx 0.9.0` currently constrains the dependency
to `< 0.38.0`; the upgrade is blocked upstream and is tracked for the next
dependency refresh. CI ignores exactly this advisory (all other advisories fail
the build).

Practical exposure is limited: RAST never executes user-supplied SQL — queries
are parameterised and issued by the server itself — and the database, WAL and
secret key are only readable by the `rast` service account (`0750`/`0640`/`0600`).
An attacker who can already read those files has local access to the service
account. Keep other local users out of the data directory, apply OS updates,
and upgrade RAST once a build with the patched SQLite is published.

## Hardening checklist for operators

- [ ] Serve RAST only over HTTPS; keep `bind` on loopback behind the proxy.
- [ ] Set `public_url` and restrict `trusted_proxies` to your proxy's CIDRs.
- [ ] Firewall the RAST port so only the proxy can reach it.
- [ ] Use strong unique passwords; one account per person; least privilege
      (`user` unless administration is needed).
- [ ] Enable `setup_token_required` for any internet-facing instance and run
      `rast setup-token` for the first-run flow.
- [ ] Keep the host, Docker and RAST patched; subscribe to release notifications.
- [ ] Enable scheduled backups and keep encrypted copies off-server.
- [ ] Restrict the `docker` group (`rast-worker` only!) — every member is
      root-equivalent.
- [ ] Review Activity Logs periodically and after any suspicious event.
- [ ] Set `RAST_MAX_UPLOAD_BYTES` and proxy body limits to values you need;
      missing limits allow disk-filling uploads by authenticated users.
- [ ] Run `sudo -u rast rast doctor` after upgrades or configuration changes.
- [ ] Verify release checksums (and signatures once published) before
      installing, especially on internet-facing servers.
