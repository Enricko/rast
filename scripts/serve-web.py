#!/usr/bin/env python3
"""Serve the browser build with the MIME types Rune's web target needs.

    python3 scripts/serve-web.py [DIR] [PORT]

DIR defaults to dist/web, PORT to 8765, bound to 127.0.0.1. Development only:
for a real deployment serve the same folder with nginx (deploy/nginx-revamp.conf).
"""
import functools
import http.server
import sys

directory = sys.argv[1] if len(sys.argv) > 1 else "dist/web"
port = int(sys.argv[2]) if len(sys.argv) > 2 else 8765


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


handler = functools.partial(Handler, directory=directory)
server = http.server.ThreadingHTTPServer(("127.0.0.1", port), handler)
print(f"serving {directory} on http://127.0.0.1:{port}", flush=True)
server.serve_forever()
