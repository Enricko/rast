# GPU-backed native sessions — production rollout evidence

Rollout build: `RAST 0.18.3 (4cc60dc0105e)` (first GPU-verified build). Final
runtime 2.1 build also verified: `RAST 0.18.3 (f1d96a0625fe)`, binary sha256
`292b07b60834ee456f382dc1c641e97ad92ceb971224a03d464e0286f2b62481`.
Production config: `linux_profile="balanced"`, `linux_display_backend="auto"`.
Rollout date: 2026-11-24.

## What is enabled

`linux_display_backend = "auto"` now prefers a privileged per-session NVIDIA
Xorg display and verifies it before reporting acceleration:

1. the native helper (`rast-native.service`, root) starts a transient
   `rast-display-u<id>.service` unit running only the trusted `/usr/bin/Xorg`,
   with a generated root-only configuration and a random per-display
   MIT-MAGIC-COOKIE written to the identity-owned `~/.Xauthority`; Xorg listens
   on the UNIX socket only (`-nolisten tcp`);
2. a headless RandR output is forced (`ConnectedMonitor/UseDisplayDevice`,
   candidates `DFP-0`, `HDMI-0`, `DP-0`) — without it Xpra clients were
   disconnected with `invalid output number 0, only 0 outputs`;
3. the helper runs a real `glxinfo -B` and `xrandr --query` **as the target
   identity**; only a reported NVIDIA renderer plus at least one connected
   output marks the session `displayBackend="gpu"`, `gpuAccelerationVerified=true`;
4. Xpra attaches with `--use-display=yes`; on any failure `auto` falls back to
   Xvfb (the strict `gpu` backend fails instead of pretending), and session end
   stops the display and removes its credential/config files;
5. the chosen backend is persisted in `linux_sessions` (migration 16) for
   restart adoption and cleanup.

## Verified on production

| Check | Result |
| --- | --- |
| `GET /api/linux-apps` runtime report | `display.active="gpu"`, `gpuDisplayAvailable=true`, `nvidiaXorgDriver=true` |
| Session (t-alice) | `displayBackend="gpu"`, `gpuAccelerationVerified=true`, `renderer="NVIDIA GeForce GTX 1660 Ti/PCIe/SSE2"`, display `:101` |
| Helper journal | `started a verified GPU-backed display user_id=127 display=101 renderer="NVIDIA GeForce GTX 1660 Ti/PCIe/SSE2"` |
| Host unit | `rast-display-u127.service` active: `/usr/bin/Xorg :101 -config /run/rast-linux/.xorg/xorg-u200127-101.conf -noreset -nolisten tcp -auth …` |
| Identity OpenGL (independent) | `glxinfo -B` as `rast-u127` on `:101`: `OpenGL renderer string: NVIDIA GeForce GTX 1660 Ti/PCIe/SSE2`, OpenGL 4.6.0 NVIDIA 595.99.02 |
| RandR output | `DP-0 connected primary 1920x1080+0+0` |
| Chrome's own verdict on `:101` | `GL_RENDERER: ANGLE(NVIDIA Corporation, NVIDIA GeForce GTX 1660 Ti/PCIe/SSE2, OpenGL 4.5.0 NVIDIA 595.99.02)`; Canvas, Compositing, Rasterization, WebGL, WebGPU and Video Decode **hardware accelerated**; Video Encode software only |
| Browser adoption + input | adopted `.linux-native-window` (focus on `.win-body`, not the hidden transport), `Ctrl+K` navigation reached remote Chrome (`about:blank`) |
| Concurrent users | t-alice `:102` and t-bob `:101` both `gpu`+verified simultaneously; two separate `rast-display-u*` units; per-identity `~/.Xauthority` 0600 owned by the identity |
| Cross-user session proxy | another account cannot read the other session URL (403/401 as before) |
| Cleanup on close | test sessions closed via API → both `rast-display-*` units stopped and removed |
| Xpra encoder | still software (NVENC unavailable: Xpra 6.5.5 module vs driver NVENC SDK mismatch); GPU rendering ≠ GPU encoding |

## Security review notes (honest)

- Xorg runs as root (it needs DRM/GPU access); X clients are unprivileged
  identities. Mitigations implemented: only root-owned trusted Xorg/xauth are
  executed (no user wrapper/arguments), generated config is root-only and
  contains no user input, UNIX-socket-only listener, random per-display
  cookie, separate display per identity, transient unit with memory/task
  limits, cleanup on stop. Residual risk: an X11-protocol exploit against Xorg
  would run with root privileges; the host has one trusted GPU stack and the
  helper is only reachable from the RAST service account. A future hardening
  step is a non-root Xorg (+logind/DRM lease) or a compositor-based backend.
- GPU VRAM is **not** isolated between accounts by cgroups (Linux offers no
  supported per-cgroup GPU memory isolation here); concurrent sessions share
  the GPU (observed ~285 MiB used while two sessions ran). GPU rendering is
  verified per session, stream encoding remains software and CPU-bound.
- `rast.service` runs with `PrivateDevices=yes`; GPU availability detection
  therefore uses the loaded driver (/proc/driver/nvidia/version) and the
  helper's real probe instead of device-node visibility.

## Rollback

Set `linux_display_backend = "software"` (or `custom`) in
`/etc/rast/rast.toml` and restart `rast.service`; new sessions use Xvfb.
Existing GPU sessions are only restarted when they end naturally (or after an
explicit `POST /api/linux-apps/session/close`). Full binary rollback follows
`docs/OPERATIONS.md` / the atomic deploy script.

## Final runtime 2.1 verification (build f1d96a0)

Same GPU path re-verified after the final deploy: production config
`linux_display_backend="auto"`, `linux_prewarm=true`; doctor reports
`display backend: gpu`; a pre-warmed GPU session reported
`displayBackend="gpu"`, `gpuAccelerationVerified=true`,
`renderer="NVIDIA GeForce GTX 1660 Ti/PCIe/SSE2"`, and the browser adopted its
window with `Ctrl+K` navigation working on the GPU-backed session. Discovered
application icons are also live (WordCraft icon served as a validated
3,350-byte PNG; 9/11 discovered rows have icons, the rest correctly fall back).
Multi-user isolation re-verified 6/6 on this build (cross-user session 403,
anonymous 401, per-identity session units, no raw display ports, display
firewall present).
