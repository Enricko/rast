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
* native Linux applications and their per-user graphical sessions;
* the host itself (Docker group membership is effectively root; the native
  helper runs as root by design).

### Trust boundaries

```
untrusted network ──TLS proxy──▶ rast.service (unprivileged)
                                      │ Unix socket (0660, group rast)
                    ┌─────────────────┴──────────────────┐
                    ▼                                    ▼
        rast-worker.service (docker group)      rast-native.service (root)
                    │ /var/run/docker.sock               │ systemd-run / apt-get / useradd
                    ▼                                    ▼
             Docker Engine / host              rast-linux.slice: rast-u<uid>
                                              session + application processes
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
7. **RAST → native helper.** The helper is the only root component of the
   native backend and is deliberately thin: a Unix socket (0660 root:rast,
   optional peer-uid allowlist), JSON-lines requests with `deny_unknown_fields`,
   no shell anywhere, absolute binaries and argument vectors, per-operation
   field validation (usernames, units, displays, ports, argv[0] prefixes, env
   keys, limits, managed paths), bounded timeouts/output, serialized APT
   transactions and an admin-only trigger set. `.deb` installation is
   explicitly *administrator-approved privileged software installation*; the
   application processes themselves run under unprivileged identities.
8. **Native application ↔ other users.** Each RAST user maps to a distinct
   unprivileged Linux identity; profiles are 0700 per app/user, sessions bind
   to loopback with Xpra authentication disabled (RAST's authenticated
   same-origin proxy is the only path to them), and users cannot read each
   other's homes or connect to each other's sockets. Applications inside one
   person's session share that person's trust boundary by design. The person's
   RAST home is shared with *their own* identity through an ACL only. Chromium
   runs unprivileged with its sandbox enabled (`Seccomp: 2`,
   `NoNewPrivs: 1` on renderer processes); `--no-sandbox` is never added.

### Out of scope / limitations

* An attacker with root or physical access to the host; no user-space service
  can defend against that.
* Malicious Docker images or compose files an administrator deliberately
  deploys — administrators are trusted; the worker limits *how* commands are
  executed, not *what* the chosen containers do.
* Malicious native application packages or installation scripts an
  administrator deliberately installs — an admin-approved `.deb` runs its
  maintainer scripts as root during `apt-get`; the helper bounds *which*
  operations are possible, not what the chosen package does. Treat App Center
  → Add app like `sudo apt install`.
* Availability attacks that exhaust host resources (bounded by systemd limits,
  but there is no per-user request quota beyond file quotas).
* Supply-chain compromise of crates/npm packages beyond what `Cargo.lock`,
  `package-lock.json`, checksummed releases, build-provenance attestations and
  dependency auditing detect.
* Users who can read the data directory or `secret.key` from the host: that is
  game over for encrypted secrets and sessions.
* Missing browser security features in very old browsers.

### Not implemented (yet) — do not assume these protections exist

* **Project-managed release signing** (long-lived minisign/cosign keys) is not
  enabled. Integrity relies on TLS + the release `SHA256SUMS`, and releases now
  publish GitHub/Sigstore build-provenance attestations for both binaries and
  `SHA256SUMS` (verify with `gh attestation verify ... --repo Enricko/rast`).
  Provenance proves build origin, not source review or reproducibility. The
  fail-closed `RAST_REQUIRE_SIGNATURE` hooks remain for operators who verify
  with their own keys.
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
* **Build-provenance attestations**: the release workflow creates Sigstore
  keyless attestations (`actions/attest-build-provenance`) for both binaries
  and for `SHA256SUMS`, and verifies them with `gh attestation verify` before
  publishing. The installer can enforce the same check with
  `--verify-attestation` / `RAST_VERIFY_ATTESTATION=1` (fail-closed; requires
  GitHub CLI 2.49+ and GitHub API access).
* Both architectures are exercised end to end on their native runners before
  release: the built binary starts, answers `/healthz`, creates an
  administrator through the CLI and logs in through the JSON API.
* The release workflow validates the version against `Cargo.toml`, extracts
  release notes from `CHANGELOG.md` and re-verifies all assets before
  publishing.
* Fail-closed signature verification hooks (`RAST_REQUIRE_SIGNATURE`,
  minisign/cosign) remain available for operators who manage their own keys;
  project-managed signatures are planned.

Manual verification of a downloaded release:

```bash
sha256sum --check --strict SHA256SUMS
gh attestation verify rast-linux-x86_64 --repo Enricko/rast
gh attestation verify SHA256SUMS --repo Enricko/rast
```

(`gh` must be 2.49+ and authenticated for the attestation API — `gh auth login`
or `GH_TOKEN` — even though the repository is public.)

An attestation proves the file was produced by this repository's public
release workflow for a specific commit and is recorded in Sigstore's
transparency log. It does **not** prove the source was reviewed, that the
build is reproducible, or that the binary is free of vulnerabilities. Checksums
alone protect against corruption and transfer errors, not against a compromised
release; provenance plus a pinned version narrows that window.

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
- [ ] Keep `rast-native.service` limited to the `rast` peer uid, review
      `linuxapp.*` audit entries, and remember that installing a `.deb` (App
      Center → Add app) is privileged software installation: review the
      dependency transaction in the preview before confirming.
- [ ] Keep `/run/rast-linux` (0700 per user) and `/var/lib/rast/linux-users`
      only; never add the `rast` account to a native identity's group.
- [ ] Review Activity Logs periodically and after any suspicious event.
- [ ] Set `RAST_MAX_UPLOAD_BYTES` and proxy body limits to values you need;
      missing limits allow disk-filling uploads by authenticated users.
- [ ] Run `sudo -u rast rast doctor` after upgrades or configuration changes.
- [ ] Verify release checksums **and** build-provenance attestations
      (`sha256sum --check --strict SHA256SUMS`, `gh attestation verify …`)
      before installing, especially on internet-facing servers.
