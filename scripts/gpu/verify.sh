#!/bin/bash
# verify.sh — report GPU / display / encoder capabilities of this host.
#
# Read-only: installs nothing, changes nothing, starts nothing. Copy-pasteable:
#   sudo scripts/gpu/verify.sh          # full report (sudo only for /dev + dmesg)
#   scripts/gpu/verify.sh | tee /tmp/gpu-report.txt
#
# Exit status: 0 always (this is a report, not a test), unless a probe crashes.
set -u

have() { command -v "$1" >/dev/null 2>&1; }
section() { printf '\n=== %s ===\n' "$*"; }
kv() { printf '%-38s %s\n' "$1:" "$2"; }

section "host"
kv "uname" "$(uname -srm)"
kv "nvidia-smi" "$(have nvidia-smi && nvidia-smi --query-gpu=name,driver_version,cuda_version --format=csv,noheader 2>&1 | head -1 || echo 'missing')"

section "NVIDIA device nodes"
for f in /dev/nvidia0 /dev/nvidiactl /dev/nvidia-modeset /dev/nvidia-uvm; do
    if [ -e "$f" ]; then
        kv "$f" "$(ls -l "$f" | awk '{print $1, $3":"$4}')"
    else
        kv "$f" "MISSING"
    fi
done
for f in /dev/dri/card0 /dev/dri/card1 /dev/dri/renderD128 /dev/dri/renderD129; do
    [ -e "$f" ] && kv "$f" "$(ls -l "$f" | awk '{print $1, $3":"$4}')"
done

section "kernel modules"
for m in nvidia nvidia_modeset nvidia_drm nvidia_uvm; do
    if grep -q "^$m " /proc/modules 2>/dev/null; then
        kv "$m" "loaded"
    else
        kv "$m" "not loaded"
    fi
done
kv "nvidia_drm modeset" "$(cat /sys/module/nvidia_drm/parameters/modeset 2>/dev/null || echo unknown)"

section "NVENC / NVDEC / nvJPEG runtime libraries"
for lib in libnvidia-encode.so.1 libnvcuvid.so.1 libnvidia-fbc.so.1 libnvjpeg.so.13 libcuda.so.1; do
    if have ldconfig; then
        path="$(ldconfig -p 2>/dev/null | awk -v l="$lib" '$1 == l {print $NF; exit}')"
    else
        path="$(echo /usr/lib/x86_64-linux-gnu/"$lib)"
        [ -e "$path" ] || path=""
    fi
    if [ -n "$path" ] && [ -e "$path" ]; then
        kv "$lib" "$path"
    else
        kv "$lib" "MISSING"
    fi
done

section "NVENC API version (driver) vs xpra build"
if have python3; then
    python3 - "$@" <<'PY' 2>&1 | sed 's/^/  /'
import ctypes, sys
lib = None
for name in ("libnvidia-encode.so.1", "libnvidia-encode.so"):
    try:
        lib = ctypes.CDLL(name); break
    except OSError:
        continue
if lib is None:
    print("libnvidia-encode: cannot load")
    sys.exit(0)
v = ctypes.c_uint32(0)
rc = lib.NvEncodeAPIGetMaxSupportedVersion(ctypes.byref(v))
print(f"NvEncodeAPIGetMaxSupportedVersion: rc={rc} version={v.value >> 4}.{v.value & 0xF}")
PY
else
    kv "python3" "missing"
fi
if have xpra; then
    xpra encoding 2>/dev/null | grep -iE 'nvenc|nvdec|nvjpeg|nvfbc' | sed 's/^/  /'
else
    kv "xpra" "missing"
fi

section "Xpra codec packages"
dpkg-query -W -f='${Package} ${Version}\n' 2>/dev/null |
    grep -E '^(xpra|xpra-codecs|xpra-codecs-nvidia|libnvidia-encode-595|libnvidia-fbc1-595|libnvidia-decode-595|libnvidia-gl-595|libnvjpeg-13-1|python3-pycuda) ' |
    sed 's/^/  /'

section "headless Xorg capability"
kv "Xorg binary" "$([ -x /usr/lib/xorg/Xorg ] && echo /usr/lib/xorg/Xorg || echo '"'"'MISSING'"'"')"
kv "nvidia_drv.so" "$([ -f /usr/lib/x86_64-linux-gnu/nvidia/xorg/nvidia_drv.so ] && echo present || echo '"'"'MISSING'"'"')"
kv "libglxserver_nvidia.so" "$([ -f /usr/lib/x86_64-linux-gnu/nvidia/xorg/libglxserver_nvidia.so ] && echo present || echo '"'"'MISSING'"'"')"
kv "dummy_drv.so" "$([ -f /usr/lib/xorg/modules/drivers/dummy_drv.so ] && echo present || echo missing)"
kv "VirtualGL (vglrun)" "$(have vglrun && echo present || echo missing)"

section "current GL renderer on DISPLAY=${DISPLAY:-unset}"
if have glxinfo && [ -n "${DISPLAY:-}" ]; then
    glxinfo -B 2>/dev/null | grep -E 'OpenGL (vendor|renderer|version) string|direct rendering' | sed 's/^/  /'
else
    echo "  (set DISPLAY and install mesa-utils to see the renderer)"
fi

section "groups of the current user"
id

section "RAST native sessions (read-only)"
if have systemctl; then
    systemctl list-units --no-legend 'rast-xpra-*' 'rast-app-*' 2>/dev/null | sed 's/^/  /' || true
fi
