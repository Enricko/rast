#!/lsiopy/bin/python3
"""RAST: install one Linux app into this image and describe it.

    rast-install <deb|appimage|tarball> <package file>

Runs once, as root, while the worker builds the app's image. It installs the
app, then writes:

  /rast/launch     the command that starts the app (run by the desktop session)
  /rast/meta.json  name, version, icon and update source, read by the worker

Problems the person can act on are printed as "RAST-ERROR: <message>" and end
the build with exit status 2.
"""

import base64
import json
import os
import shlex
import shutil
import struct
import subprocess
import sys
import tarfile

APP_DIR = "/opt/rast-app"
SOURCES_DIR = "/etc/apt/sources.list.d"
MAX_ICON_BYTES = 512 * 1024
# Chromium-based browsers and Electron apps: the container is the sandbox
# (Chromium's own sandbox needs privileges a container does not have).
CHROMIUM_FLAGS = [
    "--no-sandbox",
    "--test-type",
    "--no-first-run",
    "--password-store=basic",
    "--start-maximized",
]
FIELD_CODES = {"%f", "%F", "%u", "%U", "%d", "%D", "%n", "%N", "%i", "%c", "%k", "%v", "%m"}
SKIP_PROGRAMS = {
    "chrome-sandbox",
    "chrome_crashpad_handler",
    "crashpad_handler",
    "crashreporter",
    "updater",
    "update",
    "uninstall",
    "uninstaller",
    "glxtest",
    "vaapitest",
    "pingsender",
    "minidump-2-core",
}


class InstallError(Exception):
    pass


def fail(message):
    raise InstallError(message)


def run(cmd, **kwargs):
    subprocess.run(cmd, check=True, **kwargs)


def output(cmd):
    return subprocess.run(cmd, check=True, capture_output=True, text=True).stdout.strip()


def parse_desktop(path):
    """Keys of the [Desktop Entry] group of a .desktop file."""
    entry, group = {}, None
    try:
        with open(path, encoding="utf-8", errors="replace") as handle:
            for raw in handle:
                line = raw.strip()
                if not line or line.startswith("#"):
                    continue
                if line.startswith("["):
                    group = line
                    continue
                if group == "[Desktop Entry]" and "=" in line:
                    key, value = line.split("=", 1)
                    entry.setdefault(key.strip(), value.strip())
    except OSError:
        return {}
    return entry


def exec_argv(value):
    """Exec= value without field codes (Desktop Entry spec, "Exec key")."""
    try:
        parts = shlex.split(value)
    except ValueError:
        parts = value.split()
    return [part.replace("%%", "%") for part in parts if part not in FIELD_CODES]


def usable_entry(entry):
    return (
        entry.get("Type", "Application") == "Application"
        and entry.get("NoDisplay", "false").lower() != "true"
        and entry.get("Hidden", "false").lower() != "true"
        and bool(entry.get("Exec"))
    )


def pick_desktop(paths, hint):
    """The app's main launcher among its .desktop files."""
    best, best_score = None, None
    for path in paths:
        entry = parse_desktop(path)
        if not usable_entry(entry):
            continue
        stem = os.path.basename(path)[: -len(".desktop")].lower()
        score = 0
        if hint and stem == hint:
            score += 4
        elif hint and (hint in stem or stem in hint):
            score += 2
        if "handler" in stem or "url" in stem:
            score -= 3
        if best_score is None or score > best_score:
            best, best_score = (path, entry), score
    return best


def png_size(path):
    try:
        with open(path, "rb") as handle:
            head = handle.read(24)
        if head[:8] == b"\x89PNG\r\n\x1a\n":
            return struct.unpack(">II", head[16:24])
    except OSError:
        pass
    return None


def icon_score(path):
    """Prefer PNGs of 128-512 px, then SVG, then smaller PNGs."""
    if path.lower().endswith(".svg"):
        return 300
    size = png_size(path)
    if not size:
        return -1
    side = min(size)
    if side > 512:
        return 200
    return 400 + side if side >= 128 else side


def find_icon(name, roots):
    if not name:
        return None
    if os.path.isabs(name):
        return name if os.path.isfile(name) else None
    found = []
    for root in roots:
        if os.path.isfile(root):
            stem = os.path.splitext(os.path.basename(root))[0]
            if stem == name:
                found.append(root)
            continue
        for dirpath, _dirs, files in os.walk(root):
            for file in files:
                stem, ext = os.path.splitext(file)
                if stem == name and ext.lower() in (".png", ".svg"):
                    found.append(os.path.join(dirpath, file))
        if found:
            break
    found = [path for path in found if icon_score(path) >= 0]
    return max(found, key=icon_score) if found else None


