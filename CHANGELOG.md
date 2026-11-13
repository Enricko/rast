# Changelog

All notable changes to RAST are documented here. The format is based on
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/) and this project
adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

Release notes published on GitHub are generated from the matching section of
this file by `scripts/changelog-section.sh`.

## [Unreleased]

### Added

- Nothing yet.

### Planned

- Project-key release signatures (minisign/cosign) in addition to the keyless
  build-provenance attestations; the installer already supports fail-closed
  verification via `RAST_REQUIRE_SIGNATURE`.
- Provenance enforcement inside `rast update` (today: SHA-256 + version probe;
  attestations can be verified manually or with the installer).
- Automated restore drills and a documented JSON schema for backup manifests.
- Static musl builds are **not** planned for 0.1.x (see
  [docs/COMPATIBILITY.md](docs/COMPATIBILITY.md)).

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
  [SECURITY.md](SECURITY.md#known-dependency-advisory-01).
- Frontend build requires Node.js >= 22 and npm (build-time only); the
  installed server needs neither.

