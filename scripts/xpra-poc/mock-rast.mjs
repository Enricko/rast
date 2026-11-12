// Mock RAST backend for the native-apps end-to-end check.
//
// Serves the real built frontend (frontend/dist), the handful of /api endpoints
// the desktop needs to boot, and reverse-proxies the single native session at
// `/linux-apps/session/<key>/` to the live Xpra server (127.0.0.1:14591),
// injecting the real shim exactly like src/linux_apps/native_proxy.rs will.
//
// Usage: node scripts/xpra-poc/mock-rast.mjs [port]
//   frontend must be built first: (cd frontend && npm run build)

import http from "node:http";
import net from "node:net";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(__dirname, "../..");
const DIST = path.join(ROOT, "frontend/dist");
const SHIM = path.join(ROOT, "src/linux_apps/xpra_shim.js");
const XPRA_HOST = process.env.XPRA_HOST || "127.0.0.1";
const XPRA_PORT = Number(process.env.XPRA_PORT || 14591);
const PORT = Number(process.argv[2] || process.env.MOCK_PORT || 14593);
const SESSION_KEY = "0123456789abcdef0123456789abcdef";
const SESSION_URL = `/linux-apps/session/${SESSION_KEY}/`;

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "application/javascript; charset=utf-8",
  ".mjs": "application/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json; charset=utf-8",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".ico": "image/x-icon",
  ".woff2": "font/woff2",
  ".wasm": "application/wasm",
};

const NATIVE_APP = {
  id: "chrome",
  desktopId: "linux:chrome",
  name: "Google Chrome",
  format: "deb",
  fileName: "google-chrome-stable_130.0.6723.116-1_amd64.deb",
  version: "130.0.6723.116",
  package: "google-chrome-stable",
  description: "Fast, secure web browser (native install on the server)",
  command: ["/usr/bin/google-chrome"],
  launchGuess: false,
  iconUrl: null,
  status: "ready",
  error: null,
  jobId: null,
  update: { source: null, latestVersion: null, available: false, checkedAt: null, error: null },
  memoryMb: 2048,
  cpus: 2,
  sizeBytes: 108_000_000,
  visibility: "users",
  running: 0,
  createdAt: Date.now() - 86_400_000,
  updatedAt: Date.now(),
  backend: "native",
  native: {
    installPath: "/opt/google/chrome/google-chrome",
    previousPath: null,
    launcher: { argv: ["/usr/bin/google-chrome"], name: "Google Chrome", desktopEntry: "google-chrome.desktop" },
    architecture: "amd64",
    packageVersion: "130.0.6723.116-1",
    preexistingPackage: false,
    dependencies: ["libgtk-3-0", "libnss3"],
    packagesAdded: ["google-chrome-stable"],
    compatibility: { ok: true, reasons: [] },
    installSource: { kind: "url", url: "https://dl.google.com/linux/direct/google-chrome-stable_current_amd64.deb" },
    migration: { state: "none", error: null, backup: null },
    updateSource: null,
  },
};

// A Docker-backed app with a pending native migration: proves the App Center
// migration notice and the per-app migration section.
const LEGACY_APP = {
  id: "notes",
  desktopId: "linux:notes",
  name: "Notes",
  format: "appimage",
  fileName: "notes.AppImage",
  version: "1.4.0",
  package: null,
  description: "Simple notes (still runs in a Docker container)",
  command: ["/opt/notes/notes.AppImage"],
  launchGuess: false,
  iconUrl: null,
  status: "ready",
  error: null,
  jobId: null,
  update: { source: null, latestVersion: null, available: false, checkedAt: null, error: null },
  memoryMb: 1024,
  cpus: 1,
  sizeBytes: 42_000_000,
  visibility: "admins",
  running: 0,
  createdAt: Date.now() - 172_800_000,
  updatedAt: Date.now(),
  backend: "docker",
  native: {
    installPath: null,
    previousPath: null,
    launcher: null,
    architecture: null,
    packageVersion: null,
    preexistingPackage: false,
    dependencies: [],
    packagesAdded: [],
    compatibility: { ok: true, reasons: [] },
    installSource: null,
    migration: { state: "pending", error: null, backup: null },
    updateSource: null,
  },
};

function session(status = "ready") {
  return {
    id: "native",
    status,
    url: SESSION_URL,
    startedAt: Date.now() - 60_000,
    viewers: 1,
    separateWindows: true,
    display: ":101",
    reason: null,
    error: null,
    apps: [
      { runId: "run-1", appId: "chrome", name: "Google Chrome", status: "running", startedAt: Date.now() - 60_000 },
    ],
  };
}

function json(res, status, body) {
  const data = Buffer.from(JSON.stringify(body));
  res.writeHead(status, { "content-type": "application/json; charset=utf-8", "content-length": String(data.length), "cache-control": "no-store" });
  res.end(data);
}