def guess_icon(root, hints):
    """Any reasonable icon in an unpacked archive (tarballs rarely say)."""
    found = []
    for dirpath, dirs, files in os.walk(root):
        if dirpath[len(root):].count(os.sep) >= 6:
            dirs[:] = []
        for file in files:
            lower = file.lower()
            if not lower.endswith((".png", ".svg")):
                continue
            stem = os.path.splitext(lower)[0]
            if any(word in stem for word in ("icon", "logo")) or any(h and h in stem for h in hints):
                path = os.path.join(dirpath, file)
                if icon_score(path) >= 0:
                    found.append(path)
    return max(found, key=icon_score) if found else None


def read_icon(path):
    if not path:
        return None
    try:
        real = os.path.realpath(path)
        if os.path.getsize(real) > MAX_ICON_BYTES:
            return None
        with open(real, "rb") as handle:
            data = handle.read()
    except OSError:
        return None
    kind = "svg" if real.lower().endswith(".svg") else "png"
    if kind == "png" and not data.startswith(b"\x89PNG"):
        return None
    return {"type": kind, "data": base64.b64encode(data).decode("ascii")}


def resolve_program(program, base):
    if os.path.isabs(program):
        return program
    if base and os.path.exists(os.path.join(base, program)):
        return os.path.join(base, program)
    found = shutil.which(program, path="/usr/local/bin:/usr/bin:/bin:/usr/games:/opt/bin")
    return found or program


def is_chromium(paths):
    return any(os.path.basename(path) == "chrome-sandbox" for path in paths)


def walk_files(root, depth):
    for dirpath, dirs, files in os.walk(root):
        if dirpath[len(root):].count(os.sep) >= depth:
            dirs[:] = []
        for file in files:
            yield os.path.join(dirpath, file)


def is_elf(path):
    try:
        with open(path, "rb") as handle:
            return handle.read(4) == b"\x7fELF"
    except OSError:
        return False


def simplify(name):
    return "".join(ch for ch in name.lower() if ch.isalnum())


def common_prefix(a, b):
    size = 0
    for x, y in zip(a, b):
        if x != y:
            break
        size += 1
    return size


def guess_program(root, hint):
    """The program to start in an unpacked archive without a .desktop file."""
    hint = simplify(hint)
    best, best_score = None, None
    for path in walk_files(root, 2):
        name = os.path.basename(path)
        lower = name.lower()
        if lower in SKIP_PROGRAMS or ".so" in lower or lower.endswith((".py", ".pl", ".txt")):
            continue
        try:
            mode = os.stat(path).st_mode
        except OSError:
            continue
        if not mode & 0o111 or not os.path.isfile(path):
            continue
        rel = os.path.relpath(path, root)
        depth = rel.count(os.sep)
        score = 0
        if is_elf(path):
            score += 3
        if lower.endswith(".sh"):
            score += 1
        if depth == 0:
            score += 2
        elif rel.startswith("bin" + os.sep):
            score += 1
        prefix = common_prefix(simplify(os.path.splitext(name)[0]), hint)
        if prefix >= 3:
            score += 4 + min(prefix, 6)
        if best_score is None or score > best_score:
            best, best_score = path, score
    return best


def desktop_files_in(root, depth):
    return [path for path in walk_files(root, depth) if path.endswith(".desktop")]


def appimage_update_info(path):
    """The AppImage's embedded update information (`.upd_info` section)."""
    try:
        with open(path, "rb") as handle:
            ident = handle.read(16)
            if ident[:4] != b"\x7fELF":
                return None
            wide, order = ident[4] == 2, "<" if ident[5] == 1 else ">"
            if wide:
                handle.seek(0x28)
                (shoff,) = struct.unpack(order + "Q", handle.read(8))
                handle.seek(0x3A)
            else:
                handle.seek(0x20)
                (shoff,) = struct.unpack(order + "I", handle.read(4))
                handle.seek(0x2E)
            shentsize, shnum, shstrndx = struct.unpack(order + "HHH", handle.read(6))
            if shnum > 4096 or shentsize < (64 if wide else 40):
                return None

            def section(index):
                handle.seek(shoff + index * shentsize)
                data = handle.read(shentsize)
                if wide:
                    name, _t, _f, _a, offset, size = struct.unpack(order + "IIQQQQ", data[:40])
                else:
                    name, _t, _f, _a, offset, size = struct.unpack(order + "IIIIII", data[:24])
                return name, offset, size

            _n, names_offset, _s = section(shstrndx)
            for index in range(shnum):
                name, offset, size = section(index)
                handle.seek(names_offset + name)
                if handle.read(16).split(b"\0", 1)[0] == b".upd_info":
                    handle.seek(offset)
                    raw = handle.read(min(size, 1024)).split(b"\0", 1)[0]
                    return raw.decode("utf-8", "replace").strip() or None
    except (OSError, struct.error):
        return None
    return None


