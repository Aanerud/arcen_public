#!/usr/bin/env python3
# SPDX-License-Identifier: AGPL-3.0-only
"""Tiny host-side server for the real-screen latency page.

Serves latency.html on 127.0.0.1 and appends every POST /log batch, stamped
with the server's receive time, to a JSON-lines file. GET /time returns the
host's CLOCK_REALTIME in nanoseconds so the Mac driver can estimate the
clock offset over an ssh tunnel (minimum-RTT midpoint method).

Usage: page_server.py <port> <log.jsonl>
"""
import http.server
import json
import pathlib
import sys
import threading
import time

HERE = pathlib.Path(__file__).resolve().parent
LOCK = threading.Lock()


class Handler(http.server.BaseHTTPRequestHandler):
    log_path = None

    def log_message(self, *_args):
        pass

    def _send(self, code, body, ctype):
        self.send_response(code)
        self.send_header("Content-Type", ctype)
        self.send_header("Content-Length", str(len(body)))
        self.send_header("Cache-Control", "no-store")
        self.end_headers()
        self.wfile.write(body)

    def do_GET(self):
        if self.path.startswith("/time"):
            self._send(200, str(time.time_ns()).encode(), "text/plain")
        elif self.path.startswith("/latency.html") or self.path == "/":
            self._send(200, (HERE / "latency.html").read_bytes(), "text/html; charset=utf-8")
        else:
            self._send(404, b"not found", "text/plain")

    def do_POST(self):
        received = time.time_ns()
        length = min(int(self.headers.get("Content-Length", "0")), 16 * 1024 * 1024)
        payload = self.rfile.read(length)
        try:
            batch = json.loads(payload)
        except ValueError:
            self._send(400, b"bad json", "text/plain")
            return
        batch["received_ns"] = received
        with LOCK, open(self.log_path, "a", encoding="utf-8") as out:
            out.write(json.dumps(batch, separators=(",", ":")) + "\n")
        self._send(204, b"", "text/plain")


def main():
    port, log_path = int(sys.argv[1]), sys.argv[2]
    Handler.log_path = log_path
    server = http.server.ThreadingHTTPServer(("127.0.0.1", port), Handler)
    server.serve_forever()


if __name__ == "__main__":
    main()
