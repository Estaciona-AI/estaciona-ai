# SPDX-FileCopyrightText: 2026 Guilherme Pedroza
# SPDX-License-Identifier: Apache-2.0

"""Loopback-only read-only inspector. One bounded HTTP connection at a time."""

from http.server import BaseHTTPRequestHandler, HTTPServer
import json
from pathlib import Path
import threading
import time
from urllib.parse import parse_qs, urlsplit

ASSETS = Path(__file__).with_name("inspector_assets")


class _Server(HTTPServer):
    request_queue_size = 4
    allow_reuse_address = False

    def get_request(self):
        connection, address = super().get_request()
        connection.settimeout(0.5)
        return connection, address


class InspectorServer:
    def __init__(self, broker, *, port=8765):
        self.broker = broker
        self.budget = 20.0
        self.updated = time.monotonic()
        self.server = _Server(("127.0.0.1", port), _Handler)
        self.server.inspector = self
        self.port = self.server.server_address[1]
        self.origin = f"http://127.0.0.1:{self.port}"
        self.thread = threading.Thread(
            target=self.server.serve_forever, kwargs={"poll_interval": 0.1}, daemon=True
        )

    def admit(self):
        now = time.monotonic()
        self.budget = min(20.0, self.budget + (now - self.updated) * 10)
        self.updated = now
        if self.budget < 1:
            return False
        self.budget -= 1
        return True

    def __enter__(self):
        self.thread.start()
        return self

    def __exit__(self, *_):
        self.server.shutdown()
        self.server.server_close()
        self.thread.join(timeout=2)


class _Handler(BaseHTTPRequestHandler):
    server_version = "EstacionaInspector"
    sys_version = ""

    def log_message(self, *_):
        # Raw requests may contain credentials supplied accidentally by a client.
        pass

    def reply(self, status, body=b"", content_type="application/json; charset=utf-8"):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.send_header("X-Content-Type-Options", "nosniff")
        self.send_header("X-Frame-Options", "DENY")
        self.send_header("Referrer-Policy", "no-referrer")
        self.send_header("Cross-Origin-Resource-Policy", "same-origin")
        self.send_header("Cross-Origin-Opener-Policy", "same-origin")
        self.send_header(
            "Content-Security-Policy",
            "default-src 'self'; img-src 'self' data:; object-src 'none'; "
            "base-uri 'none'; frame-ancestors 'none'; form-action 'none'",
        )
        if status == 429:
            self.send_header("Retry-After", "1")
        self.send_header("Connection", "close")
        self.end_headers()
        self.close_connection = True
        if self.command != "HEAD":
            self.wfile.write(body)

    def do_GET(self):
        inspector = self.server.inspector
        if not inspector.admit():
            self.reply(429)
            return
        if (
            len(self.path) > 1024
            or sum(len(k) + len(v) for k, v in self.headers.items()) > 8192
        ):
            self.reply(413)
            return
        hosts = self.headers.get_all("Host", [])
        origins = self.headers.get_all("Origin", [])
        if (
            hosts != [f"127.0.0.1:{inspector.port}"]
            or origins not in ([], [inspector.origin])
            or self.headers.get("Sec-Fetch-Site", "none") not in ("same-origin", "none")
        ):
            self.reply(403)
            return
        if (
            self.headers.get("Transfer-Encoding")
            or self.headers.get("Content-Length", "0") != "0"
        ):
            self.reply(400)
            return
        parsed = urlsplit(self.path)
        if parsed.scheme or parsed.netloc or parsed.fragment:
            self.reply(400)
            return
        if parsed.path.startswith("/api/"):
            if self.headers.get_all("X-Inspector-Request", []) != ["1"]:
                self.reply(403)
                return
            try:
                if parsed.path == "/api/status" and not parsed.query:
                    data = inspector.broker.status()
                elif parsed.path == "/api/frame":
                    query = parse_qs(
                        parsed.query, strict_parsing=True, max_num_fields=1
                    )
                    if set(query) != {"input"} or len(query["input"]) != 1:
                        raise ValueError("invalid input")
                    data = inspector.broker.frame(query["input"][0])
                else:
                    self.reply(404)
                    return
            except ValueError:
                self.reply(400)
                return
            except KeyError:
                self.reply(404)
                return
            self.reply(200, json.dumps(data, allow_nan=False).encode())
            return
        assets = {
            "/": ("index.html", "text/html; charset=utf-8"),
            "/app.js": ("app.js", "text/javascript; charset=utf-8"),
            "/style.css": ("style.css", "text/css; charset=utf-8"),
        }
        if parsed.path not in assets or parsed.query:
            self.reply(404)
            return
        name, content_type = assets[parsed.path]
        self.reply(200, (ASSETS / name).read_bytes(), content_type)

    def do_POST(self):
        self.reply(405)

    do_PUT = do_POST
    do_DELETE = do_POST
    do_PATCH = do_POST
    do_OPTIONS = do_POST
    do_HEAD = do_POST
