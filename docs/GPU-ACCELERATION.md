# GPU acceleration & headless display backends on the RAST host

Status: measured on `lufria-server` (GTX 1660 Ti Mobile, driver 595.99.02) on
2026-11-24. Every number below was produced on this host; nothing is estimated.

> **Validated today: headless NVIDIA Xorg + Chrome hardware acceleration as root.**
> **NOT enabled in RAST production:** Xorg requires root, while per-user sessions
> run unprivileged (`rast-u<id>`); wiring that path needs the privileged helper.
> **The default display backend remains software Xvfb + x264.** The NVIDIA
> codec stack (`xpra-codecs-nvidia`, PyCUDA, `libnvidia-{encode,decode,fbc1}-595`,
> `libnvjpeg-13-1`) was **removed** from this host because Xpra 6.5.5 cannot use
> NVENC with driver 595.99.02 (NVENC SDK 13.1 vs driver API 13.0); do not
> reinstall it with this driver.

## TL;DR verdict

| Capability | State on this host | Evidence |
|---|---|---|
| NVIDIA GPU present and usable (CUDA 13.2, compute 7.5) | **yes** | `nvidia-smi`, `nvidia-smi --query-gpu=...` |
| NVENC silicon works | **yes** | `ffmpeg -c:v h264_nvenc` 1080p60, NVENC util 27 % |
| NVENC usable **through Xpra 6.5.5** | **NO — build/API mismatch** | `xpra encoding` self-test fails: `invalid struct version` |
| Headless NVIDIA **Xorg** display (`glxinfo` = NVIDIA, not llvmpipe) | **YES, verified** | `glxinfo -B` renderer `NVIDIA GeForce GTX 1660 Ti/PCIe/SSE2` |
| Chrome with hardware GPU on that Xorg display | **YES, verified** | `chrome://gpu`: `GL_RENDERER: ANGLE (NVIDIA Corporation, ...)` + Canvas/Compositing/Rasterization/WebGL/WebGPU `Hardware accelerated` |
| Chrome with hardware GPU on the default Xvfb path | **NO** (llvmpipe/SwiftShader) | Xvfb has no NVIDIA GLX |
| Xpra session whose display is the NVIDIA Xorg (end-to-end) | **YES** when the wrapper is launched as root | disposable session `:114`, `glxinfo` NVIDIA |

**What is accelerated today for RAST apps: nothing on the default path.**
The default RAST `--xvfb=Xvfb` sessions render with llvmpipe (software OpenGL)
and encode with `x264` (software). The GPU can be used, but only with two
integration changes that are *not* in the default path yet:

1. a root-launched headless Xorg display (this document + `scripts/gpu/rast-xvfb-wrapper.sh`),
2. and, for encode offload, either a driver with NVENC API 13.1+ or an
   `xpra-codecs-nvidia` built against NVENC API 13.0 — otherwise NVENC is
   unavailable to Xpra and x264 stays the encoder.

## 1. Capability detection (copy-paste, no guessing)

```bash
sudo scripts/gpu/verify.sh              # one-shot host report
scripts/gpu/verify.sh | tee /tmp/gpu.txt

# GPU + driver
nvidia-smi
nvidia-smi --query-gpu=name,driver_version,cuda_version,pci.bus_id --format=csv,noheader
nvidia-smi --query-gpu=utilization.gpu,utilization.encoder,power.draw --format=csv -l 1

# device permissions (the NVIDIA nodes are 0666 on this host)
ls -l /dev/nvidia0 /dev/nvidiactl /dev/nvidia-modeset /dev/nvidia-uvm
ls -l /dev/dri/          # card* root:video 0660, renderD* root:render 0660

# what Xpra can encode with
xpra encoding | grep -iE 'nvenc|nvdec|nvjpeg|nvfbc'
xpra encoding | sed -n '/codecs versions/,$p'

# NVENC API version of the driver vs. the Xpra module build
python3 scripts/gpu/nvenc-probe.py

# did the driver load cleanly?
sudo dmesg | grep -i nvidia | tail -20
```

## 2. What is installed on this host and what it does

NVIDIA driver stack (already present before this work, **unchanged**):

