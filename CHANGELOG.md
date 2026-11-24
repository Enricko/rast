# Changelog

All notable changes to RAST are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Release notes published on GitHub are generated from the matching section of
this file by `scripts/changelog-section.sh`.

## [Unreleased]

### Added

- **RAST Desktop native stream broker (Phase 3).** Additive server support for
  the native macOS client (`docs/PROTOCOL.md` §2/§4 in the RAST Desktop
  repository): `POST /api/linux-apps/session/stream-token` issues a 120 s,
  single-use `wss://…/linux-apps/native/<token>/` URL (32 random bytes; only
  SHA-256 kept in memory; bound to the account, the RAST login session and the
  exact native session; max 8 outstanding per user with oldest-eviction; 30
  issuances/minute; `404` without a live session, `409` when native Linux apps
  are disabled), and `DELETE /api/linux-apps/session/stream-tokens` revokes all
  outstanding tokens (`{revoked}`). The new public WebSocket route
  `GET /linux-apps/native/<token>/<rest>?<query>` is mounted outside cookie auth
  and outside the browser same-origin `Origin` check because the native Xpra
  client is not a browser: the token is its sole credential, consumed atomically
  on the first successful upgrade, and the relay is transparent to the loopback
  Xpra WebSocket (64 MiB frame cap, same relay semantics as the HTML5 stream,
  sub-protocols negotiated end to end, 5 s revalidation of the issuing login
  session). No token, URL, session key or cookie is ever logged or audited;
  denied attaches get a bodyless `401`. `GET /api/bootstrap` now advertises
  `desktop: {protocol: 1, streamTokens: true, minClientVersion: "0.1.0"}`.
  Native Xpra must run with `XPRA_WEBSOCKET_MASK=1` (RFC 6455 masking; the
  relay is strict and closes unmasked client frames). The browser HTML5 path is
  unchanged, no new public port is opened, and Xpra stays on loopback. New
  documentation: [docs/NATIVE-BROKER.md](docs/NATIVE-BROKER.md).

### Fixed

- **LibreOffice launches.** `/usr/bin/libreoffice` resolves into the root-owned
  `/usr/lib/libreoffice` tree, which was outside the argv[0] canonical
  allow-list; the tree is now allowed, and LibreOffice Writer imports/launches
  as a normal discovered application.
- **Snap packages are honestly unsupported for identity users.** Snapd refuses
  to run snaps for RAST's mapped service identities; install `.deb`, official
  tarballs or AppImages instead. Documented in the operator guide so a
  discovered Snap entry is not mistaken for a runnable app.



### Fixed

- **Session startup under host disk pressure.** Xpra startup is file-I/O bound;
  when other workloads saturate the disk (`/proc/pressure/io`), a 30 s
  readiness deadline can expire while the session is still initialising,
  causing a needless retry/rollback. The session manager now uses a bounded
  75 s readiness window when `some avg10` is >= 25 %, and the deployment
  Terminal smoke scales its browser waits under the same condition. Normal
  sessions keep the 30 s deadline.
- **Snap-packaged applications** can be imported/launched through the generic
  discovery path (`/snap/bin/*` wrappers canonicalise through `/usr/lib/snapd`).



### Added

- **Verified GPU-backed native sessions.** `linux_display_backend` accepts
  `auto | software | gpu | custom`; `auto` starts a privileged per-session
  NVIDIA Xorg display, verifies the NVIDIA OpenGL renderer and a connected
  RandR output as the person's identity, attaches Xpra with
  `--use-display=yes`, and falls back to Xvfb if the real probe fails. Sessions
  report `displayBackend`, `gpuAccelerationVerified` and `renderer`; the
  backend is persisted for restart adoption and cleaned up on session end.
  Chrome reports Canvas/Compositing/Rasterization/WebGL/WebGPU/Video-Decode
  hardware accelerated on such sessions. Honest limits: Xorg runs as root,
  GPU VRAM is shared, browser video decoding stays software, and stream
  encoding remains software (NVENC unavailable). Evidence:
  `tests/e2e/results/gpu-display-rollout-4cc60dc.md`.


### Fixed

- **Native session pre-warming no longer steals keyboard focus, so it is safe
  to enable.** With a reliably pre-warmed session the hidden Xpra transport
  iframe used to connect right after sign-in, and its clipboard autofocus could
  take keyboard ownership from local RAST apps (Terminal opened but ignored
  typing until a click). The desktop now keeps the transport iframe
  **disconnected while a session exists only because of a pre-warm** (no app
  run yet): the viewer is created on the first native application launch, which
  waits bounded (15 s) for the shim to report the transport connected and only
  then calls the launch API. Every launch entry point uses that gate (start
  menu, desktop icons, taskbar, App Center *Open*). If the viewer cannot
  connect in time the launch still proceeds and a clear warning is shown. The
  earlier viewer-blurring patch (`93585ba`) remains reverted: it stopped the
  shim's synthetic key events reaching the Xpra client. The pre-warm benefit
  (~9 s background session start, ~3 s first click-to-window) is kept.
- **Terminal failed after a binary upgrade with "Failed to find executable
  /usr/local/bin/rast (deleted)".** The privileged helper passed
  `std::env::current_exe()` — which the kernel reports with a `" (deleted)"`
  suffix after the on-disk binary has been replaced — to `systemd-run` for the
  terminal and root-file agents. The helper now captures its resolved
  executable at start-up, the child refuses to run when its build differs from
  the helper that spawned it, and `scripts/deploy-release.sh` installs
  versioned binaries behind a stable symlink, restarts the three services
  together, preserves running-session units, smoke-tests Terminal and rolls
  back on failure.
