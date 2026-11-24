#!/bin/bash
# rast-xvfb-wrapper.sh — drop-in `xpra --xvfb=<this script>` that prefers a
# headless NVIDIA Xorg display and falls back to the stock Xvfb command.
#
# Contract (matches xpra's server start code, xpra/x11/vfb_util.py):
#   * xpra is given `--xvfb=/path/to/this/script`.
#   * xpra appends its own options to that command line:
#       -depth <N>            (when the depth is known)
#       -screen <W>x<H>x<D>   (no screen number because argv[0] != *Xvfb)
#       :<display>            (the display name, always last)
#     We parse the display + geometry from argv and ignore the rest.
#   * The child process is expected to own the display for the session
#     lifetime; xpra kills it when the session ends.
#
# NVIDIA path (only when every host check passes AND we are root):
#   /usr/lib/xorg/Xorg :<n> -config <generated conf> -noreset -nolisten tcp ...
#   with the NVIDIA driver, AllowEmptyInitialConfiguration + Virtual screen.
#
# Xorg needs root (VT/console setup: "parse_vt_settings: Cannot open /dev/tty0").
# RAST sessions run as the per-user identity (e.g. rast-u1), so this script
# reaches the NVIDIA path only when it is launched with root rights (privileged
# helper / narrow sudoers entry). Otherwise it execs the fallback Xvfb, which is
# the default and always-safe behaviour.
#
# Environment (all optional):
#   RAST_XVFB_BIN            fallback Xvfb binary (default /usr/bin/Xvfb)
#   RAST_GPU_XORG_CONFDIR    where generated configs are written
#                            (default /run/rast-gpu/xorg, root-owned)
#   RAST_GPU_LOG_DIR         where the Xorg log is written
#                            (default /run/rast-gpu/logs)
#   RAST_GPU_FORCE_XVFB=1    skip the NVIDIA path entirely
#   RAST_GPU_XVFB_EXTRA_ARGS args appended to the fallback Xvfb command line
#
# No secrets are read or written. Safe to run concurrently: every display gets
# its own config/log file and its own Xorg instance (the X server itself locks
# the display number).
set -u

self="$(readlink -f "${BASH_SOURCE[0]}")"
xorg_bin="${RAST_XORG_BIN:-/usr/lib/xorg/Xorg}"
xvfb_bin="${RAST_XVFB_BIN:-/usr/bin/Xvfb}"
confdir="${RAST_GPU_XORG_CONFDIR:-/run/rast-gpu/xorg}"
logdir="${RAST_GPU_LOG_DIR:-/run/rast-gpu/logs}"
nvidia_xorg_dir="${RAST_GPU_NVIDIA_XORG_DIR:-/usr/lib/x86_64-linux-gnu/nvidia/xorg}"

log() { logger -t rast-xvfb-wrapper -- "$*" 2>/dev/null || true; }

