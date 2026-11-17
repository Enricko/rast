# Native Linux apps

How RAST runs desktop Linux applications natively on the server, what the
isolation does and does not protect, and how windows, input and sessions behave
from the browser.

This page is the operator and user guide. Installation prerequisites are in
[INSTALL.md](INSTALL.md#native-linux-apps-xpra), the HTTP API in
[API.md](API.md#linux-apps-app-id-applications-managing-them-is-administrator-only),
the internals in [ARCHITECTURE.md](ARCHITECTURE.md) and the engineering
migration notes in
[native-linux-apps-handoff.md](native-linux-apps-handoff.md).

## How it works

* **One unprivileged Linux identity per RAST user.** A user `id` maps to the
  host account `rast-u<id>` (`uid = linux_uid_base + id`, default base
  `200000`) with a private group and a 0700 home under
  `<data_dir>/linux-users/<id>`. Nothing in the graphical stack runs as root.
* **One shared Xpra seamless session per user.** The first launch starts the
  session lazily (`rast-xpra-u<id>`); every app that person opens — and every
  window Chrome opens — reuses it. Each attached browser viewer holds one
  transport (the session page plus one WebSocket), never one per window or
  application; a second tab or browser shares the same session (see
  [lifecycle](#lifecycle-and-quitting)).
* **Native processes, not containers.** Apps are installed on the host
  (`.deb` through the host package manager, AppImage or portable archive into
  the managed app tree) and started as transient systemd units
  (`rast-app-u<uid>-<app>-<n>`) inside `rast-linux.slice`. Per-session and
  per-app `MemoryMax`, `CPUQuota` and `TasksMax` come from the
  `linux_session_*`/`linux_app_*` configuration keys.
* **One root helper.** `rast-native.service` (`rast native-helper`) creates the
  identities, starts/stops the transient units, runs administrator-approved
  APT transactions and applies managed path operations. The unprivileged web
  server talks to it over a JSON-lines Unix socket
  (`/run/rast/native.sock`, 0660 root:rast, optional peer-uid allowlist).
* **Xpra is an external program.** The installer requires Xpra **6.5+** from
  [xpra.org](https://xpra.org) and the version is checked by the installer,
  `rast doctor` and `rast native-helper --check`. The implementation is
  validated against **Xpra 6.5.5** with the **HTML5 client version 21** that is
  part of the installed Xpra packages. The server is invoked as an external
  process; the HTML5 client is served from the installed package. Neither is
  linked into or distributed with the RAST binary. See
  [THIRD-PARTY-LICENSES.md](THIRD-PARTY-LICENSES.md#external-programs-invoked-at-runtime).


## Install Linux App

Administrators install apps from **App Center → Install Linux App** (the
primary button in the App Center toolbar; the empty-state tile with the same
name opens the same dialog). Regular accounts never see the button, and the
preview/install endpoints are administrator-only on the server.

The dialog has two sources:

* **Upload package** — choose or drop a file, or pick one from File Station.
  `.deb` packages are fully supported (installed with the host package
  manager through the root helper). Portable archives (`.tar.gz`, `.tgz`,
  `.tar.xz`, `.txz`, `.tar.bz2`, `.tbz2`, `.tar`) are supported when they ship
  a desktop launcher. **AppImage is not offered** in this workflow because it
  has not been validated on this server; the dialog says so instead of failing
  later.
* **Install from URL** — an official vendor `https://` link. The server
  downloads the file itself (redirects included) and validates it; the URL is
  not fetched by the browser.

Before anything is installed the server inspects the file and the dialog shows
a preview: detected app name/version and package format, source, host
distribution and architecture plus compatibility result, dependency/
package-transaction changes, whether the host package is **already installed**
("Add existing installation to RAST" instead of "Install"), whether a RAST app
entry already exists, how many saved per-user profiles already exist (they are
preserved), server warnings (for Chromium: the `--password-store=basic`
headless-keyring note), and **who can use the app** (all RAST users by default,
or administrators only). Unknown values stay unset rather than guessed.

After confirming, the dialog shows the installation stages — *Uploading →
Validating → Installing → Registering → Ready* — driven by the real job. The
dialog can be closed while the job runs (it continues in the background) and
App Center reattaches to the running job when it is reopened, so the app tile
keeps showing progress. The app is only reported as Ready after the job
finished **and** the app row is `ready` with a registered launcher; failures
show the server's actionable error with **Try again** for the same source.
Duplicate submissions are rejected (button disabled, server returns 409 while
an install for the same app is running). When ready, the dialog offers
**Open**; installed apps appear in App Center, the application launcher and on
the desktop (the dialog notes when the icon is already on the desktop).

Screenshots: [button](screenshots/native-install-button.png),
[dialog](screenshots/native-install-dialog.png),
[preview](screenshots/native-install-preview.png),
[progress](screenshots/native-install-progress.png),
[ready](screenshots/native-install-ready.png),
[installed Chrome](screenshots/native-chrome-installed.png),
[Chrome windows](screenshots/native-chrome-windows.png).

### Existing host packages and profiles

If the package is already installed on the host and only missing from RAST, the
preview recognises it and the action is **Add existing installation to RAST**:
RAST registers the existing installation (it does not claim a fresh package
installation, does not re-download anything for the `deb` transaction and does
not remove the host package on uninstall when it pre-dated RAST). A real fresh
install (APT transaction run by the helper) is used when the package is not on
the host; the test suite validates both paths.

Uninstalling an app keeps per-user profiles unless the administrator explicitly
deletes user data. The same `profiles/<app-id>/<user-id>` directories are
reused by a later reinstall, so browser sign-ins, cookies and app settings
survive (browser-session cookies by definition do not survive a browser
restart; persistent cookies do). Reinstalling an existing package preserves the
profiles by default — both paths are verified in the test run.

Credential and file-access limits of the native mode are described above:
per-user profiles are protected by the Linux identity, not by an OS keyring;
only the person's own RAST home is ACL-exposed (read-only) and shared folders
are not mapped into app profiles.

## Requirements

| Component | Requirement |
| --- | --- |
| Host | Linux with systemd (249+ for transient units and cgroup v2). Debian/Ubuntu validated on x86_64; aarch64 is build-supported. |
| Xpra | 6.5+ from xpra.org (`xpra`, `xpra-html5`, `xpra-codecs`, `xpra-x11`), Xvfb, plus the audio packages listed in [INSTALL.md](INSTALL.md#native-linux-apps-xpra) for sound |
| nftables | `nftables` package (the `nft` command) — required to restrict the loopback Xpra display ports to the RAST service account; without it `rast doctor` and `rast native-helper --check` warn and the display ports stay reachable by other local accounts |
| ACL support | `acl` package and an ACL-capable filesystem (ext4, xfs, btrfs) for read access to the person's RAST home |
| `rast-native.service` + `rast-linux.slice` | Shipped in `deploy/`, installed by the installer |

**Quarantining the feature:** set `linux_apps_enabled = false` to hide the
App Center Linux-apps section and its "Install Linux App" button and to reject
new installs (both native and legacy Docker-backed), while keeping the
implementation, the database schema and any app data in place. Set
`native_enabled = false` instead when only host-native execution should be
disabled and legacy Docker-backed installs may stay available.

Only one RAST instance per host may use the native backend: transient unit
names are keyed by RAST user id (`rast-xpra-u<id>`, `rast-app-u<uid>-…`), so a
second instance on the same host would fight over the same sessions. Test or
secondary instances must set `native_enabled = false` (or use another host).

## Security model

The security boundary is **between users**, not between the applications of one
user. It is not a hostile-application sandbox.

| Control | What it does |
| --- | --- |
| Authenticated proxy | Every HTML, asset and WebSocket request under `/linux-apps/session/<128-bit key>/` is authenticated as the signed-in RAST user and must own the session: another user's key is `403`, an expired/revoked session is `401`, and WebSocket upgrades require a same-origin `Origin`. The Xpra display port itself is never exposed beyond loopback. |
| Live session revalidation | A live display WebSocket re-checks the RAST login session every **5 s** and closes within a few seconds on logout, session revocation, expiry or account deactivation. The browser loses the display; the person's applications keep running (see [lifecycle](#lifecycle-and-quitting)). |
| Display-port firewall | Xpra's HTML5 transport is bound to `127.0.0.1:<ephemeral>` with Xpra-level authentication disabled, so RAST's authenticated proxy is the only intended path. Because loopback is not a security boundary between local uids, the helper installs one **nftables** rule at startup: table `inet rast_native`, set `xpra_ports`, output chain rule `ip daddr 127.0.0.1 tcp dport @xpra_ports meta skuid != <rast uid> reject with tcp reset`. Every session port is added before the session is usable and removed when it stops; the set is reconciled against live `rast-xpra-*` units when the helper starts. This is defence in depth behind the authenticated proxy. |
| Account deactivation | Deactivating an account (Control Panel or `DELETE /api/users/{id}`) stops its native session and applications. Revoking only the *browser* sessions keeps the person's apps running under the retention policy. |
| Per-user filesystem isolation | Distinct uids, 0700 homes/profiles, unique displays and sockets. Cross-user reads and connections fail (`EACCES`). |
| Helper hardening | The helper is the only root component: socket 0660 root:rast, peer-uid allowlist, `deny_unknown_fields` JSON, strict per-field validation, no shell, bounded timeouts/output, serialized APT transactions. |
| Chromium | Runs unprivileged with its sandbox enabled; `--no-sandbox` is never added by RAST. Installing a `.deb` through App Center is *administrator-approved privileged software installation* — treat it like `sudo apt install`. |

Per-user separation is **not** a hostile-app sandbox. All applications of one
person run as the same Linux identity in the same session and trust boundary:
one compromised app can read the other apps' profiles and inject input or
clipboard data into their windows. Isolation between different RAST users is
enforced. The host root account remains privileged and is not contained by any
of this.

### RAST authentication vs application accounts

Signing in to RAST says nothing about the applications. Each native app starts
with an empty, private browser/application profile per app and person
(`<data_dir>/linux-apps/profiles/<app>/<user>`, 0700) and keeps its own
sign-ins inside it. Signing out of RAST does not sign the person out of a
native app; ending the session or removing the profile is what resets it.

### Passwords and the missing keyring

A headless Xpra session has no unlocked OS keyring (no Secret Service, no
login keyring password). Chromium-family apps are therefore started with an
explicit Chromium `--password-store=basic`; letting Chromium auto-detect a
keyring would either hang on a keyring prompt or fall back silently. The
install preview shows this as a warning.

Consequences:

* Site passwords and cookies saved by the app are stored in Chromium's own
  profile store, protected by the person's Linux identity and the 0700 profile
  permissions — **not** by OS-level keyring encryption.
* Anyone who can read that profile (host root, a process of the same identity,
  or a backup that includes the data directory) can read those credentials.
* Do not store credentials in a native app that you would not keep in that
  profile.

## Files and folders

* **One-HOME accounts** (after `rast migrate home`, ADR 0005) have no app
  profiles, `RAST` link or grant: every app starts in the person's HOME
  `<storage_dir>/homes/<username>` with `HOME` set to it and the usual
  `~/.config`, `~/.local/share`, `~/.cache`, so a file an app saves is in
  File Station at once and every app sees the same settings. The rest of
  this list describes **legacy accounts**, which keep their profiles until
  they are migrated.
* The person's **own RAST home** (`<storage_dir>/homes/<username>`) is linked
  into the app profile as `RAST` and granted a recursive read + traverse ACL
  (`rx`) for the app identity. Native apps can open and read files from it, but
  they are **not** granted write access to it (the current implementation
  exposes the home read-only). Files an app saves itself go into its own
  profile/session storage, which File Station does not browse (it belongs to
  the app identity, not the `rast` service account).
* The grant runs in the background each time an app opens, so opening an app
  never waits for it; at most one grant runs per person at a time. It also
  sets a default ACL entry on every folder, so files and folders created in
  the home later are readable by the app at once. Files *moved* into the home
  (chunked uploads, restores from the recycle bin, moves from a shared folder)
  become readable when the next app opens. On the first app start the
  person's files can appear a few seconds after the window opens. The helper
  stops a grant after 13 seconds, so a home with more than about 1.5 million
  files and folders is only partly granted.
* **Shared folders are not mapped into native app profiles.** Only the
  person's own RAST home is exposed. To open a file from a shared folder in a
  native app, first copy it through File Station into the person's own RAST
  home, then open it in the app. To get a file *out* of an app, use the app's
  own export/upload function rather than expecting it in File Station.
* File Station's browser-side and server-side access checks protect the web
  interface. They do **not** apply to a native program: once a file is readable
  by the person's app identity (for example because it was copied into their
  home), the application can read or upload it wherever the app decides. Treat
  what a native app can read as shared with that app.
* **One HOME (in progress, ADR 0005).** The helper can already prepare a
  person's HOME `<storage_dir>/homes/<username>` as their own Linux account's
  directory (`home_ensure`: 0700, owned by `rast-u<id>`, with `~/.config`,
  `~/.cache`, `~/.local/…`) and start a per-user file agent for it
  (`files_agent_start`, unit `rast-files-u<uid>`). File Station then serves
  that person through the agent instead of the `rast` service. Accounts move
  to one HOME one at a time with `rast migrate home` (dry-run, apply,
  rollback; see [OPERATIONS.md](OPERATIONS.md)): the HOME's entries, the
  legacy identity home and the app profiles become the person's, merged
  into the HOME per file (`config` → `~/.config`, `data` →
  `~/.local/share`, `state` → `~/.local/state`, `cache` → `~/.cache`,
  `home` → `~/`; an entry the HOME already has stays in the old profile and
  is reported), and the recycle bin moves into XDG Trash. New accounts
  still start in legacy mode until the migration has been verified on
  production (ADR 0005, D8).
* **Shared folders as Linux groups (ADR 0005, section 4).** Every shared
  folder has two Linux groups, `rast-s<h>-rw` (read-write) and
  `rast-s<h>-ro` (read-only), where `<h>` is the first 16 hex digits of the
  share id. The server keeps their members in line with Effective Access:
  write or admin → `-rw`, read → `-ro`, deny, no access or a deactivated
  account → neither. A migrated container is `root:rast-s<h>-rw` 2770 with
  the ACL `g:rast-s<h>-ro:r-x` and `u:rast:rwx` (also as default ACL), so
  apps and terminals of members reach it with the kernel's own checks, and
  a one-HOME account finds it as `~/Shared/<name>` (a link its file agent
  keeps). Existing shared folders move to this layout with
  `rast migrate shares --share <name>` (dry-run, apply, rollback; see
  [OPERATIONS.md](OPERATIONS.md)).
  * Running apps and terminals keep the groups they started with. When an
    administrator revokes access, the person sees a warning and their apps
    and terminals restart after 60 seconds; when access is granted, the
    desktop offers *Restart session now*. File Station is right at once
    (its agent restarts immediately). During the warning, a shared folder
    the person lost entirely denies them by name (`u:rast-u<id>:---` on
    its container), which stops new paths into it but not a shell whose
    working directory is already inside, or files that are already open:
    the restart is what ends those. A session that still carries a lost
    group later (after a server restart) is found and restarted again.
  * **File permissions inside a shared folder are managed by RAST.** A
    repair pass (when the server starts, then once a day) gives every
    folder and file the shared folder's layout again: the read-write
    group, group read and write (and execute where the owner may execute),
    no access for other accounts, and the RAST ACL. The owner's own bits
    and the owner stay. A `chmod`, `chgrp` or `setfacl` inside a shared
    folder, from an app, a terminal or `cp -p`, therefore lasts only until
    the next pass: a file saved with mode 0600 (many apps do this for
    files they consider private) is readable and writable by every
    read-write member afterwards, and readable by read-only members.
    **Keep private files in HOME, not in a shared folder.** Until the
    pass, such a file is narrower than the layout (the group loses write
    access through the ACL mask, as on any Linux file server). The pass
    never changes symlinks, files with several hard links, entries owned
    by root or by accounts that are not RAST's, or other file systems and
    mounts.
* **Administrators reach every shared folder from their apps (D6).** An
  administrator is a member of every shared folder's read-write group, which
  keeps the File Station administrator bypass. The consequence is that
  *every* app and terminal of an administrator account can open every
  shared folder with the kernel's permission, not only File Station: a
  compromised or misbehaving app in an administrator session reaches all
  shared data. **Use a non-admin account for everyday app use** (browsing,
  mail, chat, downloaded tools), and keep the administrator account for
  administration.
* Application installs live under `<data_dir>/linux-apps/apps/<id>/<version>/`
  (portable/AppImage, read-only to app users) and per-person profiles under
  `<data_dir>/linux-apps/profiles/<app>/<user>/` (0700, owned by the identity;
  legacy accounts only). Uninstalling with *Also delete its settings* removes
  only those profiles, never files in RAST homes; a one-HOME account keeps
  an app's settings in its HOME, where the app put them.

### Recycle bin and quota (one-HOME accounts)

For an account with one HOME (ADR 0005), File Station's recycle bin is the
XDG Trash that its apps use: a file an app moves to the trash (GIO, KDE,
Electron) appears in File Station's recycle bin and can be restored there,
and what File Station deletes appears in the apps' trash. Deletions in a
shared folder stay in that folder (`.Trash-<uid>`, private to the person).
Entries whose original path points outside HOME or the person's shared
folders are restored to the top of HOME.

Apps, caches and the trash count against the quota. Without kernel quota
(the RAST fallback) apps are not stopped at the quota, but File Station and
App Center are; see "Recycle bin and quota of one-HOME accounts" in
`docs/OPERATIONS.md`.

## Windows and the desktop

* Every normal remote top-level window becomes **one RAST window** with its own
  title, focus, resize, minimize, maximize, close and taskbar entry — including
  additional Chrome windows.
* Dialogs, popups, menus and other transient/override-redirect windows are
  attached to their parent RAST window: they never get their own taskbar entry
  and follow the parent's minimize/close. A dialog that appears for a popup is
  parented to the window the popup belongs to.
* Closing a RAST window sends the application's normal close request, so its
  own unsaved-changes dialog appears inside the session as a dialog window.
  Terminating an app without that prompt is a separate force-quit action (see
  [lifecycle](#lifecycle-and-quitting)).
* Minimize hides the RAST window and the adopted surface; restore shows it
  again. The remote X11 window deliberately stays mapped: Xpra's
  `window-unmap`/`window-map` pair saves rendering work for hidden windows but
  loses the X input focus, so a restored application ignored keyboard input
  until it was clicked or the viewer reconnected. Correct input routing wins;
  minimized applications keep running and painting in the background (use
  **End application** to stop one).
* A maximize the application performs itself is mirrored on the RAST window
  (and the reverse direction is not forced onto X11 to avoid feedback loops).
* Window titles and other application-supplied metadata are escaped before the
  pinned Xpra HTML5 client renders them. Titles are untrusted text (a web page
  title, a file name or a credential prompt can contain HTML).

## Clipboard

Clipboard integration is **best-effort** in both directions:

* Copy inside a native app → the text may be offered to the browser clipboard
  through the Clipboard API. Browsers require clipboard permission and
  sometimes a user gesture; if they refuse, the copy silently stays inside the
  session.
* Copy in the browser → paste into a native app is forwarded when the browser
  makes clipboard data available (usually only for a paste event and only with
  clipboard permission).

RAST does not prompt for or override clipboard permissions. If copying fails in
one direction, use a file (for example through File Station and the RAST home)
instead of the clipboard.

## Keyboard shortcuts

Keyboard input is forwarded to the focused native window, but the browser keeps
control of its own shortcuts:

* Browser-reserved combinations (`Ctrl+W`, `Ctrl+T`, `Ctrl+N`, and similar) are
  captured for the native app only while a native window has focus, and only
  where the browser lets the page cancel them. Many browsers keep these for
  themselves (close/navigate the tab) regardless of the page.
* `F5`, `F11`, `F12` and the developer-tools shortcuts stay with the browser
  and the RAST shell.
* When the user is operating RAST controls (taskbar, start menu, dialogs, the
  session window itself), keyboard input stays with the RAST shell so those
  controls remain usable while a native window is focused. Clicking anywhere
  inside the application's surface moves focus back to the native window, so
  typing works after clicking a title-bar control or after minimizing and
  restoring the window.

There is no mode in which *all* browser shortcuts are intercepted; do not
configure an app expecting to disable tab management or the browser UI.

## High-DPI displays

The Xpra session is pinned at **DPI 96** and RAST adopts the remote canvases at
**1:1** scaling (canvas pixels equal CSS pixels). On a retina/HiDPI browser the
browser scales the whole canvas, so the image is enlarged rather than rendered
at the display's native density; text is not remastered at native resolution.
This is a known limitation of the current client integration. Lower the browser
zoom on the RAST tab if the remote windows look too small.

## Lifecycle and quitting

| Action | Browser / display | Running apps | How to resume |
| --- | --- | --- | --- |
| Close the RAST tab or lose the network | Display disconnects; the session and apps keep running | Keep running | Reopen RAST or reload: the page reconnects to the same session and its windows are rebuilt. The session is not duplicated. |
| Explicit sign-out | The RAST login session is revoked; a live display stream closes within ~5 s | Keep running — a session with a running app is never reclaimed by idle cleanup | Sign in again; the running session is adopted by the new display. |
| Session revoked / password reset by an administrator | Same as sign-out: the live stream closes within ~5 s | Keep running | Sign in again |
| Account deactivated | Live stream closes; the native session and its applications are stopped | Stopped | Reactivate the account, then launch again |
| Press the app's close button / close its RAST window | Window closes (the app may show its own unsaved-changes dialog) | That app exits; other apps/windows stay | Launch again |
| "End application" in App Center (or `POST /api/linux-apps/session/runs/{run}/close`) | All windows of that run disappear | That run's process group is stopped; other apps keep running | Launch again |
| Stop `rast-linux.slice` (maintenance) | Display closes | All native sessions and apps stop | Launch again (fresh session) |
| Reboot | Nothing survives | Fresh processes on next launch | Launch again |
| No viewer and no running app for 60 s + `linux_idle_minutes` | Session ended | None were running | Launch again (a fresh session) |

Closing the tab is only a disconnect, not "quit": a running application keeps
running. A session is never reclaimed while a viewer is attached or while an
application is running, so unsaved work is not discarded by the idle cleanup.
To end work deterministically, close the app's own window (normal quit, with
its save prompt) or use **End application**.

### Forced termination

* **End application** in App Center (More actions on the app) stops that run's
  whole process group and leaves the session and other apps running.
  API: `POST /api/linux-apps/session/runs/{run}/close` (the shorter
  `POST /api/linux-apps/session/runs/{run}` path is kept for compatibility).
* There is **no "End session" action in the App Center UI**. The API
  `POST /api/linux-apps/session/close` exists (and `POST
  /api/linux-apps/sessions/native/close`); the only UI route that stops a whole
  session is closing the fallback "Native desktop" whole-session window, which
  appears only when per-window adoption is unavailable. Operators who need to
  end every native session on the host should stop `rast-linux.slice` (see
  [OPERATIONS.md](OPERATIONS.md#linux-apps)).

### Multiple browsers, tabs and users

Each RAST user has exactly one graphical session. Two tabs or two browsers
signed in as the same user attach to the same session (standard Xpra sharing
semantics: both viewers are attached; the newest input wins focus and both see
the same windows). There are not two independent desktops for one account.
Different RAST users always get separate identities, displays and sessions.

## Audio

Playback is forwarded from the person's private PulseAudio null sink through the
same authenticated WebSocket; microphone forwarding, webcam and printing are
disabled. A browser autoplay policy may block sound until the user clicks inside
a native window. Sessions created by releases before the audio support keep
their old startup options and are recreated automatically on the first RAST
start after the upgrade (running apps in such a session are stopped; save work
before upgrading). See [INSTALL.md](INSTALL.md#native-linux-apps-xpra).

## Recovery

* **`rast.service` restarted or crashed:** the transient units keep running;
  the new server adopts the sessions and runs from `linux_sessions`/
  `linux_runs` and the systemd units. Displays and window keys survive.
* **`rast-native.service` stopped or restarted:** this does **not** stop the
  running transient session/application units — they are independent systemd
  units, not children of the helper. The display streams keep working (the
  proxy connects straight to the Xpra port); only helper-backed actions
  (launch, close, install) fail until the helper is back. On start the helper
  reconciles the nftables display-port set against the live `rast-xpra-*`
  units and the server adopts the still-running sessions.
* **Per-user file agents** (`rast-files-u<uid>`, one-HOME accounts only) are
  transient units in `rast-linux.slice` too. When the helper is down, a
  stopped agent cannot be started again and File Station answers `503` for
  that person until the helper is back.
* **To actually end everything:** stop `rast-linux.slice` (or the individual
  `rast-xpra-*`/`rast-app-*` units). A reboot also starts from fresh
  processes; profiles and installs stay on disk.
* **App crash:** its RAST windows disappear; other apps and the session stay.
* **Stuck app:** **End application** first; if the whole session is wedged,
  stop the session's unit or `rast-linux.slice` and launch again.

## Limitations

* Several viewers can be attached to one user's session; they share the same
  windows and input (standard Xpra sharing semantics). There is never a second
  independent desktop for the same account.
* Applications of one user share a trust boundary (same uid/session). Isolation
  between applications of the same user is not provided.
* Shared folders are not mapped into native apps; copy through File Station
  (see [Files and folders](#files-and-folders)).
* The RAST home is exposed to native apps **read-only** (`rx` ACL) in the
  current implementation; files an app writes stay in its profile/session and
  are not browsable through File Station. See
  [Files and folders](#files-and-folders).
* Chromium-family apps use `--password-store=basic` (no OS keyring; see
  [Passwords and the missing keyring](#passwords-and-the-missing-keyring)).
* DPI is pinned at 96 with 1:1 scaling; HiDPI browsers see an upscaled canvas.
* Browser-reserved shortcuts cannot all be captured; the browser UI stays
  reachable (see [Keyboard shortcuts](#keyboard-shortcuts)).
* Software rendering/encoding is the default; NVENC is available only with the
  optional `xpra-codecs-nvidia` package. GPU-backed X servers are not set up by
  RAST.
* Source archives are rejected by design: install a prebuilt `.deb`/AppImage or
  a prepared portable archive.
* One RAST instance per host may use the native backend (unit names are keyed
  by RAST user id).
* `.deb` uninstall fidelity depends on the host package manager: RAST removes
  only packages it added when nothing else depends on them.

## Related

* [INSTALL.md — Native Linux apps (Xpra)](INSTALL.md#native-linux-apps-xpra)
* [OPERATIONS.md — Linux apps](OPERATIONS.md#linux-apps)
* [API.md — Linux apps](API.md#linux-apps-app-id-applications-managing-them-is-administrator-only)
* [ARCHITECTURE.md](ARCHITECTURE.md)
* [SECURITY.md](../SECURITY.md)
* [native-linux-apps-handoff.md](native-linux-apps-handoff.md)
