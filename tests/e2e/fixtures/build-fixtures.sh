#!/usr/bin/env bash
# Build the RAST E2E fixtures. They are committed as source and (re)built on
# demand, never downloaded:
#
#   fixtures/rast-test-app            tiny GTK3 app (window "RAST Test App")
#   fixtures/rast-test-app-1.0.tar.gz portable archive with a .desktop entry
#   fixtures/invalid-corrupt.deb      a .deb that is not a Debian package
#   fixtures/wrong-arch.deb           a valid .deb for the wrong architecture
#   fixtures/invalid-archive.tar.gz   a truncated/corrupt archive
#   fixtures/rast-test-app.png        48x48 icon used by the archive
#
# The build needs gcc, pkg-config and the GTK3 development headers
# (Ubuntu: libgtk-3-dev). The invalid fixtures need dpkg-deb and python3.
# The fixtures are architecture-specific on purpose: they test the host they
# are built on (x86_64 here).
#
# Usage: tests/e2e/fixtures/build-fixtures.sh [--clean]
set -euo pipefail

HERE="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
SRC="$HERE/src"
APP="$HERE/rast-test-app"
ARCHIVE="$HERE/rast-test-app-1.0.tar.gz"
CORRUPT_DEB="$HERE/invalid-corrupt.deb"
WRONG_ARCH_DEB="$HERE/wrong-arch.deb"
INVALID_ARCHIVE="$HERE/invalid-archive.tar.gz"
ICON="$HERE/rast-test-app.png"

say() { printf '\033[1;34m[e2e/fixtures]\033[0m %s\n' "$*"; }
die() { printf '\033[1;31m[e2e/fixtures]\033[0m ERROR: %s\n' "$*" >&2; exit 1; }

if [ "${1:-}" = "--clean" ]; then
  rm -f "$APP" "$ARCHIVE" "$CORRUPT_DEB" "$WRONG_ARCH_DEB" "$INVALID_ARCHIVE" "$ICON"
  say "removed built fixtures"
  exit 0
fi

need() { command -v "$1" >/dev/null 2>&1 || die "missing command: $1"; }
need gcc
need pkg-config
need tar
need dpkg-deb
need python3
pkg-config --exists gtk+-3.0 || die "GTK3 development headers missing (install libgtk-3-dev)"

say "compiling the GTK3 fixture app"
# shellcheck disable=SC2046,SC2086
gcc -O2 -Wall -Wextra -o "$APP" "$SRC/rast-test-app.c" \
  $(pkg-config --cflags gtk+-3.0) $(pkg-config --libs gtk+-3.0)
strip "$APP" 2>/dev/null || true
say "built $APP ($(du -h "$APP" | cut -f1))"

say "generating the fixture icon"
python3 - "$ICON" <<'PY'
import struct, sys, zlib

path = sys.argv[1]
size = 48
rows = []
for y in range(size):
    row = bytearray([0])  # filter type 0
    for x in range(size):
        # Simple blue rounded-ish square with a lighter diagonal.
        edge = 4
        inside = edge <= x < size - edge and edge <= y < size - edge
        if inside:
            base = (58, 134, 255)
            if (x + y) % 17 < 4:
                base = (120, 190, 255)
            row += bytes(base + (255,))
        else:
            row += bytes((30, 30, 40, 255))
    rows.append(bytes(row))
raw = b"".join(rows)

def chunk(kind, data):
    return (struct.pack(">I", len(data)) + kind + data +
            struct.pack(">I", zlib.crc32(kind + data) & 0xFFFFFFFF))

png = b"\x89PNG\r\n\x1a\n"
png += chunk(b"IHDR", struct.pack(">IIBBBBB", size, size, 8, 6, 0, 0, 0))
png += chunk(b"IDAT", zlib.compress(raw, 9))
png += chunk(b"IEND", b"")
open(path, "wb").write(png)
PY
say "built $ICON"

say "building the portable archive"
stage="$(mktemp -d)"
trap 'rm -rf "$stage"' EXIT
root="$stage/rast-test-app-1.0"
mkdir -p "$root"
cp "$APP" "$root/rast-test-app"
cp "$ICON" "$root/rast-test-app.png"
cat >"$root/rast-test-app.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Version=1.0
Name=RAST Test App
GenericName=Test Application
Comment=GTK3 fixture used by the RAST end-to-end tests
Exec=rast-test-app
Icon=rast-test-app
Terminal=false
Categories=Utility;
StartupNotify=true
StartupWMClass=rast-test-app
EOF
chmod 755 "$root/rast-test-app"
tar -C "$stage" --owner=0 --group=0 -czf "$ARCHIVE" "rast-test-app-1.0"
say "built $ARCHIVE ($(du -h "$ARCHIVE" | cut -f1))"

say "building invalid fixtures"
printf 'This is not a Debian package.\n' >"$CORRUPT_DEB"
head -c 2048 /dev/urandom >"$INVALID_ARCHIVE"

wrong="$stage/wrong-arch"
mkdir -p "$wrong/DEBIAN"
mkdir -p "$wrong/usr/share/applications"
cat >"$wrong/DEBIAN/control" <<'EOF'
Package: rast-e2e-wrong-arch
Version: 1.0.0
Section: utils
Priority: optional
Architecture: arm64
Maintainer: RAST E2E <e2e@example.invalid>
Description: RAST E2E fixture built for the wrong CPU architecture
EOF
cat >"$wrong/usr/share/applications/rast-e2e-wrong-arch.desktop" <<'EOF'
[Desktop Entry]
Type=Application
Name=RAST Wrong Arch App
Exec=true
Terminal=false
EOF
dpkg-deb --build --root-owner-group "$wrong" "$WRONG_ARCH_DEB" >/dev/null
say "built $CORRUPT_DEB, $WRONG_ARCH_DEB, $INVALID_ARCHIVE"

say "fixtures ready"
ls -l "$HERE"
