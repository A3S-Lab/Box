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
INTERPRETER_SANDBOX_ID = "fixture-interpreter"
MISSING_SANDBOX_ID = "missing-sandbox"


def sandbox_response(sandbox_id: str) -> dict[str, Any]:
    return {
        "clientID": "fixture-client",
        "domain": "fixture.invalid",
        "envdAccessToken": "fixture-envd-token",
        "envdVersion": "0.1.3",
        "sandboxID": sandbox_id,
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
    create_count: ClassVar[int] = 0
    sandbox_paused: ClassVar[bool] = False

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
            with self.capture_lock:
                self.__class__.create_count += 1
                sandbox_id = (
                    SANDBOX_ID
                    if self.create_count == 1
                    else INTERPRETER_SANDBOX_ID
                )
            self._json(HTTPStatus.CREATED, sandbox_response(sandbox_id))
        elif self.command == "POST" and path == f"/sandboxes/{SANDBOX_ID}/pause":
            with self.capture_lock:
                already_paused = self.__class__.sandbox_paused
                self.__class__.sandbox_paused = True
            self._empty(
                HTTPStatus.CONFLICT if already_paused else HTTPStatus.NO_CONTENT
            )
        elif self.command == "POST" and path == f"/sandboxes/{SANDBOX_ID}/connect":
            with self.capture_lock:
                was_paused = self.__class__.sandbox_paused
                self.__class__.sandbox_paused = False
            self._json(
                HTTPStatus.CREATED if was_paused else HTTPStatus.OK,
                sandbox_response(SANDBOX_ID),
            )
        elif self.command == "GET" and path == "/v2/sandboxes":
            self._json(HTTPStatus.OK, [listed_sandbox()])
        elif self.command == "POST" and path == f"/sandboxes/{SANDBOX_ID}/timeout":
            self._empty(HTTPStatus.NO_CONTENT)
        elif self.command == "DELETE" and path in {
            f"/sandboxes/{SANDBOX_ID}",
            f"/sandboxes/{INTERPRETER_SANDBOX_ID}",
        }:
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
        transfer_encoding = self.headers.get("Transfer-Encoding", "")
        if "chunked" in transfer_encoding.lower():
            raw = self._read_chunked_body()
        else:
            length = int(self.headers.get("Content-Length", "0"))
            raw = self.rfile.read(length) if length else b""
        if not raw:
            return None
        content_type = self.headers.get("Content-Type", "")
        if "json" in content_type:
            return json.loads(raw)
        return raw.decode("utf-8")

    def _read_chunked_body(self) -> bytes:
        body = bytearray()
        while True:
            size_line = self.rfile.readline()
            if not size_line:
                raise EOFError("request ended before the next chunk size")
            size_text = size_line.split(b";", 1)[0].strip()
            try:
                size = int(size_text, 16)
            except ValueError as error:
                raise ValueError(f"invalid HTTP chunk size: {size_text!r}") from error
            if size == 0:
                while True:
                    trailer = self.rfile.readline()
                    if trailer in (b"\r\n", b"\n"):
                        return bytes(body)
                    if not trailer:
                        raise EOFError("request ended inside chunk trailers")
            chunk = self.rfile.read(size)
            if len(chunk) != size:
                raise EOFError(f"request chunk ended after {len(chunk)} of {size} bytes")
            if self.rfile.read(2) != b"\r\n":
                raise ValueError("request chunk is missing its CRLF terminator")
            body.extend(chunk)

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
