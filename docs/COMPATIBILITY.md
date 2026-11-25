# Compatibility matrix

RAST is developed and released on Linux/systemd. This page states **honestly**
what is tested, what is only built in CI and what is unsupported. “Tested”
means the complete install → run → upgrade path was exercised; “installer
tested in a clean container” means the full install/upgrade/uninstall flow was
run by `scripts/test-installer-clean.sh` in a disposable systemd container (see
[INSTALL.md](INSTALL.md#clean-environment-installer-test-maintainers)); “runtime
smoke-tested in CI” means the release workflow starts the built server on a
native runner, hits `/healthz`, creates an administrator through the CLI and
logs in through the JSON API; “CI” means the code builds and the test suite
passes there but the installer/units were not manually deployed; “build-only”
means a release binary is produced there but runtime behaviour is unverified.

## Platform support

| Platform | Architecture | Status | Notes |
| --- | --- | --- | --- |
| Ubuntu 26.04 LTS | x86_64 | **Tested (reference platform)** | Primary development and deployment target (Cloudflare Tunnel + nginx in front); installer, units and upgrade path are exercised here |
| Ubuntu 24.04 LTS | x86_64 | **Installer tested in a clean container** | `scripts/test-installer-clean.sh` installs/upgrades/uninstalls RAST inside a disposable systemd container built from the official `ubuntu:24.04` image (64 checks). Also a CI build/test runner |
| Ubuntu 22.04 LTS | x86_64 | CI + release build host + **runtime smoke-tested in CI** | Produces the official `rast-linux-x86_64` binary (glibc 2.35 baseline); the release workflow runs the server smoke test on this native runner |
| Ubuntu 24.04 LTS | aarch64 | **Runtime smoke-tested in CI (native arm64)** | The official `rast-linux-aarch64` binary is built and smoke-tested (serve + `/healthz` + CLI admin + API login) on a native `ubuntu-24.04-arm` runner. The full installer flow is still only exercised on x86_64; Debian/Ubuntu arm64 installs remain best-effort |
| Debian 12 / 13 | x86_64, aarch64 | Expected, untested | Installer uses standard systemd, `useradd` and `install`; verify `rast doctor` on your host |
| Other systemd distributions (Fedora, RHEL 9+, Rocky, …) | x86_64, aarch64 | Untested, best effort | Installer warns on non-Debian/Ubuntu families; manual install is the safe route ([INSTALL.md](INSTALL.md#manual-installation-no-installer)) |
| Alpine, other musl systems | — | **Unsupported** | No musl/static build is published; glibc binaries do not run |
| Non-systemd init (OpenRC, runit, s6, containers without systemd) | — | **Unsupported** | The binary may run under a custom supervisor, but the installer and units assume systemd; unsupported configurations are not covered by support |
| WSL2 | x86_64 | Untested | Recent WSL2 has systemd; treat as best effort |
| macOS 15 (11+ expected) | Apple Silicon (arm64) | **Preview: CI only (native arm64)** | Builds from source; no release binary yet. CI runs clippy and the test suite on `macos-15` and starts the server through `scripts/run-local.sh` (setup token, setup, login). Runs as your user, not as a service; see [macOS](#macos-preview) |
| macOS 11+ | Intel (x86_64) | Expected, untested | Same code path as Apple Silicon; not built in CI |

### macOS (preview)

On a Mac, RAST runs as your own user through `scripts/run-local.sh`: one
folder (default `~/RAST-local`) holds the database and the files, the web
desktop listens on `127.0.0.1` only, and Ctrl+C stops it. There is no
installer, no launchd service and no `rast update` for macOS yet.

* **Docker** works when the `docker` command does (Docker Desktop). The
  Docker helper (`rast worker`) runs as you, with your Docker access.
  Bind mounts must sit in folders Docker Desktop shares (by default
  `/Users`, which includes `~/RAST-local`).
* **App Center** lists apps but installs no packages. The catalog only
  publishes Linux builds. The version checks that every install runs need
  seccomp and Landlock, which exist only on Linux, so on macOS RAST refuses
  to run them rather than run package code unconfined. Imported GitHub
  packages are refused with an explanation.
* Everything else (File Station, accounts, Control Panel, Resource Monitor,
  backups) uses the same code as on Linux.

### glibc baseline of the release binaries

| Asset | Built on | Minimum glibc |
| --- | --- | --- |
| `rast-linux-x86_64` | Ubuntu 22.04 | 2.35 |
| `rast-linux-aarch64` | Ubuntu 24.04 (arm64 runner) | 2.39 |

The installer downloads the binary, then **runs `rast version` before installing
anything**; a glibc mismatch is reported with instructions to build from source
instead of leaving a broken service. On arm64 systems with glibc < 2.39 (for
example Ubuntu 22.04 arm64), build from source.

### Repeatable installer verification

The clean-container test is the reference procedure for validating installer,
systemd-unit or packaging changes:

```bash
scripts/test-installer-clean.sh            # build artifacts + 64-check container test
scripts/test-installer-clean.sh --skip-build --artifacts dist
```

It is also the fastest way to reproduce a platform-specific installer bug
without touching a real machine (see
[INSTALL.md](INSTALL.md#clean-environment-installer-test-maintainers)).

### CI runtime vs installer coverage

| Architecture | Build + unit/e2e tests | Runtime smoke (serve, health, login) | Full installer flow |
| --- | --- | --- | --- |
| x86_64 | ✅ CI on Ubuntu 24.04 | ✅ native x86_64 release runner | ✅ clean systemd container (`scripts/test-installer-clean.sh`) |
| aarch64 | ✅ CI on native arm64 runner | ✅ native arm64 release runner | ❌ not automated (manual/expected only) |

Both release binaries are additionally covered by Sigstore build-provenance
attestations created and verified by the release workflow — see
[INSTALL.md](INSTALL.md#release-integrity-and-provenance) and
[SECURITY.md](../SECURITY.md#releases).

## Build requirements

Only needed when building from source or packaging; the installed server has no
runtime dependencies beyond glibc.

| Requirement | Version | Notes |
| --- | --- | --- |
| Rust | pinned in [`rust-toolchain.toml`](../rust-toolchain.toml) = **1.99.1** | CI and releases always use the pinned toolchain. Effective minimum is **1.94** (`sqlx 0.9` requires it); the `rust-version` field in `Cargo.toml` is being aligned with that |
| Components | `rustfmt`, `clippy` | Installed automatically by `rustup` from the toolchain file |
| Node.js | **>= 22** | Frontend build only |
| npm | **>= 10** | Uses `frontend/package-lock.json` (`npm ci`) |
| C toolchain | any recent `cc` | SQLite is compiled in via `libsqlite3-sys` (`bundled`) |
| pkg-config | optional | Not needed because SQLite/OpenSSL alternatives are bundled/vendored |
| Target | Linux, x86_64 or aarch64 | Release artifacts are Linux-only |

Build commands are in [INSTALL.md](INSTALL.md#build-from-source) and
[CONTRIBUTING.md](../CONTRIBUTING.md).

## systemd

| Feature used | Minimum systemd |
| --- | --- |
| `RuntimeDirectoryPreserve=restart`, `ProtectProc=invisible`, `SystemCallFilter=` in the shipped units | 247+ |
| Recommended | **249+** (Ubuntu 22.04+, Debian 12+) |
| Known good | Ubuntu 22.04/24.04/26.04, Debian 12/13 |

Older systemd versions log unknown directives and ignore them; the service still
starts, but hardening is reduced. `Type=simple`, `Restart=on-failure` and
`EnvironmentFile=-` work on all supported versions.

## Docker (Applications app only)

| Component | Requirement | Status |
| --- | --- | --- |
| Docker Engine | 24+ recommended | Worker calls the Docker CLI; not exercised in CI (protocol is unit-tested with a fake socket) |
| Compose | **v2 plugin** (`docker compose`, not legacy `docker-compose`) | Required; v1 is unsupported |
| Socket | `/var/run/docker.sock`, group `docker` | The worker user is added to `docker` when the group exists |
| Docker absent | Applications app reports “unavailable” | The rest of RAST is fully functional |

## Browsers

The web desktop targets current evergreen browsers (latest two stable versions
of Firefox, Chrome/Chromium, Edge and Safari). It relies on ES modules, fetch,
server-sent events and modern CSS; Internet Explorer and other legacy engines
are unsupported. A TLS-terminating proxy is required for anything other than
localhost because session cookies are `Secure` whenever `public_url` is HTTPS.

## Reverse proxies and tunnels

| Proxy | Status | Notes |
| --- | --- | --- |
| nginx (TLS termination) | Deployment target (maintainers) | Example config in [INSTALL.md](INSTALL.md#nginx); needs `client_max_body_size`, `proxy_buffering off` for SSE, `proxy_request_buffering off` for uploads |
| Cloudflare Tunnel | Deployment target (maintainers) | No inbound ports; Cloudflare plan body-size limits apply to uploads |
| Caddy v2 | Expected, untested | `flush_interval -1` for SSE, `request_body max_size` for uploads |
| Other HTTP proxies | Best effort | Must preserve `Host`, `X-Forwarded-For`, `X-Forwarded-Proto` and support streaming |

Set `trusted_proxies` to the proxy's source CIDRs and `public_url` to the
external HTTPS URL; see [CONFIGURATION.md](CONFIGURATION.md).

## Filesystems and storage

| Filesystem | Status | Notes |
| --- | --- | --- |
| ext4 / xfs / btrfs (local) | Supported | SQLite locking and file operations rely on POSIX semantics |
| NFS / SMB / CIFS | **Unsupported** | SQLite locking and `flock` semantics are unreliable; the data directory must be local |
| FUSE mounts (network backends) | Unsupported | Same locking caveats |
| Encrypted local volumes (LUKS) | Supported | Use full-disk encryption for data at rest |

## Reporting a compatibility result

Tested RAST on another distribution? Open an issue with:

* distribution/version and architecture, kernel and systemd versions;
* install method (installer/manual) and RAST version (`rast version`);
* `rast doctor` output;
* anything that needed adjustment (unit directives, paths, glibc).

That is how rows in this table move from “untested” to “tested”.
