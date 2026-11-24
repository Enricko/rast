# Runtime 2.2 — Phase 0 UI audit (production, build 8a08a9a3)

Date: 2026-11-24. Method: signed into the production desktop at
https://desktop.lufria.tech with the authorized `t-admin` test account in a
real headless Chromium, captured full-page screenshots at several viewports
(`docs/screenshots/runtime2.2-before/`) and recorded DOM inventories of the
desktop, Applications menu and App Center (`audit.json` in the same folder).
The last visually stable desktop reference is `docs/screenshots/desktop.jpg`
and `docs/screenshots/native-chrome-windows.png` (Runtime 2.0 era).

## Confirmed regressions and inconsistencies

### R1 — Discovered applications flood the desktop (critical)

The desktop shows **28 icons** including every externally discovered/imported
application: `GTK Zenity`, `MPV video`, `Qt QTerminal`, `VS Code (Electron)`,
`Firefox (tarball)`, `GIMP`, `LibreOffice Writer`, `VLC media player`,
`Openbox Configuration Manager`, plus `ArtCraft Launcher` and `WordCraft`
duplicates. Cause: the initial automatic desktop layout fills with *every app
the account may open* (`visibleApps()`), and Runtime 2.1 added every launchable
discovered entry to that app list
(`frontend/src/apps/linuxapp/index.ts` → `linuxAppDefinitions()` includes
`discoveredOpenableLinuxApps()`). The requirement "discovery must not flood the
desktop; applications belong in the Applications menu and App Center by
default" is violated.

### R2 — Duplicate application identities everywhere (critical)

The Applications menu (28 entries) and the desktop contain duplicates the
account cannot tell apart:

| Duplicate | Rows | Cause |
| --- | --- | --- |
| ArtCraft Launcher | managed app + discovered `.desktop` entry (same `/usr/bin/artcraft-launcher`) | no reconciliation between managed and discovered inventories |
| Google Chrome | managed app + discovered `google-chrome.desktop` (pending) + system `Chromium` | same |
| WordCraft | u1 profile AppImage entry + `WordCraft (RAST test)` fixture (owner 127) | a debug fixture was left in the production inventory |
| GIMP | manual import `/usr/bin/gimp` + xdg `GNU Image Manipulation Program` (pending, `gimp-3.2`) | import created during compat testing + the system desktop entry |
| LibreOffice | `LibreOffice Writer` (manual, ready) + `LibreOffice Writer` (xdg, pending) + `LibreOffice` startcenter (pending) — all `/usr/bin/libreoffice` | imports + two system desktop entries, no per-exec dedupe |
| QTerminal | `Qt QTerminal` (manual, ready) + `QTerminal` + `QTerminal Drop-down` (xdg, pending) — all `/usr/bin/qterminal` | same |
| VLC | `VLC media player` (manual, ready) + `VLC media player` (xdg, pending) — same exec and name | same |
| mpv | `MPV video` (manual, ready) + `mpv Media Player` (xdg, pending) — same exec | same |

`git grep` of the server inventory (28 discovered rows) is in the session log;
the App Center shows all of them, several with three inline action buttons.

### R3 — App Center discovered section exposes technical metadata in every row

Each discovered row currently renders, inline: install source, scope, owner id
(`owner #1`), the **full executable path in monospace**, and up to three
text buttons (`Launch/Approve`, `Hide`, `Forget`). With 28 rows the section is
a wall of paths and repeated buttons — the requirement asks for icon, name,
version, description, one Open action, with technical details in a secondary
view.

### R4 — New windows can fully cover earlier windows

The window manager cascades new top-level windows by only **26 px**
(`frontend/src/core/wm.ts` → `applyDefaultRect`, `(cascade % 7) * 26`). With
two same-sized windows the lower window’s title bar (≈34 px tall) ends up
entirely under the raised one: the E2E clipboard harness aborted with
"window win-3 is fully covered by another window" in production, and a user
cannot reach the covered window by clicking. Requirement: cascade placement
must keep every window’s title bar reachable.

### R5 — Test-fixture pollution of the production inventory

Rows `WordCraft (RAST test)`, `GTK Zenity`, `MPV video`, `Qt QTerminal`,
`VS Code (Electron)`, `Firefox (tarball)`, `GIMP`, `LibreOffice Writer` and
`VLC media player` (all `source: "manual"`) were created **by Runtime 2.1
compatibility testing on the production database**. `WordCraft (RAST test)`
duplicates the real account’s WordCraft. These are leftovers, not user
choices.