- **Root file agent** (File Station's root file system) had the same stale
  executable path and is fixed by the same resolver.

### Added

- **Discovered-application icons and the "Import existing application"
  dialog.** Applications installed outside App Center now show their own icon:
  the identity-scoped scan (or a manual import) resolves the desktop entry's
  `Icon=` theme name or absolute path only inside known icon roots (identity
  XDG icon directories, system icon themes, a managed profile's `data/icons`),
  stores validated PNG/SVG bytes (≤96 KiB) on the inventory row (migration 17
  adds `working_dir`, `icon_type`, `icon_data` to `discovered_apps`), and the
  authenticated `GET /api/linux-apps/discovered/{id}/icon` serves them with
  cache headers. The URL contains no path and the endpoint only serves stored
  bytes, so no path traversal or symlink escape is possible. Administrators can
  register an already-installed executable from App Center with name, absolute
  path, optional arguments, working directory and icon; an imported working
  directory (or a `.desktop` `Path=`) is honoured when it is inside the managed
  application trees. App Center rows and desktop shortcuts use the icon with
  the existing generic fallback.
- **Discovered applications (installed outside App Center).** RAST now finds
  desktop applications that were not installed through App Center — `.desktop`
  entries in XDG directories, AppImages, portable programs, and applications an
  external installer put inside a managed app profile (the real WordCraft /
  ArtCraft Launcher case). They appear in a new App Center "Discovered
  applications" list and as desktop apps, can be launched in the person's
  shared Xpra session with the same window adoption/limits as managed native
  apps, and are matched to their windows by `StartupWMClass`/`WM_CLASS`. A
  bounded identity-scoped helper operation (`desktop_scan`, run as the person's
  identity) reads 0700 homes the web server cannot; `.desktop` `Exec` lines are
  parsed into argv arrays and never go through a shell. System-wide entries
  require administrator approval by default (`linux_discovery_system_apps`),
  and admins can approve/hide/rename/forget or manually import an executable.
  New settings: `linux_discovery_enabled` (default on),
  `linux_discovery_roots`, `linux_discovery_system_apps` (default `approval`),
  `linux_discovery_interval_seconds` (default 300). New API under
  `/api/linux-apps/discovered` and the `linuxapps.discovered` realtime event;
  schema migration 15 adds the `discovered_apps` inventory and lets
  `linux_runs` reference discovered app ids.
- **Configurable session pre-warming** (`linux_prewarm`, default off, enabled
  in production):
  after a person with native apps signs in the desktop starts their shared Xpra
  session in the background (`POST /api/linux-apps/session/prewarm`), so the
  first application launch is warm (measured cold session start is ~12 s;
  warm launch ~0.8 s). The hidden viewer transport is deferred until the first
  native launch (see Fixed), so enabling pre-warming does not affect keyboard
  focus. One session per person, admission-checked and reclaimed by the normal
  idle cleanup; the trade-off is ~150–250 MiB idle memory per signed-in
  native-app user.
- **Adaptive native session performance profiles.** `linux_profile` selects
  `responsive` (lowest input latency), `balanced` (Xpra adaptive defaults, the
  previous behaviour), `media` (smooth video) or `quality` (sharp text/images);
  `linux_quality`, `linux_min_quality`, `linux_speed`, `linux_min_speed`,
  `linux_video_scaling`, `linux_av_sync` and `linux_encoding` override the
  profile. Applies to every native application without per-app code.
- **GPU/display capability detection, without unverified acceleration
  claims.** `GET /api/linux-apps` and `rast doctor` now report the effective
  profile, display backend, NVIDIA driver/devices, NVENC/NVDEC codec
  availability, DRM render nodes and actionable notes. A validated
  `linux_xvfb_command` wrapper can replace Xvfb for GPU-backed rendering, with
  a safe fallback to software rendering at session start.
- **Native session diagnostics in App Center.** App details show the runtime
  policy and GPU capability; the session API now carries the live cgroup
  memory/task/CPU usage of the person's session unit.

### Fixed

- **Native Linux apps always open as individual RAST windows.** The desktop no
  longer has a fallback that renders the whole Xpra remote desktop (its
  background, Xpra toolbar and remote decorations) inside a RAST window: a
  session that is not seamless is reported instead of embedded. A native app is
  also never downgraded to the legacy whole-session viewer when the native
  backend or helper is unavailable — the launch fails with an actionable
  message, so the RAST window manager stays the only visible window manager.
- **Opening a Linux app before the installed-app list finished loading** no
  longer falls back to the legacy Docker viewer: the list is loaded and the
  launch retried, or the app is reported as not installed.

### Fixed

- **Keyboard focus and clipboard for native apps are now owned by the RAST
  desktop instead of the hidden Xpra viewer.** Keys reach the focused native
  window exactly once (repeat preserved, keyups delivered after focus moves,
  all pressed keys/buttons released on blur/disconnect), macOS Command
  shortcuts are translated for the common editing/shortcut set when a native
  window is focused, and text clipboard works both ways with visible fallbacks
  when the browser refuses clipboard access instead of silently failing. Text
  only: HTML/image clipboard formats and IME composition are documented as
  unsupported by the pinned Xpra HTML5 client.
- **Session cgroup metrics** in the native session API were always null because
  the cgroup path was missing the `.service` suffix.

### Changed

- **The legacy in-window Linux-app viewer is limited to Docker-backed apps.**
  A native app routed to that legacy path re-routes to the shared native
  session instead of mounting a remote desktop inside a RAST window.

## [0.18.3] - 2026-11-16

### Fixed

- **Native Linux apps could not see the person's own files.** Opening an
  app, and moving a Docker app to the native backend, linked and granted read
  access to `homes/rast-u<id>` (named after the app's Linux account) instead
  of the person's home `homes/<username>`. The folder was created empty, so
  apps never saw the person's files. Apps now get the person's own home, as
  `docs/NATIVE-LINUX-APPS.md` describes, and RAST no longer creates
  `homes/rast-u<id>` folders.
- **An app could read another account's home.** Because of the bug above,
  an app of user id 5 was granted read access to the home of an account
  named `rast-u5`, if one existed. New usernames of the form
  `rast-u<number>` are now refused, so such a name cannot be chosen again.

### Changed

- **Opening a native app no longer waits for the home access grant.** The
  recursive read ACL on the person's home now runs in the background, at most
  one at a time per person. Measured on ext4 with a warm cache: about 0.4 s
  for 100,000 files and 1.1 to 2.9 s for 300,000 files, which a blocking
  grant would add to every app start.

### Upgrade notes

- After upgrading, empty `homes/rast-u<id>` folders left by earlier versions
  can be removed by an administrator; RAST does not delete them. An existing
  account whose username has the form `rast-u<number>` keeps working, and
  apps of other users no longer reach its home.
- The first app start after the upgrade grants the app's Linux account
  read access to the person's home (recursive `rx` ACL), as documented. Apps
  still save into their own profile. The grant runs in the background, so on
  the first app start the person's files can become readable a few seconds
  after the app window opens. The helper stops a grant after 13 seconds, so a
  home with more than about 1.5 million files and folders only gets a partial
  grant.
- The grant also adds a default ACL entry for the app's Linux account to
  every folder in the home (`d:u:rast-u<id>:rx`), so files created later are
  readable by the app at once. Files moved into the home (chunked uploads,
  restores from the recycle bin, moves from shared folders) become readable
  at the next app start.

## [0.18.2] - 2026-11-15

### Fixed

- **Composer could not be installed or updated** ("PHP's phar extension is
  missing"). The version check that ends an install runs in a sandbox that
  could reach neither the PHP that App Center installed nor the
  configuration of the distribution's PHP (`/etc/php`, where the phar
  extension is enabled), so Composer started on a PHP without phar. A check
  of an official package now finds the runtimes App Center installed, ahead
  of the system's, exactly as a terminal does, and the system PHP reads its
  configuration. Imported packages still see only their own files.

## [0.18.1] - 2026-11-15

### Fixed

- **An idle Terminal kept reconnecting and repeating its screen.** Behind a
  reverse proxy or tunnel that closes quiet WebSockets (Cloudflare: after 100
  seconds), a terminal nobody typed in was disconnected again and again, and
  every reconnect printed the session's recent output below what was already
  shown, hiding the command being typed. The server now pings the connection
  every 20 seconds so it stays open, and rejoining a session redraws the
  screen instead of adding to it.

## [0.18.0] - 2026-11-15

### Added

- **Three roles: super administrator, administrator and user.** A super
  administrator is the server owner: besides everything an administrator
  does, they get the server itself (the root file system in File Station and
  the root shell in Terminal) and they alone create, change, disable or reset
  administrator accounts and change roles. An administrator runs RAST for its
  people (users, groups, shared folders, apps, backups, settings) without the
  server's own files or shells. Control Panel → Users shows and sets the
  three roles. The first account and `rast user create-admin` accounts are
  super administrators; the last active one cannot be demoted or disabled.
- **Root file system in File Station**, for super administrators. File
  Station now has three places: **Home** (your own storage), **Shared
  folders**, and **Server → Root file system**, the server's own files from
  `/` with full access as root: browse, preview, download, upload (resumable),
  new folder, rename, copy, move and delete. There is no quota and no recycle
  bin there, so a delete is permanent and every change asks first. New files
  get the owner of their folder; a replaced file keeps its owner and mode.
  Changes and downloads are in the activity log. Turn it off for everyone
  with `files_root_enabled = false`.

### Changed

- **Upgrade note (schema v14):** every administrator that exists at the
  upgrade becomes a super administrator, so nobody loses access. Demote the
  accounts that should only run RAST in Control Panel → Users.
- The root (or `terminal_admin_account`) shell in Terminal is for super
  administrators. Administrators without that level get the unprivileged
  identity, like selected users; a root terminal closes when its owner stops
  being a super administrator.
- `@root` is a reserved name for new files and folders in Home and shared
  folders.

### Fixed

- The in-process access cache is keyed per database as well as per account.
  A single server was never affected; the test suite, which runs several
  servers in one process, could read another server's decision (the
  intermittent `role_enforcement_and_user_management` failure).

## [0.17.4] - 2026-11-15

### Fixed

- **A terminal with `terminal_admin_account` did not start** ("the terminal
  unit did not start"): the login account could not enter RAST's terminal
  runtime directory to create the agent socket. The directory is now
  traversable for it, as it already was for ordinary users' identities.
  Broken in 0.17.2 and 0.17.3.

## [0.17.3] - 2026-11-15

### Fixed

- A terminal that runs as a login account (`terminal_admin_account`) gets the
  umask of that account's login session (pam_umask: `UMASK` from `login.defs`
  with `USERGROUPS_ENAB`, `0002` on Debian/Ubuntu) instead of `0022`, so files
  created there have the same permissions as over SSH.

## [0.17.2] - 2026-11-15

### Added

- **`terminal_admin_account`: the administrators' Terminal as a Linux login
  account, exactly like SSH.** Set it to the account you log in to the server
  with (it must be in `sudo`, `admin` or `wheel`) and an administrator's
  terminal is a login shell of that account instead of a root shell: its own
  home, groups, `~/.profile`/`~/.bashrc`, version managers and tool logins
  (rustup's toolchain, `claude` with its sign-in and skills, …), with `sudo`
  for root. Nothing RAST-specific is added to that environment and the
  Terminal runtime selection does not apply to it. A folder the account
  cannot enter (Open in Terminal) starts in its home instead.

## [0.17.1] - 2026-11-15

### Added

- **Root terminals run the tools of the server's administrators.** Commands
  installed for the Linux account of someone who may already become root
  (members of `sudo`, `admin` or `wheel`) — `claude`, `uv`, rustup's `cargo`,
  `opencode`, pnpm, Bun, Deno, a Flutter checkout, … in `~/.local/bin`,
  `~/.cargo/bin` and the other usual per-user directories — are on the `PATH`
  of an administrator's root shell. They come last, so they never shadow a
  system command or an App Center default; directories other accounts can
  write to are left out.
- **App Center finds runtimes installed in an administrator's home**, such as
  rustup's Rust in `~/.cargo/bin`, and lists them as *System* installs with
  the verified version as an update. The privileged helper looks (the service
  cannot read `/home`) and runs version commands as the owning account.
  Flutter is detected too; its version is read from the SDK instead of
  running `flutter`.
- **`terminal_path_extra`**: further directories to append to the `PATH` of
  administrator root shells.

## [0.17.0] - 2026-11-15

### Added

- **Terminals survive a page reload.** A shell now stays open until its tab
  (or the Terminal window) is closed. Reloading the page, closing the browser
  or losing the network only detaches it: the shell and its programs keep
  running, and Terminal shows every running shell again as a tab when it is
  opened next, with recent output replayed. A shell nobody is viewing is
  closed after `terminal_idle_minutes` without output (default now 24 hours;
  it used to be reaped 30 seconds after the page went away). Unattended shells
  are still closed within 30 seconds when their owner is disabled, loses the
  Terminal permission or (root shells) stops being an administrator.
- **App Center detects every official runtime the server already has**, not
  only Go, Node.js, Python and PHP: Rust, Java, Bun, Deno, .NET, Clang,
  Composer, Zig, Kotlin, CMake, Gradle and Maven installed with apt, snap or
  by hand are listed as *System* installs with an update to the verified
  version. With `packages_system_wide`, their commands are linked into
  `/usr/local/bin` as well, so the updated version replaces the old one for
  terminals, `sudo` and services (the apt files are shadowed, never changed).

### Changed

- **Administrator root shells behave like a root login.** They are no longer
  held to the per-terminal memory/CPU/task limits (1 GiB and 512 tasks stopped
  builds and larger tools), and their `PATH` includes `~/.local/bin`, `~/bin`
  and `/snap/bin` like a login shell. Ordinary users keep their limits.
- A system runtime on a release line the catalog no longer carries is offered
  the newest verified version instead of no update, and **Install** no longer
  fails with "No compatible version is available" in that case.

### Fixed

- A system version is recognised for packages with one rolling line
  (`stable`); Composer 2.9 was treated as its own release line with nothing to
  install.
- The version of a system command that cannot run inside the check sandbox
  (Composer needs PHP's extensions, distribution JDK builds such as
  `25.0.5.0.3`) is read from the dpkg database or shortened instead of being
  reported as unknown.
- A terminal window that lost its session to another window no longer detaches
  the new viewer.

## [0.16.2] - 2026-11-15

### Fixed

- **Runtime icons are readable in the dark theme.** The official Simple Icons
  brand marks are monochrome (black); they were drawn almost invisibly on the
  dark App Center tile. Package icons now sit on a light neutral plate, so both
  monochrome SVG marks and coloured PNG icons are legible in light and dark
  themes.

## [0.16.1] - 2026-11-15

### Fixed

- **Catalog: republishing without re-verification works.** In a
  `republish` workflow run (no new versions, verify/php skipped) GitHub unpacks
  the single plan artifact directly instead of into a `plan/` directory;
  `rast catalog collect` now accepts both layouts, so a metadata/icon-only
  republish can assemble and sign the catalog. This is what published the
  runtime icons; no runtime versions changed.

## [0.16.0] - 2026-11-15

### Added

- **Package icons in App Center.** Every official runtime now shows its
  programming-language/runtime brand logo (Python, PHP, Go, Node.js, Rust,
  Eclipse Temurin, Bun, Deno, .NET, Clang/LLVM, Flutter, Composer, Zig, Kotlin,
  CMake, Gradle, Maven) and app-type packages can ship their own icon instead
  of a letter monogram. Package manifests support an optional `icon`
  (`mediaType` `image/svg+xml`/`image/png`, standard base64, decoded image at
  most 64 KiB, optional attribution); RAST validates the encoding, image
  signature and SVG content (no scripts, event handlers, `foreignObject`,
  `javascript:` or external references) on import, install and catalog
  publication, and renders icons through `<img>` so SVG can never execute in
  the page. Package authors can embed an icon with the new
  `rast package embed-icon <image> --manifest rast-package.json` command; the
  signed catalog carries one icon per package, so all versions of a runtime
  show the same logo without duplication. Icons use Simple Icons (CC0) brand
  marks; trademarks belong to their owners and identify the corresponding
  runtime. See [docs/PACKAGE-ICONS.md](docs/PACKAGE-ICONS.md).

## [0.15.2] - 2026-11-15

### Fixed

- **Clang catalog verification note fits the manifest limit.** The 0.15.1
  explanation made the Clang verification text longer than the manifest's
  300-character limit, so publication kept Clang in the failed state. The note
  is now concise (the lldb/libpython caveat stays in the package notes and
  README), so Clang 23.1.5 can be published.

## [0.15.1] - 2026-11-15

### Fixed

- **Clang/LLVM catalog verification no longer executes `lldb`.** The official
  LLVM Linux release links `lldb` against a specific host libpython shared
  object (e.g. `libpython3.14.so.1.0`), which is absent on the catalog CI
  runners even though `clang`, `clang++` and `ld.lld` run correctly. `lldb`
  is still installed and finds `liblldb.so` through `LD_LIBRARY_PATH`, but the
  version check now covers the compilers and linker only; using `lldb`
  requires installing the matching python3 library on the server (documented
  in the package notes and rast-packages README).

## [0.15.0] - 2026-11-15

### Added

- **Managed runtime catalog: eight more verified language/tool packages.**
  App Center now discovers, verifies and publishes, side by side with the
  existing runtimes:
  **Clang/LLVM** (C and C++, clang/clang++/lld/lldb from the official LLVM
  Linux release, GitHub build-provenance attestation verified);
  **Flutter with Dart** (official Google Linux SDK archives; x86_64 host SDK
  only upstream — aarch64 reports an explicit incompatibility);
  **Composer** (PHP dependency manager, verified `composer.phar` plus a
  generated read-only `bin/composer` wrapper, SHA-256 cross-checked with
  getcomposer.org);
  **Zig**; **Kotlin** command-line compiler; **CMake**; **Gradle**; and
  **Apache Maven 3.10**. The .NET SDK entry is now labelled for C#/.NET.
  Integrity is anchored in upstream metadata (GitHub asset digests, official
  JSON indexes, published checksum files; LLVM additionally via its SLSA
  provenance bundle and `gh attestation verify`), and every artifact is
  executed on the native runner during verification. The manifest schema
  gained safe `tar.xz` extraction and verified single-file `phar` packages
  with generated wrappers; existing manifests remain compatible. The 12 GiB
  extraction ceiling was raised to 16 GiB after measuring LLVM's 11.55 GiB
  toolchain, with the installer's free-space checks still bounding installs.
  Database **servers** (MySQL/PostgreSQL) are deliberately not runtime-catalog
  packages: they are daemons/OS packages, not read-only toolchains.
- **Administrators get a real root shell in Terminal (configurable).** With
  `terminal_admin_root = true` (default), an administrator's Terminal runs as
  root with `HOME=/root` and full server/filesystem access — package manager,
  system services and every command are usable — while still living in its own
  transient cgroup with the configured memory/CPU/task limits. Selected
  ordinary users always run as their unprivileged `rast-u<id>` identity;
  administrators only. Set `terminal_admin_root = false` to keep
  administrators unprivileged too. The terminal status bar shows a `root`
  badge for these shells.

### Changed

- The start menu no longer opens when the Cmd/Windows key is pressed on its
  own: it reacted to Cmd before every Cmd+C/Cmd+V/screenshot shortcut, which
  was intrusive. Use **Ctrl+Space**, the taskbar start button or the desktop
  context menu instead. Browser copy/paste and screenshot shortcuts are
  unaffected.

## [0.14.2] - 2026-11-15

### Fixed

- **Catalog: Deno release attestations are confirmed through the GitHub
  attestations API.** `gh attestation verify` reports "no attestations found"
  for Deno's in-toto release predicate even when the GitHub API returns the
  signed bundle for the exact artifact digest (verified against gh 2.104 and
  the API). Deno verification now validates the API response itself: the
  expected release predicate must be present and the artifact's SHA-256 must
  be named in the attestation subjects; the upstream `.sha256sum` and the
  GitHub asset digest remain the integrity anchors. This unblocks publishing
  Deno 2.9.8/2.9.7 instead of leaving them in the failed state.

## [0.14.1] - 2026-11-15

### Fixed

- **Catalog: Deno releases are verified with their actual provenance
  predicate.** `gh attestation verify` enforces the SLSA provenance v1
  predicate by default, while Deno publishes the in-toto release predicate
  (`https://in-toto.io/attestation/release/v0.2`). The Deno provider now asks
  GitHub for that predicate explicitly, so Deno 2.9.8/2.9.7 verification (and
  therefore catalog publication) succeeds instead of staying in the `failed`
  state. Other providers are unchanged.

## [0.14.0] - 2026-11-15

### Added

- **Terminal: a first-class native application with real PTY shells.** Open
  interactive Linux shells from the desktop (multiple tabs and windows) through
  xterm.js over an authenticated, same-origin WebSocket. Shells run directly on
  the server as the mapped unprivileged `rast-u<id>` identity — no Docker, VM,
  Xpra, VNC or graphical session — with a private home, shell history and
  caches. Bash is preferred with an explicit `sh` fallback; resizing sends
  SIGWINCH, Ctrl+C/D/Z and job control work, and the app offers copy, paste,
  search, scrollback, clear, font-size controls, reconnect after network
  interruptions (same shell, no duplicates), and visible connecting/connected/
  disconnected/exited states. File Station gets **Open in Terminal** on folders
  the account may access: the browser passes a RAST folder reference, the
  server resolves it with the storage ACLs, and the helper grants the identity
  the matching OS ACL, so a shell cannot read folders the account cannot open.
  Administrators always receive an unprivileged shell; ordinary accounts are
  denied until an administrator grants the `terminal` application. Sessions are
  bounded (count, memory, CPU, tasks), re-checked for auth/permission every 30
  seconds, and terminate their own process tree only. Requires the native
  helper (`rast-native.service`) and the `acl` package; it does not require the
  native GUI apps feature. New settings: `terminal_enabled`,
  `terminal_max_sessions`, `terminal_grace_seconds`, `terminal_idle_minutes`,
  `terminal_memory_mb`, `terminal_cpus`, `terminal_tasks`; see
  [docs/TERMINAL.md](docs/TERMINAL.md).
- **Terminal runtime environment panel.** New shells receive the PATH and
  runtime variables of the selected managed runtimes (Rust, Temurin JDK, Bun,
  Deno, .NET, Python, PHP, Go, Node.js) without restarting the RAST server.
  Per-user defaults and per-tab overrides are supported; the panel shows
  available versions, executable locations, whether a runtime comes from RAST
  or the operating system, and missing dependencies (e.g. a missing linker for
  Rust or `libicu` for .NET). Shared runtime binaries stay read-only and
  writable caches live in the user's private home. Existing tabs are left
  untouched and the UI says when a tab must be reopened.


- **A clear "Install Linux App" workflow in App Center.** Administrators get a
  prominent primary button (and matching empty-state tile) that opens one
  responsive dialog with *Upload package* and *Install from URL* sources.
  Supported formats are stated in plain language (`.deb` and portable archives;
  AppImage is explicitly not offered until validated). Before installing, the
  dialog shows the server preview: detected name/version, format and source,
  host distribution/architecture and compatibility, dependency and package
  changes, whether the host package is already installed (the action becomes
  "Add existing installation to RAST"), an existing RAST entry, how many saved
  per-user profiles will be preserved, warnings (including Chromium's
  `--password-store=basic` headless-keyring note) and who can use the app (all
  RAST users or administrators only, stored as the app's visibility).
  Installation shows real stages (Uploading → Validating → Installing →
  Registering → Ready), survives closing the dialog/App Center (the job
  continues and progress is reattached when App Center reopens), rejects
  duplicate submissions, only reports Ready after the app row and launcher are
  verified, and offers Try again with the original source on failure. The ready
  view opens the app and points at the existing desktop-shortcut behaviour.
  Uninstall/reinstall keeps per-user profiles unless data deletion is chosen.

- **Native app prerequisites are checked up front.** Xpra 6.5+ is required
  (validated against 6.5.5 with HTML5 client version 21); the installer
  detects an older Xpra and disables native apps instead of failing at launch,
  and `rast doctor` / `rast native-helper --check` report the detected version.
  The installer also installs the `nftables` package used by the display-port
  firewall below.
- **Chromium apps explain their password storage.** The install preview warns
  that Chromium-family apps run with `--password-store=basic` because a
  headless session has no unlocked OS keyring; saved site credentials live in
  the per-person profile (0700) and are not protected by keyring encryption.

- **Managed runtime catalog: Rust, Eclipse Temurin JDK, Bun, Deno and .NET
  SDK.** App Center's signed catalog now discovers, verifies and publishes
  five additional runtimes for `linux-x86_64` and `linux-aarch64`, alongside
  the existing Python, PHP CLI, Go and Node.js entries. Rust is installed as a
  standalone component-merged toolchain (rustc, cargo, rust-std, rustfmt,
  clippy) from the GPG-signed `static.rust-lang.org` stable channel manifest
  (manifest SHA-256 + gpgv with the pinned Rust release-key fingerprint);
  Eclipse Temurin JDKs 8/11/17/21/25/27 come from the Adoptium API with the
  GitHub asset digest and published `.sha256.txt` cross-checked; Bun and Deno
  are verified against their upstream checksum files plus, for Deno, GitHub
  build-provenance attestations; .NET SDK 10.0 LTS and 11.0 STS come from
  Microsoft's release metadata with SHA-512 verification and runtime checks.
  The manifest schema gained safe `zip` support and optional multi-archive
  installs (Rust components merge into one prefix) plus `runtime.env`/
  `runtime.pathDirs` hints; existing manifests stay compatible. Managed
  prefixes are read-only and caches stay per user. .NET/Java/Bun/Deno JITs run
  normally in Terminal shells (no service-wide hardening change; install-time
  checks read only their own `/proc/<pid>` under Landlock).
  Limits: aarch64 artifacts are hash/metadata verified but exercised in CI
  only; publishing the new versions requires a catalog workflow run with the
  `v0.14.0` RAST tool (see rast-packages README).

### Fixed

- **Windows can be resized again.** The resize grab strips were positioned
  outside the window but clipped by the window box's `overflow: hidden`, so the
  bottom-right handle was unreachable when another window covered the corner
  (and keyboard focus handling could swallow Ctrl+N while a native window was
  focused). The window clips its content instead, and the adopted remote window
  follows the RAST resize.
- **Reopening an app right after closing it starts it again.** A launch that
  arrived while the previous process had exited but the 5 s sweeper had not
  noticed yet returned the dead in-memory run; launch now verifies the systemd
  unit first and marks stale runs exited.
- **A minimized native window is iconified in X11.** Minimizing now sends
  `window-unmap` (and restore sends `window-map`) instead of only hiding the
  canvas, so the application stops rendering an invisible window and reports
  the iconic state.
- **Remote window titles are escaped before the pinned Xpra HTML5 client
  renders them** (`jQuery .html(title)` sink), and the RAST desktop renders the
  raw title through text nodes only. A native app or a web page visited in a
  native browser can no longer execute script in RAST's origin through its
  window title.

### Security

- **Loopback display ports are restricted to the RAST service account.**
  Xpra's HTML5 transport listens on `127.0.0.1` with Xpra-level authentication
  disabled, and loopback is not a uid boundary. At startup the privileged
  helper now installs one owned nftables rule (table `inet rast_native`, set
  `xpra_ports`) that rejects connections to the session ports from every uid
  except the RAST server, adds each port before a session is usable, removes it
  when the session stops, and reconciles the set with the live sessions when
  the helper restarts. This is defence in depth behind the authenticated proxy.
  Requires the `nftables` package; `rast doctor` and `rast native-helper
  --check` warn when it is missing.
- **Live display streams revalidate the RAST login session.** A native display
  WebSocket now re-checks the login session, expiry, revocation and account
  state every 5 s and closes when the session is no longer valid, so signing
  out, an administrator revoking the browser sessions or deactivating the
  account ends the live display within seconds. Deactivating an account also
  stops its native session; revoking only the browser sessions keeps the
  person's applications running under the retention policy.

## [0.13.3] - 2026-11-14

Native Linux apps are restored on top of the newer `main` line (the previous
0.11.x release had the App Center install entry temporarily disabled). Schema
v13: the native migration is renumbered after the main line's package app-type
migration; existing databases from either line are reconciled with a backup
during the upgrade.

### Added

- **Install Linux App workflow in App Center** (restored from 0.13.2 and
  re-integrated): administrator-only primary button, Upload package / Install
  from URL dialog, server preview (app/version/format/source, host
  compatibility, package changes, existing host package or RAST entry, saved
  per-user profiles, warnings, availability), real install stages with job
  persistence, duplicate-submission prevention, verified Ready state and
  profile-preserving uninstall/reinstall. See the 0.13.2 notes below for the
  full behaviour.
- The newer main-line features from 0.11.5–0.11.37 are kept: desktop
  multi-selection/group dragging, File Station marquee selection and bulk
  moves, fine-grid layouts, widget sizing, package app-type support and the
  package authoring CLI.

### Fixed

- **Keyboard input survives minimize/restore and title-bar clicks.** Native
  windows keep keyboard focus after being minimized and restored, and clicking
  the application surface moves DOM focus off RAST's title-bar/taskbar
  controls so typing and app shortcuts reach the app. Minimize now hides the
  RAST window instead of issuing Xpra `window-unmap`/`window-map`; the X11
  focus loss that pair caused made a restored app ignore keys until the viewer
  reconnected.
- **No more `getMouse` console error during pointer release.** The RAST bridge
  only forwards a release to Xpra when it has recorded a real adopted pointer
  event, so a focus loss before any app interaction can no longer throw
  `Cannot read properties of undefined (reading 'clientX')` inside the viewer.
- **Window-manager integration keeps both APIs** from the features ported into
  this release (saved window geometry/state and native transient
  dialogs/owner/taskbar flags) when reopening apps after a reload.
- **Native display-port protection survives a helper restart.**
  `rast-native.service` reconciles the nftables `xpra_ports` set with the live
  sessions on startup, but the reconciliation called the gated `allow_port()`
  helper while the rule was still not marked active. It therefore silently
  emptied the set, leaving the Xpra ports of sessions that survived a helper
  restart reachable by every local account (the display port has no
  application-level authentication of its own; the nftables rule is what
  restricts it to the RAST service account). Reconciliation now adds the
  discovered ports directly, and a unit test pins the ordering.
- **No stray loading ring on native windows.** Xpra's per-window connection
  spinner (`.spinneroverlay`) was left in the adopted window's DOM. Without
  the viewer document's stylesheet it was laid out as an in-flow strip, and
  RAST's global `.spinner` styling turned it into a permanently visible,
  animated ring at the top-left of every native window (and its animation
  kept costing paint work). The overlay is now removed on adoption; RAST
  shows its own connection/reconnect state.
- **Clicking inside a native application window works on the integrated main
  line.** Two merge regressions broke mouse input while keyboard input kept
  working. The desktop multi-selection guard inherited from the 0.11 main line
  tested pointer targets with `instanceof Element`; a native window canvas is
  created by the Xpra viewer document and keeps that realm's prototypes, so
  the guard treated the click as a desktop-background click, started a marquee
  gesture and called `preventDefault()` on the pointerdown, which makes the
  browser suppress the `mousedown`/`mouseup` events the application needs.
  In addition, a missing closing brace nested the entire native-app stylesheet
  inside `.package-app-frame`; the off-screen Xpra viewer therefore fell back
  to the browser default 300×150 iframe size, so Xpra's virtual desktop was
  300×150 and server-side pointer coordinates were clamped to it — clicks in
  native windows landed at the wrong place. The element check is now
  realm-independent, the stylesheet is balanced again, and the viewer is sized
  to the browser viewport plus a margin so maximized windows stay inside the
  Xpra desktop. Covered by frontend unit tests (including a stylesheet
  balance guard) and native E2E scenario b (clicking "New Window" opens a
  second window).

## [0.13.2] - 2026-11-13

### Fixed

- **Mouse clicks near a window edge are no longer swallowed.** The window
  resize grab strips now sit outside the window instead of covering the last
  few pixels of the application, so controls at the right edge (Chrome's menu
  and profile buttons) receive clicks. Dragging a tab out of a window still
  releases the remote button when the pointer moves over the desktop.
- **Sound works on sessions created before native audio existed.** A running
  Xpra session keeps its original command line across RAST restarts, so an
  older session stayed silent after the upgrade. On startup RAST now recognises
  such sessions from their process arguments, ends that stale session once and
  starts a fresh audio-enabled one on the next app launch. Apps running in the
  old session are stopped, as with any service upgrade that changes session
  flags.
- **Video playback is smoother.** The reference deployment raises the native
  session to 6 CPUs / 6144 MiB and each application to 4 CPUs / 3072 MiB
  (`linux_session_*`, `linux_app_*`); the previous 2-CPU app cap throttled
  software video decoding and made YouTube playback stutter.

## [0.13.1] - 2026-11-13

First published native-app release since 0.11.4; includes all 0.12.0 and 0.13.0
changes below. Schema v12: back up the database and app profiles before upgrading.
Rolling back to 0.11.4 requires its matching pre-upgrade database backup.

### Added

- Native Linux desktop apps share one isolated Xpra session per RAST account,
  rather than a Docker desktop per app. Top-level app windows, additional Chrome
  windows and dialogs integrate into the RAST desktop. App Center supports native
  package installation, preview, updates and guided legacy-profile migration;
  existing Docker apps are not automatically migrated.
- Playback-only audio forwarding from a private per-user virtual speaker to the
  browser. No host audio device, host microphone or browser microphone is used.
  Xpra audio/GStreamer/PulseAudio packages are required; the installer checks an
  additive package plan before installing dependencies. Click the native window
  to allow sound under browser autoplay policies.

### Fixed

- Pointer motion, clicks and drags use the same remote-window coordinates,
  including moved and scaled windows. Preserve button release outside a canvas
  when dragging a Chrome tab into another window.
- Repeated session snapshots no longer raise Chrome over other apps. Focusing an
  already focused RAST window no longer recursively notifies window subscribers.
- Adopted canvas repaint uses the visible desktop's animation clock rather than
  the hidden viewer's clock, avoiding background-frame throttling.
- Cold-launch recovery, private Chromium D-Bus sessions and initial frame refresh
  from the unpublished 0.13.0 deployment are included.

### Upgrade notes

- Install Xpra 6.5+ with its HTML5/X11/audio components for native apps. Without
  Xpra, legacy Docker apps remain available. Chromium's sandbox stays enabled.
- Use the release installer for upgrades from 0.11.4 so native helper units,
  permissions and audio dependencies are installed. Refresh open desktop tabs to
  load the updated input/audio bridge. Existing graphical sessions keep their
  startup options until ended and opened again; save work before ending a
  session to enable audio.
- Linux CI and release builds cover x86_64 and aarch64; macOS CI is temporarily
  disabled. This release does not provide a macOS server binary.

## [0.13.0] - 2026-11-13

Schema v12 (`migrations/0012_native_linux_apps.sql`). The new native backend is
the default for newly installed desktop apps when Xpra is installed; existing
Docker-backed apps keep working and can be migrated from App Center.

### Added

- **Linux apps run natively.** Newly installed desktop applications (`.deb`,
  AppImage, portable archive) no longer run inside a per-app Docker image and
  streamed desktop; they are installed on the server and started as native
  processes under the person's own unprivileged Linux identity. Each RAST user
  gets one lazily started Xpra seamless session shared by all their
  applications, and every native top-level window (including extra Chrome
  windows and dialogs) is a RAST-managed window with its own taskbar entry,
  title, focus, resize, minimize and close behaviour. Cold launch is much
  faster and memory no longer duplicates a streamed desktop per application.
  Includes: an installation preview for `.deb` packages (package identity,
  version, architecture, dependency transaction with removals/conflicts),
  Update from file/URL for existing apps, staged portable updates with a
  recoverable previous version, safe archive validation (no path traversal,
  links, special files or excessive expansion), AppImage extraction, package
  ownership tracking so uninstall respects shared/independently installed
  software, systemd/cgroup resource limits per session and per application,
  memory-pressure-based admission, ACL-based access to the person's RAST home,
  and a guided Docker->native migration of existing apps that keeps the app id,
  shortcuts and profiles and retains the old data until cleanup.
- Privileged native helper service (`rast-native.service`) and
  `rast-linux.slice`; `rast doctor` checks Xpra, the helper socket, the slice
  and the uid range.
- New configuration keys `native_enabled`, `native_helper_socket`,
  `linux_xpra_bin`, `linux_uid_base`, `linux_idle_minutes`,
  `linux_session_memory_mb/cpus/tasks`, `linux_app_memory_mb/cpus/tasks` and
  `linux_admission_reserve_mb` (see docs/CONFIGURATION.md).

### Fixed

- Native windows now reliably appear after a launch initiated from the desktop
  UI (the user-visible "Google Chrome did not open in time" case): the shell
  re-reads the shim window snapshot once per second (a session created by the
  launch could publish its state before the shell attached its listeners),
  reloads only the embedded viewer once if a launched app still has no window
  after eight seconds (the viewer could connect before the Xpra web server was
  ready), pulls the first frame right after adopting a window (no more
  momentarily black bodies), and the first-launch window timeout is 240 s
  instead of 30 s. Chromium/Electron apps run through `dbus-run-session`, so
  they no longer stall for minutes retrying D-Bus calls during a cold start.
- CI: the macOS job is disabled for now (the native Linux back-end and its
  tests are Linux-only) and two native unit tests no longer depend on the CI
  runner's binaries (`/usr/bin/xpra` is provided by the test fixture;
  `/usr/bin/firefox` on Ubuntu runners is a snap wrapper).
- Migration of a Docker app to the native backend works on a real host: the
  Docker worker may write legacy volume exports under
  `<data_dir>/linux-apps/migrations` (its sandbox previously returned EROFS),
  legacy data is imported into the identity-owned profile by the privileged
  helper (`profile_install`, with a server-writable staging tree), and the
  adopted app gets the configured native memory/CPU limits instead of the old
  Docker values (which made the first launch fail validation).
- The first painting of a native window now always fills its RAST window; a
  freshly started browser window that initially matched the Xpra virtual screen
  could keep its old scale and show only in the top-left corner until resized.
- `rast doctor` no longer warns that an unprivileged run "must be root": the
  root requirement belongs to `rast-native.service`, whose state is checked
  separately.
- Native Chromium launches no longer hang or delegate to a dead instance when a
  previous browser process left `Singleton*` files in its profile: the
  privileged helper removes those stale files (narrowly scoped to the person's
  profile, only Chromium/Electron singleton names) before starting a browser,
  and the launcher adds only the ordinary first-run flags. The Chromium sandbox
  stays enabled; `--no-sandbox` is never used.
- Native apps receive the X11 auth file the running Xvfb actually uses (it may
  be `$HOME/.Xauthority` or Xpra's runtime fallback), discovered by the helper
  from the session process tree, and the session manager recreates a session
  whose identity home had to be repaired instead of leaving stale windows.
- `rast-native.service` and the installer now enforce the canonical native
  directory modes (`linux-apps` 0751, `apps` 0755, `staging` 0750,
  `profiles`/`linux-users` 0711, per-user homes 0700) and repair a data
  directory that lacks the traverse bit, so app processes can chdir into their
  profiles on a fresh host.

### Changed

- The legacy Docker backend remains for apps installed before this release
  (`backend = "docker"`) and can be enabled for new installs with
  `native_enabled = false`; legacy apps work unchanged until an administrator
  migrates them.
- The unconditional 4 GiB free-memory requirement for starting a Linux app was
  replaced by memory-pressure-based admission with a configurable reserve.
- Chromium sandbox: native Chrome/Chromium launches keep the browser sandbox
  enabled and run without root; the legacy image's `--no-sandbox` is gone.

### Planned

- Project-key release signatures (minisign/cosign) in addition to the keyless
  build-provenance attestations; the installer already supports fail-closed
  verification via `RAST_REQUIRE_SIGNATURE`.
- Provenance enforcement inside `rast update` (today: SHA-256 + version probe;
  attestations can be verified manually or with the installer).
- Automated restore drills and a documented JSON schema for backup manifests.
- Static musl builds are **not** planned for 0.1.x (see
  [docs/COMPATIBILITY.md](docs/COMPATIBILITY.md)).

## [0.11.37] - 2026-11-14

No database migration (schema stays v12).

### Fixed

- Desktop multi-selection now drives actions: dragging a selected item moves the
  entire mixed shortcut/widget selection (single-item drag handlers suppressed),
  the click after a group drag cannot collapse it, right-click menus are built
  from the selection (bulk remove/pin/unpin, selection count), Delete/Backspace
  removes every selected desktop item with one Undo, and widget/shortcut
  layouts are saved together.

## [0.11.36] - 2026-11-14

No database migration (schema stays v12).

### Added

- Desktop group dragging for the marquee selection: dragging a selected
  shortcut or widget moves the whole mixed selection, snapped and clamped as
  a group, collision-checked against unselected items, with a live preview;
  Escape/cancel restores everything, and the widget + shortcut layouts are
  saved in one appearance request.
- File Station marquee selection in grid and list views (Shift adds,
  Ctrl/Cmd/Shift-click retained, auto-scroll near pane edges) and dragging a
  multi-selection onto a folder to move it (descendants deduplicated, moving a
  folder into itself rejected, destination highlight, existing job/progress
  UI and realtime refresh).

## [0.11.35] - 2026-11-14

No database migration (schema stays v12).

### Added

- Desktop marquee multi-selection: drag a rectangle from empty desktop space in
  any direction to select shortcuts and widgets, Ctrl/Cmd-click toggles,
  Shift adds, Ctrl/Cmd+A selects when the desktop has focus, Escape cancels or
  clears, and empty clicks clear. Shared selection geometry/auto-scroll helpers
  and tests were added for re-use by File Station.

## [0.11.34] - 2026-11-14

No database migration (schema stays v12).

### Fixed

- Fine-grid shortcut moves did not stick: saving used the previous display
  rectangle instead of the new pixel fields. Pixel positions are now the
  source of truth for save/render, so Alt+Arrow and drags persist.

## [0.11.33] - 2026-11-14

No database migration (schema stays v12).

### Changed

- Desktop application shortcuts now use the same **8px fine-grid** as widgets:
  fixed shortcut dimensions (labels included), free 8px movement, collision
  against widgets and other shortcuts, snapped drag preview + grid guides,
  boundary clamping, and per-account persistence of pixel positions
  (`gridVersion: 2`). Legacy row/column layouts migrate once preserving their
  visual position; arrow-key selection is kept, Alt+Arrow moves by 8px
  (Shift: 64px), and Applications drags/new shortcuts use fine placement.

## [0.11.32] - 2026-11-14

No database migration (schema stays v12).

### Changed

- Desktop widgets store **fixed CSS-pixel preferred sizes and positions**
  (`gridVersion: 2`) instead of viewport-proportional fractions. Resizing the
  browser or using a larger monitor now provides more grid space, it no longer
  enlarges widgets, charts, text or gaps. Legacy fractional layouts are
  migrated once to conservative per-widget defaults; the pixel fields are
  authoritative afterwards. Widget sizes only change through an explicit
  resize. Content-aware defaults/minima per widget, a fixed sparkline height,
  compact presentation when the viewport is smaller, and app-icon collision
  with widgets are included.

## [0.11.31] - 2026-11-14

No database migration (schema stays v12).

### Changed

- Desktop layout now uses a shared 8px snap grid, a 16px safe inset and the
  usable area above the taskbar for icons and widgets. Widget positions and
  **sizes** snap to the grid (resize handle, minimum 200×200), stay inside the
  usable desktop, avoid app icons and each other, show a subtle 8px grid with
  a snapped preview while dragging/resizing, and repaired saved layouts are
  persisted. Default widgets sit in a right-aligned two-column group with
  16px gaps. Widget sizes are stored with the existing appearance API.

## [0.11.30] - 2026-11-14

No database migration (schema stays v12).

### Fixed

- The widget layer had a 12 px top/left inset while the app icons start at the
  desktop origin, so widget cells were offset by half an icon cell: the space
  between two widgets never lined up with a 1×1 app slot. The layer now shares
  the icon grid origin, so a free column between widgets is exactly one app
  cell and an app icon fits there.

## [0.11.29] - 2026-11-14

No database migration (schema stays v12).

### Changed

- Every widget is now the **same-size tile**: two app-icon columns wide and
  three icon rows tall, with the footer pinned to the bottom. Storage, System
  Health, CPU & Memory and Network no longer have different heights, so a set
  of four widgets forms an even 2×2 block.

## [0.11.28] - 2026-11-14

No database migration (schema stays v12).

### Fixed

- Hovering a widget showed its action buttons inline and clipped the title
  again. The buttons are now an overlay that replaces the status pill, so the
  title always fits on the narrow app-grid tiles.

## [0.11.27] - 2026-11-14

No database migration (schema stays v12).

### Fixed

- The narrower (two app-column) widget tiles clipped their titles. The
  action buttons only take space while hovered/focused, and the title/pill
  font sizes were reduced, so "System Health" and "CPU & Memory" fit fully.

## [0.11.26] - 2026-11-14

No database migration (schema stays v12).

### Changed

- Widgets use the **same desktop grid as application icons**: two app columns
  wide, snapped to icon cells. A widget can no longer be dropped on an app
  icon or on another widget — the drop preview turns red and the tile moves to
  the nearest free grid slot. Existing/saved layouts are also nudged off
  occupied cells when the desktop changes.

## [0.11.25] - 2026-11-14

No database migration (schema stays v12).

### Changed

- Dragging a widget now snaps it to a 2×2-tile grid and shows a dashed
  **drop preview** of the exact slot it will be clipped into before you
  release the pointer (arrow-key moves snap to the same grid).

## [0.11.24] - 2026-11-14

No database migration (schema stays v12).

### Fixed

- Widgets could feel impossible to move: only the title bar started a drag,
  and the collision reflow immediately re-arranged a tile dropped over
  another. The whole tile is now a drag handle (except its action buttons),
  and the column reflow runs only when the desktop size actually changes, so
  a placement the user chooses sticks.

## [0.11.23] - 2026-11-14

No database migration (schema stays v12).

### Changed

- Widgets are fixed-size tiles (280 px, roughly a 2×2 block of desktop apps)
  instead of scaling with the monitor, and their type sizes are fixed too, so
  text and titles are no longer shrunk or clipped on smaller/larger screens.
  When the saved arrangement would overlap, they still reflow into columns.

## [0.11.22] - 2026-11-14

No database migration (schema stays v12).

### Fixed

- The boot splash (the RAST logo while the page loads) painted the built-in
  alpine wallpaper even when the account uses a custom wallpaper. The last
  applied wallpaper is cached locally and a tiny same-origin script paints it
  on the splash before the app bundle loads, so a refresh no longer flashes
  the default background.

## [0.11.21] - 2026-11-14

No database migration (schema stays v12).

### Added

- **Workspace restore on refresh.** The open windows (including Linux apps
  such as Google Chrome, and package apps), their position, size, maximized/
  minimized state and the focused window are saved in `sessionStorage` on
  page hide/refresh and reopened after the page reloads. A different account
  signing in never inherits another account's windows.

### Changed

- Widgets no longer grow to 1.5× on very large monitors; the cap is 1.1× so
  they stay proportionate at 1440p/4K.
- The account's appearance is loaded before the desktop is painted, and
  appearance requests bypass the browser cache, so a refresh reliably shows
  the saved custom wallpaper instead of the previous one.

## [0.11.20] - 2026-11-14

No database migration (schema stays v12).

### Fixed

- Widget layout on a smaller monitor: the saved fractional positions could
  make cards overlap when the screen shrank. On collision they now reflow into
  non-overlapping, right-aligned columns (order preserved), and the minimum
  scale was raised to 0.9 so they stay readable. A layout that does not
  collide keeps the account's exact arrangement.

## [0.11.19] - 2026-11-13

No database migration (schema stays v12).

### Fixed

- Widget scaling from 0.11.18 did not apply: the card's own `--widget-scale: 1`
  default shadowed the layer value. Cards now inherit the computed scale.

## [0.11.18] - 2026-11-13

No database migration (schema stays v12).

### Changed

- Desktop resource widgets scale with the desktop size (0.8×–1.5×) and keep
  their relative positions, so switching to a smaller or larger monitor leaves
  them readable and inside the screen instead of cramped or tiny.

## [0.11.17] - 2026-11-13

No database migration (schema stays v12).

### Fixed

- Reader view forwards ordinary GET form submissions (search boxes on Google,
  Brave, Wikipedia, …) back through the proxy, so their in-page search buttons
  work instead of doing nothing. POST forms remain blocked.

## [0.11.16] - 2026-11-13

No database migration (schema stays v12).

### Changed

- Web app packages may embed external `https:` pages in their window
  (`frame-src https:`), so the Browser app can offer a direct compatibility
  view for sites that allow framing. Their documents still cannot run in the
  RAST origin (the iframe is sandboxed).

## [0.11.15] - 2026-11-13

No database migration (schema stays v12).

### Changed

- Browser page proxy lets page scripts run again, with compatibility shims for
  cookies and local/session storage inside the sandboxed frame. Stripping
  scripts (0.11.14) kept server-rendered results visible but made normal
  JavaScript sites, including single-page apps, render blank.

## [0.11.14] - 2026-11-13

No database migration (schema stays v12).

### Fixed

- Browser page proxy strips page scripts and `javascript:` links. Remote pages
  are server-rendered documents; running their scripts in the sandbox made
  some (Brave Search) hide their whole content when storage access was denied.
  The injected link-interception script is added after stripping.

## [0.11.13] - 2026-11-13

No database migration (schema stays v12).

### Fixed

- Browser page proxy: Google search URLs that answer with Google's
  JavaScript-only anti-bot page are transparently retried on Brave Search
  (plain HTML results), so searching keeps working from a datacenter IP.
- Failed page fetches now return a readable HTML error page inside the
  Browser window instead of a JSON API error.

## [0.11.12] - 2026-11-13

No database migration (schema stays v12).

### Added

- **Installed app packages are first-class applications.** App-type packages
  from App Center now appear in the **Applications** folder next to the
  built-in apps, can be dragged to the desktop or pinned to the taskbar, and
  open through the normal app window path. The list refreshes on the
  `package.changed` event; web apps keep their in-window UI, command apps
  keep the argument/output window.

## [0.11.11] - 2026-11-13

No database migration (schema stays v12).

### Added

- **Web viewer proxy for the Browser app.** `GET /api/browser/page?url=…`
  fetches one public HTTPS HTML page for a signed-in account: HTTPS on port
  443 only, a pinned DNS resolver that rejects any private/loopback/
  link-local/multicast address (no DNS rebinding), manually re-validated
  redirects, 4 MiB / timeout limits, no cookies. The returned document gets a
  `<base>` tag and link interception and is framed `SAMEORIGIN` with a
  restrictive CSP, so the Browser package can offer address bar,
  back/forward, reload and home without exposing the RAST origin. Sites that
  require cookies, logins or heavy SPA JavaScript may not work.

## [0.11.10] - 2026-11-13

No database migration (schema stays v12).

### Fixed

- Web app packages can be framed by the RAST desktop: their files are served
  with `X-Frame-Options: SAMEORIGIN` and `frame-ancestors 'self'` instead of
  inheriting the API's `DENY`, so the App Center application window shows the
  calculator page instead of an empty frame.

## [0.11.9] - 2026-11-13

No database migration (schema stays v12). Docker-only 0.11 line.

### Added

- **Web app packages.** App-type manifests may declare
  `"app": {"kind": "web", "entry": "index.html"}`; App Center's Open button
  then renders the package's `app/` page in a desktop window (files served by
  RAST only to signed-in accounts, path-safe, `nosniff` and a restrictive
  CSP). The calculator package uses this for its full scientific keypad while
  its Rust CLI stays available in the terminal.

## [0.11.8] - 2026-11-13

Schema v12 (`migrations/0012_packages_app_type.sql`): `packages.type` may be
`runtime` or `app`. Rolling back to 0.11.4 requires its matching pre-upgrade
database backup.

### Fixed

- Installing an app-type package over an existing runtime package of the same
  id updates the stored package type, so App Center moves it from Runtimes to
  Apps after the update.

## [0.11.7] - 2026-11-13

No database migration (schema stays v11). Docker-only 0.11 line.

### Added

- **App packages from GitHub.** Package Center accepts manifests with
  `type: "app"`; installed app packages appear under **Apps** (not Runtimes)
  with an **Open** button. Open runs the app's command in a desktop window
  (arguments from the text field are passed as argv, never through a shell;
  10 s timeout, output capped). The Rust calculator package uses this to be a
  real App Center app.

## [0.11.6] - 2026-11-13

No database migration (schema stays v11). Docker-only 0.11 line.

### Added

- **The whole catalog pipeline is now Rust.** `rast catalog` implements
  `previous`, `discover`, `interim`, `verify`, `php-result`, `assemble`,
  `sign`, `check`, `php-inputs`, `php-publish`, `collect`, `sequence` and
  `build-php`, replacing `scripts/catalog.py`, `scripts/rastpkg.py` and
  `scripts/build-php.sh` in the official catalog repository. Catalog
  manifests are validated with RAST's own validator, Ed25519 signing and
  verification use `ring` (no openssl), artifacts are extracted with the same
  safe extractor the installer uses, and the PHP build orchestration moved
  from shell into the binary. The package-template and catalog repositories
  now run registered RAST release binaries only — no Python at all.

## [0.11.5] - 2026-11-13

No database migration (schema stays v11). This is the Docker-only 0.11 line:
the native Linux apps backend is not part of this build.

### Added

- `rast package` authoring commands used by the package repositories, all in
  Rust and sharing RAST's own rules: `manifest` (write `rast-package.json` for
  a release tag from `dist/`), `validate` (schema plus local archive size and
  SHA-256), `smoke` (extract like the installer and run the runtime checks) and
  `check-release` (the same digest/asset cross-check as App Center → Install
  from GitHub). The `rast-package-template` and `rast-packages` repositories no
  longer need `scripts/package.py` / `scripts/rastpkg.py`.

### Changed

- App Center no longer offers **Add app** (the toolbar button, empty-state
  tile and drag-and-drop installer): adding Linux apps is disabled for now,
  while installed apps/runtimes remain manageable.

## [0.11.4] - 2026-11-13

No database migration (schema stays v11). Apps installed before this release
get the window arrangement after *Reinstall* (the app's page, More actions);
the container setting applies from the next start.

### Fixed

- **A second app window is no longer stacked full-size on the main one.** The
  app's main window keeps filling the RAST window, and windows the app opens
  later (a tab dragged out of Chrome, a second document) float on top at a
  readable size, cascaded, so both are visible; the window bar lists them.
  Dialogs and popups keep the size the app gives them.

## [0.11.3] - 2026-11-13

No database migration (schema stays v11). Apps installed before this release
get the window guard after *Reinstall* (the app's page, More actions); the
browser preferences are applied at every start.

### Fixed

- **Linux app windows fill their RAST window again.** A window that the app
  restored smaller, or that was dragged inside the picture, could leave the
  desktop visible around it, and Chrome could also restore an old window
  position. A small guard in the app image now maximizes every app window that
  is not maximized within a second (dialogs and popups keep the size the app
  gave them), and Chromium browsers forget their saved window position.

## [0.11.2] - 2026-11-13

No database migration (schema stays v11). Chrome installs made before this
release get the single window frame after *Reinstall* (the app's page, More
actions); the setting itself is applied every time the app starts.

### Fixed

- **One window frame for Linux apps, and the window can be moved again.**
  0.11.1 hid the desktop title bar of streamed app windows, which left no way
  to move, minimize or maximize them from the desktop. The desktop title bar
  is back and the app image now turns off the browser's own window bar
  (Chrome's *Use system title bar and borders* preference), so a Chrome window
  shows one set of controls: RAST's. Closing the app from inside still closes
  the window (0.11.0). Chromium browsers other than Chrome are covered too;
  apps that draw their own title bar keep it.

## [0.11.1] - 2026-11-12

No database migration (schema stays v11).

### Changed

- **Linux app windows no longer show a second title bar.** A streamed Linux
  app draws its own window controls (Chrome's own minimize/maximize/close), so
  the desktop hides its title bar for those windows: one frame, and the app's
  own close button closes the window. The taskbar still minimizes, maximizes
  and closes the window and the edges still resize it; on phones the desktop
  title bar stays because nothing else would grab the window.

## [0.11.0] - 2026-11-12

Database migration 0011 (schema v11): `linux_apps` gains a `visibility`
column. It applies automatically on the next start; apps installed before this
release are shared with every signed-in user.

### Added

- **Linux apps for every signed-in user.** Installed Linux apps now appear on
  each account's desktop (and in the Applications folder and taskbar); every
  user opens their own copy with their own settings, sign-ins and windows.
  Installing, updating, removing, limiting and changing who can use an app stay
  administrator actions. A new **Who can use it** setting (App Center → the
  app's page → More actions) can restrict an app to administrators; shared is
  the default.

### Changed

- **Closing a Linux app from inside it closes its RAST window.** When the app
  quits from its own close button (for example Chrome's ×) the RAST window now
  closes instead of staying blank while the server notices; RAST checks whether
  the app is still running as soon as its picture stops rather than waiting for
  the periodic sweep. Reloading the page reconnects to the same copy.

## [0.10.3] - 2026-11-12

No database migration (schema stays v10). Apps installed before this release
get the window bar and the settings fix after *Reinstall* (the app's page,
More actions), or after uninstalling and adding them again.

### Added

- **Several windows in one Linux app.** When an app opens a second window
  (another browser window, a profile chooser) or one is minimized, a slim
  window bar appears at the bottom of its RAST window to switch between them.
  With a single window the app fills the whole RAST window as before.
- **Reinstall** on a Linux app's page rebuilds it from its official source.

### Fixed

- Linux apps can be pinned to the taskbar and placed on the desktop: the
  browser dropped them from the saved layout.
- A Linux app whose saved settings belonged to another system account quit
  right after starting ("WebSocket disconnected"). RAST now fixes the owner
  when the app starts.
- Behind nginx, Linux app windows need the WebSocket upgrade headers; see
  [docs/OPERATIONS.md](docs/OPERATIONS.md#linux-apps).

## [0.10.2] - 2026-11-12

No database migration (schema stays v10).

### Added

- **Add app: choose a file, drop it, or paste a link.** App Center's *Add
  Linux app* button is now *Add app*. Its dialog takes a `.deb`, `.AppImage`
  or `.tar.gz` file chosen from your device, dropped anywhere while it is
  open, picked from File Station, or downloaded by the server from an
  `https://` link (so the file never has to be on your device). Links into
  the server's own network are refused.

## [0.10.1] - 2026-11-12

No database migration (schema stays v10).

### Added

- **Drop a file on App Center to install a Linux app.** Dragging a `.deb`, an
  AppImage or a `.tar.gz` archive from your computer onto the App Center
  window uploads it to your `Downloads` folder and installs it.

## [0.10.0] - 2026-11-12

Linux apps: administrators can install desktop Linux apps (`.deb`, AppImage,
portable archive) from App Center and use them in a RAST window. Database
migration 0010 (schema v10).

### Added

- **Linux apps (administrators).** App Center can install desktop Linux apps
  from a file in File Station: a `.deb` package, an AppImage or a portable
  `.tar.gz` archive (*Add Linux app*, or *Install as Linux app* on the file in
  File Station). Each app gets a desktop icon and opens in its own RAST window.
  Every person who opens it gets their own copy on the server, with their RAST
  home folder as the app's files (downloads go to `Downloads`). A copy stops
  when its window closes or after 30 minutes without use. By default a copy may
  use 4 GB of memory and 6 CPU cores (changeable per app), and a new copy only
  starts while the server has 4 GB free. RAST checks the app's official source
  daily (its apt repository, GitHub releases or the AppImage's update server)
  and offers *Update* when there is a newer version. Needs Docker on a Linux
  server; see [docs/OPERATIONS.md](docs/OPERATIONS.md#linux-apps). Database
  migration 0010 (schema v10).

## [0.9.1] - 2026-11-12

Fixes "Could not list images" after updating to 0.9.0: `rast update` now
restarts the Docker helper too and RAST reports a helper that runs an older
version. Also a preview of RAST on macOS (built from source; the release
binaries stay Linux-only) and a symlink fix for aarch64 Linux. No database
migration (schema stays v9).

### Added

- **macOS preview.** RAST builds and runs on Apple Silicon Macs from source.
  `scripts/run-local.sh` starts it as your user, with everything in one
  folder and the web desktop on `127.0.0.1` only. On the first run it prints a
  setup token. With Docker Desktop running, the Docker app works too. CI now
  lints, tests and starts RAST on macOS (see docs/COMPATIBILITY.md).

### Changed

- Package version checks need Linux (seccomp and Landlock). On other systems
  RAST refuses to run them instead of running package code unconfined, so the
  App Center installs no packages there.

### Fixed

- **Docker: "Could not list images" after updating to 0.9.0.** `rast update`
  restarted only `rast.service`, so the Docker helper (`rast-worker.service`)
  kept running the old binary. That binary did not know the image and volume
  actions and answered with a reply the server could not match ("worker
  response id does not match the request"). The updater now restarts the
  helper too. The helper reports its RAST version: `rast doctor` and the
  Docker app say when it needs a restart. A request the helper cannot read
  now gets a reason instead of the id error.
- On aarch64 Linux, File Station now refuses symlinks when it opens a file
  (downloads, copies, wallpapers). The `O_NOFOLLOW` value was hard-coded for
  x86_64, and on aarch64 that value means `O_LARGEFILE`. The inode check after
  opening already stopped symlink swaps there.

## [0.9.0] - 2026-11-12

Docker works like Docker Desktop: every container with logs, configuration,
live usage and a terminal; images (pull, run, clean up) and volumes. Package
Center is now App Center and Container Manager is now Docker. No database
migration (schema stays v9).

### Added

- **Docker works like Docker Desktop.** A sidebar with Containers, Images and
  Volumes. Containers lists every container on the server, grouped by Compose
  project (RAST apps first), with status, CPU, memory, port links and
  start/stop/restart. Each container has a page with live logs (search, follow,
  stderr marked), its configuration (environment values hidden until shown)
  and live usage charts.
- Images: list with size and usage, pull from Docker Hub or a registry (runs as
  a background job), delete unused images, and clean up untagged ones.
- **Run an image.** The Run button on an image opens a short form: name, the
  ports to publish (prefilled with free host ports), volumes for the image's
  data folders and environment variables. RAST turns it into a Compose file
  and runs it as a normal app, so it can be stopped, redeployed and backed up.
  Environment values stay out of the Compose file (secrets stay encrypted).
- Volumes: list with the containers using each one; delete unused volumes
  after typing the name.
- **Terminal into containers.** A Terminal tab on the container page opens a
  shell (bash, or sh) in the running container, with colours, resizing,
  Ctrl+C and copy (Ctrl+Shift+C). Terminals are refused for containers that
  would give access to the server itself (privileged, host processes or
  network, devices, extra capabilities, system folders or the Docker socket
  mounted), and the page says why. Openings are audited; signing out closes
  open terminals, and a terminal idle for 60 minutes closes itself.
- Containers RAST does not manage can be deleted (type the name to confirm;
  their volumes always stay) or turned into a RAST app from their menu.
  Containers and volumes of a RAST app stay managed through the app.
- New admin-only endpoints under `/api/docker/containers/{id}` (including the
  `/terminal` WebSocket), `/api/docker/images*`, `/api/docker/volumes*` and
  `/api/docker/stats`, backed by new whitelisted worker operations; every
  change is audited.

### Changed

- Package Center is now called **App Center**, and Container Manager is now
  called **Docker**. Only the displayed names change: app ids (`packages`,
  `applications`), API paths, group rules, desktop layouts and pins stay.
- Starting, stopping and restarting a RAST app no longer asks for
  confirmation (like containers); redeploy and delete still do.

## [0.8.0] - 2026-11-12

Movable and removable desktop icons, a new Applications folder, and RAST's
own apps in Package Center (Container Manager and Backup Center can be
uninstalled). Database migration to schema v9.

### Added

- **Movable desktop icons.** Drag an icon to any spot on the desktop; it
  snaps to a grid, and dropping it on another icon swaps the two. Drop an icon
  on the taskbar to pin it. Alt+arrow keys move the selected icon one cell.
- **Remove from desktop.** Right-click an icon (or press Delete) to take it
  off the desktop, with Undo. The desktop menu has **Reset icon layout**.
  Each account keeps its own layout (`desktopIcons` in `/api/appearance`).
- **Applications folder.** A new desktop app, **Applications**, lists every
  app the account may open, with search. Drag an app from it to the desktop
  or taskbar, or use its menu to add or remove the desktop icon and taskbar
  pin.
- **Apps in Package Center.** Package Center lists RAST's own apps next to
  the runtimes. Built-in apps (File Station, Control Panel, Package Center,
  Applications, Resource Monitor, Activity Logs) are always installed.
  Container Manager and Backup Center can be uninstalled and installed again
  (`POST /api/packages/apps/{id}/install`,
  `DELETE /api/packages/apps/{id}?confirm={id}`, audited as
  `package.app_install` / `package.app_uninstall`). Uninstalling hides the app
  and closes its API for every account, administrators included. Its data
  stays: containers keep running and scheduled backups keep their schedule.

### Changed

- The Docker app is now called **Container Manager**. Its app id
  (`applications`), API paths and group rules are unchanged.
- `GET /api/auth/me` reports `installed` for each app. An uninstalled app is
  hidden from the desktop, start menu and taskbar, and its open windows close.

### Migration

- Schema version 9 adds `user_appearance.desktop_icons`. An older binary
  refuses to start on this database; restore the pre-update backup to go back.

## [0.7.0] - 2026-11-12

Uninstall and Cancel in Package Center, optional system-wide defaults for
`sudo` and services, and Package Center management for granted groups. No
database migration (schema stays v8).

### Added

- Package Center: **Uninstall** on a package page removes every installed
  version and its commands (`DELETE /api/packages/{id}?confirm={id}`, audited
  as `package.uninstall`); the system's own runtime is used again.
- Package Center: **Cancel** next to the progress bar (tile and package page)
  stops an install or update while it downloads, extracts or runs its checks;
  nothing is changed. From "Moving into place" the install finishes.
- `packages_system_wide` (off by default): the official defaults are also
  linked into `/usr/local/bin`, so `sudo`, systemd services and scripts run
  the version chosen in Package Center, not only login shells. A root systemd
  path unit (`rast-packages-links.path`, installed by `sudo rast packages path
  sync`) keeps the links current; apt files are never changed and a manual
  install is moved aside and put back. New `rast packages path links`.

### Changed

- A group granted Package Center now manages packages (install, update,
  default, remove, uninstall, cancel, consumers) instead of only viewing;
  approving a new GitHub source stays administrator-only. Package job events
  reach every account allowed to open Package Center.

### Fixed

- Any RAST command or a restart reset the data directory to `0750`, so other
  users lost access to the Package Center runtimes until the next
  `rast packages path sync`.
- Queued package jobs showed no progress bar after a reload.

## [0.6.1] - 2026-11-12

A calmer Package Center, and the package catalog is checked after every
start. No database migration (schema stays v8).

### Changed

- Package Center has a calmer layout. Packages are shown as tiles: a large
  icon, the name, one version line and one button (**Install**, **Update**,
  or **Installed**). Clicking a tile opens the package page. The page has the
  status, one main button with a menu for the other actions, the
  description, an *Other information* grid and the versions on the server
  with **Make default** and **Remove**. Paths, checks, consumers and job
  history are folded away. Catalog, server and terminal PATH details moved
  to **Settings**. The refresh button in the toolbar checks for updates.
- When the catalog this server has lists no versions, Package Center says so
  and offers **Check for updates**, instead of showing packages without an
  install button.

### Fixed

- RAST checks the package catalog shortly after every start. Before, a
  restart (for example `rast update`) kept the cached catalog until the check
  interval (6 hours by default) ran out, so new versions did not appear.

## [0.6.0] - 2026-11-12

Package Center works with the runtimes the server already has and puts its
versions on the terminal PATH. No database migration (schema stays v8).
After updating from 0.5.0, run `sudo rast packages path sync` once: the 0.5.0
updater does not know this step (later updates run it themselves).

### Added

- Package Center lists the runtimes the server already has (for example PHP
  and Python from apt, or Node.js installed by hand) as installed *System*
  versions. It reads each version by running the command, confined, and names
  the apt package. When the signed catalog has a newer version of the same
  release line, the card offers **Update**. The update installs the new
  version next to the system one, which stays untouched, so services such as
  PHP-FPM keep working. **Use the system version** switches back.
- Package Center versions work in a terminal. `/etc/profile.d/rast-packages.sh`
  puts the default of each package first on every login shell's PATH, and the
  installed runtimes are readable by all users. The data directory gets only
  the traverse bit for others; the database, keys and backups stay private.
  Controlled by `packages_shell_path` (default on). `sudo rast packages path
  sync` applies it, and the installer and `rast update` run that command.
- The official catalog tracks Node.js 22 (maintenance LTS).

### Fixed

- A failed package catalog check is now logged and retried after 5, 10, 20,
  40… minutes (capped at the check interval) instead of waiting a full interval,
  so a catalog published after RAST started is picked up within minutes.

## [0.5.0] - 2026-11-11

Package Center: verified runtimes installed side by side. Upgrade with a
database backup: migration `0008` (schema v8) is not reversible by an older
binary. Package Center needs the signed catalog published by
[Enricko/rast-packages](https://github.com/Enricko/rast-packages); until it
exists the application reports that the catalog could not be verified. See
[docs/package-center-handoff.md](docs/package-center-handoff.md).

### Added

- **Package Center** (administrators; a group `allow` grant gives read-only
  access): install **Python, PHP CLI, Go and Node.js** from a signed catalog,
  several versions side by side under `<data_dir>/packages/runtimes/<id>/<version>/`,
  with a selectable RAST default (`runtimes/<id>/current` and shims in
  `packages/bin/`). RAST never changes the system `PATH` or the operating
  system's own runtimes.
- Staged installs: download over HTTPS from the catalog's allowed hosts, size
  and SHA-256 check, safe extraction (no absolute paths, `..`, devices or
  escaping links), version checks before and after the move; a failed install
  or update leaves the installed versions untouched.
- Updates install next to the current version and keep it for rollback
  (*Make default*). *New upstream version detected* (still being verified by
  the catalog workflow) is distinguished from *Update ready to install*.
  Automatic checks (`packages_check_interval_minutes`, default 6 h) plus
  *Check updates*; a failed check marks the catalog stale and keeps the last
  verified one.
- Catalog security: Ed25519 signature, anti-rollback by sequence (also for
  the cached copy), expiry (an expired catalog is refused and drives no
  installs); extra keys for your own mirror (`packages_catalog_keys`).
- Archive limits: GNU long-name/PAX records over 64 KiB are refused before
  they are read into memory, and downloads and extraction must leave 512 MiB
  of free disk space. Reinstalling a version keeps the old files until the new
  ones are recorded (restored after a crash); a failed *Make default* is
  rolled back.
- Consumers: record what uses a runtime, follow the default or pin a version;
  pinned and default versions cannot be removed; removal requires typing the
  version.
- **Install from GitHub**: inspect a repository or release link, review the
  preview, approve the source and install. Imports are pinned to the inspected
  release and digests, bound to one repository and publisher (checked again
  when the install starts and when it is recorded), cannot take an official
  package id or command name, and are validated like official packages.
  Importing needs a verified catalog and a kernel with Landlock. Starter:
  [Enricko/rast-package-template](https://github.com/Enricko/rast-package-template).
- Version checks run in a sandbox: own process group (killed on timeout and
  after exit), seccomp (no sockets, io_uring or ptrace), Landlock (read only
  the package and system libraries, write only a scratch home, no TCP), no
  core dumps, a file size limit. A third-party package still runs its author's
  code on the server; the import preview says so.
- Live job progress and live catalog/package updates in every open window;
  every action is audited (`package.*`).
- API: `/api/packages…` (see [docs/API.md](docs/API.md)); manifest schema
  [docs/rast-package.schema.json](docs/rast-package.schema.json).
- Configuration: `packages_catalog_url`, `packages_catalog_keys`,
  `packages_check_interval_minutes`, `packages_github_token`
  (`RAST_PACKAGES_*`).

### Fixed

- Relative times no longer read "0 min ago" between 45 and 59 seconds.

## [0.4.1] - 2026-11-11

Taskbar and widget defaults, and a complete Add user dialog. Upgrade with a
database backup: migration `0007` is not reversible by an older binary (see
"Upgrade and rollback").

### Added

- **Add user** sets up the whole account at once: username, display name,
  role, quota, initial password, whether the account is active, whether the
  password must be changed at the first sign-in (on by default), group
  membership (searchable picker), and direct shared-folder and application
  rules. Section headers count the selected groups and rules; choosing the
  Administrator role explains that administrators always have full access.
- `POST /api/users` accepts `active`, `mustChangePassword`, `groupIds`,
  `folderRules` and `appRules`. They are validated before anything is written
  and saved in the account's transaction, so an invalid choice creates no
  account and a failure never leaves a half-configured one.

### Changed

- **Taskbar**: pinned applications keep their place. Opening a pinned
  application shows its window in the pinned slot instead of moving it to the
  end; windows of unpinned applications follow the pins in opening order.
- **Taskbar pins are saved per account** (`taskbarPins` in
  `GET`/`PUT /api/appearance`) instead of per browser, so they follow the
  account to any browser and are no longer shared by accounts using the same
  browser. Every account starts with File Station, Resource Monitor and
  Control Panel pinned and can pin or unpin any application.
- **Default widgets**: accounts that never set up widgets see System Health and
  CPU & Memory (when they may use Resource Monitor). Changing or removing them
  saves the account's own layout; removing both is remembered.
- Widgets keep a small margin from the screen edges and the taskbar.
- Taskbar buttons show the application's name only ("Control Panel", not
  "Control Panel — Profile"); the full window title remains in the tooltip.

### Fixed

- A storage quota of 0 (unlimited) was rejected with "Must be greater than
  zero" in Add user, in Edit user (so an unlimited account's changes could not
  be saved) and in Settings → default quota, although each field says 0 means
  unlimited.
- The Add user and Edit user dialogs showed their fields in one column at any
  size; they now use two columns and stack only when the dialog is narrow.
  Rule selects are wide enough for "Read & Write".
- `rast update` reported `database migration failed: migration N was
  previously applied but is missing` after a successful update that added a
  migration: the previous binary opened the database with its own migrations
  to report the schema. It now reads the recorded schema version without
  migrating. (An update *from* 0.4.0 still runs the 0.4.0 updater and can show
  the message once; verify with `rast doctor`.)

### Upgrade and rollback

- Migration `0007_desktop_defaults.sql` adds `user_appearance.widgets_customized`
  (accounts that already saved widgets count as customised) and
  `user_appearance.taskbar_pins` (default `["files","monitor","controlpanel"]`).
  Pins previously kept in a browser's local storage are not imported.
- Migrations are forward-only: the 0.4.0 binary refuses to start on a
  schema-v7 database. Roll back by stopping the service, restoring the
  pre-upgrade database backup and then installing 0.4.0. See
  `docs/OPERATIONS.md`.

## [0.4.0] - 2026-11-11

Desktop reliability: live shared-folder updates across accounts, desktop
resource widgets, per-account appearance that cannot be overwritten, and
layouts that follow the window size. Upgrade with a database backup: migration
`0006` is not reversible by an older binary (see "Upgrade and rollback").

### Security

- **Private wallpapers** were cached by the browser for a year
  (`max-age=31536000, immutable`), so the next account signing in to the same
  browser could see the previous account's uploaded wallpaper. Uploads are now
  served `private, no-cache` and revalidated with the current session; other
  accounts get `404`.
- **Access-decision cache**: a decision read just before a policy change could
  be cached as current and keep a revoked grant working until the next change.
  Decisions now carry the policy generation observed before their rules were
  read.

### Added

- **Desktop widgets** (right-click the desktop → Widgets…): System Health,
  CPU & Memory, Storage and Network. They use the Resource Monitor data and
  access check, show loading/live/stale/unavailable/no-access/error states,
  can be dragged or moved with the arrow keys, and their layout is saved per
  account (`widgets` in `GET`/`PUT /api/appearance`).

### Fixed

- Shared-folder changes (upload, rename, move, copy, delete) now reach every
  account that can read the folder, including administrators, without a
  reload; the event stream survives server restarts (fresh cursor per
  connection) and restarts no longer hang on open streams.
- A failed appearance load, implicit saves, late responses after an account
  switch or concurrent partial saves could overwrite an account's theme,
  accent or wallpaper. Saves are now explicit and partial, scoped to the
  account and serialised on the server.
- Taskbar: labels were unreadable on hover (a pinned app took the page text
  colour) and the accent never reached the taskbar; it now follows the
  selected mode and accent in every state with themed tooltips.
- Appearance: disabled Save/Discard looked hovered; disabled buttons are now
  flat and neutral, Save is the primary action, and saving keeps focus.
- Narrow windows on wide screens kept desktop layouts (words broke into single
  letters, values were clipped). Window and dialog contents now respond to
  their own size; the Control Panel uses an icon rail in narrow windows.

### Upgrade and rollback

- Migration `0006_appearance_widgets.sql` adds `user_appearance.widgets`
  (JSON array, default `[]`); existing accounts start without widgets.
- Migrations are forward-only: the 0.3.0 binary refuses to start on a
  schema-v6 database. Roll back by stopping the service, restoring the
  pre-upgrade database backup and then installing 0.3.0. See
  `docs/OPERATIONS.md`.

## [0.3.0] - 2026-11-11

Security audit and hardening of the v0.2.0 access policy, plus usability work
for large installations. Upgrade with a database backup: migration `0005` is
not reversible by an older binary (see "Upgrade and rollback").

### Security

- **Backup Center**: archive downloads and the restore guide are
  administrator-only. A read-only Backup Center grant previously allowed
  downloading archives that contain the account database, the server master
  key and (with the users scope) every user's files. Read-only viewers now see
  status metadata only (`downloadable: false`); downloads are audited.
- **Applications**: read-only grants no longer receive environment values,
  container logs (REST or live stream), deployment-history details or Docker
  reasons with host paths; they get `envCount`/`secretCount` and status.
- **Shared-folder rules** reference the stable share id instead of the display
  name (migration `0005`). Renames no longer rewrite rules (the previous
  rewrite could silently drop a conflicting rule); deleting a share cascades.
- **No implicit owner access**: v0.2.0 granted full access to a shared
  folder's creator when no rule applied, so a demoted former administrator kept
  it. Access now comes only from rules or legacy memberships (as in v0.1.x).
- **Resource Monitor** container names/ids are limited to administrators and
  accounts allowed to use Applications (REST and live samples, re-evaluated per
  sample); overlay/container-runtime mounts are hidden from the disk list.
- **Live log streams** revalidate the session and administrator status before
  every poll and end on revocation, demotion or deactivation.
- **Forced password changes** are enforced by the server, not only the UI.
- **Release provenance**: binaries and `SHA256SUMS` carry GitHub/Sigstore
  build-provenance attestations, verified in the release workflow; both
  architectures are smoke-tested at runtime on native runners; the installer
  can enforce provenance with `--verify-attestation`.

### Added

- Paginated, server-searched member pickers that keep selections across
  searches and pages and show deactivated members for review/removal
  (`GET /api/groups/{id}/members`, `GET /api/users?active=`).
- Effective Access search, level filter and pagination with conflict
  explanations from the evaluator; group-deletion impact preview per member
  (`GET /api/groups/{id}/impact`).
- Clearer separation of Group Membership, Direct Overrides and Effective Access
  in the editors.

### Fixed

- Restore guide extracted user files into `/`; it now restores into the
  storage roots. `rast restore` and `rast serve` use a data-directory lock
  (`rast.lock`) instead of `systemctl is-active rast.service`, so a restore can
  never replace the database under a live server, and two servers cannot share
  a data directory.
- Failed saves (e.g. duplicate group name) were silently swallowed by the
  Control Panel save workflow; the server message now stays visible.
- Account search ignored one- and two-character queries.
- Effective Access labelled folders without any grant as "legacy membership".
- Text contrast: light `--text-faint` and disabled primary buttons were below
  WCAG AA; all shared tokens and control states now meet 4.5:1 for small text
  in light and dark themes with every accent preset.

### Upgrade and rollback

- Migration `0005_share_rule_ids.sql` rebuilds the two folder-rule tables keyed
  by share id. Effective permissions are identical before and after for every
  (account, folder); rules whose share no longer existed (they had no effect)
  are dropped and counted in `settings` (`migration.0005.orphaned_rules`,
  `migration.0005.conflicts`).
- Migrations are forward-only: an older binary refuses to start on a newer
  database. Roll back by stopping the service, restoring the pre-upgrade
  database backup and then installing the older binary. Binaries older than
  0.2.0 do not evaluate group rules at all, so rolling back below 0.2.0 widens
  access. See `docs/OPERATIONS.md`.

## [0.2.0] - 2026-11-11

NAS-style **Users & Groups** with an enforced access policy.

### Added

- Control Panel → **Users & Groups**: Users tab (search, group membership chips,
  direct overrides, Effective Access explanation) and Groups tab (create,
  search, edit, delete groups with General / Members / Shared Folders /
  Applications sections and a single save action). Deleting a group reports how
  many users and rules it affects and keeps users, shared folders and files.
- Group policy for shared folders (`Inherit`, `No Access`, `Read Only`,
  `Read & Write`) and applications (`Inherit`, `No Access`, `Allow`) for File
  Station, Applications, Resource Monitor, Backup Center, Activity Logs and
  Control Panel. Ten catalogued app ids; group grants for Applications/Backup are
  read-only, and management/deployment actions stay administrator-only.
- One permission evaluator used by API enforcement, storage, SSE filtering, the
  launcher permissions in `/api/auth/me` and the Effective Access view:
  explicit No Access wins, otherwise strongest grant, otherwise the legacy
  `share_members` ACL/owner fallback (so upgrades preserve effective access).
  Personal homes keep ownership rules; admins bypass folder policy.
- Per-user direct rules (explicit grant/deny) for conflict handling, provenance
  in Effective Access ("read through Project Viewers, write through Project
  Editors"), and live `group.changed`/`access.changed` events (open windows,
  launcher and sessions reauthorize without a refresh).
- Audit events for group/membership/rule changes and the last-active-admin
  safeguard now runs inside the same SQLite transaction as account/role
  changes.

### Changed

- File Station, search, previews, downloads and mutations now resolve shared
  folders through the evaluator; background copy/move/delete jobs re-check
  permissions when they start.
- Applications/Backup read endpoints accept administrators or read-only group
  grants; mutations remain admin-only.
- SSE subscriptions are filtered by the current application policy.

### Database

- Additive migration `0004_groups.sql` (new tables/indexes only); legacy ACL
  rows and appearance data are preserved. Production upgrades take a database
  backup before applying the migration.

## [0.1.8] - 2026-11-11

Follow-up layout polish from screenshot review.

### Fixed

- File Station footer item counter no longer truncates to “6 ite…” next to the
  pager (it keeps its intrinsic width).

## [0.1.7] - 2026-11-11

Follow-up polish found while capturing the release screenshots.

### Fixed

- **File Station toolbar and pagination layout regressed under the new form
  system.** The canonical `.input`/`.select` rules set `width: 100%`, which also
  captured toolbar search fields and the page-size selector: the search box
  wrapped onto its own row, view toggles dropped below it and the pager summary
  was squeezed to “3 ite…”. Toolbar-scoped fields now keep their intrinsic
  width, File Station search is a fixed flexible column and the pagination
  selector keeps its natural width. Same guard covers the logs/control-panel/
  user toolbars.
- **File Station footer counter was truncated** (“6 ite…”) next to the pager;
  the selection counter no longer shrinks below its content.
- **`rast version | head -n 1` could panic with a broken pipe** (exit 134) on
  slower release runners, failing the aarch64 release job. The banner is now
  written once and stdout errors are ignored, so shell pipelines are safe.

## [0.1.6] - 2026-11-11

A coherence and live-updates release: one control system, ten accent presets and
automatic synchronization for every window.

### Added

- **Ten accent presets** (Ocean Blue, Forest Green, Teal, Sage, Amber,
  Terracotta, Rose, Plum, Indigo, Graphite) with coordinated light/dark values,
  readable foregrounds, hover/pressed/focus/selection tokens and a small live
  preview. Available in Control Panel → Appearance and the desktop Personalize
  dialog; Ocean Blue is the default and settings persist per user.
- **Shared realtime layer**: one authenticated SSE connection per browser tab
  with sequence numbers, gap detection, heartbeats, exponential backoff with
  jitter, online/visibility/wake handling and `resync` snapshots. A
  subtle Live/Reconnecting/Offline indicator shows the real state and last
  successful update.
- **Live data everywhere**: File Station (own operations and external filesystem
  changes via a bounded watcher, quota/usage), Applications (deployment
  progress, container state, bounded live log stream), Resource Monitor,
  Backup Center (queued/running/completed/failed jobs and history), Activity
  Logs (new audit events), Control Panel (users, settings, shares, components)
  and the desktop (notifications, appearance).
- **Cross-session appearance sync**: changes saved in one session are applied in
  the user's other sessions; a session editing appearance gets a conflict
  notice instead of losing its draft.
- Server-side event authorization: scoped events (`user`, `admin`, `users`,
  broadcast), per-connection session revalidation (expiry, revocation,
  deactivation, role change), and no credentials in event URLs.

### Changed

- **One control system** (`styles/controls.css` + `styles/tokens.css`): shared
  default/hover/pressed/selected/focus-visible/disabled/loading states for
  buttons, icon buttons, window controls, segmented controls, inputs, selects,
  checkboxes, radios, switches, uploads, tabs, menus, context menus, tables,
  dialogs, toasts, tooltips, scrollbars and chart accents.
- **Neutral semantic light/dark palette** with solid application surfaces,
  readable text over any wallpaper, thin borders, restrained accents and soft
  shadows. Main surfaces stay neutral; status colours keep their meaning.
- **Appearance save workflow**: Save is disabled with “All changes saved” when
  the form is clean, enabled when dirty, shows Saving… and Saved states, inline
  validation and a retry path on request failure. Preview applies instantly to
  every open window; Cancel restores the saved appearance.
- Routine Refresh buttons were removed where realtime synchronization replaced
  them (File Station, Backup Center, Activity Logs, storage/components tabs);
  contextual Retry/Reconnect remains for genuine failures.
- Resource Monitor continues to use one shared backend sampler (default ~5 s);
  live updates add one SSE connection per tab instead of polling loops per menu.

### Fixed

- **Theme mode buttons no longer use browser-default styling.** The old
  hand-built Light/Dark/System buttons lacked the shared `.segment` class, so
  the browser painted its default `2px outset` border and grey fill. The mode
  and accent selectors now use the shared accessible segmented control and
  swatch components; a defensive reset prevents any unclassed button in a
  segmented group from leaking UA styles.
- Unselected theme/accent options are no longer styled as disabled; selected
  state is communicated by accent surface plus `aria-checked`.
- Legacy pastel/blue-tinted hardcoded control colours were replaced with
  semantic tokens across shell and applications, including Control Panel
  navigation, menus, tables, toasts and overlays.
- `appearance.changed` events update the user's other sessions, and audit
  events are published for authorized subscribers only.

## [0.1.5] - 2026-11-10

Appearance polish found while verifying v0.1.4 on a live deployment.

### Fixed

- **A freshly uploaded wallpaper did not appear in the Personalize dialog until
  it was reopened.** The editor refreshed its draft copy of the wallpaper list
  separately from the shared appearance service, so only the built-in gallery
  updated. The upload list now merges the service result immediately, so the new
  thumbnail is selectable (and previewed) at once.

## [0.1.4] - 2026-11-10

A visual and personalisation release: the desktop now follows a calm,
NAS-inspired visual language and every account can choose its own wallpaper and
theme, stored on the server.

### Added

- **Neutral NAS-inspired visual theme.** White windows and panels, `#F5F6F8`
  toolbars, `#DDE1E6` borders, dark neutral text (`#27313D` / `#657180`) and a
  single blue accent (`#2785D8`). The charcoal taskbar (`#303740`) keeps the
  desktop readable on any wallpaper; the light theme is now the default and the
  dark theme uses matching neutral charcoal surfaces. Oversized glows,
  decorative gradients, heavy blur and the background grid pattern are gone.
- **Original bundled nature wallpapers.** Alpine Dawn, Forest Mist, Coastal
  Calm and Desert Dusk (1920×1080 JPEG, CC0-1.0, generated for the project; see
  [docs/WALLPAPERS.md](docs/WALLPAPERS.md)).
- **Per-user appearance settings stored in SQLite.** Wallpaper source (bundled
  scene, solid colour or private upload), fit (cover/contain/center), position,
  darkening overlay and theme are saved per account and survive restarts,
  sign-out/sign-in and browser changes. See `GET/PUT /api/appearance`.
- **Personalize dialog** (right-click the desktop → *Personalize…*, start menu,
  account menu or Control Panel → **Appearance**): live preview, Apply, Cancel
  and Reset to default, with the previous look restored when cancelling.
- **Private wallpaper uploads.** JPEG/PNG/WebP up to 10 MiB, validated by
  content (magic bytes + dimensions) and served only to the owner with
  immutable private caching; server-generated identifiers, per-user storage and
  ownership checks on read/delete. Limits: 20 images / 100 MiB per account.
- **Coloured application icons** with one consistent icon language: amber folder
  for File Station, blue grid for Applications, teal monitor, warm orange for
  Backup Center, slate gear for Control Panel and a document for Activity Logs.
- Documentation screenshots under `docs/screenshots/`.

### Changed

- Theme and wallpaper settings are consolidated into one server-backed
  appearance system; the previous browser-only wallpaper picker and the
  duplicate Appearance card in Profile are gone.
- Desktop icon labels wrap between words instead of truncating mid-word, and
  the "No applications open" card is smaller, calmer and no longer dominates
  the wallpaper.
- `GET /api/bootstrap` now reports whether the session cookie is valid, so a
  signed-out boot no longer probes a protected endpoint (and no longer logs a
  spurious 401 in the browser console).
- Additive database migration `0002_appearance.sql` (new tables only); existing
  installations upgrade automatically on start.

### Fixed

- Personalisation cache is cleared on sign-out and session expiry so a previous
  account's wallpaper never appears for the next user on a shared browser.

## [0.1.3] - 2026-11-10

Self-update fix found by running the real update flow on a live installation.

### Fixed

- **`rast update` failed with `downloaded binary is not executable`.** The
  release asset is downloaded to a temporary directory with mode `0644`; the
  version self-check and the atomic installation step then tried to execute it.
  The artifact is now made executable (`0755`) immediately after its checksum is
  verified, before it is probed or installed.

## [0.1.2] - 2026-11-10

Deployment fix found while verifying managed application deployment on a real
installation. No schema or API changes.

### Fixed

- **Managed applications could not be deployed when the privileged worker ran as
  its own service account.** The Applications API wrote `compose.yaml`/`.env`
  with owner-only permissions (`0600`) and the app directory `0700`, but the
  Docker worker runs as `rast-worker` (group `rast`) and therefore could not read
  the definitions; validation failed with `application '<slug>' has no
  compose.yaml`. Definitions are now written `0640` inside a `0750` directory so
  the worker can validate and deploy them without granting write access.
  (Deploying on the same account as the web server — the earlier verification
  setup — accidentally hid this.)

## [0.1.1] - 2026-11-10

Maintenance release: two production-deployment fixes found while verifying the
v0.1.0 installation on a real host. No schema, API or data changes.

### Fixed

- **`rast update` ownership of pre-update backups.** Because the update command
  runs as root, the database snapshot (and any SQLite sidecar files it touched)
  could end up owned by root inside the service data directory. They are now
  handed back to the account that owns the data directory, so retention pruning
  and later writes by `rast.service` keep working.
- **`rast doctor` false alarms on a healthy installation.** The listen-address
  check reported a failure when the running service legitimately held the port,
  and the Docker check probed the unprivileged Docker CLI instead of the
  privileged `rast-worker.service`/socket that the web server actually uses.
  Both checks now describe the real state (and `doctor` exits 0 on a healthy
  deployment).

## [0.1.0] - 2026-11-10

Initial public release. RAST replaces a pile of ad-hoc scripts and web tools
with one self-hosted binary: a desktop-like web UI, a REST API, a CLI and
hardened systemd units for a home or small-office Linux server. Includes the
File Station, Applications (Docker Compose), Resource Monitor, Control Panel,
Backup Center and Activity Logs apps, all backed by the same SQLite database
and embedded frontend — no runtime Node.js, database server or external web
server required.

### Added

- **Single binary distribution.** Axum HTTP server, SQLite metadata store and
  the built frontend (TypeScript + Vite, embedded via `rust-embed` with a strict
  CSP) in one `rast` executable for Linux x86_64 and aarch64, plus hardened
  `rast.service` and `rast-worker.service` systemd units.
- **CLI**: `rast serve [--bind]`, `rast status [--json]`, `rast doctor`,
  `rast user create-admin`, `rast user list`, `rast user reset-password`,
  `rast setup-token`, `rast backup [--scope …] [--out DIR] [--note TEXT]`,
  `rast restore --from FILE [--dry-run] [--yes]`, `rast update [--version X]
  [--check]`, `rast version`, and the internal `rast worker`.
- **Authentication and accounts**: Argon2id password hashing (19 MiB, 2
  iterations, 1 lane), password policy (minimum 10 characters), opaque
  server-side sessions stored only as SHA-256 hashes with sliding expiry and a
  30-day absolute cap, per-session revocation and "sign out everywhere", CSRF
  tokens bound to the session, `HttpOnly`/`SameSite=Lax` cookies with automatic
  `Secure` when `public_url` is HTTPS, login rate limiting (per IP and per
  account), persistent lockout after repeated failures, no default credentials
  and no public registration. First administrator via CLI or a one-shot
  `rast setup-token` (60 minutes, single use).
- **File Station**: home and shared-folder roots with breadcrumbs, list/grid
  views, sorting, pagination and bounded search; streaming uploads (single
  request and resumable chunked upload via init/chunk/complete) and streaming
  downloads with `Range` support; basic operations (mkdir, rename, copy, move,
  delete); sandboxed previews for images, text and PDF with a restrictive CSP;
  transactional quota accounting (`used`/`reserved` bytes per user, `0` =
  unlimited); recycle bin with restore, permanent delete and empty; shared
  folders with per-user `read`/`write`/`admin` ACLs; usage statistics; every
  mutation audited.
- **Applications (Docker Compose)**: deploy, start, stop, restart, redeploy,
  inspect logs and deployment history for managed apps; compose validation
  (bind mounts restricted to the app data directory, host namespaces, devices
  and privileged mode refused) and encrypted per-app environment secrets that
  are masked in API responses; detection of unmanaged containers with an
  explicit import flow (never silently adopted); delete with a deliberate
  keep-data vs remove-volumes choice. All Docker access goes through
  `rast-worker.service` over a Unix socket; the web server never touches
  `/var/run/docker.sock` and the worker runs only whitelisted
  `docker`/`docker compose` operations with structured arguments, no shell, and
  per-operation timeouts.
- **Resource Monitor**: one process-wide sampler for CPU/load, memory/swap,
  uptime, disks, network throughput and (when Docker is available) container
  stats; bounded in-memory history; live updates over Server-Sent Events with
  stale/unavailable states instead of falling over.
- **Backup Center**: self-contained `.tar.gz` archives with a checksummed
  manifest and `db` (consistent `VACUUM INTO` snapshot), `config` (including
  `secret.key`), `users` and `apps` scopes; verification, download, deletion;
  hourly/daily/weekly schedules evaluated in the server's local timezone with
  configurable retention (default schedule disabled: daily 03:00, keep 7);
  integrity-checked restore that refuses to run while the service is active and
  keeps a pre-restore copy of the database.
- **Control Panel and Activity Logs**: profile, password change, active
  sessions, theme; user management with roles and quotas (with safety checks
  around the last administrator); shared-folder management; storage and
  component health; audit-log browsing with filters, pagination, row details
  and CSV export.
- **Web desktop**: window manager, taskbar, start menu, desktop icons, light
  and dark themes, keyboard navigation and accessibility labels, responsive
  mobile layout, toasts, session-expiry handling that preserves context;
  real-time updates over SSE without unbounded DOM growth.
- **Operations**: `rast update` verifies release `SHA256SUMS`, checks the
  downloaded binary version, takes a database backup, replaces the binary
  atomically, restarts the service and rolls the binary back if health checks
  fail; migrations are compiled into the binary and applied at startup
  (forward-only), with the schema version reported by `rast status`/`doctor`.
- **Packaging and deployment**: POSIX-sh installer usable as
  `curl … | sudo sh` (architecture/OS/systemd detection, SHA-256 verification,
  optional fail-closed minisign/cosign hook, idempotent upgrades preserving
  `/etc/rast`, `/var/lib/rast` and `/srv/rast`, optional Docker worker setup,
  peer-uid restriction on the worker socket) and a safe uninstaller
  (data-preserving by default, typed confirmation for `--purge`); fully
  documented configuration template; nginx, Caddy and Cloudflare Tunnel
  examples; compatibility matrix; security policy; third-party license
  inventory; `rust-toolchain.toml` pinning the build toolchain.
- **CI/CD and testing**: `cargo fmt`, strict `cargo clippy -D warnings` and the
  full test suite (199 unit/integration tests covering safe-path escapes,
  quota races, ACL enforcement, trash restore, worker protocol validation,
  authentication flows and API end-to-end paths), frontend type-check/build
  with Node.js + npm, native x86_64/aarch64 release builds, ShellCheck/YAML and
  installer-asset sync checks, secret scanning, dependency audit reporting, and
  a tag-driven release workflow that publishes `rast-linux-x86_64`,
  `rast-linux-aarch64`, `SHA256SUMS`, `install.sh`, `uninstall.sh` and
  `rast.toml.example` with release notes extracted from this file. A
  clean-environment installer test (`scripts/test-installer-clean.sh`, 64
  checks) installs, upgrades and uninstalls RAST inside a disposable systemd
  container built from the official Ubuntu 24.04 image.

### Security

- Unprivileged HTTP server (loopback bind by default, forwarded headers trusted
  only from configured proxy CIDRs); Docker/root operations delegated to a
  separate worker account over a Unix socket (mode `0660`, group `rast`,
  optional peer-uid allowlist).
- Strict systemd sandboxing for both units: `ProtectSystem=strict` with explicit
  `ReadWritePaths`, `ProtectHome`, `PrivateTmp`, `PrivateDevices`, empty
  capability bounding sets, `NoNewPrivileges`, syscall-group filtering, memory
  limits and `UMask=0027`.
- Per-user filesystem isolation: every path resolved and canonicalised under
  the caller's root, with traversal and symlink-escape rejection before any
  filesystem call; previews of user-supplied active content are served with a
  sandboxing CSP and never execute in the application origin.
- Secrets: master key at `<data_dir>/secret.key` (0600) with derived keys for
  CSRF and AES-256-GCM encryption of application secrets at rest; session
  tokens hashed in the database; release archives verified against
  `SHA256SUMS` by both the installer and `rast update`.
- Installer and uninstaller never modify firewall, DNS, TLS or reverse-proxy
  configuration, and require explicit confirmation before deleting any data.

### Known limitations

- **Release signing is not enabled yet.** Integrity relies on HTTPS plus the
  published `SHA256SUMS`; the installer's fail-closed `RAST_REQUIRE_SIGNATURE`
  (minisign/cosign) path exists for operators who manage their own keys.
- **aarch64 is build-only.** The official `rast-linux-aarch64` binary is built
  on a native arm64 runner, but runtime testing on arm64 hardware is still
  pending; x86_64 is the tested platform. The arm64 build also needs glibc
  >= 2.39 (built on Ubuntu 24.04), while x86_64 needs glibc >= 2.35.
- **glibc + systemd only.** No musl/static build is published, and the
  installer/service units assume systemd. Other init systems are unsupported
  (the binary can run under a custom supervisor, but that is not a supported
  configuration).
- **Docker app path is not exercised against a live daemon in CI.** The worker
  protocol, path/slug/compose validation and Docker-manager error paths are
  unit-tested with a fake worker socket; a live-Docker end-to-end test is not
  part of CI (the clean-environment installer test installs the worker only
  when Docker is present).
- **Proxy request-body limits still apply.** The File Station supports chunked
  resumable uploads for environments with per-request limits (e.g. Cloudflare
  plans cap request bodies), but a proxy's per-request limit is outside RAST's
  control and the chunk size must be configured below it; direct large uploads
  are also bounded by `RAST_MAX_UPLOAD_BYTES`.
- **Not implemented yet:** two-factor authentication, passkey/WebAuthn login,
  remote audit-log forwarding/SIEM integration, and cryptographically chained
  (tamper-evident) audit records. Audit logs are append-only by convention and
  can be altered by an administrator with database access.
- **Known dependency advisory.** `RUSTSEC-2026-0459` (`libsqlite3-sys` 0.37.0,
  CVE-2026-119375) affects the bundled SQLite WAL implementation; the patched
  `>=0.39.0` is blocked by `sqlx 0.9.0`'s dependency constraint and needs an
  upstream update. CI ignores exactly this advisory (any other advisory fails
  the build) and the exposure/limits are documented in
  [SECURITY.md](SECURITY.md#known-dependency-advisory-010).
- Frontend build requires Node.js >= 22 and npm (build-time only); the
  installed server needs neither.

### Changed

- **Session pre-warming is enabled in production** and safe again: the hidden
  Xpra transport connects lazily on the first native launch, so a pre-warmed
  session has no iframe that could steal keyboard focus. Measured: session ready
  ~2.0 s after desktop load; Terminal typing works without clicking while a
  pre-warmed GPU session exists (`tests/e2e/results/prewarm-reenabled-413ad2c.md`).