def parse_update_info(raw):
    if not raw:
        return None
    parts = raw.split("|")
    if parts[0] == "gh-releases-zsync" and len(parts) == 5:
        return {"kind": "github", "owner": parts[1], "repo": parts[2], "tag": parts[3], "file": parts[4]}
    if parts[0] == "zsync" and len(parts) == 2 and parts[1].startswith("https://"):
        return {"kind": "zsync", "url": parts[1]}
    return None


def install_deb(source):
    try:
        package = output(["dpkg-deb", "-f", source, "Package"])
    except subprocess.CalledProcessError:
        fail("This file is not a valid .deb package.")
    architecture = output(["dpkg-deb", "-f", source, "Architecture"])
    native = output(["dpkg", "--print-architecture"])
    if architecture not in ("all", native):
        fail(f"This package is built for {architecture}; this server needs {native}.")
    before = set(os.listdir(SOURCES_DIR)) if os.path.isdir(SOURCES_DIR) else set()
    env = dict(os.environ, DEBIAN_FRONTEND="noninteractive")
    run(["apt-get", "update"], env=env)
    try:
        run(["apt-get", "install", "-y", "--no-install-recommends", source], env=env)
    except subprocess.CalledProcessError:
        fail("The package could not be installed (see the build log for the missing dependencies).")
    after = set(os.listdir(SOURCES_DIR)) if os.path.isdir(SOURCES_DIR) else set()
    shutil.rmtree("/var/lib/apt/lists", ignore_errors=True)
    os.makedirs("/var/lib/apt/lists/partial", exist_ok=True)

    files = output(["dpkg-query", "-L", package]).splitlines()
    version = output(["dpkg-query", "-W", "-f=${Version}", package])
    description = output(["dpkg-query", "-W", "-f=${Description}", package]).split("\n")[0]
    desktops = [f for f in files if f.endswith(".desktop") and "/applications/" in f]
    picked = pick_desktop(desktops, package)
    if not picked:
        fail("This package has no app to open (it installs no desktop launcher).")
    path, entry = picked
    argv = exec_argv(entry["Exec"])
    if not argv:
        fail("The app's launcher has no command.")
    argv[0] = resolve_program(argv[0], None)
    icon_roots = [f for f in files if f.endswith((".png", ".svg"))] + [
        "/usr/share/icons/hicolor",
        "/usr/share/pixmaps",
    ]
    icon = find_icon(entry.get("Icon"), icon_roots)
    sources = sorted(after - before)
    return {
        "name": entry.get("Name") or package,
        "version": version,
        "package": package,
        "description": entry.get("Comment") or description,
        "exec": argv,
        "chromium": is_chromium(files),
        "launchGuess": False,
        "icon": read_icon(icon),
        "update": {"kind": "apt", "sources": sources} if sources else None,
    }


def install_appimage(source):
    work = "/tmp/rast-appimage"
    os.makedirs(work, exist_ok=True)
    image = os.path.join(work, "app.AppImage")
    shutil.copyfile(source, image)
    os.chmod(image, 0o755)
    update = parse_update_info(appimage_update_info(image))
    try:
        run([image, "--appimage-extract"], cwd=work, stdout=subprocess.DEVNULL)
    except (subprocess.CalledProcessError, OSError):
        fail("This AppImage could not be unpacked (it may be damaged or very old).")
    shutil.rmtree(APP_DIR, ignore_errors=True)
    shutil.move(os.path.join(work, "squashfs-root"), APP_DIR)
    shutil.rmtree(work, ignore_errors=True)
    os.chmod(APP_DIR, 0o755)

    picked = pick_desktop(desktop_files_in(APP_DIR, 1), None)
    entry = picked[1] if picked else {}
    apprun = os.path.join(APP_DIR, "AppRun")
    if not os.path.exists(apprun):
        fail("This AppImage has no AppRun program.")
    argv = [apprun] + exec_argv(entry.get("Exec", ""))[1:]
    icon = find_icon(entry.get("Icon"), [APP_DIR, os.path.join(APP_DIR, "usr/share/icons/hicolor")])
    if not icon and os.path.exists(os.path.join(APP_DIR, ".DirIcon")):
        icon = os.path.join(APP_DIR, ".DirIcon")
    files = list(walk_files(APP_DIR, 4))
    return {
        "name": entry.get("Name"),
        "version": entry.get("X-AppImage-Version"),
        "package": None,
        "description": entry.get("Comment"),
        "exec": argv,
        "chromium": is_chromium(files),
        "launchGuess": False,
        "icon": read_icon(icon),
        "update": update,
    }