### Not regressions (checked, no action)

- No horizontal/vertical document overflow at 1440×900
  (`scrollWidth == clientWidth`).
- The “Installing…” strings seen in the DOM dump are inside `.pkg-progress`
  elements that carry the `hidden` class (invisible); the managed apps the API
  reports `ready`.
- Only one CSP console error (Cloudflare beacon blocked by script-src) —
  pre-existing, harmless.
- Taskbar shows pins + clock with no duplicate entries at rest.

## Repair plan (P0 → P1)

1. Desktop default layout excludes discovered entries (`linux:d-*`) — they
   stay in the Applications menu and App Center; explicit shortcuts still
   work (drag from Applications, saved layouts untouched).
2. Reconcile identities: a discovered entry whose executable or desktop-id
   matches a managed app is hidden from the launcher (menu/desktop/launch
   surface); within the discovered inventory, one visible row per executable
   (representative preferred: ready > approved > pending, xdg > manual).
3. App Center discovered rows become compact: icon, name, status when not
   "Ready", one short context line, one primary action (Launch or Approve),
   overflow menu (Unapprove/Hide/Forget/Details), and technical metadata
   (path, exec, owner, scope, install dir) in a collapsed details block.
4. Window cascade steps become 40 px so a raised window never covers an
   earlier window’s title bar.
5. Production inventory cleanup: delete the test-fixture imports (R5), rename
   `Firefox (tarball)` → `Firefox` and `VS Code (Electron)` → `VS Code`,
   approve the canonical system entries (GIMP, LibreOffice Writer, VLC, mpv,
   QTerminal) and delete their now-duplicate manual imports. Registry-only:
   no files are uninstalled.

## Phase 0 repair verification (build 4d25249..8e878e0, after screenshots in `docs/screenshots/runtime2.2-after/`)

| Regression | Fix | After-state evidence |
| --- | --- | --- |
| R1 desktop flood (28 icons) | `isDiscoveredDesktopId()` + default-layout filter (`desktop-icons.ts`) | 13 desktop icons: built-ins, package apps and the two managed Linux apps only; Applications menu and App Center retain every launchable discovered app (`01-desktop-1440x900.png`, menu 23 items) |
| R2 duplicates (menu + desktop) | `discoveredIsShadowed` / `reconciledDiscoveredApps` in the catalog; production registry cleanup (fixture imports deleted, canonical xdg entries approved, duplicates hidden) | menu contains each app exactly once (ArtCraft ×1, Google Chrome ×1, WordCraft ×1); `menu_no_duplicates` assertion passes |
| R3 App Center technical clutter | discovered entries render with the shared `pkg-tile` pattern; raw path/owner/scope moved into a details dialog; one primary action + overflow menu | tiles read “WordCraft — Installed by another app — Open”, “Alacritty — Desktop entry — Open”, “Htop — Waiting for approval — Approve” (`04-app-center-discovered-tiles.png`) |
| R4 covered windows | cascade 26 → 40 px (`wm.ts`) | two native windows open offset (Chrome 8,88 / ArtCraft 128,128); taskbar click minimizes and restores with the active indicator following focus (`05/06/08-*.png`) |
| R5 production test fixtures | registry-only cleanup via the discovered API (7 deletes, 5 approvals, 2 renames, 5 hides) + fixture files removed from the test account home | inventory now lists one record per executable; the live `rast-xpra-u1` session was untouched |

Additional fix found during verification: an adopted Chrome window attached to the
leftover discovered google-chrome entry (taskbar id `linux:d-f19b…`) instead of
the managed app. Window identity matching now considers every non-wrapper
command token, strips the `linux:` desktop-id prefix and uses the same
reconciliation as the launcher, so the taskbar shows `linux:google-chrome`.

Not regressed (verified): no document overflow at 1440×900 and 1280×720, the
start menu scrolls within its panel, the taskbar running-window lifecycle
(appears → click minimizes → click restores → active indicator) works, the
Terminal smoke passes on both deploys, and existing native sessions were
preserved across both deployments.

Remaining P1 (next rounds): Firefox keyboard focus in WM-less GPU sessions
(planned: invisible focus-helper started per session), real-account WordCraft
acceptance path, LibreOffice/VS Code/GIMP/VLC interaction matrices.
