#!/usr/bin/env python3
"""Record official SDK control-plane requests and return deterministic fixtures."""

from __future__ import annotations

import argparse
import json
import signal
import threading
import urllib.parse
from http import HTTPStatus
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import Any, ClassVar


SANDBOX_ID = "fixture-sandbox"
MISSING_SANDBOX_ID = "missing-sandbox"


def sandbox_response() -> dict[str, Any]:
    return {
        "clientID": "fixture-client",
        "domain": "fixture.invalid",
        "envdAccessToken": "fixture-envd-token",
        "envdVersion": "0.1.3",
        "sandboxID": SANDBOX_ID,
        "templateID": "fixture-template",
        "trafficAccessToken": "fixture-traffic-token",
    }


def listed_sandbox() -> dict[str, Any]:
    return {
        "clientID": "fixture-client",
        "cpuCount": 2,
        "diskSizeMB": 1024,
        "endAt": "2026-07-14T12:05:00Z",
        "envdVersion": "0.1.3",
        "memoryMB": 512,
        "metadata": {"team": "alpha beta"},
        "sandboxID": SANDBOX_ID,
        "startedAt": "2026-07-14T12:00:00Z",
        "state": "running",
        "templateID": "fixture-template",
        "volumeMounts": [],
    }


class FixtureHandler(BaseHTTPRequestHandler):
    """Capture stable request fields and implement the lifecycle fixture."""

    capture_path: ClassVar[Path]
    client_name: ClassVar[str]
    capture_lock: ClassVar[threading.Lock] = threading.Lock()

    def do_GET(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        self._handle()

    def do_POST(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        self._handle()

    def do_DELETE(self) -> None:  # noqa: N802 - BaseHTTPRequestHandler API
        self._handle()

    def log_message(self, _format: str, *args: object) -> None:
        del args

    def _handle(self) -> None:
        parsed = urllib.parse.urlsplit(self.path)
        body = self._read_body()
        self._capture(parsed, body)

        path = parsed.path
        if self.command == "POST" and path == "/sandboxes":
            self._json(HTTPStatus.CREATED, sandbox_response())
        elif self.command == "POST" and path == f"/sandboxes/{SANDBOX_ID}/connect":
            self._json(HTTPStatus.OK, sandbox_response())
        elif self.command == "GET" and path == "/v2/sandboxes":
            self._json(HTTPStatus.OK, [listed_sandbox()])
        elif self.command == "POST" and path == f"/sandboxes/{SANDBOX_ID}/timeout":
            self._empty(HTTPStatus.NO_CONTENT)
        elif self.command == "DELETE" and path == f"/sandboxes/{SANDBOX_ID}":
            self._empty(HTTPStatus.NO_CONTENT)
        elif MISSING_SANDBOX_ID in path:
            self._json(
                HTTPStatus.NOT_FOUND,
                {"code": 404, "message": "Sandbox not found"},
            )
        else:
            self._json(
                HTTPStatus.NOT_FOUND,
                {"code": 404, "message": f"Unexpected fixture route {path}"},
            )

    def _read_body(self) -> Any:
        length = int(self.headers.get("Content-Length", "0"))
        if length == 0:
            return None
        raw = self.rfile.read(length)
        content_type = self.headers.get("Content-Type", "")
        if "json" in content_type:
            return json.loads(raw)
        return raw.decode("utf-8")

    def _capture(self, parsed: urllib.parse.SplitResult, body: Any) -> None:
        selected_headers = {}
        for name in [
            "authorization",
            "content-type",
            "user-agent",
            "x-api-key",
            "x-supabase-team",
            "x-supabase-token",
        ]:
            value = self.headers.get(name)
            if value is not None:
                selected_headers[name] = value
        record = {
            "body": body,
            "client": self.client_name,
            "headers": selected_headers,
            "method": self.command,
            "path": parsed.path,
            "query": sorted(
                [list(item) for item in urllib.parse.parse_qsl(parsed.query, True)]
            ),
        }
        encoded = json.dumps(record, sort_keys=True, separators=(",", ":"))
        with self.capture_lock:
            with self.capture_path.open("a", encoding="utf-8") as capture:
                capture.write(encoded)
                capture.write("\n")

    def _json(self, status: HTTPStatus, body: Any) -> None:
        encoded = json.dumps(body, sort_keys=True, separators=(",", ":")).encode()
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(encoded)))
        self.end_headers()
        self.wfile.write(encoded)

    def _empty(self, status: HTTPStatus) -> None:
        self.send_response(status)
        self.send_header("Content-Length", "0")
        self.end_headers()


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("--capture", type=Path, required=True)
    parser.add_argument("--client", required=True)
    parser.add_argument("--port-file", type=Path, required=True)
    args = parser.parse_args()

    FixtureHandler.capture_path = args.capture
    FixtureHandler.client_name = args.client
    server = ThreadingHTTPServer(("127.0.0.1", 0), FixtureHandler)
    args.port_file.write_text(str(server.server_port), encoding="utf-8")

    def stop(_signal: int, _frame: object) -> None:
        raise KeyboardInterrupt

    signal.signal(signal.SIGTERM, stop)
    try:
        server.serve_forever()
    except KeyboardInterrupt:
        pass
    finally:
        server.server_close()


if __name__ == "__main__":
    main()