- `nvidia-headless-no-dkms-595` / `nvidia-kernel-common-595` / `nvidia-utils-595` 595.99.02
- kernel modules `nvidia`, `nvidia_modeset`, `nvidia_drm`, `nvidia_uvm` loaded
- `/dev/nvidia*` mode 0666, so **any** local user (including `rast-u<id>`)
  can open the GPU for CUDA/NVENC; no `video`/`render` group membership is
  needed for the NVIDIA device nodes. `/dev/dri/*` are `root:video`/`root:render`
  0660, so the Intel iGPU EGL/VA-API path *would* require those groups (the
  Intel path was not used by any working configuration below).

Added by this investigation (all additive, listed with exact commands):

```bash
# 1) NVIDIA codec runtime + Xpra codec module (matching the installed xpra 6.5.5)
sudo apt-get install -y --no-install-recommends \
    xpra-codecs-nvidia=6.5.5-r0-1 libnvidia-encode-595 libnvidia-fbc1-595 \
    libnvidia-decode-595 libnvjpeg-13-1
# 2) PyCUDA — required by *every* Xpra NVIDIA codec module (it imports pycuda
#    unconditionally). This drags the CUDA 12.4 toolkit (59 packages, ~1.5 GB):
sudo apt-get install -y --no-install-recommends python3-pycuda
# 3) headless Xorg + GL verification tooling (driver-side, independent of Xpra)
sudo apt-get install -y --no-install-recommends \
    xserver-xorg-core xserver-xorg-video-nvidia-595 xserver-xorg-video-dummy \
    mesa-utils x11-utils python3-xlib xterm
# 4) NVIDIA GLX/EGL user-space libraries. libnvidia-gl-595 ships
#    /usr/lib/x86_64-linux-gnu/nvidia/xorg/libglxserver_nvidia.so, which the
#    X server needs; without it glxinfo falls back to swrast/llvmpipe.
sudo apt-get install -y --no-install-recommends libnvidia-gl-595
```

Resulting versions:

```
xpra                     6.5.5-r0-1        (UNCHANGED — never upgraded)
xpra-codecs              6.5.5-r0-1        (UNCHANGED)
xpra-codecs-nvidia       6.5.5-r0-1
libnvidia-encode-595     595.99.02-0ubuntu0.26.04.1
libnvidia-fbc1-595       595.99.02-0ubuntu0.26.04.1
libnvidia-decode-595     595.99.02-0ubuntu0.26.04.1
libnvidia-gl-595         595.99.02-0ubuntu0.26.04.1
libnvjpeg-13-1           13.0.3.75-0ubuntu1
python3-pycuda           2025.1.2~dfsg-1build2
xserver-xorg-core        2:21.1.22-1ubuntu1.3
xserver-xorg-video-nvidia-595 595.99.02-0ubuntu0.26.04.1
```

`apt-get install -s` showed `0 upgraded` for every command above; the running
`xpra`, the NVIDIA driver and `rast.service` were never restarted. Installing
PyCUDA also installs `libnvidia-compute-590`, which on Ubuntu 26.04 is a
*transitional* package that depends on the already-installed
`libnvidia-compute-595` (it only ships docs), so it is not a driver change.

## 3. NVENC through Xpra 6.5.5: does NOT work (build/API mismatch)

`xpra encoding` after installing the NVIDIA codecs:

```
* nvenc  : .../xpra/codecs/nvidia/nvenc/encoder.cpython-314-x86_64-linux-gnu.so
* nvfbc  : .../xpra/codecs/nvidia/nvfbc/capture_linux...so
* nvenc                           : 13.1        <-- NVENC SDK version of the build
NVidia driver version 595.99.2
   device NVIDIA GeForce GTX 1660 Ti is not supported
   RuntimeError('cannot open encoding session: This indicates that an invalid
   struct version was used by the client., 0 contexts are in use')
no valid NVENC devices found
⚠️ dec_nvjpeg failed its self test
```

Root cause, measured with `scripts/gpu/nvenc-probe.py`:

- `NvEncodeAPIGetMaxSupportedVersion()` on driver 595.99.02 returns
  **13.0** (`0xd0`).
- `xpra-codecs-nvidia 6.5.5-r0-1` is built against **NVENC SDK 13.1** and
  requests 13.1 struct versions, which the 595.99 driver rejects with
  `NV_ENC_ERR_INVALID_VERSION` ("invalid struct version"). The self-test then
  marks the device unsupported and Xpra silently falls back to software
  encoders. `dec_nvdec` does not exist as a module in 6.5.5 at all
  (`No module named 'xpra.codecs.nvidia.nvdec'`).

