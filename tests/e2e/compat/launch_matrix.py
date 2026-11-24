#!/usr/bin/env python3
"""Runtime 2.1 compatibility matrix: launch externally-discovered/system apps
into one test account's shared GPU session and record windows.

Run as the repo owner (uses sudo -n read-only for xwininfo as the identity).
Writes tests/e2e/results/compat-runtime21-<stamp>.json/.md and prints a table.
"""
import json, os, subprocess, sys, time, datetime

BASE = "http://127.0.0.1:8720"
PUBLIC = "https://desktop.lufria.tech"
CREDS = "/home/lufria/.config/rast-test/creds"
USER = "t-alice"
ADMIN = "t-admin"
RESULTS = "/home/lufria/RAST/tests/e2e/results"

def creds():
    out = {}
    with open(CREDS) as fh:
        for line in fh:
            line = line.strip()
            if "=" in line:
                k, v = line.split("=", 1)
                out[k] = v
    return out

class Api:
    def __init__(self, user, password):
        self.user = user
        self.jar = f"/tmp/rast-compat-{user}.cookies"
        self.csrf = ""
        self.login(password)

    def curl(self, path, method="GET", body=None):
        cmd = ["curl", "-sS", "-c", self.jar, "-b", self.jar, "-X", method,
               "-H", "Origin: " + PUBLIC, "-H", "content-type: application/json",
               "-w", "\n%{http_code}", BASE + path]
        if self.csrf and method != "GET":
            cmd[2:2] = ["-H", "X-CSRF-Token: " + self.csrf]
        if body is not None:
            cmd += ["--data-binary", json.dumps(body)]
        raw = subprocess.run(cmd, capture_output=True, text=True, timeout=120).stdout
        text, _, code = raw.rpartition("\n")
        try:
            data = json.loads(text) if text else None
        except json.JSONDecodeError:
            data = {"raw": text}
        return int(code or 0), data

    def login(self, password):
        code, data = self.curl("/api/auth/login", "POST", {"username": self.user, "password": password})
        assert code == 200, f"login {self.user} -> {code} {data}"
        self.csrf = data.get("csrfToken", "")
        return data

    def close_session(self):
        return self.curl("/api/linux-apps/session/close", "POST", {})

    def wait_no_session(self, limit=30):
        for _ in range(limit):
            _, d = self.curl("/api/linux-apps")
            s = (d or {}).get("session") or {}
            if not s or s.get("status") in ("ended", None):
                return True
            time.sleep(0.5)
        return False

def host_snapshot(display):
    home = f"/var/lib/rast/linux-users/127"
    cmd = ["sudo", "-n", "-u", "rast-u127", "env", f"DISPLAY=:{display}",
           f"XAUTHORITY={home}/.Xauthority", "xwininfo", "-root", "-tree"]
    out = subprocess.run(cmd, capture_output=True, text=True, timeout=20).stdout
    windows = []
    for line in out.splitlines():
        line = line.strip()
        if '"' in line and not line.startswith(("xwininfo", "Root", "Parent", "Window id")):
            windows.append(line)
    return windows