def install_tarball(source):
    work = "/opt/rast-unpack"
    shutil.rmtree(work, ignore_errors=True)
    os.makedirs(work)
    try:
        with tarfile.open(source, "r:*") as archive:
            archive.extractall(work, filter="data")
    except (tarfile.TarError, OSError) as error:
        fail(f"This archive could not be unpacked ({error}).")
    entries = os.listdir(work)
    top = ""
    if len(entries) == 1 and os.path.isdir(os.path.join(work, entries[0])):
        top = entries[0]
        root = os.path.join(work, top)
    else:
        root = work
    shutil.rmtree(APP_DIR, ignore_errors=True)
    shutil.move(root, APP_DIR)
    shutil.rmtree(work, ignore_errors=True)
    os.chmod(APP_DIR, 0o755)

    picked = pick_desktop(desktop_files_in(APP_DIR, 3), simplify(top))
    files = list(walk_files(APP_DIR, 4))
    if picked:
        path, entry = picked
        argv = exec_argv(entry["Exec"])
        argv[0] = resolve_program(argv[0], os.path.dirname(path))
        if not os.path.exists(argv[0]):
            argv[0] = resolve_program(os.path.basename(argv[0]), APP_DIR)
        guessed = not os.path.exists(argv[0])
        icon = find_icon(entry.get("Icon"), [APP_DIR])
        name = entry.get("Name")
        description = entry.get("Comment")
    else:
        entry, guessed, name, description = {}, True, None, None
        program = guess_program(APP_DIR, top)
        if not program:
            fail("No program to start was found in this archive.")
        argv = [program]
        icon = None
    if guessed and picked:
        program = guess_program(APP_DIR, top)
        if not program:
            fail("No program to start was found in this archive.")
        argv = [program]
    if not icon:
        icon = guess_icon(APP_DIR, [simplify(top)[:6], simplify(os.path.basename(argv[0]))[:6]])
    if not name:
        name = os.path.basename(argv[0]).replace("-", " ").replace("_", " ").rsplit(".", 1)[0].title()
    return {
        "name": name,
        "version": None,
        "package": None,
        "description": description,
        "exec": argv,
        "chromium": is_chromium(files),
        "launchGuess": guessed,
        "icon": read_icon(icon),
        "update": None,
    }


def write_launch(meta):
    argv = list(meta["exec"])
    if meta["chromium"]:
        argv[1:1] = [flag for flag in CHROMIUM_FLAGS if flag not in argv]
    script = (
        "#!/bin/sh\n"
        "# Written by RAST when the app was installed.\n"
        'cd "$HOME/RAST" 2>/dev/null || cd "$HOME"\n'
        "exec " + " ".join(shlex.quote(arg) for arg in argv) + "\n"
    )
    with open("/rast/launch", "w", encoding="utf-8") as handle:
        handle.write(script)
    os.chmod("/rast/launch", 0o755)
    meta["exec"] = argv


def main():
    if len(sys.argv) != 3:
        print("usage: rast-install <deb|appimage|tarball> <package>", file=sys.stderr)
        return 64
    kind, source = sys.argv[1], sys.argv[2]
    installers = {"deb": install_deb, "appimage": install_appimage, "tarball": install_tarball}
    if kind not in installers:
        print(f"unknown format {kind}", file=sys.stderr)
        return 64
    try:
        meta = installers[kind](source)
        meta["format"] = kind
        write_launch(meta)
    except InstallError as error:
        print(f"RAST-ERROR: {error}", file=sys.stderr, flush=True)
        return 2
    with open("/rast/meta.json", "w", encoding="utf-8") as handle:
        json.dump(meta, handle)
    print(f"rast-install: {meta['name']} {meta.get('version') or ''} -> {' '.join(meta['exec'])}")
    return 0


if __name__ == "__main__":
    sys.exit(main())