This is **not** a silicon limitation: the same GPU encodes H.264 1080p60 with
system `ffmpeg`:

| Encoder | wall | CPU user+sys | GPU util mean | NVENC util mean | power | size (6 s, 8 M target) |
|---|---|---|---|---|---|---|
| `h264_nvenc -preset p1` | 4.04 s | **1.18 s + 0.24 s** | 3 % | 6 % (peaks 27 %) | 33 W | 6,563,233 B |
| `libx264 -preset ultrafast` | 1.13 s | **4.82 s + 0.17 s** | 0 % | 0 % | 8.6 W | 6,227,329 B |

The important number is CPU core-seconds: NVENC moves the encode off the CPU
(0.7 s vs 5.0 s for the identical source), even though wall-clock is shorter
for `ultrafast` x264 at this trivial resolution (NVENC is limited by GPU idle
clocks/rate control, and this GPU cannot use AV1 at all). `x264` remains the
production encoder until a compatible driver/module pair is installed.

**Consequence for production:** installing `xpra-codecs-nvidia` 6.5.5 makes
every new Xpra session log the `invalid struct version` error before falling
back to software codecs. Because the module is unusable and noisy, it is
**removed** from this host (see rollback below); it is harmless functionally but
must not be installed with driver 595.99.02.

## 4. Headless NVIDIA Xorg display: WORKS

### Start it

```bash
# Disposable, manual start (root required):
sudo scripts/gpu/xorg-nvidia-start.sh 110                # 1920x1080x24
sudo scripts/gpu/xorg-nvidia-start.sh 111 2560x1440x24
scripts/gpu/xorg-nvidia-start.sh 110 --check             # verify renderer
```

The script writes `/run/rast-gpu/xorg/xorg-nvidia-headless-<n>.conf`:

```
Section "Device"
    Driver      "nvidia"
    BusID       "PCI:1:0:0"                     # nvidia-smi pci.bus_id 0000:01:00.0
    Option      "AllowEmptyInitialConfiguration" "true"
    Option      "UseDisplayDevice" "none"
    Option      "ModeValidation" "NoEdidModes,NoMaxPClkCheck,NoVertRefreshCheck,NoHorizSyncCheck,AllowNonEdidModes"
EndSection
Section "Screen"
    DefaultDepth 24
    SubSection "Display"
        Depth 24
        Virtual 1920 1080                        # the whole virtual screen
    EndSubSection
EndSection
Section "Files"
    ModulePath "/usr/lib/x86_64-linux-gnu/nvidia/xorg"
    ModulePath "/usr/lib/xorg/modules"
EndSection
```

Key finding: **Xorg must run as root** (`parse_vt_settings: Cannot open
/dev/tty0 (Permission denied)` otherwise), and the X server must be able to load
`libglxserver_nvidia.so` from `libnvidia-gl-595`. Without it, Xorg starts but
`glxinfo` reports `llvmpipe` (swrast) even though the NVIDIA DDX loaded.

### Evidence

```
$ DISPLAY=:110 glxinfo -B
direct rendering: Yes
OpenGL renderer string: NVIDIA GeForce GTX 1660 Ti/PCIe/SSE2
OpenGL core profile version string: 4.6.0 NVIDIA 595.99.02
OpenGL version string: 4.6.0 NVIDIA 595.99.02
OpenGL ES profile version string: OpenGL ES 3.2 NVIDIA 595.99.02

$ DISPLAY=:110 glxgears -info
GL_RENDERER   = NVIDIA GeForce GTX 1660 Ti/PCIe/SSE2
162073 frames in 5.0 seconds = 32414.494 FPS   # llvmpipe: hundreds, not 32k

$ nvidia-smi        # proves the X server is a real GPU client
|  0  N/A  N/A   3927879      G   /usr/lib/xorg/Xorg      14MiB |
```

### Chrome on the headless display: hardware accelerated (chrome://gpu)

```bash
DISPLAY=:110 scripts/gpu/chrome-gpu-check.sh :110
```

`chrome://gpu` text extracted over the DevTools protocol (Chrome 158):

