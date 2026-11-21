# The RAST client

The first real slice of the rewrite is the **client shell**: a native Rune window
for Linux, Windows and macOS (and a WebGPU wasm build for the browser) that knows
which RAST server it belongs to and survives that server going away.

## Device login

In the browser the shell signs in to the **RAST server** (accounts with the
roles Super Admin, Admin and User, an `HttpOnly` session cookie, and a one-time
setup token for the first Super Admin; see [docs/AUTH-STORAGE.md](AUTH-STORAGE.md)).
Administrators manage accounts in **Control Panel → Users & Groups** (classic
RAST's users table with search and paging, and a menu per account for
temporary passwords, sessions, unlocking and disabling).
Super Admins also allow or deny apps for groups in its **Groups** view and per
account under **Apps & groups**. Under **Profile** everyone changes their name
and password, sees which apps they may open, and ends sessions on other devices.
Someone given a temporary password chooses their own on the sign-in page before
the desktop opens.

The native shell opens on a **device login**: the first launch asks for a username and a
password (8+ characters), later launches prefill the username, and *Sign out*
returns to the login screen. The classic RAST layout uses the original Alpine
Dawn wallpaper (CC0-1.0), a two-panel desktop view, and a single-column compact
view. In the browser, the form is built with HTML and CSS so zoom and mobile
scrolling do not resize or clear a Rune canvas. The Rune workspace opens only
after the local login succeeds. The password is hashed with **Argon2id** and
never stored in plaintext:

- natively: one `0600` file per OS (paths below).

This is the *device* unlock, not server authentication: the server will
authenticate users itself once the server phase lands. The full recommendation
(device file vs database, and why server accounts belong in SQLite) is in
[docs/AUTH-STORAGE.md](AUTH-STORAGE.md).

## Windows

Apps open in windows that behave as on the classic RAST desktop
(`crates/rast-ui/src/desktop/windows.rs` places them, `window.rs` draws them):

- Drag the title to move a window. Dropping it within 18 pt of the top edge
  maximizes it, of the left or right edge snaps it to that half; an outline
  previews the drop. Dragging a maximized or snapped window restores its own
  size under the pointer. Escape cancels a drag.
- The eight strips just outside a floating window's edges and corners resize it,
  down to the app's minimum (Control Panel: 420 × 380).
- Double-clicking the title maximizes or restores. New windows open at their
  app's size (Control Panel 1060 × 700) where it fits, cascading 26 pt down and
  right, and stay grabbable: at least 120 pt on the left, 80 pt on the right and
  the title bar under the top edge.
- A taskbar button per open window, in opening order: a click minimizes the
  front window, restores a minimized one or brings another to the front.
- At 820 pt wide or less (or under 420 pt high) windows fill the screen above
  the taskbar, only the front one shows, and they neither move nor resize.
- A 1 pt border rings a floating or maximized window, tinted with the accent on
  the front one; a snapped half keeps only its left and right sides and a
  phone's window has none. It takes room as the classic CSS border did: the
  title bar and the app lay out inside it, so nothing filled paints over it.

## Control Panel

Control Panel is laid out as classic RAST's (`desktop/control/shell.rs` draws the
frame, `control.rs`, `control/users.rs` and `control/groups.rs` the pages):

- A 214 pt sidebar of sections in the classic order: Profile and Appearance for
  everyone; Users & Groups, Shared folders, Storage, Components and Settings for
  administrators (a User sees a note that administrator sections are hidden).
  The open section is tinted with the accent and outlined; the others take a
  neutral tint under the pointer.
- When Control Panel is 820 pt wide or narrower, phones included, the sidebar
  becomes a 56 pt icon rail; the names stay the buttons' labels and show as
  tooltips.
- A header names the section, says what it holds and carries its action
  (*Add user*, *Create group*); the page scrolls under it. Users & Groups
  switches between its Users and Groups views (Groups for Super Admins).
- It opens on Profile. Shared folders, Storage, Components and Settings say
  plainly that the server doesn't offer them yet rather than show invented
  numbers. Without a server session the desktop app shows Connection and
  Appearance instead.
- Each page scrolls on its own and opens at its top: an account opened from a
  scrolled list, or the list after an account, doesn't open part-way down.

### Users table

*Users & Groups → Users* is classic RAST's table (`control/users.rs`):

- Columns: **User** (avatar, name, a *YOU* badge on your own row, `@username`),
  **Role** (Super Admin in the warning tone, Admin in the accent), **Groups**
  (two chips, then *+N*), **Status** (*Active*, *Disabled*, *Locked*, or
  *Password due* for someone holding a temporary password), **Last sign-in**
  (*just now*, *5 min ago*, *3 h ago*, *2 d ago*, then the date from 30 days;
  the exact time as a tooltip) and a **⋮** button. Classic RAST's Quota column waits until the
  server has storage.
- Above it: a search over username, display name, role and groups; the count
  (*1–25 of 40*); and rows per page (10, 25, 50 or 100). Each new search starts
  on the first page. Below it: *Page 1 of 2 · 40 items* with *Previous* and
  *Next*. When exactly one active Super Admin is left, a banner says RAST won't
  let it be demoted, disabled or deleted.
- **⋮** opens the account's menu: *Edit user*, *Reset password* (the account,
  scrolled to its temporary password field and ready to type), *View sessions*
  (the account, scrolled to its sessions), *Unlock account* for a locked one, and
  *Enable account* or *Disable account*, which asks for a second press. What you
  may not do (an Admin on a Super Admin, anyone on their own account) is dimmed.
  Double-clicking a row opens the account.
- In a Control Panel narrower than 680 pt, and on a phone, the table leaves out
  Groups and Last sign-in and scrolls sideways; a name too long for its column
  ends in "…" and keeps its *YOU*.

