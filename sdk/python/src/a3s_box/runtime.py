"""Structured local transport to the installed ``a3s-box`` runtime."""

from __future__ import annotations

import asyncio
import json
import os
import shutil
import subprocess
import threading
from collections.abc import Mapping, Sequence
from dataclasses import dataclass
from typing import Any, Protocol

from .exceptions import A3SBoxError, A3SBoxNotInstalledError

BRIDGE_PROTOCOL_VERSION = 2
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


def _validate_capabilities(result: Mapping[str, object]) -> dict[str, Any]:
    protocol_version = result.get("protocol_version")
    if protocol_version != BRIDGE_PROTOCOL_VERSION:
        raise A3SBoxError(
            f"Unsupported local A3S Box capability protocol version "
            f"{protocol_version!r}",
            code="bridge_protocol_error",
        )

    raw_operations = result.get("operations")
    if (
        not isinstance(raw_operations, Sequence)
        or isinstance(raw_operations, (str, bytes, bytearray))
        or any(not isinstance(operation, str) for operation in raw_operations)
    ):
        raise A3SBoxError(
            "Invalid local A3S Box capability inventory",
            code="bridge_protocol_error",
        )
    operations = tuple(raw_operations)
    if len(set(operations)) != len(operations):
        raise A3SBoxError(
            "Invalid local A3S Box capability inventory: duplicate operations",
            code="bridge_protocol_error",
        )
    available = set(operations)
    missing = sorted(
        operation
        for operation in SUPPORTED_BRIDGE_OPERATIONS
        if operation not in available
    )
    if missing:
        raise A3SBoxError(
            "Installed A3S Box runtime is missing required operations: "
            + ", ".join(missing),
            code="unavailable",
        )
    return {
        "protocol_version": protocol_version,
        "operations": list(operations),
    }


class _CompatibleLocalRuntime:
    def __init__(self, runtime: LocalRuntime) -> None:
        self._runtime = runtime
        self._lock = threading.Lock()
        self._capabilities: dict[str, Any] | None = None

    def request(self, request: Mapping[str, object]) -> dict[str, Any]:
        if request.get("operation") == "sdk_capabilities":
            return dict(self._verify())
        self._verify()
        return self._runtime.request(request)

    def _verify(self) -> dict[str, Any]:
        if self._capabilities is not None:
            return self._capabilities
        with self._lock:
            if self._capabilities is None:
                result = self._runtime.request(
                    {"operation": "sdk_capabilities"}
                )
                self._capabilities = _validate_capabilities(result)
        return self._capabilities


class _CompatibleAsyncLocalRuntime:
    def __init__(self, runtime: AsyncLocalRuntime) -> None:
        self._runtime = runtime
        self._lock = asyncio.Lock()
        self._capabilities: dict[str, Any] | None = None

    async def request(self, request: Mapping[str, object]) -> dict[str, Any]:
        if request.get("operation") == "sdk_capabilities":
            return dict(await self._verify())
        await self._verify()
        return await self._runtime.request(request)

    async def _verify(self) -> dict[str, Any]:
        if self._capabilities is not None:
            return self._capabilities
        async with self._lock:
            if self._capabilities is None:
                result = await self._runtime.request(
                    {"operation": "sdk_capabilities"}
                )
                self._capabilities = _validate_capabilities(result)
        return self._capabilities


def _ensure_compatible_runtime(runtime: LocalRuntime) -> LocalRuntime:
    if isinstance(runtime, _CompatibleLocalRuntime):
        return runtime
    return _CompatibleLocalRuntime(runtime)


def _ensure_compatible_async_runtime(
    runtime: AsyncLocalRuntime,
) -> AsyncLocalRuntime:
    if isinstance(runtime, _CompatibleAsyncLocalRuntime):
        return runtime
    return _CompatibleAsyncLocalRuntime(runtime)


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