```
GPU0                 : VENDOR=0x10de, DEVICE=0x2191, DRIVER_VENDOR=Nvidia,
                       DRIVER_VERSION=595.99.02 *ACTIVE*
GL_VENDOR            : Google Inc. (NVIDIA Corporation)
GL_RENDERER          : ANGLE (NVIDIA Corporation, NVIDIA GeForce GTX 1660 Ti/PCIe/SSE2,
                       OpenGL 4.5.0 NVIDIA 595.99.02)
Ozone platform       : x11
Canvas               : Hardware accelerated
Compositing          : Hardware accelerated
Rasterization        : Hardware accelerated
WebGL                : Hardware accelerated
WebGPU               : Hardware accelerated
Video Decode         : Hardware accelerated
Video Encode         : Software only. Hardware acceleration disabled
Vulkan               : Disabled
```

So: real GPU rendering (2D/WebGL/WebGPU) inside Chrome on the headless
display; video encode stays software (the Xpra NVENC path is the same API
mismatch as above).

## 5. Xpra + headless NVIDIA Xorg end-to-end (validated, with a caveat)

Xpra can use any command as its display backend via `--xvfb=`. A drop-in
wrapper is provided:

```bash
# as root (or via sudoers NOPASSWD for the wrapper):
xpra start :114 --xvfb="sudo -n /home/lufria/RAST/scripts/gpu/rast-xvfb-wrapper.sh" ...
```

Validated on this host: session `:114` started, `xpra info :114` shows
`child.1.command='sudo', '-n', '.../rast-xvfb-wrapper.sh'`, `xdpyinfo :114` =
1920x1080 and `glxinfo :114` = `NVIDIA GeForce GTX 1660 Ti/PCIe/SSE2`.

**Caveat (deployment decision):** RAST sessions run as the per-user identity
(`rast-u<id>`), and Xorg refuses to run unprivileged. The wrapper therefore
takes the NVIDIA path only when it is launched with root rights; from a
`rast-u*` session it logs and `exec`s the stock Xvfb (safe fallback, verified:
`Xvfb -screen 0 1600x900x24 :113`, renderer llvmpipe). To bring the NVIDIA
display to production, the privileged helper (`rast-native.service`, which
already runs as root) must start the Xorg display for the session and let the
wrapper attach to it — or a narrow sudoers entry
`rast ALL=(root) NOPASSWD: /usr/local/sbin/rast-xvfb-wrapper.sh *` is added.
Neither is done by this document; both are integration changes for the backend.

### Wrapper contract (`scripts/gpu/rast-xvfb-wrapper.sh`)

- `xpra --xvfb=<wrapper>` appends `-screen <W>x<H>x<D>` and the display name;
  the wrapper parses those, ignores the rest, generates a per-display config
  under `/run/rast-gpu/xorg/`, and starts `/usr/lib/xorg/Xorg :<n> ...`.
- Host checks before using NVIDIA: `/usr/lib/xorg/Xorg` present,
  `nvidia_drv.so` + `libglxserver_nvidia.so` present, `/dev/nvidia0` +
  `/dev/nvidiactl` present, `nvidia-smi` works, display arg parsed, running as
  root. Any failure → `exec /usr/bin/Xvfb` with the xpra args translated to
  `-screen 0 <W>x<H>x<D>`.
- Per-display config/log files and Xorg's own display lock make concurrent
  sessions safe; the virtual screen is clamped to 4096x4096 for the NVIDIA path.
- No secrets; environment knobs: `RAST_GPU_FORCE_XVFB=1`,
  `RAST_XVFB_BIN`, `RAST_GPU_XORG_CONFDIR`, `RAST_GPU_LOG_DIR`.

## 6. Alternatives evaluated

| Option | Result |
|---|---|
| Xvfb (default) | works; **software** OpenGL (llvmpipe), x264 encode |
| Xorg + NVIDIA headless | **works** (root); NVIDIA OpenGL 4.6, Chrome hardware-accelerated |
| Xorg + dummy driver | not pursued: dummy cannot provide NVIDIA GLX (only llvmpipe) |
| Xvfb + Intel iGPU EGL | not pursued: would need `render`/`video` groups for `rast-u*` and Chrome/ANGLE EGL, and gives Intel UHD 630, not the 1660 Ti |
| VirtualGL | **not packaged** for Ubuntu 26.04 (`virtualgl` candidate: none); upstream `.deb` was not installed (out of scope, and Xorg+NVIDIA already works) |
| Xpra NVENC | module loads but self-test fails (API mismatch, §3) |

