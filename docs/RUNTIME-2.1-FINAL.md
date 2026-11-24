# RAST Universal Native Runtime 2.1 — final acceptance record

Deployed: **RAST 0.18.3 (8a08a9a39c7d)**, binary sha256
`7ce2b4c9993db5e73d393b405fdb826506875c501e357cf04a86f8db7653e1cb`,
installed as `/usr/local/lib/rast-releases/rast-8a08a9a39c7d1b5531871d44ee700d110d0d55af`
behind `/usr/local/bin/rast`. Production config: `linux_profile="balanced"`,
`linux_display_backend="auto"`, `linux_prewarm=true`.
Git: local `main` = `origin/main` = `40ac987` when this record was written; the
deployed binary's source revision is `8a08a9a3`, which is an ancestor with
identical source code (later commits are tests/docs only).

## Shipped in runtime 2.1

1. **Terminal / deployment lifecycle** — upgrade-safe executable resolution,
   helper/agent build handshake, versioned releases + atomic symlink, coordinated
   service restarts, real-browser Terminal smoke test with restart retry and
   I/O-pressure-aware timeout, automatic rollback; 5 binary-level regression tests.
2. **Universal external application discovery** — `.desktop`/XDG/AppImage/
   portable discovery as the identity via a privileged helper, separate
   `discovered_apps` inventory, approval policy, icons (validated PNG/SVG served
   from the authenticated icon endpoint), Import-existing-UI dialog, launch into
   the shared session with generic WM_CLASS/StartupWMClass window matching, live
   real-time events. WordCraft (ArtCraft-installed AppImage) is discovered
   generically; the real binary launches through the generic pipeline for a test
   account (u1 launch remains policy/credentials-blocked).
3. **Verified GPU-backed sessions** — privileged helper starts a per-session
   NVIDIA Xorg (trusted binaries, root-only generated config, per-display
   MIT-MAGIC-COOKIE, UNIX-socket-only, forced connected output), verifies the
   real NVIDIA renderer and a RandR output **as the person's identity**, attaches
   Xpra with `--use-display=yes`, persists the backend, cleans up on stop and
   falls back to Xvfb under `auto`. Chrome on such a session reports
   Canvas/Compositing/Rasterization/WebGL/WebGPU/Video-Decode hardware
   accelerated; two users run verified GPU sessions concurrently.
4. **Stream encoding** — NVENC remains unavailable (Xpra 6.5.5 module vs driver
   NVENC SDK mismatch); software encoding path documented and verified
   (h264/webp, lz4, adaptive batching). No unverified acceleration is claimed.
5. **Pre-warm** — hidden Xpra transport is deferred until the first native
   launch (no focus theft), pre-warm re-enabled; measured pre-warmed session
   ready ~2 s and click→window ~3 s on a quiet host, versus ~12 s cold path.
6. **Input/clipboard** — single-owner keyboard focus, macOS Command translation
   (emulated/partial), two-way text clipboard with explicit failure fallbacks;
   controlled-page matrix on the **final GPU build passes 18/18 tested checks
   with 0 failures** (macOS emulation recorded as not-tested); real macOS
   verification remains not-proven; text-only by design.
7. **Compatibility** — Chrome (full earlier matrix), Firefox tarball
   (window/adoption PASS; keyboard limited without a window manager), VS Code
   tarball (Electron, PASS), LibreOffice Writer (PASS), XTerm/UXTerm/Alacritty
   (Rust/OpenGL)/obconf (GTK2)/Zenity (GTK)/QTerminal (Qt)/MPV (video)/ArtCraft/
   WordCraft AppImage windows verified on the GPU display. GIMP and VLC also
   launch with real windows in verified GPU sessions. Snap packages are
   **not supported** for identity users (snapd policy) and are documented as
   such; GIMP/VLC/other untested applications are listed honestly.
8. **Multi-user / resources** — production isolation checks 6/6 (cross-user 403,
   anonymous 401, per-identity units, no raw display ports, display firewall);
   GPU VRAM is shared and not cgroup-isolated (documented); Xorg runs as root
   with documented residual risk.

## Acceptance evidence

- Rust: `cargo test --lib` **809 passed**; terminal regression **5/5**.
- Frontend: `tsc --noEmit` clean, `npm test` **207/207**.
- Deployment: Terminal smoke PASS on every successful deploy; versioned release,
  health/helper/session-preservation checks; **automatic rollback** exercised on
  two real failures earlier in the project.
- **Manual rollback/redeploy test** (`tests/e2e/results/rollback-acceptance-*.md`):
  switched to the previous release (`3d0823fa`), health 200, services active,
  live user session preserved, Terminal smoke PASS; restored current release
  (`8a08a9a3`), health 200, session preserved, Terminal smoke PASS.
- Benchmarks: before/after §7.3 is authoritative; final acceptance run under
  host-I/O pressure is recorded with its caveat (`docs/BENCHMARKS.md` §11/§12).
- Compatibility evidence: `tests/e2e/results/compat-runtime21-matrix.md` and
  related JSON/screenshots; GPU evidence `gpu-display-rollout-4cc60dc.md`.
- `origin/main` verified equal to local `main`, working tree clean, no unpushed
  commits.

## Known limitations / remaining work (honest)

- NVENC stream encoding unavailable on this driver/Xpra combination; software
  encoding only.
- Firefox keyboard input needs a minimal internal window manager/focus helper;
  Firefox window creation/adoption works.
- Real macOS/Windows clipboard behaviour cannot be verified on this host
  (macOS emulation partial/not-proven); rich-text/image clipboard unsupported.
- WordCraft launch inside the real u1 account was not attempted (no credentials/
  policy); the generic pipeline is proven with the real AppImage on a test account.
- Snap-packaged apps cannot run for identity users (snapd restriction).
- Firefox keyboard focus requires a minimal internal window manager/focus
  helper; fully overlapping adopted windows should be cascaded or raised on
  click (the harness now raises them via the RAST window DOM like a taskbar
  click); LibreOffice document-window interactions, VS Code typing, audio
  audible verification and very large-session adoption are follow-ups.
- Host disk I/O pressure from co-located workloads can slow cold session starts;
  the runtime adapts with a bounded 75 s readiness window and scaled deploy smoke,
  but normal-load numbers are the representative ones.
- GPU VRAM is shared (no supported per-cgroup GPU memory isolation) and Xorg
  runs as root with a documented residual X11 attack surface; hardening (non-root
  Xorg/logind DRM lease or compositor backend) is future work.