function me() {
  return {
    user: {
      id: 1,
      username: "admin",
      displayName: "RAST Admin",
      role: "admin",
      active: true,
      quotaBytes: 0,
      usedBytes: 0,
      mustChangePassword: false,
      createdAt: 0,
      updatedAt: 0,
      lastLoginAt: null,
    },
    csrfToken: "mock-csrf-token",
    sessionExpiresAt: Date.now() + 3_600_000,
    features: { docker: true, apps: true, backup: true, monitor: true, activityLogs: true, fileStation: true },
    serverTime: Date.now(),
  };
}

function injectShim(html) {
  const tag = '<script src="rast-shim.js"></script>';
  const close = html.lastIndexOf("</body>");
  return close >= 0 ? html.slice(0, close) + tag + "\n" + html.slice(close) : html + tag;
}

/* --------------------------------------------------------------- xpra proxy */

function proxyXpra(req, res, rest) {
  const upstream = http.request(
    {
      host: XPRA_HOST,
      port: XPRA_PORT,
      method: req.method,
      path: rest || "/",
      headers: { ...req.headers, host: `${XPRA_HOST}:${XPRA_PORT}`, "accept-encoding": "identity" },
    },
    (up) => {
      const headers = { ...up.headers };
      delete headers["content-encoding"];
      delete headers["transfer-encoding"];
      delete headers["content-security-policy"];
      const chunks = [];
      up.on("data", (chunk) => chunks.push(chunk));
      up.on("end", () => {
        let body = Buffer.concat(chunks);
        if (String(headers["content-type"] || "").includes("text/html")) {
          body = Buffer.from(injectShim(body.toString("utf8")), "utf8");
        }
        delete headers["content-length"];
        headers["content-length"] = String(body.length);
        res.writeHead(up.statusCode || 502, headers);
        res.end(body);
      });
    },
  );
  upstream.on("error", () => {
    res.writeHead(502, { "content-type": "text/plain" });
    res.end("xpra upstream error");
  });
  req.pipe(upstream);
}

function proxyUpgrade(req, socket, head, rest) {
  const upstream = net.connect(XPRA_PORT, XPRA_HOST, () => {
    let raw = `${req.method} ${rest || "/"} HTTP/1.1\r\n`;
    for (const [key, value] of Object.entries(req.headers)) {
      raw += key.toLowerCase() === "host" ? `Host: ${XPRA_HOST}:${XPRA_PORT}\r\n` : `${key}: ${value}\r\n`;
    }
    raw += "\r\n";
    upstream.write(raw);
    if (head && head.length) upstream.write(head);
    upstream.pipe(socket);
    socket.pipe(upstream);
  });
  upstream.on("error", () => socket.destroy());
  socket.on("error", () => upstream.destroy());
}

/* ------------------------------------------------------------------- static */

function serveStatic(req, res, route) {
  let file = path.join(DIST, route === "/" ? "index.html" : route);
  if (!file.startsWith(DIST)) {
    res.writeHead(403);
    res.end("forbidden");
    return;
  }
  if (!fs.existsSync(file) || fs.statSync(file).isDirectory()) file = path.join(DIST, "index.html");
  const data = fs.readFileSync(file);
  res.writeHead(200, {
    "content-type": MIME[path.extname(file)] ?? "application/octet-stream",
    "content-length": String(data.length),
    "cache-control": "no-store",
  });
  res.end(data);
}

/* ------------------------------------------------------------------- server */