Not verified (do not claim): Xpra shadow/nvfbc capture, NVDEC decode,
NVJPEG, Vulkan (Chrome reports `Vulkan: Disabled`), GPU acceleration for
`rast-xpra-u1` production sessions (they still run `--xvfb=Xvfb` + x264), and
Chrome hardware decode (Xpra sessions stream H.264 to the browser; the browser
decoding profile was not measured).

## 7. Recommended runtime configuration surface

The backend already exposes `linux_display_backend=auto|software|custom` and
`linux_xvfb_command=<absolute script>`; this is the recommended mapping:

| Setting | Value | Behaviour |
|---|---|---|
| `linux_display_backend` | `software` (default) | `xpra --xvfb=Xvfb`, llvmpipe, x264 |
| `linux_display_backend` | `custom` | `xpra --xvfb=<linux_xvfb_command>`; script must be a drop-in for Xvfb and may fall back itself |
| `linux_xvfb_command` | `/usr/local/sbin/rast-xvfb-wrapper.sh` | packaged from `scripts/gpu/rast-xvfb-wrapper.sh`; NVIDIA path only when root-launched + host checks pass, otherwise Xvfb |
| encoder policy | `x264` today; `auto` only after a driver/module with NVENC API 13.1+ (or 13.0-built Xpra module) is validated | today `xpra encoding` self-test fails for nvenc, so the server negotiates x264/vpx/jpeg automatically; do not force `--encodings=h264` expecting NVENC |

Keep `software` as the default until the root-launched Xorg path is wired into
the privileged helper and the wrapper has been validated for `rast-u*`
sessions; then switch to `custom` behind the existing automatic fallback.

## 8. Host changes and rollback commands

Remove the Xpra codec stack (this is what the deploy does; re-adding it with
driver 595.99 is what produced the error spew):

```bash
sudo apt-get purge -y xpra-codecs-nvidia python3-pycuda \
    libnvidia-encode-595 libnvidia-fbc1-595 libnvidia-decode-595 libnvjpeg-13-1
# pycuda's dependency tree (CUDA 12.4 toolkit) can then be reclaimed:
sudo apt-get autoremove --purge -y
# verify xpra is untouched and back to software only:
xpra encoding | grep -i nvidia || echo 'no NVIDIA codecs (expected)'
dpkg-query -W xpra xpra-codecs
```

Driver-side headless-Xorg packages — keep **only** if the privileged Xorg path
is adopted; otherwise remove completely (the default Xvfb path never used them):

```bash
sudo apt-get purge -y xserver-xorg-core xserver-xorg-video-nvidia-595 \
    xserver-xorg-video-dummy libnvidia-gl-595 libnvidia-common-595 \
    libnvidia-egl-wayland1 mesa-utils x11-utils python3-xlib xterm
sudo apt-get autoremove --purge -y
```

Experiment leftovers (not part of the package state): `/run/rast-gpu/`,
`/tmp/xorg-nvidia-headless.conf`, any Xorg instances started on `:110/:112/:114`,
and the disposable Xpra sessions `:105` (as `rast`) / `:114` (as `lufria`).
Remove with:

```bash
# disposable Xpra sessions started by this investigation (NOT rast-* units)
sudo -u rast env HOME=/var/lib/rast XDG_RUNTIME_DIR=/var/tmp/rast-gpu-xpra-runtime xpra stop :105
xpra stop :114
sudo pkill -x Xorg            # only the headless Xorg instances from this doc
sudo rm -rf /run/rast-gpu /tmp/xorg-nvidia-headless.conf /var/tmp/rast-gpu-xpra-runtime
```

Production was never touched: `rast.service`, `rast-native.service`,
`/etc/rast/rast.toml`, `rast-xpra-u1` and the other live sessions were read-only
throughout.

## 9. Reproduce the evidence

```bash
sudo scripts/gpu/verify.sh > /tmp/gpu-report.txt
python3 scripts/gpu/nvenc-probe.py --encode-test
sudo scripts/gpu/xorg-nvidia-start.sh 110
DISPLAY=:110 glxinfo -B
DISPLAY=:110 scripts/gpu/chrome-gpu-check.sh :110
scripts/gpu/nvenc-bench.sh 1920x1080 60 6
```

