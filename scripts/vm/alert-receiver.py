#!/usr/bin/env python3
"""State-scoped local Alertmanager webhook recorder for microVM validation."""

from __future__ import annotations

import json
import os
import signal
from datetime import datetime, timezone
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer

OUTPUT = os.environ.get("ALERT_RECEIVER_OUTPUT", "/data/notifications.jsonl")
MAX_BODY_BYTES = 1024 * 1024


class Handler(BaseHTTPRequestHandler):
    def do_GET(self) -> None:  # noqa: N802
        if self.path != "/health":
            self.send_error(404)
            return
        self.send_response(200)
        self.end_headers()
        self.wfile.write(b"ok\n")

    def do_POST(self) -> None:  # noqa: N802
        if self.path != "/alerts":
            self.send_error(404)
            return
        try:
            length = int(self.headers.get("Content-Length", "0"))
        except ValueError:
            self.send_error(400, "invalid content length")
            return
        if length <= 0 or length > MAX_BODY_BYTES:
            self.send_error(413, "invalid alert payload size")
            return
        try:
            payload = json.loads(self.rfile.read(length))
        except (UnicodeDecodeError, json.JSONDecodeError):
            self.send_error(400, "invalid JSON")
            return

        record = {
            "received_at": datetime.now(timezone.utc).isoformat(),
            "payload": payload,
        }
        encoded = json.dumps(record, separators=(",", ":"), sort_keys=True)
        fd = os.open(OUTPUT, os.O_APPEND | os.O_CREAT | os.O_WRONLY, 0o600)
        try:
            os.write(fd, encoded.encode("utf-8") + b"\n")
            os.fsync(fd)
        finally:
            os.close(fd)

        self.send_response(200)
        self.end_headers()

    def log_message(self, _format: str, *_args: object) -> None:
        return


if __name__ == "__main__":
    server = ThreadingHTTPServer(("127.0.0.1", 19093), Handler)

    def terminate(_signum: int, _frame: object) -> None:
        raise SystemExit(0)

    signal.signal(signal.SIGTERM, terminate)
    try:
        server.serve_forever()
    finally:
        server.server_close()
