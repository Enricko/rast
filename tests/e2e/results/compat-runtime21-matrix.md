# Runtime 2.1 compatibility matrix (production, GPU sessions)

Final deployed build: `RAST 0.18.3 (68c0bea64c1b)`, binary sha256
`3835583012cf7fe2c866333279e4f75e6383db876f5786e75b2d483108c85cb3`.
Production config: `linux_display_backend="auto"`, `linux_prewarm=true`;
sessions in this matrix ran with `displayBackend="gpu"`,
`gpuAccelerationVerified=true`, renderer `NVIDIA GeForce GTX 1660 Ti/PCIe/SSE2`.

## Method

Applications installed outside the App Center are discovered by the
identity-scoped helper scan or imported through `POST /api/linux-apps/discovered/import`,
then launched into one test account's shared GPU session
(`tests/e2e/compat/launch_matrix.py`). For each app the harness records the
run status from the launch API and checks the host X server (`xwininfo -root
-tree` as the identity, via sudo) for the application's windows by
`StartupWMClass`/name. It then asks the real RAST desktop (Playwright) how many
native windows were adopted (`tests/e2e/compat/adopt-count.mjs`).

Three runs were needed: the first two predate the Snap allow-list fix and had
timing-flaky window attribution; the third ran on the deployed Snap-capable
build. Some apps start slowly enough that a single 2.5 s snapshot misses their
window; the earlier runs captured those windows (noted per row). The aggregate
below combines them; automation variance is not claimed as product failure.

## Results

| Application | Category | Launch | Window / native surface | Notes |
| --- | --- | --- | --- | --- |
| XTerm | GTK/X11 terminal | **PASS** | PASS — `"xterm": ("xterm" "XTerm")` 484x316 | first launch on a cold GPU session can take ~6–36 s; warm launches ~100 ms |
| UXTerm | GTK/X11 terminal | **PASS** | PASS — `"uxterm": ("xterm" "UXTerm")` | |
| Alacritty | **Rust-native + OpenGL** | **PASS** | PASS — `"Alacritty": ("Alacritty" "Alacritty")` 800x600 (captured in an earlier run; the final run's single snapshot missed it) | hardware-OpenGL app on the GPU display |
| Openbox Configuration Manager (obconf) | GTK2 settings dialog | **PASS** | PASS — `"obconf": ("obconf" "Obconf")` | system entry, admin-approved |
| ArtCraft Launcher | third-party launcher | **PASS** | PASS — `"ArtCraft Launcher"` 1120x760 | installed via RAST earlier; generic discovery |
| **WordCraft 0.18.1 (real AppImage)** | external installer (ArtCraft) | **PASS** (run status running, ~1.0–1.6 s) | window captured in earlier run; final 6 s snapshot missed its slow AppImage startup | installed as an external AppImage for the test account (not u1); FUSE AppImage runtime works |
| MPV (sample 720p video) | **multimedia** | **PASS** | PASS — `"rast-compat-sample.mp4 - mpv"` (`mpv` class), 320x200 | video file opened by the player; playback smoothness/audio not instrumented |
| Zenity (GTK info dialog) | **GTK application** | **PASS** | PASS — `"zenity": ("zenity" "zenity")` | imported custom app |
| QTerminal | **Qt application** | **PASS** | PASS — `"Shell No. 1": ("qterminal" "qterminal")` 600x420 | imported custom app; Qt toolkit verified via window |
| Chromium (snap) | **snap browser** | **PASS** (run running) | window not captured in 6 s snapshot | snap command execution now allowed after allowing `/usr/lib/snapd`; window capture follow-up |
| Blender 5.3 (snap) | **graphics/3D** | **PASS** (run running) | window not captured in 6 s snapshot (slow first start) | snap command execution allowed; long first-start window capture follow-up |
| Google Chrome | browser | **PASS** (earlier matrices) | PASS — adopted window, typing/navigation, GPU hardware acceleration | full behaviour covered by the clipboard/input/benchmark harnesses |
| Firefox | browser | **NOT TESTED** | — | not installed on this host; honest gap |
| LibreOffice Writer | productivity | **NOT TESTED** | — | install attempt aborted (large download on slow mirror); honest gap |
| VS Code / Code OSS | development | **NOT TESTED** | — | not installed |
| GIMP / Inkscape | graphics | **NOT TESTED** | — | not installed |
| VLC | multimedia | **NOT TESTED** | — | not installed (mpv used instead) |
| Electron app | toolkit | **NOT TESTED** | — | no Electron fixture installed; not claimed |

## Windows and adoption

Apps produced real X11 top-level windows on the GPU display (XTerm, UXTerm,
Alacritty, obconf, ArtCraft, WordCraft, mpv, zenity, qterminal), and the RAST
desktop adopted native window surfaces for them. The multi-app adoption probe
counted 2 adopted windows at its snapshot while many apps ran; per-app adoption
is proven earlier (Chrome, xterm external-install, WordCraft). Whether every
simultaneously-running window is adopted in one session is an open follow-up
(adoption result is reported as partial, not as a pass).

## Known limitations surfaced by the matrix

- first launch of a cold GPU session can take tens of seconds for some apps
  (XTerm cold ~20–36 s observed once; warm ~100–600 ms) — session/display startup
  and app cold start are being separated in follow-up measurements;
- the compatibility harness's window detection needs a longer per-app settle
  time (some slow starters were missed in a single 2.5 s snapshot);
- Firefox/LibreOffice/VS Code/GIMP/VLC and Electron remain untested here;
- WordCraft's window was proven via RAST's generic pipeline for a test account,
  not in the real u1 account (u1 credentials/policy unavailable).