fallback() {
    # xpra appends "-screen <W>x<H>x<D>" (no screen number) for custom display
    # commands, but the Xvfb command line wants "-screen 0 <W>x<H>x<D>".
    # Translate that one shape so the fallback is a real drop-in; every other
    # argument is forwarded unchanged.
    local -a src=("$@")
    local -a fwd=()
    local i a next
    for ((i = 0; i < ${#src[@]}; i++)); do
        a="${src[i]}"
        if [ "$a" = "-screen" ]; then
            next="${src[i + 1]:-}"
            if [[ "$next" =~ ^[0-9]+x[0-9]+x[0-9]+$ ]]; then
                fwd+=("-screen" "0" "$next")
                i=$((i + 1))
                continue
            fi
        fi
        fwd+=("$a")
    done
    # Shellcheck excuse: we deliberately do not forward our extra env args.
    # shellcheck disable=SC2086
    exec "$xvfb_bin" "${fwd[@]}" ${RAST_GPU_XVFB_EXTRA_ARGS:-}
}

# ---------------------------------------------------------------- argv parsing
display=""
geometry=""
have_screen=0
args=("$@")
n=${#args[@]}
for ((i = 0; i < n; i++)); do
    a="${args[i]}"
    if [[ "$a" == "-screen" && $((i + 1)) -lt $n ]]; then
        have_screen=1
        candidate="${args[$((i + 1))]}"
        if [[ "$candidate" =~ ^([0-9]+)x([0-9]+)x([0-9]+)$ ]]; then
            geometry="$candidate"
        fi
        i=$((i + 1))
        continue
    fi
    # xpra appends "-screen WxHxD" without a screen number for custom commands
    if [[ "$a" =~ ^([0-9]+)x([0-9]+)x([0-9]+)$ ]]; then
        geometry="$a"
        have_screen=1
        continue
    fi
    if [[ "$a" =~ ^:[0-9]+$ ]]; then
        display="$a"
    fi
done

# ------------------------------------------------------------------ capability
nvidia_ok() {
    [ "${RAST_GPU_FORCE_XVFB:-0}" = "1" ] && return 1
    [ -x "$xorg_bin" ] || return 1
    [ -f "$nvidia_xorg_dir/nvidia_drv.so" ] || return 1
    [ -f "$nvidia_xorg_dir/libglxserver_nvidia.so" ] || return 1
    [ -c /dev/nvidia0 ] && [ -c /dev/nvidiactl ] || return 1
    command -v nvidia-smi >/dev/null 2>&1 || return 1
    # A driver that cannot talk to the GPU must not be used for an X server.
    nvidia-smi --query-gpu=name --format=csv,noheader >/dev/null 2>&1 || return 1
    return 0
}

if [ -z "$display" ]; then
    log "no display argument found, falling back to Xvfb"
    fallback "$@"
fi
nvidia_ok || fallback "$@"

if [ "$(id -u)" != "0" ]; then
    # Only log once per display to avoid noise; Xorg cannot start unprivileged.
    log "not root: cannot start Xorg for display $display, using Xvfb"
    fallback "$@"
fi

# ------------------------------------------------------------------ geometry
width=1920
height=1080
depth=24
if [ -n "$geometry" ]; then
    IFS='x' read -r gw gh gd <<<"$geometry"
    # Keep the requested screen size; clamp to what a headless NVIDIA GPU can
    # reasonably hold (VRAM and max framebuffer size).
    if [ "$gw" -ge 320 ] && [ "$gh" -ge 240 ]; then
        width=$gw
        height=$gh
    fi
    if [ "$gd" -ge 16 ]; then
        depth=$gd
    fi
fi
# xpra's default is 8192x4096; a full 8K shadow buffer is wasteful and slow for
# the NVIDIA path, so cap the virtual screen but keep it larger than any
# realistic single-app window.
[ "$width" -gt 4096 ] && width=4096
[ "$height" -gt 4096 ] && height=4096

# ------------------------------------------------------------- config/log dirs
mkdir -p "$confdir" "$logdir" || fallback "$@"
conf="$confdir/xorg-nvidia-headless-${display#:}.conf"
xlog="$logdir/Xorg-${display#:}.log"

cat >"$conf".tmp.$$ <<EOF || fallback "$@"
# Generated by $self for display $display ($width x $height x $depth).
Section "ServerLayout"
    Identifier     "rast-headless-$display"
    Screen      0  "rastscreen"
EndSection

Section "Files"
    ModulePath     "$nvidia_xorg_dir"
    ModulePath     "/usr/lib/xorg/modules"
EndSection

Section "ServerFlags"
    Option         "AutoAddDevices"    "false"
    Option         "AutoEnableDevices" "false"
    Option         "DontZap"           "true"
EndSection

Section "Monitor"
    Identifier     "rastmonitor"
    HorizSync       28.0 - 80.0
    VertRefresh     48.0 - 75.0
    Option         "DPMS" "false"
EndSection

Section "Device"
    Identifier     "rastgpu"
    Driver         "nvidia"
    BusID          "PCI:1:0:0"
    Option         "AllowEmptyInitialConfiguration" "true"
    Option         "UseDisplayDevice" "none"
    Option         "ModeValidation" "NoEdidModes,NoMaxPClkCheck,NoVertRefreshCheck,NoHorizSyncCheck,AllowNonEdidModes"
    Option         "HardDPMS" "false"
    Option         "Interactive" "false"
EndSection

Section "Screen"
    Identifier     "rastscreen"
    Device         "rastgpu"
    Monitor        "rastmonitor"
    DefaultDepth   $depth
    Option         "AllowEmptyInitialConfiguration" "true"
    SubSection     "Display"
        Depth       $depth
        Virtual     $width $height
    EndSubSection
EndSection
EOF
mv -f "$conf".tmp.$$ "$conf" || fallback "$@"

# The display number must be locked by the X server normally; remove a stale
# lock left behind by a crashed server so Xorg can bind the display.
if [ -e "/tmp/.X11-unix/X${display#:}" ] || [ -e "/tmp/.X${display#:}-lock" ]; then
    log "display $display already in use, falling back to Xvfb"
    fallback "$@"
fi

# Xorg appends its own timestamped log lines; keep one rotated log per display.
if [ -f "$xlog" ]; then
    mv -f "$xlog" "$xlog.previous" 2>/dev/null || true
fi

log "starting NVIDIA headless Xorg on $display (${width}x${height}x${depth})"
exec "$xorg_bin" "$display" \
    -config "$conf" \
    -logfile "$xlog" \
    -noreset \
    -nolisten tcp \
    -dpi 96