export function startMock(port = PORT) {
  const server = http.createServer((req, res) => {
    const url = new URL(req.url || "/", "http://localhost");
    const route = url.pathname;

    // Native session proxy (HTTP part).
    if (route === "/xpra-shim.js") {
      const body = fs.readFileSync(SHIM);
      res.writeHead(200, { "content-type": "application/javascript; charset=utf-8", "cache-control": "no-store", "content-length": String(body.length) });
      res.end(body);
      return;
    }
    // Force Xpra sharing so another viewer (e.g. the PoC probe) cannot steal
    // the user's session; the real backend injects this too.
    if (route === `/linux-apps/session/${SESSION_KEY}/` && !url.search) {
      res.writeHead(302, { location: `${route}?sharing=yes` });
      res.end();
      return;
    }
    if (route.startsWith(`/linux-apps/session/${SESSION_KEY}/`)) {
      const rest = route.slice(`/linux-apps/session/${SESSION_KEY}`.length) || "/";
      if (rest === "/rast-shim.js") {
        const body = fs.readFileSync(SHIM);
        res.writeHead(200, { "content-type": "application/javascript; charset=utf-8", "cache-control": "no-store", "content-length": String(body.length) });
        res.end(body);
        return;
      }
      return proxyXpra(req, res, rest + url.search);
    }

    // API
    if (route === "/api/bootstrap") {
      return json(res, 200, {
        product: "RAST",
        description: "Mock backend for the native apps PoC",
        version: "0.13.0-poc",
        setupRequired: false,
        setupTokenRequired: false,
        registrationEnabled: false,
        authenticated: true,
        maxUploadBytes: 32 * 1024 * 1024,
        sessionTtlHours: 12,
        serverTime: Date.now(),
      });
    }
    if (route === "/api/auth/me") return json(res, 200, me());
    if (route === "/api/linux-apps") {
      return json(res, 200, {
        apps: [NATIVE_APP, LEGACY_APP],
        sessions: [],
        limits: { minFreeMemoryBytes: 0, idleMinutes: 30, maxCpus: 4 },
        dockerEnabled: true,
        nativeEnabled: true,
        session: session(),
      });
    }
    if (route === "/api/linux-apps/session") return json(res, 200, { session: session() });
    if (/^\/api\/linux-apps\/[^/]+\/launch$/.test(route)) {
      return json(res, 200, { run: { id: "run-1", appId: "chrome", appName: "Google Chrome", status: "running" }, session: session() });
    }
    if (route === "/api/linux-apps/install") {
      return json(res, 200, { jobId: "job-install-1", installing: true });
    }
    if (/^\/api\/linux-apps\/[^/]+\/update$/.test(route)) {
      return json(res, 200, { jobId: "job-update-1" });
    }
    if (/^\/api\/linux-apps\/[^/]+\/migrate$/.test(route)) {
      return json(res, 200, { jobId: "job-migrate-1" });
    }
    if (route === "/api/linux-apps/preview") {
      return json(res, 200, {
        preview: {
          format: "deb",
          fileName: NATIVE_APP.fileName,
          sizeBytes: 350_000_000,
          sha256: "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
          source: { kind: "url", url: "https://example.test/google-chrome.deb" },
          architecture: "amd64",
          app: {
            name: "Google Chrome",
            version: NATIVE_APP.version,
            package: "google-chrome-stable",
            description: "Web browser",
            icon: null,
            launcher: { argv: ["/usr/bin/google-chrome"], desktopEntry: "google-chrome.desktop" },
            launchGuess: false,
          },
          candidates: [
            { name: "Google Chrome", argv: ["/usr/bin/google-chrome"], desktopEntry: "google-chrome.desktop" },
          ],
          needsChoice: false,
          compatibility: { ok: true, reasons: [], warnings: [] },
          deb: {
            package: { name: "google-chrome-stable", version: NATIVE_APP.version, preexisting: false },
            transaction: {
              installs: [
                { name: "google-chrome-stable", version: NATIVE_APP.version },
                { name: "libgtk-3-0", version: "3.24.38-2" },
              ],
              removals: [],
              upgrades: [],
              downloadBytes: 108_000_000,
            },
            dependencies: ["libgtk-3-0"],
          },
          update: null,
          warnings: [],
        },
      });
    }
    if (route === "/api/events") {
      res.writeHead(200, {
        "content-type": "text/event-stream",
        "cache-control": "no-cache",
        connection: "keep-alive",
        "x-accel-buffering": "no",
      });
      res.write("retry: 3000\n\n");
      res.write("event: hello\nid: 0\ndata: 0\n\n");
      const ping = setInterval(() => res.write(": ping\n\n"), 15_000);
      req.on("close", () => clearInterval(ping));
      return;
    }
    if (route === "/api/packages") {
      return json(res, 200, {
        apps: [],
        host: { os: "linux", arch: "x86_64", target: "linux-x86_64", libc: "glibc", rastVersion: "0.13.0-poc" },
        catalog: {
          url: "https://example.test/catalog.json",
          official: true,
          officialKey: true,
          intervalMinutes: 1440,
          stale: false,
          expired: false,
          checking: false,
        },
        shell: null,
        packages: [],
        counts: { updates: 0 },
        canManage: true,
      });
    }
    // Older/mocked endpoints the shell tolerates being absent.
    if (route.startsWith("/api/")) return json(res, 404, { error: { code: "not_found", message: "Not mocked: " + route } });
    return serveStatic(req, res, route);
  });
  server.on("upgrade", (req, socket, head) => {
    const url = req.url || "/";
    if (url.startsWith(`/linux-apps/session/${SESSION_KEY}/`)) {
      const rest = url.slice(`/linux-apps/session/${SESSION_KEY}`.length) || "/";
      return proxyUpgrade(req, socket, head, rest);
    }
    socket.destroy();
  });
  return new Promise((resolve) => {
    server.listen(port, "127.0.0.1", () => {
      resolve({ server, url: `http://127.0.0.1:${port}/`, close: () => new Promise((done) => server.close(() => done())) });
    });
  });
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  startMock().then(({ url }) => console.log(`mock RAST backend on ${url} (frontend dist + native session proxy)`));
}