## Personalize

The desktop's look belongs to the signed-in account and is stored on the server
(`GET|PUT /api/appearance`, see [AUTH-STORAGE.md](AUTH-STORAGE.md#appearance)):

- **Colour mode**: Light, Dark, or System, which follows the browser's
  `prefers-color-scheme` live. The taskbar's sun/moon button flips the mode and
  saves it.
- **Accent**: ten presets (`crates/rast-ui/src/desktop/appearance.rs`), each with
  a light and a dark shade. The `Palette` in `desktop.rs` derives the soft fill,
  border, hover and text tints the way the classic CSS `color-mix()` did.
- **Wallpaper**: one of five bundled pictures or a solid colour (`#RRGGBB`), with
  fit (Cover, Contain, Center), one of nine positions and a 0–60 % dark overlay.

*Personalize…* (Start menu, account menu, or the desktop's right-click menu)
opens a dialog that previews every change on the live desktop; *Apply* saves,
*Cancel*, Escape or a click outside restores the saved look. Control Panel →
*Appearance* shows the same cards on a page with a sticky save bar. While a
save is in flight the controls wait for its answer; a refused save keeps the
preview and offers *Try again*. The browser build keeps the last saved look in
`localStorage` (`rast.appearance.v1.<account id>`) so the next sign-in paints it
before the server answers; the server's copy wins.

Right-click the desktop for *Personalize…*, *Show desktop*, *Open Control
Panel* (when the account may open it), *Keyboard shortcuts*, *About RAST* and
*Sign out* (`desktop/menu.rs`).

## What it does today

- Shows the connection state to the configured RAST server: *Connecting*,
  *Connected*, *Server unreachable*, or *Set up the server* when the address is
  invalid.
- Keeps running while the server is down, and **reconnects automatically** when
  the server comes back — on a backoff of 1 s, 2 s, 4 s, 8 s, 15 s, then every
  30 s. Once connected it re-checks every 10 s (`HEARTBEAT` in
  `crates/rast-ui/src/connection.rs`) so a server that goes away is noticed too.
- Offers a **Reconnect now** action for an immediate attempt, also while
  automatic reconnection is off.
- A **Settings** screen to change the server address and to turn automatic
  reconnection on or off. The choice is persisted immediately, so it survives
  restarts.
- The reachability check is a plain TCP connection to `host:port`, closed at
  once. It tells "server down" from "server up" and will be replaced by a real
  health/API check once the server exists.

Screens (rendered headlessly by `cargo run -p rast-ui --bin rast -- --snap …`):

![Client offline screen](screenshots/2026-11-21-client-offline.png)
![Client settings screen](screenshots/2026-11-21-client-settings.png)

## Turning automatic reconnection off (optional setup)

Three equivalent ways — the setting is optional and off means *stay offline
until the operator presses Reconnect*:

1. **In the app**: Settings → *Reconnect automatically* switch → **Apply**.
2. **One run**: `RAST_AUTO_RECONNECT=0 rast` (also `false`, `no`, `off`).
3. **By hand**: set `auto_reconnect = false` in the settings file below.

The app still reconnects with **Reconnect now** when automatic reconnection is
off. Nothing polls in the background in that mode.

## Settings file

| OS | Settings file | Device login |
| --- | --- | --- |
| Linux | `$XDG_CONFIG_HOME/rast/client.conf` (usually `~/.config/rast/client.conf`) | `~/.config/rast/device-account` (0600) |
| macOS | `~/Library/Application Support/rast/client.conf` | `~/Library/Application Support/rast/device-account` |
| Windows | `%APPDATA%\rast\client.conf` | `%APPDATA%\rast\device-account` |

`RAST_CONFIG=/path/to/client.conf` overrides the location — useful for portable
setups, test machines and packaging.

```ini
# RAST client settings. Edited by the app's Settings screen; safe to edit by hand.
server = desktop.lufria.tech:443
auto_reconnect = true
```

The server value accepts `host`, `host:port`, `https://host/path`, and bracketed
IPv6 (`[::1]:8720`). The port defaults to 443.

## Build and run

Requirements are the same as the shell in the [README](../README.md): Rust
1.99.1 (pinned) and, on Linux, `libxkbcommon-dev`, `libwayland-dev`,
`libxkbcommon-x11-0` and a Vulkan driver to actually draw.

```sh
cargo run -p rast-ui --bin rast            # native window
cargo test -p rast-ui                      # headless tests, no GPU/display
```

Cross-platform builds are CI jobs ([`.github/workflows/ci.yml`](../.github/workflows/ci.yml)):
`cross-platform` builds and tests on `ubuntu-24.04`, `windows-latest` and
`macos-latest`; `dist` packages release binaries for Linux and Windows on
`workflow_dispatch` or a tag. macOS packaging (a signed `.app` bundle) is not
set up yet: CI builds and tests the macOS binary, but shipping it to users needs
code signing/notarisation first.

Headless renders of every screen (used by CI):

```sh
VK_ICD_FILENAMES=/usr/share/vulkan/icd.d/lvp_icd.json \
  cargo run -q -p rast-ui --bin rast -- --snap out.png --scene offline
# scenes: offline, online, settings, setup
```

## What is deliberately not here yet

- Authentication and the real server API (the server itself is the next phase;
  the TCP probe stands in for it).
- Certificate validation: the client does not speak HTTP yet, so a TCP connect is
  all it can check. When the server exists, this becomes a pinned-HTTPS health
  check.
- Auto-start at login: the app is meant to be started by the user for now. If
  "turn on" should also mean *launch at login*, that is a small addition on top
  of this shell (desktop entry / LaunchAgent / Run key) and needs its own
  switch to turn it off.
