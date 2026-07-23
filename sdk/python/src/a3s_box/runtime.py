"""Structured local transport to the installed ``a3s-box`` runtime."""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import subprocess
from collections.abc import Mapping
from dataclasses import dataclass
from typing import Any, Protocol

from .exceptions import A3SBoxError, A3SBoxNotInstalledError

BRIDGE_PROTOCOL_VERSION = 1
SUPPORTED_BRIDGE_OPERATIONS = (
    "sdk_capabilities",
    "runtime_diagnostics",
    "runtime_disk_usage",
    "image_build",
    "image_pull",
    "image_get",
    "image_list",
    "image_inspect",
    "image_history",
    "image_tag",
    "image_push",
    "image_remove",
    "image_evict",
    "volume_create",
    "volume_get",
    "volume_list",
    "volume_remove",
    "volume_prune",
    "network_create",
    "network_get",
    "network_list",
    "network_remove",
    "network_prune",
    "sandbox_list",
    "sandbox_get",
    "sandbox_create",
    "sandbox_inspect",
    "sandbox_stop",
    "sandbox_restart",
    "sandbox_remove",
    "sandbox_kill",
    "sandbox_pause",
    "sandbox_resume",
    "sandbox_logs",
    "sandbox_stats",
    "sandbox_snapshot_create",
    "filesystem_snapshot_list",
    "filesystem_snapshot_get",
    "filesystem_snapshot_size",
    "filesystem_snapshot_delete",
    "command_run",
    "file_write",
    "file_read",
    "filesystem_stat",
    "filesystem_list",
    "filesystem_make_dir",
    "filesystem_move",
    "filesystem_remove",
)


class LocalRuntime(Protocol):
    """Synchronous local runtime accepted by :class:`Sandbox`."""

    def request(self, request: Mapping[str, object]) -> dict[str, Any]:
        ...


class AsyncLocalRuntime(Protocol):
    """Asynchronous local runtime accepted by :class:`AsyncSandbox`."""

    async def request(self, request: Mapping[str, object]) -> dict[str, Any]:
        ...


@dataclass(frozen=True, slots=True)
class A3SLocalRuntime:
    """Invoke the structured bridge built into the installed A3S Box binary."""

    binary_path: str | None = None
    bridge_timeout: float = 600.0

    def request(self, request: Mapping[str, object]) -> dict[str, Any]:
        binary = _resolve_binary(self.binary_path)
        payload = json.dumps(dict(request), separators=(",", ":"))
        try:
            completed = subprocess.run(
                [binary, "sdk-bridge"],
                input=payload,
                text=True,
                capture_output=True,
                timeout=self.bridge_timeout,
                check=False,
            )
        except FileNotFoundError as error:
            raise A3SBoxNotInstalledError(binary) from error
        except subprocess.TimeoutExpired as error:
            raise A3SBoxError(
                f"Local A3S Box bridge timed out after {self.bridge_timeout:g} seconds",
                code="bridge_timeout",
            ) from error
        return _decode_response(completed.stdout, completed.stderr, completed.returncode)


@dataclass(frozen=True, slots=True)
class A3SAsyncLocalRuntime:
    """Asynchronous structured transport to the installed A3S Box binary."""

    binary_path: str | None = None
    bridge_timeout: float = 600.0

    async def request(self, request: Mapping[str, object]) -> dict[str, Any]:
        binary = _resolve_binary(self.binary_path)
        try:
            process = await asyncio.create_subprocess_exec(
                binary,
                "sdk-bridge",
                stdin=asyncio.subprocess.PIPE,
                stdout=asyncio.subprocess.PIPE,
                stderr=asyncio.subprocess.PIPE,
            )
        except FileNotFoundError as error:
            raise A3SBoxNotInstalledError(binary) from error

        payload = json.dumps(dict(request), separators=(",", ":")).encode()
        try:
            stdout, stderr = await asyncio.wait_for(
                process.communicate(payload),
                timeout=self.bridge_timeout,
            )
        except asyncio.TimeoutError as error:
            process.kill()
            await process.wait()
            raise A3SBoxError(
                f"Local A3S Box bridge timed out after {self.bridge_timeout:g} seconds",
                code="bridge_timeout",
            ) from error
        return _decode_response(
            stdout.decode(errors="replace"),
            stderr.decode(errors="replace"),
            process.returncode or 0,
        )


def _resolve_binary(configured: str | None) -> str:
    candidate = configured or os.environ.get("A3S_BOX_BINARY") or "a3s-box"
    if os.path.dirname(candidate):
        if not os.path.isfile(candidate):
            raise A3SBoxNotInstalledError(candidate)
        return candidate
    resolved = shutil.which(candidate)
    if resolved is None:
        raise A3SBoxNotInstalledError(candidate)
    return resolved


def _decode_response(stdout: str, stderr: str, returncode: int) -> dict[str, Any]:
    try:
        envelope = json.loads(stdout)
    except json.JSONDecodeError as error:
        detail = stderr.strip() or stdout.strip() or f"exit status {returncode}"
        raise A3SBoxError(
            f"Invalid response from the local A3S Box bridge: {detail}",
            code="bridge_protocol_error",
        ) from error
    if not isinstance(envelope, dict):
        raise A3SBoxError(
            "Invalid response from the local A3S Box bridge: expected an object",
            code="bridge_protocol_error",
        )
    if envelope.get("protocol_version") != BRIDGE_PROTOCOL_VERSION:
        raise A3SBoxError(
            "Unsupported local A3S Box bridge protocol version",
            code="bridge_protocol_error",
        )
    if envelope.get("ok") is not True:
        raw_error = envelope.get("error")
        if isinstance(raw_error, dict):
            code = str(raw_error.get("code", "runtime_error"))
            message = str(raw_error.get("message", "Local A3S Box request failed"))
        else:
            code = "runtime_error"
            message = "Local A3S Box request failed"
        raise A3SBoxError(message, code=code)
    result = envelope.get("result")
    if not isinstance(result, dict):
        raise A3SBoxError(
            "Invalid response from the local A3S Box bridge: missing result",
            code="bridge_protocol_error",
        )
    return result
