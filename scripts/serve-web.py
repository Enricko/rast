#!/usr/bin/env python3
"""Serve the browser build with the MIME types Rune's web target needs.

    python3 scripts/serve-web.py [DIR] [PORT]

DIR defaults to dist/web, PORT to 8765, bound to 127.0.0.1. Requests under
/api/ go to the account server at RAST_API (default http://127.0.0.1:8723), as
nginx does in production; start it with `cargo run -p rast-server`.
Development only: for a real deployment serve the same folder with nginx
(deploy/nginx-revamp-site.conf).
"""
import functools
import http.server
import os
import sys
import urllib.error
import urllib.request

directory = sys.argv[1] if len(sys.argv) > 1 else "dist/web"
port = int(sys.argv[2]) if len(sys.argv) > 2 else 8765
api = os.environ.get("RAST_API", "http://127.0.0.1:8723").rstrip("/")


class Handler(http.server.SimpleHTTPRequestHandler):
    extensions_map = {
        **http.server.SimpleHTTPRequestHandler.extensions_map,
        ".wasm": "application/wasm",
        ".js": "text/javascript",
        ".mjs": "text/javascript",
    }

    def end_headers(self):
        # Mirror the production headers in deploy/nginx-revamp-site.conf, so a
        # local smoke test also proves the strict CSP lets the page boot.
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("Referrer-Policy", "no-referrer")
        self.send_header(
            "Content-Security-Policy",
            "default-src 'self'; script-src 'self' 'wasm-unsafe-eval'; "
            "style-src 'self' 'unsafe-inline'; img-src 'self' data: blob:; "
            "font-src 'self' data:; connect-src 'self'; object-src 'none'; "
            "base-uri 'none'; form-action 'self'; frame-ancestors 'none'",
        )
        super().end_headers()

    def do_GET(self):
        if self.path.startswith("/api/"):
            self.proxy()
        else:
            super().do_GET()

    def do_POST(self):
        self.proxy_or_refuse()

    def do_PATCH(self):
        self.proxy_or_refuse()

    def do_DELETE(self):
        self.proxy_or_refuse()

    def proxy_or_refuse(self):
        if self.path.startswith("/api/"):
            self.proxy()
        else:
            self.send_error(405)

    def proxy(self):
        length = int(self.headers.get("Content-Length") or 0)
        body = self.rfile.read(length) if length else None
        headers = {
            name: value
            for name, value in self.headers.items()
            if name.lower() in ("content-type", "cookie", "x-rast")
        }
        headers["X-Client-IP"] = self.client_address[0]
        request = urllib.request.Request(api + self.path, data=body, headers=headers, method=self.command)
        try:
            response = urllib.request.urlopen(request, timeout=30)
        except urllib.error.HTTPError as error:
            response = error
        except OSError:
            self.send_error(502, f"the account server is not running at {api}")
            return
        with response:
            data = response.read()
            self.send_response(response.status)
            for name, value in response.headers.items():
                if name.lower() in ("content-type", "set-cookie", "retry-after"):
                    self.send_header(name, value)
        self.send_header("Content-Length", str(len(data)))
        self.end_headers()
        self.wfile.write(data)


handler = functools.partial(Handler, directory=directory)
server = http.server.ThreadingHTTPServer(("127.0.0.1", port), handler)
print(f"serving {directory} on http://127.0.0.1:{port}", flush=True)
server.serve_forever()