## 2026-11-24 (runtime 2.1) — integration mechanics re-validated

Additional host measurements for the privileged-display milestone:

| Check | Result |
| --- | --- |
| Two concurrent headless NVIDIA Xorg displays on the single GTX 1660 Ti (displays :120 and :121) | **Both work**: `glxinfo -B` reports `NVIDIA GeForce GTX 1660 Ti/PCIe/SSE2` on each; the earlier concern that one GPU could only back one X server is disproved |
| Identity access to a GPU display | `rast-u127` connecting to a headless NVIDIA Xorg with an identity-owned `-auth` cookie gets `OpenGL renderer string: NVIDIA GeForce GTX 1660 Ti/PCIe/SSE2` |
| GTK application rendering on the GPU display | `xterm -e 'glxinfo -B'` run as `rast-u127` inside the NVIDIA display reports the **NVIDIA renderer**, i.e. application GL is genuinely hardware-accelerated (not llvmpipe) |
| Xpra on an existing display | `xpra start :N --use-display=yes` attaches to the already-running Xorg (no own `Xvfb-for-Xpra-N` process, HTTP 200), so a privileged display lifecycle can hand a GPU display to the unprivileged session |
| Xpra NVENC | still unavailable: xpra-codecs-nvidia 6.5.5 targets NVENC SDK 13.1 while driver 595.99.02 exposes 13.0 (`invalid struct version`); codec stack removed and must not be reinstalled |
| Stream encoding | unchanged: software webp/x264 through Xpra; GPU display does not change server-side encoding |

### Remaining work before production can use it (not implemented yet)

The experiment proves the rendering path, but production enablement needs a
privileged lifecycle that is deliberately **not** enabled:
1. `rast-native.service` must start/stop one headless Xorg per session (root,
   trusted `/usr/bin/Xorg` only, generated config, identity-owned xauth cookie,
   display allocation/validation, cleanup on stop) — never a user-supplied
   wrapper or argument.
2. `xpra start` then uses `--use-display=yes` instead of `--xvfb=Xvfb`; the web
   server must fall back to Xvfb automatically if the display fails.
3. Security review: X11 clients are unprivileged, but the Xorg server itself
   runs as root; a narrowly defined helper protocol (no arbitrary commands, no
   user config paths) and per-display auth isolation are required. This is the
   main reason it is not enabled by default.
4. End-to-end validation with two users, Chrome `chrome://gpu` inside the
   session, GTK/Qt apps, and a rollback path.

Production keeps `linux_display_backend = "software"` and
`gpuAccelerationVerified = false` until that lifecycle is implemented and
verified.

## 2026-11-24 (runtime 2.1) — privileged display lifecycle implemented

The privileged per-session GPU display is now implemented in the native
helper (`display_start` / `display_stop`):

- Only the trusted `/usr/bin/Xorg` (root-owned, not writable by others) and
  `/usr/bin/xauth` are executed; there is no user-supplied command or wrapper.
- The display is a transient `rast-display-u<id>` unit in `rast-linux.slice`,
  generated configuration is written root-only under `/run/rast-linux/.xorg/`,
  Xorg listens only on the UNIX socket (`-nolisten tcp`) and authenticates with
  a random per-display MIT-MAGIC-COOKIE written to the identity's
  `~/.Xauthority` (identity-owned) and to a root-only Xorg auth file.
- The helper waits for the display socket and then runs a real `glxinfo -B`
  **as the person's identity**; the session is only reported as GPU-backed
  (`displayBackend="gpu"`, `gpuAccelerationVerified=true`, `renderer` filled)
  when that probe reports an NVIDIA renderer. Otherwise the display is stopped
  and, with `linux_display_backend="auto"`, the session falls back to Xvfb; with
  the explicit `gpu` backend the session fails with a clear error instead of
  pretending acceleration.
- Xpra attaches with `--use-display=yes`; session stop (including recovery
  paths) stops the display unit and removes the credential/config files.
- The backend choice is written into the session row (migration 16) so a RAST
  restart that adopts a live session reports and cleans up the real backend.

Operator selection: `linux_display_backend = "auto" | "software" | "gpu"`.
`auto` prefers GPU when the filesystem probe says it is plausible and falls
back at session start if the real probe fails; `software` never tries GPU.
