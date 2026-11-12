// Xpra PoC reverse proxy: serves a simulated RAST desktop wrapper page and
// proxies HTTP + WebSocket to the live Xpra server (127.0.0.1:14591, display
// :101), injecting the real RAST shim (src/linux_apps/xpra_shim.js) into the
// proxied Xpra HTML page exactly like the Rust backend will.
//
// Usage: node scripts/xpra-poc/server.mjs [port]   (default 14592)

import http from "node:http";
import net from "node:net";
import fs from "node:fs";
import path from "node:path";
import { fileURLToPath } from "node:url";

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const ROOT = path.resolve(__dirname, "../..");
const SHIM_FILE = path.join(ROOT, "src/linux_apps/xpra_shim.js");
const XPRA_HOST = process.env.XPRA_HOST || "127.0.0.1";
const XPRA_PORT = Number(process.env.XPRA_PORT || 14591);
const PORT = Number(process.argv[2] || process.env.POC_PORT || 14592);

/** Remote xpra page is mounted at this prefix. */
export const XPRA_PREFIX = "/xpra";

function readShim() {
  return fs.readFileSync(SHIM_FILE, "utf8");
}

function injectShim(html) {
  const tag = '<script src="rast-shim.js"></script>';
  const bodyClose = html.lastIndexOf("</body>");
  if (bodyClose >= 0) return html.slice(0, bodyClose) + tag + "\n" + html.slice(bodyClose);
  return html + tag;
}

function proxyRequest(req, res) {
  const targetPath = req.url.replace(new RegExp("^" + XPRA_PREFIX), "") || "/";
  const headers = { ...req.headers };
  // Serve plain (never pre-compressed) responses so the HTML can be rewritten.
  delete headers["accept-encoding"];
  headers.host = `${XPRA_HOST}:${XPRA_PORT}`;

  const upstream = http.request(
    { host: XPRA_HOST, port: XPRA_PORT, method: req.method, path: targetPath, headers },
    (up) => {
      const responseHeaders = { ...up.headers };
      // The proxy must not advertise an encoded body it rewrote.
      delete responseHeaders["content-encoding"];
      delete responseHeaders["transfer-encoding"];
      // Frame-ancestors: the wrapper page is same-origin; keep it simple.
      delete responseHeaders["content-security-policy"];

      if (targetPath.split("?")[0].endsWith(".js") || targetPath.split("?")[0].endsWith(".css")) {
        // forwarding helpers keep the proxy simple
      }

      const chunks = [];
      up.on("data", (chunk) => chunks.push(chunk));
      up.on("end", () => {
        let body = Buffer.concat(chunks);
        const type = String(responseHeaders["content-type"] || "");
        if (type.includes("text/html")) {
          body = Buffer.from(injectShim(body.toString("utf8")), "utf8");
        }
        delete responseHeaders["content-length"];
        responseHeaders["content-length"] = String(body.length);
        res.writeHead(up.statusCode || 502, responseHeaders);
        res.end(body);
      });
    },
  );
  upstream.on("error", (error) => {
    res.writeHead(502, { "content-type": "text/plain" });
    res.end("xpra upstream error: " + error.message);
  });
  if (req.method === "GET" || req.method === "HEAD") upstream.end();
  else req.pipe(upstream);
}

function proxyUpgrade(req, socket, head) {
  const targetPath = (req.url || "/").replace(new RegExp("^" + XPRA_PREFIX), "") || "/";
  const upstream = net.connect(XPRA_PORT, XPRA_HOST, () => {
    let raw = `${req.method} ${targetPath} HTTP/1.1\r\n`;
    for (const [key, value] of Object.entries(req.headers)) {
      if (key.toLowerCase() === "host") {
        raw += `Host: ${XPRA_HOST}:${XPRA_PORT}\r\n`;
      } else if (Array.isArray(value)) {
        raw += `${key}: ${value.join(", ")}\r\n`;
      } else {
        raw += `${key}: ${value}\r\n`;
      }
    }
    raw += "\r\n";
    upstream.write(raw);
    if (head && head.length) upstream.write(head);
    upstream.pipe(socket);
    socket.pipe(upstream);
  });
  upstream.on("error", () => socket.destroy());
  socket.on("error", () => upstream.destroy());
  socket.on("close", () => upstream.destroy());
  upstream.on("close", () => socket.destroy());
}

function serveStatic(req, res) {
  const url = new URL(req.url || "/", "http://localhost");
  const route = url.pathname;
  if (route === "/" || route === "/index.html") {
    return sendFile(res, path.join(__dirname, "index.html"), "text/html; charset=utf-8");
  }
  if (route === "/harness.js") {
    return sendFile(res, path.join(__dirname, "harness.js"), "application/javascript; charset=utf-8");
  }
  if (route === "/run-inspect.js") {
    return sendFile(res, path.join(__dirname, "inspect.js"), "application/javascript; charset=utf-8");
  }
  res.writeHead(404, { "content-type": "text/plain" });
  res.end("not found: " + route);
}

function sendFile(res, file, type) {
  fs.readFile(file, (error, data) => {
    if (error) {
      res.writeHead(500, { "content-type": "text/plain" });
      res.end("read error: " + error.message);
      return;
    }
    res.writeHead(200, {
      "content-type": type,
      "cache-control": "no-store",
      "content-length": String(data.length),
    });
    res.end(data);
  });
}

export function startServer(port = PORT) {
  const server = http.createServer((req, res) => {
    const route = (req.url || "/").split("?")[0];
    if (route === "/xpra/rast-shim.js") {
      const body = Buffer.from(readShim(), "utf8");
      res.writeHead(200, {
        "content-type": "application/javascript; charset=utf-8",
        "cache-control": "no-store",
        "content-length": String(body.length),
      });
      res.end(body);
      return;
    }
    if (route === XPRA_PREFIX || route.startsWith(XPRA_PREFIX + "/")) {
      return proxyRequest(req, res);
    }
    return serveStatic(req, res);
  });
  server.on("upgrade", proxyUpgrade);
  return new Promise((resolve) => {
    server.listen(port, "127.0.0.1", () => {
      resolve({
        server,
        port,
        url: `http://127.0.0.1:${port}/`,
        close: () => new Promise((done) => server.close(() => done())),
      });
    });
  });
}

if (process.argv[1] && path.resolve(process.argv[1]) === fileURLToPath(import.meta.url)) {
  startServer().then(({ url }) => console.log(`xpra-poc proxy listening on ${url}`));
}
