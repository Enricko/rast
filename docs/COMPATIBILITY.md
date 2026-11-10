# Compatibility matrix

RAST is developed and released on Linux/systemd. This page states **honestly**
what is tested, what is only built in CI and what is unsupported. “Tested”
means the complete install → run → upgrade path was exercised; “CI” means the
code builds and the test suite passes there but the installer/units were not
manually deployed; “build-only” means a release binary is produced there but
runtime behaviour is unverified.

## Platform support

| Platform | Architecture | Status | Notes |
| --- | --- | --- | --- |
| Ubuntu 26.04 LTS | x86_64 | **Tested (reference platform)** | Primary development and deployment target (Cloudflare Tunnel + nginx in front); installer, units and upgrade path are exercised here |
| Ubuntu 24.04 LTS | x86_64 | CI (build + tests) | Also the frontend build runner; installer not manually verified |
| Ubuntu 22.04 LTS | x86_64 | CI + release build host | Produces the official `rast-linux-x86_64` binary (glibc 2.35 baseline) |
| Ubuntu 24.04 LTS | aarch64 | **Build-only** | Official `rast-linux-aarch64` binary is built on a native `ubuntu-24.04-arm` runner; runtime not yet tested |
| Debian 12 / 13 | x86_64, aarch64 | Expected, untested | Installer uses standard systemd, `useradd` and `install`; verify `rast doctor` on your host |
| Other systemd distributions (Fedora, RHEL 9+, Rocky, …) | x86_64, aarch64 | Untested, best effort | Installer warns on non-Debian/Ubuntu families; manual install is the safe route ([INSTALL.md](INSTALL.md#manual-installation-no-installer)) |
| Alpine, other musl systems | — | **Unsupported** | No musl/static build is published; glibc binaries do not run |
| Non-systemd init (OpenRC, runit, s6, containers without systemd) | — | **Unsupported** | The binary may run under a custom supervisor, but the installer and units assume systemd; unsupported configurations are not covered by support |
| WSL2 | x86_64 | Untested | Recent WSL2 has systemd; treat as best effort |

### glibc baseline of the release binaries

| Asset | Built on | Minimum glibc |
| --- | --- | --- |
| `rast-linux-x86_64` | Ubuntu 22.04 | 2.35 |
| `rast-linux-aarch64` | Ubuntu 24.04 (arm64 runner) | 2.39 |

The installer downloads the binary, then **runs `rast version` before installing
anything**; a glibc mismatch is reported with instructions to build from source
instead of leaving a broken service. On arm64 systems with glibc < 2.39 (for
example Ubuntu 22.04 arm64), build from source.

## Build requirements

Only needed when building from source or packaging; the installed server has no
runtime dependencies beyond glibc.

| Requirement | Version | Notes |
| --- | --- | --- |
| Rust | pinned in [`rust-toolchain.toml`](../rust-toolchain.toml) = **1.99.1** | `Cargo.toml` declares MSRV `1.85`; CI and releases always use the pinned toolchain |
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
