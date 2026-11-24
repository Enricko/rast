#!/usr/bin/env python3
"""nvenc-probe.py — explain why (or whether) NVENC works on this host.

Two independent probes:
  1. The NVENC API version the installed driver supports
     (NvEncodeAPIGetMaxSupportedVersion).
  2. The version each installed encoder stack was built against:
       - the xpra nvenc module reports its build version in `xpra encoding`
       - ffmpeg's h264_nvenc negotiates the API version at runtime and works
         with any driver >= its build version.

A mismatch between (1) and xpra's build version is the documented reason this
host cannot use xpra's NVENC module even though the GPU supports NVENC.

Usage: python3 scripts/gpu/nvenc-probe.py [--encode-test]
Exit status: 0 if the driver reports NVENC support, 1 otherwise. The xpra
mismatch is reported but does not change the exit status.
"""
from __future__ import annotations

import ctypes
import shutil
import subprocess
import sys


def load_encode_lib():
    for name in ("libnvidia-encode.so.1", "libnvidia-encode.so"):
        try:
            return ctypes.CDLL(name), name
        except OSError:
            continue
    return None, None


def driver_max_version(lib) -> tuple[int, int] | None:
    version = ctypes.c_uint32(0)
    rc = lib.NvEncodeAPIGetMaxSupportedVersion(ctypes.byref(version))
    if rc != 0:
        return None
    return version.value >> 4, version.value & 0xF


def xpra_encoding_nvenc() -> list[str]:
    xpra = shutil.which("xpra")
    if not xpra:
        return ["xpra not installed"]
    try:
        out = subprocess.run(
            [xpra, "encoding"], capture_output=True, text=True, timeout=120, check=False
        )
    except (OSError, subprocess.TimeoutExpired) as exc:
        return [f"xpra encoding failed: {exc}"]
    lines = []
    for line in (out.stdout + out.stderr).splitlines():
        stripped = line.strip()
        lowered = stripped.lower()
        if any(k in lowered for k in ("nvenc", "nvdec", "nvjpeg", "nvfbc", "cuda")):
            lines.append(stripped)
    return lines


def ffmpeg_encode_test() -> str:
    ffmpeg = shutil.which("ffmpeg")
    if not ffmpeg:
        return "ffmpeg not installed"
    cmd = [
        ffmpeg, "-hide_banner", "-loglevel", "error",
        "-f", "lavfi", "-i", "testsrc2=size=1920x1080:rate=60", "-t", "2",
        "-c:v", "h264_nvenc", "-preset", "p1", "-f", "null", "-",
    ]
    try:
        proc = subprocess.run(cmd, capture_output=True, text=True, timeout=120, check=False)
    except (OSError, subprocess.TimeoutExpired) as exc:
        return f"ffmpeg h264_nvenc failed: {exc}"
    if proc.returncode == 0:
        return "ffmpeg h264_nvenc: OK (the GPU/ driver NVENC path works)"
    return f"ffmpeg h264_nvenc: FAILED\n{proc.stderr.strip()[-2000:]}"


def main() -> int:
    lib, libname = load_encode_lib()
    if lib is None:
        print("libnvidia-encode not installed: no NVENC runtime library")
        print("install: apt-get install libnvidia-encode-595")
        return 1
    print(f"loaded: {libname}")
    version = driver_max_version(lib)
    if version is None:
        print("NvEncodeAPIGetMaxSupportedVersion failed")
        return 1
    print(f"driver NVENC API version: {version[0]}.{version[1]}")

    print("\nxpra codec self-test (authoritative for the RAST runtime):")
    for line in xpra_encoding_nvenc():
        print(f"  {line}")

    if "--encode-test" in sys.argv:
        print("\nindependent hardware encode test:")
        print(f"  {ffmpeg_encode_test()}")

    print(
        "\nreading: if the xpra nvenc module was built against a newer NVENC API\n"
        "than the driver advertises, xpra fails with\n"
        "  'cannot open encoding session: ... invalid struct version ...'\n"
        "and the runtime must use x264. That is an API build mismatch, not a\n"
        "hardware limitation."
    )
    return 0


if __name__ == "__main__":
    sys.exit(main())
