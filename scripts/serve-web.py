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
        self.send_header("Cache-Control", "no-store")
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        super().end_headers()


handler = functools.partial(Handler, directory=directory)
server = http.server.ThreadingHTTPServer(("127.0.0.1", port), handler)
print(f"serving {directory} on http://127.0.0.1:{port}", flush=True)
server.serve_forever()