def main():
    c = creds()
    admin = Api(ADMIN, c[ADMIN])
    alice = Api(USER, c[USER])
    alice.close_session(); alice.wait_no_session()

    # sys
    admin.curl("/api/linux-apps/discovered/scan", "POST", {})
    code, data = admin.curl("/api/linux-apps/discovered")
    apps = {a["name"].lower(): a for a in data.get("apps", [])}
    results = []
    session = {}
    def launch(name, entry, note=""):
        t0 = time.time()
        code, resp = alice.curl(f"/api/linux-apps/discovered/{entry['id']}/launch", "POST", {})
        rec = {"app": name, "id": entry["id"], "http": code, "note": note, "windows": [], "launchMs": None, "error": None}
        if code != 200:
            rec["error"] = f"launch HTTP {code}: {str(resp)[:200]}"
            results.append(rec)
            print(f"FAIL {name}: {rec['error']}")
            return rec
        rec["launchMs"] = int((time.time() - t0) * 1000)
        appId = (resp.get("run") or {}).get("appId") or entry["id"]
        deadline = time.time() + 90
        run = None
        while time.time() < deadline:
            _, ov = alice.curl("/api/linux-apps")
            session = (ov or {}).get("session") or {}
            runs = session.get("apps") or []
            run = next((r for r in runs if r.get("appId") == appId), None)
            if run and run.get("status") == "running":
                break
            if run and run.get("status") in ("failed", "exited"):
                break
            time.sleep(1)
        rec["status"] = (run or {}).get("status")
        # Collect the full X window list once and attribute windows to this app
        # by WM class / name token: timing-diffs between launches mis-attribute
        # windows that are mapped late by a previous application.
        time.sleep(2.5)
        _, ov = alice.curl("/api/linux-apps")
        session = (ov or {}).get("session") or {}
        display = session.get("display")
        windows = host_snapshot(display) if display else []
        tokens = []
        for token in (entry.get("startupWmClass"), entry.get("name"), name):
            if token:
                tokens.append(str(token).lower())
        if entry.get("exec"):
            tokens.append(str(entry["exec"][0]).rsplit("/", 1)[-1].lower())
        matched = [w for w in windows if any(tok and tok in w.lower() for tok in tokens)]
        # Avoid the tiny 1x1/selection-owner helper windows when a real window
        # of the same application exists.
        substantial = [w for w in matched if "1x1+" not in w and "3x3+" not in w and "10x10+" not in w]
        rec["windows"] = substantial or matched
        results.append(rec)
        print(f"{'PASS' if rec['windows'] else 'PARTIAL'} {name} status={rec['status']} windows={len(rec['windows'])} {rec['windows'][:2]}")
        return rec

    def find(name):
        return apps.get(name.lower())

    for name, note in [
        ("XTerm", "system desktop entry"),
        ("UXTerm", "system desktop entry"),
        ("Alacritty", "Rust/OpenGL native app"),
        ("Openbox Configuration Manager", "GTK2 settings dialog"),
        ("Blender", "snap, graphics/3D (slow start)"),
        ("Chromium Web Browser", "snap browser"),
        ("ArtCraft Launcher", "third-party launcher (system entry)"),
    ]:
        entry = find(name)
        if not entry:
            results.append({"app": name, "status": "not-in-inventory", "windows": [], "note": note})
            print(f"SKIP {name}: not discovered")
            continue
        if not entry.get("approved"):
            admin.curl(f"/api/linux-apps/discovered/{entry['id']}", "PATCH", {"approved": True})
        launch(name, entry, note)

    # WordCraft (real AppImage installed as an external application in this test account)
    wc = next((a for a in apps.values() if "wordcraft" in a["name"].lower() and "test" in a["name"].lower()), None) \
         or next((a for a in apps.values() if "wordcraft" in a["name"].lower()), None)
    if wc:
        if not wc.get("approved"):
            admin.curl(f"/api/linux-apps/discovered/{wc['id']}", "PATCH", {"approved": True})
        launch("WordCraft (real AppImage)", wc, "real WordCraft AppImage installed as an external app for this test account")
    else:
        results.append({"app": "WordCraft", "status": "not-in-inventory", "windows": []})

    def import_and_launch(name, path, args, icon, note):
        code, imp = admin.curl("/api/linux-apps/discovered/import", "POST",
                               {"name": name, "path": path, "args": args, "icon": icon})
        if code != 200:
            results.append({"app": name, "status": "import-failed", "error": str(imp)[:200]})
            print(f"FAIL import {name}: {str(imp)[:160]}")
            return
        entry = imp if imp.get("id") else (imp.get("app") or imp)
        if isinstance(entry, dict) and entry.get("id"):
            launch(name, entry, note)

    # Snap-packaged apps are not under /usr/share/applications; import them manually.
    for name, path, icon in [
        ("Blender (snap)", "/snap/bin/blender", "blender"),
        ("Chromium (snap)", "/snap/bin/chromium", "chromium-browser"),
    ]:
        if os.path.exists(path):
            import_and_launch(name, path, [], icon, "snap-packaged application, manually imported")
        else:
            results.append({"app": name, "status": "not-installed", "windows": []})

    # imported fixtures (mpv video, GTK zenity, Qt qterminal, LibreOffice writer if present)
    imports = [
        ("MPV video", "/usr/bin/mpv", ["--no-terminal", "--really-quiet", "--loop=no", f"/var/lib/rast/linux-users/127/Videos/rast-compat-sample.mp4"], "mpv"),
        ("GTK Zenity", "/usr/bin/zenity", ["--info", "--title", "RAST GTK Test", "--text", "RAST GTK test window"], "zenity"),
        ("Qt QTerminal", "/usr/bin/qterminal", [], "qterminal"),
        ("LibreOffice Writer", "/usr/bin/libreoffice", ["--writer"], "libreoffice-writer"),
    ]
    for name, path, args, icon in imports:
        if not os.path.exists(path):
            results.append({"app": name, "status": "not-installed", "windows": []})
            print(f"SKIP {name}: {path} missing")
            continue
        code, imp = admin.curl("/api/linux-apps/discovered/import", "POST",
                               {"name": name, "path": path, "args": args, "icon": icon})
        if code != 200:
            results.append({"app": name, "status": "import-failed", "error": str(imp)[:200]})
            print(f"FAIL import {name}: {imp}")
            continue
        entry = imp if imp.get("id") else (imp.get("app") or imp)
        if isinstance(entry, dict) and entry.get("id"):
            launch(name, entry, "imported external application")

    # Ask the real browser to count/adopt the windows while the session is up.
    adopted = None
    try:
        adopt_out = "/tmp/rast-adopt-count.json"
        node = subprocess.run(
            ["node", "compat/adopt-count.mjs", USER, adopt_out],
            cwd="/home/lufria/RAST/tests/e2e",
            capture_output=True, text=True, timeout=300,
        )
        if os.path.exists(adopt_out):
            adopted = json.load(open(adopt_out))
        else:
            adopted = {"error": (node.stdout + node.stderr)[-300:]}
    except Exception as error:  # noqa: BLE001
        adopted = {"error": str(error)[:300]}

    _, ov = alice.curl("/api/linux-apps")
    session = (ov or {}).get("session") or {}
    summary = {
        "generatedAt": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        "user": USER,
        "display": session.get("display"),
        "backend": session.get("displayBackend"),
        "gpuVerified": session.get("gpuAccelerationVerified"),
        "renderer": session.get("renderer"),
        "browserAdoption": adopted,
        "results": results,
    }
    os.makedirs(RESULTS, exist_ok=True)
    stamp = datetime.datetime.now().strftime("%Y-%m-%dT%H-%M-%S")
    path = os.path.join(RESULTS, f"compat-runtime21-{stamp}.json")
    with open(path, "w") as fh:
        json.dump(summary, fh, indent=2)
    print(f"report: {path}")
    # cleanup: close the shared session (stops all runs + display)
    alice.close_session()
    print("session closed")

if __name__ == "__main__":
    main()
