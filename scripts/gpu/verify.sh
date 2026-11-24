#!/bin/bash
# verify.sh — report GPU / display / encoder capabilities of this host.
#
# Read-only: installs nothing, changes nothing, starts nothing. Copy-pasteable:
#   sudo scripts/gpu/verify.sh          # full report (sudo only for /dev + dmesg)
#   scripts/gpu/verify.sh | tee /tmp/gpu-report.txt
#
# Exit status: always 0 (this is a report, not a test unless a probe crashes).
set -u

have() { command -v "$1" >/dev/null 2>&1; }
section() { printf '\n=== %s ===\n' "$*"; }
kv() { printf '%-38s %s\n' "$1:" "$2"; }

section "host"
kv "uname" "$(uname -srm)"
if have nvidia-smi; then
    kv "nvidia-smi" "$(nvidia-smi --query-gpu=name,driver_version --format=csv,noheader 2>&1 | head -1)"
else
    kv "nvidia-smi" "missing"
fi

section "NVIDIA device nodes"
for f in /dev/nvidia0 /dev/nvidiactl /dev/nvidia-modeset /dev/nvidia-uvm; do
    if [ -e "$f" ]; then
        kv "$f" "$(ls -l "$f" | awk '{print $1, $3":"$4}')"
    else
        kv "$f" "MISSING"
    fi
done
for f in /dev/dri/card0 /dev/dri/card1 /dev/dri/renderD128 /dev/dri/renderD129; do
    if [ -e "$f" ]; then
        kv "$f" "$(ls -l "$f" | awk '{print $1, $3":"$4}')"
    fi
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
dirs=(/usr/lib/x86_64-linux-gnu /usr/lib /usr/local/cuda/lib64)
for extra in /usr/local/cuda-*/targets/x86_64-linux/lib; do
    [ -d "$extra" ] && dirs+=("$extra")
done
for lib in libnvidia-encode.so.1 libnvcuvid.so.1 libnvidia-fbc.so.1 libnvjpeg.so.13 libcuda.so.1; do
    found=""
    for d in "${dirs[@]}"; do
        if [ -e "$d/$lib" ]; then
            found="$d/$lib"
            break
        fi
    done
    if [ -n "$found" ]; then
        kv "$lib" "$found"
    else
        kv "$lib" "MISSING"
    fi
done

section "NVENC API version (driver) vs xpra build"
if have python3; then
    python3 - <<'PY' 2>&1 | sed 's/^/  /'
import ctypes
lib = None
for name in ("libnvidia-encode.so.1", "libnvidia-encode.so"):
    try:
        lib = ctypes.CDLL(name)
        break
    except OSError:
        continue
if lib is None:
    print("libnvidia-encode: cannot load")
else:
    version = ctypes.c_uint32(0)
    rc = lib.NvEncodeAPIGetMaxSupportedVersion(ctypes.byref(version))
    print(f"NvEncodeAPIGetMaxSupportedVersion: rc={rc} "
          f"version={version.value >> 4}.{version.value & 0xF}")
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
kv "Xorg binary" "$([ -x /usr/lib/xorg/Xorg ] && echo /usr/lib/xorg/Xorg || echo MISSING)"
kv "nvidia_drv.so" "$([ -f /usr/lib/x86_64-linux-gnu/nvidia/xorg/nvidia_drv.so ] && echo present || echo MISSING)"
kv "libglxserver_nvidia.so" "$([ -f /usr/lib/x86_64-linux-gnu/nvidia/xorg/libglxserver_nvidia.so ] && echo present || echo MISSING)"
kv "dummy_drv.so" "$([ -f /usr/lib/xorg/modules/drivers/dummy_drv.so ] && echo present || echo missing)"
if have vglrun; then kv "VirtualGL (vglrun)" "present"; else kv "VirtualGL (vglrun)" "missing"; fi

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
    systemctl list-units --no-legend 'rast-xpra-*' 'rast-app-*' 2>/dev/null | sed 's/^/  /'
fi
