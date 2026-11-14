# Native Linux apps — migration handoff

Status: **final on `main`, v0.13.3.** This document is the engineering handoff
for the migration from Docker-streamed desktop apps to native host execution
with one shared Xpra session per RAST user. The behaviour described here is the
shipped implementation; the user/operator guide is
[NATIVE-LINUX-APPS.md](NATIVE-LINUX-APPS.md), installation prerequisites are in
[INSTALL.md](INSTALL.md#native-linux-apps-xpra) and the HTTP surface in
[API.md](API.md#linux-apps-app-id-applications-managing-them-is-administrator-only).

> **Results status:** final verification numbers (2026-11-14) are under
> [Results](#results). Everything else in this document describes the shipped
> implementation.

## Goal and outcome

Before: every App Center desktop app was built into its own Docker image on top
of a streamed Selkies desktop; opening it started a container, a VNC stream and
a whole virtual desktop for one app, and extra Chrome windows appeared inside
that streamed desktop inside a RAST window.

After: an app is installed natively on the host and started as a process under
the person's own unprivileged Linux identity. Each RAST user has exactly one
lazily started Xpra seamless graphical session that all of their applications
share. Each normal native top-level window (including additional Chrome
windows) is a RAST-managed desktop window with its own taskbar entry, title,
focus, resize, minimize/maximize and close behaviour. Dialogs, popups, menus
and other transient windows are attached to their parent RAST window and never
get their own taskbar entry. There is one browser transport (one WebSocket) per
attached viewer, not one per window or app; all windows and apps of the person
share that session.

The legacy Docker backend remains available (`backend = "docker"`) for apps
installed before the migration, and can be forced for new installs with
`native_enabled = false`. Legacy apps can be migrated one by one from App
Center.

## Architecture

```
browser (RAST desktop)
  │  same-origin iframe + WebSocket, authorized per request
  │  /linux-apps/session/<128-bit key>/...
  ▼
rast.service (unprivileged: rast:rast)
  ├─ LinuxApps            app records, jobs, audit, legacy Docker backend
  ├─ NativeSessions       one Xpra session per user, lifecycle + admission
  ├─ native_proxy         HTTP + WebSocket reverse proxy + shim injection
  └─ NativeApps           native install/update/uninstall/migrate (server side)
        │  JSON lines over /run/rast/native.sock (root:rast, 0660)
        ▼
rast-native.service (root: `rast native-helper`)
  ├─ identity provisioning (useradd/groupadd per RAST user)
  ├─ systemd transient units in rast-linux.slice:
  │    rast-xpra-u<user>            Xpra seamless session (loopback TCP)
  │    rast-app-u<uid>-<app>-<n>    one application process group
  ├─ host APT transactions for admin-approved .deb packages
  └─ managed path operations (chown/chmod/ACL/symlink/profile dirs)
```

* **Identity mapping**: RAST user `id` → Linux account `rast-u<id>` with
  `uid = linux_uid_base + id` (default base 200000) and a private group. Its
  home is `<data_dir>/linux-users/<id>` (0700). Profiles per app and person are
  `<data_dir>/linux-apps/profiles/<app-id>/<user-id>` (0700, owned by the
  identity). The person's RAST home (`<storage_dir>/homes/<username>`) is
  exposed **read-only** (`rx`) to that identity through an ACL; a `RAST`
  symlink inside the app profile home points at it (same convention as the old
  Docker image's `/config/RAST`). Files an app writes stay in its 0700 profile
  and are not visible in File Station; shared folders are not mapped into app
  profiles at all (copy a file into the person's home to open it in an app).
  Chromium-family apps use Chromium's `--password-store=basic` because the
  headless session has no unlocked OS keyring; see the credential note below.
* **Session runtime**: `/run/rast-linux/<uid>` (0700) holds Xpra sockets, logs
  and tmp; Xpra writes its X11 auth file to `<linux home>/.Xauthority`, which
  every application unit uses. A session binds its web/websocket transport to
  `127.0.0.1:<ephemeral port>`; the helper adds that port to an nftables set
  (`inet rast_native`, one rule: loopback TCP to the set is rejected for every
  uid except the RAST service uid), so the only reachable path is RAST's
  authenticated same-origin proxy even for other local accounts. `nftables`
  is required for the native backend (`rast doctor` warns if missing). Display
  numbers are allocated from 100–999 by scanning `/tmp/.X11-unix` and probing
  free ports.
* **Lifecycle**: the session starts on the first launch (`POST
  /api/linux-apps/{id}/launch`) and is reused by every later app/window. It
  stops after 60 s empty grace plus `linux_idle_minutes` once it has no
  running app and no connected viewer, or immediately through the
  `POST /api/linux-apps/session/close` API (there is no "End session" button
  in App Center). A page reload reconnects to the same
  running session; a restart of `rast.service` adopts the still-running units
  (state is persisted in `linux_sessions`/`linux_runs`, and systemd units are
  adopted by name), so a web-server restart does not kill applications.
  Restarting `rast-native.service` stops all sessions (they are restarted
  lazily).
* **Limits**: session and application units get `MemoryMax`, `CPUQuota`,
  `TasksMax` from `linux_session_*`/`linux_app_*` inside `rast-linux.slice`.
  Admission checks `/proc/meminfo` `MemAvailable` against the app limit plus
  `linux_admission_reserve_mb` (replacing the old unconditional 4 GiB check).
* **Window integration**: the frontend embeds the pinned Xpra HTML5 client once
  per user in a same-origin iframe. A shim injected by `native_proxy`
  (`src/linux_apps/xpra_shim.js`) captures the client, hides its chrome and
  exposes a narrow API (`window.RASTNative`) with the remote window list and
  commands. `frontend/src/shell/xpra-native.ts` adopts each remote window's DOM
  node into a RAST window body (no per-window pixel copy and no second
  connection), forwards geometry (`window-configure`), focus, close
  (`window-close` = the app's normal close request), minimize (X11
  minimize/restore as a focus-safe hide/show), maximize, clipboard and keyboard. Transient
  windows (dialogs/popups) are attached to their parent RAST window and do not
  get their own taskbar entry. Chrome's extra windows are independent RAST
  windows.

## Installation backends

| Format | Install | Update | Uninstall |
| --- | --- | --- | --- |
| `.deb` | Preview (`dpkg-deb` metadata + `apt-get -s` transaction), then host `apt-get install` through the helper (root, admin-approved). | Upgrade from the package's apt source when available; otherwise from a new file/URL. Previous version is not kept by the package manager (APT transaction semantics are reported truthfully). | Only packages RAST added and nothing else depends on; shared/pre-existing packages are kept. |
| AppImage | Controlled `--appimage-extract` into a managed versioned dir; embedded `.upd_info` as update source; launcher = `AppRun`. | GitHub releases / zsync, or a new file/URL: stage new version, validate, activate, keep the previous directory for rollback. | Managed directory removed; optional per-user profiles. |
| Portable archive (`tar.gz/tgz/tar.xz/txz/tar.bz2/tbz2/tar`) | Path-safe extraction into `<data>/linux-apps/apps/<id>/<version>/`; `.desktop` detection or a launcher-selection step; source archives are rejected with an explanation. | Staged, validated, atomically switched; `previousPath` points at the recoverable previous version. | Managed directory removed; optional per-user profiles. |

Every install starts with `POST /api/linux-apps/preview`, which returns the
application identity, version when known, architecture, source, selected
launcher (or candidates), dependency transaction (installs/upgrades/removals,
download size), compatibility and warnings — explicitly unknown values stay
null. Installing `.deb` packages is privileged software installation: the
helper runs APT with `DEBIAN_FRONTEND=noninteractive`, no shell, and refuses
transactions that would remove packages outside the requested set.

## Security model

* Browser ↔ RAST: the native session proxy authenticates **every** request
  (HTML, assets and WebSocket handshake) against the signed-in RAST session and
  requires the session owner; another user's key is 403, expired/revoked is
  401. WebSocket upgrades additionally require a same-origin `Origin`. The Xpra
  port never leaves loopback. No session key or credential is logged; the
  reverse proxy should disable access logging for `/linux-apps/` (the URL is a
  credential).
* Per-user isolation: distinct uids, 0700 homes/profiles, unique displays and
  sockets. Verified cross-user reads fail with EACCES.
* The Xpra display transport itself has no application-level authentication;
  it is protected by the nftables uid restriction above plus loopback binding,
  and every browser-facing request is authenticated by the proxy.
* Live display WebSockets revalidate the RAST login session every 5 s; logout,
  session revocation or account deactivation closes the stream immediately
  (verified: `GET /api/auth/me` 401 within 1 s, stream closed within ~2 s).
  Deactivating an account also stops its native sessions; revoking only
  browser sessions keeps the person's processes per the documented retention
  policy.
* Applications inside one person's session share that person's trust boundary
  by design (they run as the same uid; this is **not** a hostile-app sandbox —
  an app can read and change everything that identity can reach).
* Window titles and metadata are untrusted text: the shim HTML-escapes remote
  titles before the pinned Xpra HTML5 client renders them, RAST renders the
  raw title through text nodes only, and the adopted DOM is verified to contain
  no injected elements (live injection test).
* Chromium-family apps run with Chromium's deterministic `--password-store=basic`
  because a headless session has no unlocked OS keyring/Secret Service; the
  profile is protected by the per-user identity and 0700 permissions, not by
  OS keyring encryption. This is an explicit, documented behaviour (shown as an
  install-preview warning), not a silent fallback, and third-party app
  credentials stay inside that person's app profile. RAST passwords are never
  copied into applications.
* Display ports: nftables restricts each session port to the RAST service uid
  (cross-uid connection attempt is refused at the kernel; the same host check
  shows the gateway uid still gets HTTP 200).
* Chromium runs unprivileged with its sandbox enabled: `/opt/google/chrome/
  chrome-sandbox` is setuid root and renderer processes show `Seccomp: 2`,
  `NoNewPrivs: 1`; `--no-sandbox` is never added by RAST.
* `rast-native.service` is the only root component: Unix socket 0660
  root:rast, optional peer-uid allowlist, JSON-lines protocol with
  `deny_unknown_fields`, strict per-field validation (usernames, unit names,
  displays, ports, argv[0] prefixes, env keys, limits, managed paths), no
  shell, bounded timeouts/output and serialized APT transactions.

## Changed files (overview)

Rust server:

| Path | Change |
| --- | --- |
| `src/linux_apps/mod.rs` | App model columns/JSON (`backend`, `native`), native dispatch (preview/install/update/check/uninstall/launch/session/close/migrate/rollback/cleanup), visibility/build rules, running counts, session endpoints |
| `src/linux_apps/native_session.rs` | Per-user Xpra session manager: lazy start, app launch, admission, idle/empty lifecycle, restart recovery, DB persistence |
| `src/linux_apps/native_proxy.rs` | Same-origin HTTP/WebSocket proxy with per-user authorization, shim serving/injection, CSP |
| `src/linux_apps/native_apps.rs` | Native install/update/uninstall/migration service: previews, `.deb` via helper, archive/AppImage handling, package ownership, profiles, migration |
| `src/linux_apps/native_archive.rs` | Path-safe archive extraction, `.desktop` parsing, launcher/icon discovery |
| `src/linux_apps/native_proxy` shim | `src/linux_apps/xpra_shim.js` served to the session page |
| `src/native/*` | Privileged helper: protocol, client, daemon, identities, deployment; `src/native/firewall.rs` adds the nftables display-port restriction and reconciliation |
| `src/api/users.rs` | Account deactivation stops that person's native sessions |
| `tests/native_linux_e2e.rs`, `tests/e2e/*` | Authorization/lifecycle tests and the real-browser E2E harness (13 native scenarios, XSS/revocation checks) |
| `src/config.rs` | New native keys (see docs/CONFIGURATION.md) |
| `src/state.rs` | Native client + session manager + install service wiring |
| `src/api/linux_apps.rs`, `src/api/mod.rs` | New endpoints, proxy routes, updated request/response shapes |
| `src/docker/{linux,protocol}.rs`, `src/worker/linux.rs` | Bounded legacy data-volume list/export/remove operations for the migration |
| `migrations/0013_native_linux_apps.sql` | New columns, `linux_identities`, `linux_sessions`, `linux_runs` |
| `src/main.rs` | Hidden `rast native-helper` subcommand, `doctor` checks |

Frontend:

| Path | Change |
| --- | --- |
| `frontend/src/shell/xpra-native.ts` | Shared Xpra client host + RAST window integration (adopt, input, geometry, dialogs, reconnect) |
| `frontend/src/apps/linuxapp/*` | Native app/session model, launch routing, catalog/overview types |
| `frontend/src/apps/packages/linux.ts`, `linux-preview.ts` | App Center: install preview dialog, update from file/URL, migration state/actions, status/errors |
| `frontend/src/core/wm.ts`, `shell/desktop.ts`, `styles/apps.css` | Owned/transient windows, no taskbar entry for dialogs, native launch routing, narrow-layout styles |

Docs and deployment: `docs/API.md`, `docs/ARCHITECTURE.md`,
`docs/COMPATIBILITY.md`, `docs/CONFIGURATION.md`, `docs/INSTALL.md`,
`docs/OPERATIONS.md`, `SECURITY.md`, `README.md`, `CHANGELOG.md`,
`.env.example`, `deploy/rast.toml.example`,
`deploy/rast-native.service`, `deploy/rast-linux.slice`,
`deploy/rast-native.tmpfiles.conf`, `installer/install.sh`,
`installer/uninstall.sh`, `scripts/sync-installer-assets.sh`.

## Migration procedure (Docker → native)

1. Install the native prerequisites from [INSTALL.md](INSTALL.md#native-linux-apps-xpra)
   and run `sudo -u rast rast doctor` until the native section is clean.
2. Take a backup (`rast backup --scope db,config`).
3. App Center → the app → **Migrate to native**. The job inventories the app,
   prepares a native install (original staged file, apt source or GitHub/zsync;
   if the original file is gone the UI asks for a replacement file/URL),
   copies every person's Docker settings volume into the native profile while
   the app is stopped, and switches `backend` only after validation.
4. The Docker image, containers and volumes are retained (`dockerVolume`,
   `migration.backup`); the app id and shortcuts are unchanged. Audit:
   `linuxapp.migrate`.
5. Verify the app (launch it, check the person's settings are present). If
   anything is wrong: **Roll back** (returns to the Docker backend, data
   intact). When satisfied: **Clean up old Docker data** (removes the retained
   volume/backup; live native profiles are never removed).
6. A failed migration records `migration.state = "failed"` with an actionable
   error and leaves the Docker app working.

## Recovery

* RAST restarted: running sessions/apps are adopted from `linux_sessions` /
   `linux_runs` + systemd units; no window is lost. Orphan units without a DB
   row are stopped; DB rows without units are marked ended.
* Helper restarted: the transient units under `rast-linux.slice` are systemd
   units, not children of the helper, so running sessions and apps **keep
   running**. On start the helper reconciles the nftables display-port set
   against the live `rast-xpra-*` units; `rast.service` adopts the still-live
   units from `linux_sessions`/`linux_runs`. Only stopping the units (or
   `rast-linux.slice`, or a reboot) ends them.
* App/Chrome crash: its RAST windows disappear, other windows/apps and the
   session stay.
* Browser refresh/network loss: the page reconnects to the same session; Xpra
   has `reconnect` enabled and the RAST proxy re-authenticates.
* Stuck app: App Center → **End application** (run-level; the canonical API
   is `POST /api/linux-apps/session/runs/{run}/close`, the shorter
   `/session/runs/{run}` form is accepted as an alias). Closing the app's
   window is the graceful path (the app receives WM_DELETE and can show its
   save prompt); "End application" force-stops that one run. The whole-session
   close exists only as the `POST /api/linux-apps/session/close` API (there is
   no "End session" UI).
* Wrong/failed install or update: the row shows `status` + `error`; portable
   previous versions remain on disk (`previousPath`), `.deb` state is whatever
   APT reports (the service records the actual package state after failure).

## Remaining limitations (known)

* One graphical session per user is also one Xpra *client* connection: the
  browser connects the embedded HTML5 client once; a second browser tab for the
  same user reuses the session with Xpra `sharing` (the newest viewer becomes
  the active client; this is the standard Xpra behaviour).
* Applications of one user share a trust boundary (same uid/session). Isolation
  between users is enforced; isolation between applications of the same user is
  not (documented, by design).
* X11 auth file discovery: Xpra uses `$HOME/.Xauthority` normally but falls
  back to `$XDG_RUNTIME_DIR/xpra/Xauthority-<display>` when the identity home is
  not writable at session start. The helper therefore reads the actual `-auth`
  argument from the running Xvfb of the session and hands exactly that file to
  application units instead of guessing, and the session manager recreates a
  session whose identity home had to be repaired. If an app still reports
  `Authorization required`, stop `rast-linux.slice` and launch again.
* Hardware acceleration: Xpra uses the best available encoder (software x264/
  vp9/png by default; NVENC/VAAPI when the host has the codec packages —
  `xpra-codecs-nvidia` is optional). Applications render through llvmpipe on
  Xvfb unless the operator configures a GPU-backed X server; the RAST default
  is the portable software path.
* `.deb` uninstall fidelity depends on the host package manager: shared or
  independently installed packages are never removed, but APT may still keep
  dependency packages another package needs (correct, reported as such).
* Source archives are rejected by design; a prebuilt package or build recipe is
  required.
* **One native RAST instance per host**: systemd transient unit names are keyed
  by RAST user id (`rast-xpra-u<id>`, `rast-app-u<uid>-…`), so two RAST servers
  with native apps on the same host would fight over each other's sessions.
  Development/test instances must be run one at a time (or with
  `native_enabled = false`). The Docker-backed backend does not have this
  limitation.
* **Audio (0.13.1):** playback is forwarded from a per-user PulseAudio null sink
  through the authenticated Xpra WebSocket. Requires the Xpra audio component,
  PulseAudio and GStreamer Pulse/codecs (the installer checks dependencies).
  Browser autoplay policy requires a click on the native app canvas; microphone
  forwarding is disabled. Sessions created by older releases retain their old
  audio-disabled startup options: save work, close the app and stop the
  session (`POST /api/linux-apps/session/close`) or wait for idle cleanup,
  then launch again.
  The default remains software rendering/encoding, not low-latency gaming or
  hardware-accelerated video; network latency and server CPU limits still apply.
* **Credential storage:** Chromium-family apps use Chromium's `--password-store=basic`
  because the session has no unlocked OS keyring; saved site credentials are
  protected by the 0700 per-user/per-app profile, not by keyring encryption.
  Non-Chromium apps get no OS keyring either (a private D-Bus session is
  started for Chromium; other apps may use `secret-tool` only if the operator
  provisions a keyring). Third-party app logins are independent of RAST.
* **Files:** the app identity has read-only access to the person's RAST home;
  shared folders are not mapped into native app profiles, and files saved by an
  app stay in its profile (not browsable from File Station). File Station's
  browser checks do not protect data from native programs: OS permissions/ACL
  are the enforcement boundary.
* **Clipboard and shortcuts:** clipboard copy both ways is best-effort and
  needs browser clipboard permission/a user gesture; browsers may block it.
  Keyboard shortcuts are forwarded only while a native window has focus and
  only where the browser allows it — browser-reserved shortcuts (Ctrl+W,
  Ctrl+T, Ctrl+N in a browser chrome context, …), `F5`/`F11`/`F12` and
  developer tools stay with the browser and are not universally interceptable.
* **High-DPI:** the session is pinned at DPI 96 and canvases are adopted 1:1;
  HiDPI/retina browsers see a scaled canvas rather than a natively remastered
  one.
* **Window identity:** remote X11 ids are scoped to the loaded session (frames
  are cleared when the session is replaced); the taskbar app id includes the
  remote window id, which is stable only within a session.
* **nftables required:** without the `nftables` package the helper keeps
  running but the display ports are only loopback-bound; `rast doctor` and
  `rast native-helper --check` warn about this.
* **Xpra is an external dependency:** validated against Xpra 6.5.5 / HTML5
  client v21; the installer and doctor require 6.5+. Xpra (GPL-2.0-or-later)
  is not linked into or distributed with the RAST binary; the HTML5 client
  (MPL-2.0) is served from the installed package (see
  [THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md)).
* The old Docker backend stays compiled in for compatibility; the Applications
  app (Docker Compose management) is unchanged and independent.

## Results

<!-- LEAD-VERIFICATION -->
### Final verification (2026-11-14, host `lufria-server`)

Environment: Ubuntu 26.04.1 LTS, x86_64, 12 cores, 30 GiB RAM, kernel
7.0.0-38-generic; systemd-services; Xpra 6.5.5-r0 (HTML5 client version 21)
from xpra.org; Google Chrome 157.0.8103.78 installed natively; Playwright
Chromium 1248 (headless) as the browser; RAST 0.13.2 build (the native feature was re-integrated into 0.13.3 on the newer main line; the measurements below are from that verified build). The test
instance was a disposable native-mode server on loopback
(`127.0.0.1:8742`, Xpra display :100–:103). All numbers are from this host and
this host only; they are not a promise of zero latency or a general benchmark.

#### Automated checks (all on the final tree)

| Check | Result |
| --- | --- |
| `cargo fmt --all -- --check` | clean |
| `cargo clippy --all-targets -- -D warnings` | clean (0 warnings) |
| `cargo test --all-targets` | **669 passed, 0 failed** — lib 616, `api_e2e` 32, `event_kinds` 2, `native_linux_e2e` 12, `packages_e2e` 7 |
| `npm --prefix frontend run typecheck` | clean |
| `npm --prefix frontend test` | **89 passed, 0 failed** |
| `npm --prefix frontend run build` | built (embedded into the release binary) |
| `shellcheck -s sh -x installer/install.sh installer/uninstall.sh` | clean |
| `shellcheck -s bash -x scripts/*.sh scripts/e2e/*.sh` | clean |
| `scripts/sync-installer-assets.sh --check` | installer embeds up to date |
| Live privilege checks (helper) | `ping`, `identity_ensure`, `session_start` (`rast-xpra-u1`), real Xpra HTML 200 over the authenticated proxy, `app_start` xterm owned by the identity, stop, `identity_remove`, zero leftover units |

#### Real-browser end-to-end matrix (native backend)

Run: `tests/e2e/run.mjs --mode=native --instance-root=/tmp/rast-native-lead`
against the disposable native instance; raw data
`tests/e2e/results/native-2026-11-14T09-41-14-262/results.json`, screenshots
`tests/e2e/screenshots/native/<scenario>/`. **13 PASS, 0 FAIL, 1 SKIP.**

| Scenario | Result | Evidence |
| --- | --- | --- |
| s0 smoke: health, form login, desktop shell | PASS 2.3 s | screenshots `native/s0/` |
| a admin installs a real `.deb` (Chrome 157) and launches it | PASS 11.8 s | install job `done`, native row `ready`, real Chrome window mapped |
| b portable archive + launcher + second window from inside the app | PASS 3.4 s | two real GTK windows (`RAST Test App`, `RAST Test App #2`) |
| c one user, Chrome + a second GUI app in one shared Xpra session | PASS 2.2 s | one `rast-xpra-u2` unit, two app units, both windows at once |
| d three Chrome windows: taskbar entries, focus, resize, close | PASS 9.2 s | three RAST windows/taskbar entries; resize verified in both the RAST rect and the remote Xpra geometry (`remote window geometry` record); all closed |
| e two users open Chrome concurrently with separate profiles | PASS 12.3 s | separate identities/displays/processes; combined open 9.55 s |
| f 8 concurrent launch requests reuse the intended session | PASS 2.0 s | 8 OK, one session/run/unit |
| g browser refresh reconnects without duplicating processes | PASS 3.7 s | same session/display/run before/after |
| h closing one app preserves the other app | PASS 7.6 s | remaining window still accepts input after Chrome closes |
| i idle cleanup | PASS 3 m 9 s | empty (no app, no viewer) session reclaimed after 60 s grace + 2 min idle (measured 187.3 s) |
| j service restart recovery | PASS 12.9 s | adopted session, same display/key/run, no duplicate |
| k failed install/update leave recoverable state | PASS 6.7 s | precise errors, app remains usable |
| l uninstall respects shared dependencies and user data | PASS 40.6 s | shared/pre-existing packages kept; profile data choice honoured |
| m Docker→native migration | SKIP | needs a Docker-phase run first (two-phase procedure in the E2E docs); migration unit tests cover profile copy/rollback |

The previously reported scenario-d/focus/resize failures were harness and two
real product bugs, all fixed on the final tree: resize grab strips were clipped
by `.window { overflow: hidden }` (bottom-right handle unreachable when another
window covered it), and a launch issued while the previous run's process had
exited but the 5 s sweeper had not noticed yet returned the dead run instead of
starting a new process. Regression tests cover the second issue
(`a_stale_run_does_not_swallow_the_next_launch`); the keyboard-forwarding guard
that swallowed Ctrl+N was also corrected and is exercised by scenario d.

#### Live security verification (same instance)

| Check | Result |
| --- | --- |
| Hostile remote window title (`<img src=x onerror=…>`) injected through the Xpra metadata path | No script executed; no `<img>`/`onerror` element appears in the adopted DOM; RAST renders the raw title through a text node only (`textContent`); Xpra's own `.html(title)` sink receives HTML-escaped text |
| Display port restricted to the RAST service uid (nftables `inet rast_native`) | From uid 993 (`rast`, the authenticated gateway): Xpra HTML **HTTP 200**; from another local uid (1000): TCP connection **refused**; port appears in `set xpra_ports` while the session runs and is removed on stop |
| Admin revokes the user's login sessions while the display stream is open | `GET /api/auth/me` returns **401 within 1 s**; the live WebSocket stream closes and the session iframe is removed within ~2 s (5 s revalidation budget); a new session request is rejected |
| Retention policy after revocation | The user's Xpra session unit and its cgroup stay active (apps are not killed by browser revocation); account deactivation stops the sessions |
| Cross-user display/page access | Another user's key → 403; expired/revoked RAST session → 401; WebSocket upgrade requires same-origin `Origin` (`native_linux_e2e` and proxy unit tests) |
| Chromium sandbox | Native Chrome runs unprivileged with its sandbox (setuid `chrome-sandbox`, renderer `Seccomp: 2`, `NoNewPrivs: 1`); `--no-sandbox` is never added |

#### Performance — native backend, direct session page vs RAST-integrated desktop

Same host, same server, same Xpra session and Chrome app, loopback only (no WAN);
hardware/versions as above. "Direct" = `tests/e2e/perf/quick-perf.mjs` drives the
authenticated session page (`/linux-apps/session/<key>/`) without the desktop
shell. "Integrated" = a custom harness (`tests/e2e/results/perf-integrated-native-latest.json`)
that launches from the desktop start menu and measures the adopted RAST window.
Both are upper bounds that include Playwright/browser and screenshot-encoding
cost, and their pixel-polling intervals differ (100 ms direct vs 30 ms
integrated), so compare directions and rough magnitude, not single milliseconds.

| Metric | Direct Xpra page | RAST integrated desktop | RAST-added overhead (measurement) |
| --- | ---: | ---: | --- |
| Cold launch — backend ready (API) | 9.15 s | n/a (UI path) | — |
| Cold launch — first painted frame (direct) | n/a (page painted before window) | n/a | — |
| Warm launch — backend ready | 0.01 s | — | — |
| Warm launch — start-menu click to first Chrome RAST window | — | **1.22 s** (median of 3: 1.28/1.22/1.22) | window adoption + shell rendering |
| Keyboard input → first changed window pixel | **108 ms** (samples 122/108/82) | **132 ms** (samples 133/128/136/132/132) | ≈ 24 ms (polling/Playwright-bound) |
| Resize drag → RAST window and remote Xpra geometry settled | n/a | **199 ms** | debounced remote configure (90 ms) |
| Idle memory after 60 s (session cgroup, Chrome live) | 194.8 MiB | same session (server side unchanged) | browser-side rendering not counted |
| Active memory during 10 s typing/scroll | 197.7 MiB | same | — |
| CPU during 10 s interaction (one core) | 15.9 % | same | — |
| Two users, Chrome each: combined memory | 394.1 MiB | same | — |
| Two users: combined CPU during interaction | 21.2 % of one core | same | — |
| Two users: both apps ready | 8.95 s | 9.55 s (two-user E2E open) | — |
| Session unit memory current/peak | 194.8 / 270.6 MiB | same | — |

Raw data: `tests/e2e/results/perf-quick-native-latest.{json,md}` (direct) and
`tests/e2e/results/perf-integrated-native-latest.json` (integrated); method:
`tests/e2e/results/README.md`.

Interpretation: RAST adds no per-frame transcoding — the same Xpra WebSocket
feeds the adopted canvases — so steady-state session CPU/memory are identical
between the direct page and the integrated desktop; the measurable RAST-specific
cost is window adoption at launch (≈1.2 s warm), a small keyboard-forwarding
delta bounded by the measurement granularity, and the 90 ms debounce on resize
configure. The dominant costs remain Chrome startup and software rendering
(`llvmpipe`) plus Xpra software encoding. No GPU/NVENC acceleration was enabled
for these measurements, and no claim of zero latency is made.

Docker-vs-native comparison: the fresh run above is native-only. The historical
Docker baseline (2026-11-12, same host, legacy backend; cold backend-ready
14.82 s, idle 464.4 MiB, two-user 1236.4 MiB) is kept in
`tests/e2e/results/perf-comparison.md` and `perf-quick-docker-latest.*` and was
not re-measured in this final round; treat it as historical context, not a
same-session comparison.

#### Not verified in this round (explicit)

* **AppImage live extraction/launch** — unit-tested (`native_archive`/`native_apps`) but no real AppImage fixture was run on this host. Reproduce with a prebuilt AppImage through App Center → Add app and `tests/e2e/run.mjs --mode=native`.
* **aarch64 native runtime** — the project builds for aarch64 and the architecture checks are unit-tested, but no arm64 host was available; Xpra/Chrome end-to-end was validated on x86_64 only.
* **Docker→native migration end-to-end (`scenario m`)** — requires two server modes/phases; covered by unit tests plus the documented two-phase commands, not in one automated run.
* **Sustained 2+-user load and long sessions** — only a short two-user concurrency run; no soak test.
* **WAN conditions, bandwidth, dropped frames, GPU/NVENC, HiDPI browsers** — loopback software path only; no measurements claimed.
* **Failure injection on the display transport** (kill Xpra mid-stream, network partition) beyond the server-restart/refresh scenarios; recovery paths are unit-tested.
* **Native package install on a second distribution** — validated on Ubuntu 26.04 x86_64 only; the `.deb` compatibility checks (architecture, dependencies, APT simulation) are unit/e2e-tested.
### Post-integration pointer-input fixes and re-verification (2026-11-14, 0.13.3)

Re-integrating the native feature on the 0.11 main line (`c83dd62` /
`68b13d4`) introduced two pointer-input regressions that the scenario matrix
above did not catch, because its final run predates that merge:

* the main-line desktop multi-selection guard tested pointer targets with
  `instanceof Element`. A canvas adopted from the Xpra viewer document keeps
  the creating document's prototypes, so a click inside a native window was
  treated as a desktop-background click, started the marquee gesture and
  called `preventDefault()` — which makes the browser suppress the
  `mousedown`/`mouseup` events the native application needs;
* `frontend/src/styles/apps.css` lost the closing brace of
  `.package-app-frame`, nesting every native-app rule (including the
  off-screen session viewer size) inside it. The viewer fell back to the
  browser's default 300×150 iframe size, so Xpra's virtual desktop was
  300×150 and its pointer coordinates were clamped to that desktop: clicks
  inside native windows landed at the wrong place.

Both are fixed on `main`: a realm-independent `isElementNode` (`nodeType`)
check in the desktop marquee guard, the stylesheet brace restored, and the
off-screen viewer sized to the browser viewport plus a margin so a maximized
window still fits inside Xpra's virtual desktop. Regression coverage:
`frontend/tests/dom.test.ts`, `frontend/tests/styles.test.ts` and native E2E
scenario b (clicking **New Window** opens a second window).

Re-run of the full native matrix after the fixes (same host, Xpra 6.5.5,
Chrome 157, RAST 0.13.3 at `68b13d4` + the fixes):
`node tests/e2e/run.mjs --mode=native --instance-root=...` → **13 passed,
0 failed, 1 skipped** (`m`, needs a Docker-phase run first); raw data in
`tests/e2e/results/native-2026-11-14T20-36-25-712/`. Frontend unit tests at
that tree: 119 passed; Rust: `cargo fmt`/`clippy` clean, `cargo test
--all-targets` 715 passed (Rust code unchanged by these fixes).

<!-- /LEAD-VERIFICATION -->
