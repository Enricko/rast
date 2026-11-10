# Contributing to RAST

Thanks for considering a contribution! RAST is a small, opinionated project:
one Rust binary, one embedded web UI, a strong preference for boring, safe
defaults. This document covers how to build, test and submit changes.

By participating you agree to the [Code of Conduct](CODE_OF_CONDUCT.md). Report
security issues privately per [SECURITY.md](SECURITY.md) — never in a public
issue.

## Development setup

Prerequisites:

* Linux (RAST targets systemd distributions; development on other Unix systems
  is not supported by the service units but the test suite runs anywhere Rust
  runs);
* Rust via [rustup](https://rustup.rs/): the toolchain is pinned in
  [`rust-toolchain.toml`](rust-toolchain.toml) and installed automatically by
  `rustup` when you enter the directory;
* Node.js **>= 22** and npm **>= 10** — only to build the frontend;
* `sqlite3` CLI (optional, useful for inspecting dev databases).

```bash
git clone https://github.com/Enricko/rast.git
cd rast
rustup show                        # installs the pinned toolchain if needed

# Build the frontend once (it is embedded by rust-embed)
(cd frontend && npm ci && npm run build)

# Build and run the server against throw-away directories
source scripts/dev-source-env.sh
cargo run -- serve
```

`scripts/dev-source-env.sh` exports `RAST_*` variables pointing at
`target/dev-data` / `target/dev-storage` (git-ignored), writes a development
`rast.toml`, and prints the next commands. Create the first administrator in a
second terminal:

```bash
source scripts/dev-source-env.sh
cargo run -- user create-admin admin --password-stdin
```

Then open <http://127.0.0.1:8720>.

Useful commands:

| Command | What it does |
| --- | --- |
| `cargo run -- serve` | Run the server (default subcommand) |
| `cargo run -- status --json` | CLI status output |
| `cargo run -- doctor` | Configuration/permissions diagnostics |
| `cargo run -- user list` | List accounts |
| `cargo run -- backup --scope db,config` | Create a backup in the dev data dir |
| `cargo run -- restore --from … --dry-run` | Verify a trash-safe restore plan |
| `cargo run -- update --check` | Check GitHub Releases for a newer version |
| `cargo run -- worker` | Run the Docker worker (needs Docker; normally systemd does this) |

## Code quality gates

CI (and `cargo`-based review) runs:

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

Additionally, packaging changes are checked with:

```bash
shellcheck -s sh -x installer/install.sh installer/uninstall.sh
shellcheck -s bash -x scripts/*.sh
scripts/sync-installer-assets.sh --check     # deploy/ ↔ embedded installer blocks
yamllint -c .github/yamllint.yml .github/workflows
```

Please run the relevant commands before opening a pull request. New behaviour
needs tests; bug fixes should add a regression test where practical.

### Tests

* Unit tests live next to the code (`#[cfg(test)] mod tests`); integration
  tests go in `tests/`.
* Use `tempfile` for filesystem/database fixtures — never touch `/etc/rast` or
  `/var/lib/rast` in tests.
* Tests must not require Docker, a network connection, root or a live systemd.
  Mock the worker socket with a temporary Unix socket where needed.
* Prefer deterministic tests (fixed inputs, injected clocks) over sleeps.

## Frontend

* Source: `frontend/src` (TypeScript + Vite, no framework lock-in beyond Vite).
* Build: `cd frontend && npm ci && npm run build` (runs `tsc --noEmit` and
  produces `frontend/dist`, which is embedded into the binary — never edit
  `frontend/dist` by hand and never commit it; it is git-ignored).
* The installed server never needs Node.js; npm is a build-time tool only.
* Keep the UI dependency-free unless there is a strong reason; new
  dependencies must be added to `package-lock.json` via `npm install` and are
  subject to review (license + supply-chain impact).
* Frontend code must not assume a public CDN: everything is served from the
  binary (`default-src 'self'` CSP).

## Installer, units and release tooling

* `installer/install.sh` and `installer/uninstall.sh` are **POSIX sh** and must
  work when piped (`curl … | sudo sh`). Test with `dash -n`, `sh -n` and
  ShellCheck; avoid bashisms and never read from stdin for normal operation.
* systemd units and the config template in `deploy/` are the canonical sources;
  `scripts/sync-installer-assets.sh` embeds them into the installer. **After
  editing `deploy/*`, run `scripts/sync-installer-assets.sh` and commit the
  regenerated `installer/install.sh`** (CI fails otherwise).
* The installer must stay idempotent: a re-run upgrades the binary/units and
  never deletes or overwrites configuration and user data.
* Release assets must keep the documented names:
  `rast-linux-x86_64`, `rast-linux-aarch64`, `SHA256SUMS`, `install.sh`,
  `uninstall.sh` (plus `rast.toml.example`).
* Never programmatically modify a user's firewall, DNS, TLS or reverse proxy
  from the installer.
* Run the clean-environment installer test before submitting changes to
  `installer/`, `deploy/` or the unit files:
  `scripts/test-installer-clean.sh` (Docker required; it never touches the
  host — see [docs/INSTALL.md](docs/INSTALL.md#clean-environment-installer-test-maintainers)).

## Commit and pull-request style

* Use [Conventional Commits](https://www.conventionalcommits.org/) prefixes:
  `feat:`, `fix:`, `docs:`, `chore:`, `refactor:`, `test:`, `perf:`, `ci:`,
  `build:`. Keep the subject imperative and under ~72 characters; explain the
  *why* in the body.
* Keep changes focused: one logical change per PR, with a clear description of
  behaviour and risk. Reference issues (`Fixes #123`).
* Avoid committing generated artifacts (`target/`, `frontend/dist/`,
  `frontend/node_modules/`, `dist/`, local databases or `.env` files).
* Do not reformat unrelated files. `cargo fmt` changes belong in their own
  commit if they would obscure the diff.
* Update documentation when behaviour, configuration or CLI output changes:
  `README.md`, `docs/*.md`, `deploy/rast.toml.example` and `.env.example` are
  part of the public contract.
* Add a `CHANGELOG.md` entry under `[Unreleased]` for user-visible changes.
* Breaking changes to the API, database schema or configuration must be called
  out explicitly in the PR and the changelog. Database migrations are
  forward-only — coordinate before adding one (`migrations/` is owned by the
  project lead).

## No secrets

Never commit credentials, tokens, private keys, real hostnames tied to private
infrastructure or customer data. CI scans the repository with gitleaks. If you
accidentally commit a secret, remove it from history and rotate it immediately;
tell the maintainers if it lived in a public branch.

Sensitive values in tests must be obvious dummies (`correct horse battery
staple`, `example.test` hostnames, documentation IP ranges such as
`192.0.2.0/24`).

## Review and merge

* Maintainers review for correctness, security, tests and documentation.
* CI must be green (format, clippy, tests, frontend build, packaging checks,
  secret scan) before merge.
* Do not force-push a branch that is under review; push fixup commits and let
  the maintainer squash if desired.
* Release tags (`vX.Y.Z`) and publishing are done by maintainers only.

## License

Contributions are accepted under the [MIT license](LICENSE). By submitting a
pull request you confirm you have the right to license your contribution under
these terms.
