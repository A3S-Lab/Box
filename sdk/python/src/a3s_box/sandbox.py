"""Native Python Sandbox API backed by the local A3S Box runtime."""

from __future__ import annotations

import asyncio
import base64
import hashlib
import os
import threading
import uuid
from collections.abc import AsyncIterator, Iterator, Mapping, Sequence
from typing import Any, Literal, cast

from ._bridge_values import (
    boolean as _boolean,
    command_result as _command_result,
    decoded_base64 as _decoded_base64,
    entry_info as _entry_info,
    execution_event_batch as _execution_event_batch,
    execution_process_inventory as _execution_process_inventory,
    execution_stats as _execution_stats,
    filesystem_snapshot_info as _snapshot_info,
    integer as _integer,
    mapping as _mapping,
    mapping_sequence as _mapping_sequence,
    sandbox_log_entry as _sandbox_log_entry,
    sandbox_stats as _sandbox_stats,
    string as _string,
)
from .exceptions import A3SBoxError
from ._sandbox_requests import (
    DEFAULT_IMAGE,
    command_request as _command_request,
    create_request as _create_request,
)
from .models import (
    Artifact,
    CommandResult,
    EntryInfo,
    ExecutionEventBatch,
    ExecutionProcessInventory,
    ExecutionResourceUpdate,
    ExecutionRuntimeEvent,
    ExecutionStats,
    FilesystemSnapshotInfo,
    PortMapping,
    SandboxNetwork,
    SandboxLogEntry,
    SandboxStats,
    Script,
    TmpfsMount,
    VolumeMount,
    WriteInfo,
)
from .runtime import (
    A3SAsyncLocalRuntime,
    A3SLocalRuntime,
    AsyncLocalRuntime,
    LocalRuntime,
    _ensure_compatible_async_runtime,
    _ensure_compatible_runtime,
)
from .script import AsyncScriptBuilder, ScriptBuilder


MAX_ARTIFACT_BYTES = 8 * 1024 * 1024
MAX_EXECUTION_EVENT_BATCH_ITEMS = 4_096
DEFAULT_EVENT_STREAM_BATCH_ITEMS = 256
DEFAULT_EVENT_STREAM_WAIT_TIMEOUT_MS = 1_000


def _isolation(
    result: Mapping[str, Any],
) -> Literal["microvm", "sandbox"]:
    isolation = _string(result.get("isolation"))
    if isolation not in {"microvm", "sandbox"}:
        raise A3SBoxError(
            "Bridge result has an invalid isolation",
            code="bridge_protocol_error",
        )
    return cast(Literal["microvm", "sandbox"], isolation)


def _artifact_limit(max_bytes: int) -> int:
    if (
        isinstance(max_bytes, bool)
        or not isinstance(max_bytes, int)
        or max_bytes <= 0
        or max_bytes > MAX_ARTIFACT_BYTES
    ):
        raise A3SBoxError(
            f"max_bytes must be between 1 and {MAX_ARTIFACT_BYTES}",
            code="invalid_request",
        )
    return max_bytes


def _events_request(
    after_sequence: int,
    limit: int,
    wait_timeout_ms: int | None,
) -> dict[str, object]:
    for name, value in (
        ("after_sequence", after_sequence),
        ("limit", limit),
    ):
        if isinstance(value, bool) or not isinstance(value, int):
            raise ValueError(f"{name} must be an integer")
    if after_sequence < 0 or after_sequence > (1 << 64) - 1:
        raise ValueError("after_sequence must fit an unsigned 64-bit integer")
    if not 1 <= limit <= MAX_EXECUTION_EVENT_BATCH_ITEMS:
        raise ValueError(
            "limit must be between 1 and "
            f"{MAX_EXECUTION_EVENT_BATCH_ITEMS}"
        )
    if wait_timeout_ms is not None and (
        isinstance(wait_timeout_ms, bool)
        or not isinstance(wait_timeout_ms, int)
        or wait_timeout_ms < 0
        or wait_timeout_ms > (1 << 64) - 1
    ):
        raise ValueError("wait_timeout_ms must be a non-negative integer")
    return {
        "after_sequence": after_sequence,
        "limit": limit,
        **(
            {}
            if wait_timeout_ms is None
            else {"wait_timeout_ms": wait_timeout_ms}
        ),
    }


def _event_stream_request(
    after_sequence: int,
    batch_size: int,
    wait_timeout_ms: int,
) -> dict[str, object]:
    request = _events_request(
        after_sequence,
        batch_size,
        wait_timeout_ms,
    )
    if wait_timeout_ms == 0:
        raise ValueError("event stream wait_timeout_ms must be greater than zero")
    return request


def _validate_execution_identity(
    sandbox_id: str,
    generation: int,
    execution_id: str,
    response_generation: int,
) -> None:
    if execution_id != sandbox_id or response_generation != generation:
        raise A3SBoxError(
            "A3S Box bridge returned a different execution generation",
            code="bridge_protocol_error",
        )


def _validate_process_inventory(
    inventory: ExecutionProcessInventory,
) -> None:
    identifiers: set[str] = set()
    for process in inventory.processes:
        if not process.process_id.strip():
            raise A3SBoxError(
                "A3S Box bridge returned an empty process ID",
                code="bridge_protocol_error",
            )
        if process.pid is not None and not 1 <= process.pid <= (1 << 32) - 1:
            raise A3SBoxError(
                "A3S Box bridge returned an invalid process PID",
                code="bridge_protocol_error",
            )
        if process.process_id in identifiers:
            raise A3SBoxError(
                "A3S Box bridge returned a duplicate process ID",
                code="bridge_protocol_error",
            )
        identifiers.add(process.process_id)


def _validate_runtime_stats(stats: ExecutionStats) -> None:
    if stats.timestamp_unix_ns <= 0:
        raise A3SBoxError(
            "A3S Box bridge returned an invalid runtime stats timestamp",
            code="bridge_protocol_error",
        )
    if stats.cpu.user_ns + stats.cpu.system_ns > stats.cpu.usage_ns:
        raise A3SBoxError(
            "A3S Box bridge returned inconsistent runtime CPU counters",
            code="bridge_protocol_error",
        )
    if (
        stats.memory.peak_bytes is not None
        and stats.memory.peak_bytes < stats.memory.usage_bytes
    ):
        raise A3SBoxError(
            "A3S Box bridge returned inconsistent runtime memory counters",
            code="bridge_protocol_error",
        )
    if any(
        not name
        or len(name) > 256
        or any(
            character.isspace() or not character.isprintable()
            for character in name
        )
        for name in stats.metrics
    ):
        raise A3SBoxError(
            "A3S Box bridge returned an invalid runtime metric name",
            code="bridge_protocol_error",
        )


def _validate_event_batch(
    batch: ExecutionEventBatch,
    after_sequence: int,
) -> None:
    previous = after_sequence
    for event in batch.events:
        if event.sequence <= previous or event.timestamp_unix_ns <= 0:
            raise A3SBoxError(
                "A3S Box bridge returned an invalid runtime event order",
                code="bridge_protocol_error",
            )
        previous = event.sequence
    if batch.next_sequence < previous:
        raise A3SBoxError(
            "A3S Box bridge returned a regressed runtime event cursor",
            code="bridge_protocol_error",
        )


def _validate_resource_update_response(
    sandbox_id: str,
    generation: int,
    isolation: Literal["microvm", "sandbox"],
    result: Mapping[str, Any],
) -> None:
    if _string(result.get("sandbox_id")) != sandbox_id:
        raise A3SBoxError(
            "A3S Box bridge returned a different sandbox ID",
            code="bridge_protocol_error",
        )
    response_generation = _integer(result.get("generation"))
    if response_generation <= 0 or response_generation != generation:
        raise A3SBoxError(
            "A3S Box bridge returned a different execution generation",
            code="bridge_protocol_error",
        )
    if _string(result.get("state")) != "running":
        raise A3SBoxError(
            "A3S Box bridge returned an invalid resource-update state",
            code="bridge_protocol_error",
        )
    if _isolation(result) != isolation:
        raise A3SBoxError(
            "A3S Box bridge changed sandbox isolation",
            code="bridge_protocol_error",
        )


def _require_running(sandbox_id: str, state: str) -> None:
    if state != "running":
        raise ValueError(f"sandbox {sandbox_id} is not running")


def _require_observable(sandbox_id: str, state: str) -> None:
    if state not in {"running", "paused"}:
        raise ValueError(f"sandbox {sandbox_id} is neither running nor paused")


def _require_stream_generation(
    sandbox_id: str,
    expected_generation: int,
    generation: int,
    state: str,
) -> None:
    if generation != expected_generation:
        raise A3SBoxError(
            f"sandbox {sandbox_id} changed generation while streaming events",
            code="conflict",
        )
    if state not in {"running", "paused"}:
        raise A3SBoxError(
            f"sandbox {sandbox_id} is no longer observable",
            code="conflict",
        )


def _artifact_source_path(path: str) -> str:
    if not isinstance(path, str) or not path.strip():
        raise A3SBoxError(
            "artifact source path cannot be empty",
            code="invalid_request",
        )
    return path


def _artifact_destination(
    destination: str | os.PathLike[str] | None,
) -> str | None:
    if destination is None:
        return None
    try:
        host_path = os.fspath(destination)
    except TypeError as error:
        raise A3SBoxError(
            "destination must be a host filesystem path",
            code="invalid_request",
        ) from error
    if not isinstance(host_path, str) or not host_path.strip():
        raise A3SBoxError(
            "destination must be a non-empty host filesystem path",
            code="invalid_request",
        )
    return host_path


def _decoded_file_result(
    result: Mapping[str, object],
    expected_path: str,
) -> bytes:
    response_path = _string(result.get("path"))
    if response_path != expected_path:
        raise A3SBoxError(
            "A3S Box bridge returned file data for a different path",
            code="bridge_protocol_error",
        )
    data = _decoded_base64(result.get("data_base64"), "data_base64")
    declared_size = _integer(result.get("size"))
    if declared_size < 0 or declared_size != len(data):
        raise A3SBoxError(
            "A3S Box bridge returned inconsistent file size metadata",
            code="bridge_protocol_error",
        )
    return data


def _write_new_host_file(host_path: str, data: bytes) -> None:
    try:
        descriptor = os.open(
            host_path,
            os.O_WRONLY | os.O_CREAT | os.O_EXCL,
            0o600,
        )
    except OSError as error:
        raise A3SBoxError(
            f"Could not create artifact destination {host_path!r}: {error}",
            code="runtime_error",
        ) from error

    write_error: OSError | None = None
    try:
        remaining = memoryview(data)
        while remaining:
            written = os.write(descriptor, remaining)
            if written == 0:
                raise OSError("artifact destination write made no progress")
            remaining = remaining[written:]
        os.fsync(descriptor)
    except OSError as error:
        write_error = error

    try:
        os.close(descriptor)
    except OSError as error:
        if write_error is None:
            write_error = error

    if write_error is not None:
        cleanup_error: OSError | None = None
        try:
            os.unlink(host_path)
        except OSError as error:
            cleanup_error = error
        message = f"Could not write artifact destination {host_path!r}: {write_error}"
        if cleanup_error is not None:
            message += f"; partial-file cleanup failed: {cleanup_error}"
        raise A3SBoxError(message, code="runtime_error") from write_error


class Sandbox:
    """A local A3S Box Sandbox with command and filesystem namespaces."""

    def __init__(
        self,
        sandbox_id: str,
        generation: int,
        state: str,
        isolation: Literal["microvm", "sandbox"],
        runtime: LocalRuntime,
    ) -> None:
        self.sandbox_id = sandbox_id
        self.generation = generation
        self.state = state
        self.isolation = isolation
        self._runtime = runtime
        self.commands = Commands(self)
        self.files = Filesystem(self)

    @property
    def id(self) -> str:
        return self.sandbox_id

    @classmethod
    def create(
        cls,
        template: str | None = None,
        *,
        command: Sequence[str] | None = None,
        entrypoint: Sequence[str] | None = None,
        timeout: int = 3600,
        envs: Mapping[str, str] | None = None,
        metadata: Mapping[str, str] | None = None,
        name: str | None = None,
        cpus: int | None = None,
        memory_mb: int | None = None,
        isolation: Literal["microvm", "sandbox"] = "microvm",
        filesystem_snapshot_id: str | None = None,
        workspace: str | None = None,
        workdir: str | None = None,
        user: str | None = None,
        hostname: str | None = None,
        mounts: Sequence[VolumeMount] | None = None,
        tmpfs: Sequence[TmpfsMount] | None = None,
        network: SandboxNetwork | None = None,
        ports: Sequence[PortMapping] | None = None,
        dns: Sequence[str] | None = None,
        host_aliases: Mapping[str, str] | None = None,
        read_only: bool = False,
        persistent: bool = False,
        auto_remove: bool = True,
        runtime: LocalRuntime | None = None,
    ) -> Sandbox:
        local_runtime = _ensure_compatible_runtime(runtime or A3SLocalRuntime())
        result = local_runtime.request(
            _create_request(
                template,
                command,
                entrypoint,
                timeout,
                envs,
                metadata,
                name,
                cpus,
                memory_mb,
                isolation,
                filesystem_snapshot_id,
                workspace,
                workdir,
                user,
                hostname,
                mounts,
                tmpfs,
                network,
                ports,
                dns,
                host_aliases,
                read_only,
                persistent,
                auto_remove,
            )
        )
        return cls._from_result(result, local_runtime)

    @classmethod
    def connect(
        cls,
        sandbox_id: str,
        *,
        runtime: LocalRuntime | None = None,
    ) -> Sandbox:
        local_runtime = _ensure_compatible_runtime(runtime or A3SLocalRuntime())
        result = local_runtime.request(
            {"operation": "sandbox_inspect", "sandbox_id": sandbox_id}
        )
        return cls._from_result(result, local_runtime)

    @classmethod
    def _from_result(
        cls,
        result: Mapping[str, Any],
        runtime: LocalRuntime,
    ) -> Sandbox:
        return cls(
            sandbox_id=_string(result["sandbox_id"]),
            generation=_integer(result["generation"]),
            state=_string(result["state"]),
            isolation=_isolation(result),
            runtime=runtime,
        )

    def kill(self) -> None:
        if self.state in {"killed", "removed"}:
            return
        self._runtime.request(self._lifecycle_request("sandbox_kill"))
        self.state = "killed"

    def stop(self) -> None:
        if self.state in {"killed", "removed"}:
            return
        result = self._runtime.request(
            self._lifecycle_request("sandbox_stop")
        )
        self._update_lifecycle(result, fallback_state="stopped")

    def restart(
        self,
        *,
        operation_id: str | None = None,
        stop_timeout: int | None = None,
    ) -> None:
        if self.state in {"killed", "removed"}:
            raise ValueError(f"sandbox {self.sandbox_id} has been removed")
        if operation_id is not None and not operation_id.strip():
            raise ValueError("operation_id cannot be empty")
        if stop_timeout is not None and stop_timeout < 0:
            raise ValueError("stop_timeout cannot be negative")
        result = self._runtime.request(
            {
                **self._lifecycle_request("sandbox_restart"),
                "operation_id": (
                    operation_id
                    if operation_id is not None
                    else f"sdk-restart-{uuid.uuid4()}"
                ),
                **(
                    {}
                    if stop_timeout is None
                    else {"stop_timeout_seconds": stop_timeout}
                ),
            }
        )
        self._update_lifecycle(result, fallback_state="running")

    def remove(self) -> None:
        if self.state in {"removed", "killed"}:
            return
        self._runtime.request(self._lifecycle_request("sandbox_remove"))
        self.state = "removed"

    def pause(self, *, keep_memory: bool = True) -> None:
        result = self._runtime.request(
            {
                **self._lifecycle_request("sandbox_pause"),
                "keep_memory": keep_memory,
            }
        )
        self._update_lifecycle(result, fallback_state="paused")

    def resume(self) -> None:
        result = self._runtime.request(self._lifecycle_request("sandbox_resume"))
        self._update_lifecycle(result, fallback_state="running")

    def is_running(self) -> bool:
        try:
            result = self._runtime.request(
                {
                    "operation": "sandbox_inspect",
                    "sandbox_id": self.sandbox_id,
                }
            )
        except A3SBoxError as error:
            if error.code == "not_found":
                return False
            raise
        self._update_lifecycle(result, fallback_state=self.state)
        return self.state == "running"

    def logs(self, *, tail: int = 100) -> list[SandboxLogEntry]:
        if not 1 <= tail <= 10_000:
            raise ValueError("tail must be between 1 and 10000")
        if self.state in {"killed", "removed"}:
            raise ValueError(f"sandbox {self.sandbox_id} has been removed")
        result = self._runtime.request(
            {
                **self._lifecycle_request("sandbox_logs"),
                "tail": tail,
            }
        )
        return [
            _sandbox_log_entry(item)
            for item in _mapping_sequence(result["logs"])
        ]

    def stats(self) -> SandboxStats | None:
        if self.state in {"killed", "removed"}:
            return None
        result = self._runtime.request(
            self._lifecycle_request("sandbox_stats")
        )
        value = result.get("stats")
        return None if value is None else _sandbox_stats(_mapping(value))

    def processes(self) -> ExecutionProcessInventory:
        _require_observable(self.sandbox_id, self.state)
        inventory = _execution_process_inventory(
            self._runtime.request(
                self._lifecycle_request("sandbox_processes")
            )
        )
        _validate_execution_identity(
            self.sandbox_id,
            self.generation,
            inventory.execution_id,
            inventory.generation,
        )
        _validate_process_inventory(inventory)
        return inventory

    def runtime_stats(self) -> ExecutionStats:
        _require_observable(self.sandbox_id, self.state)
        stats = _execution_stats(
            self._runtime.request(
                self._lifecycle_request("sandbox_runtime_stats")
            )
        )
        _validate_execution_identity(
            self.sandbox_id,
            self.generation,
            stats.execution_id,
            stats.generation,
        )
        _validate_runtime_stats(stats)
        return stats

    def events(
        self,
        *,
        after_sequence: int = 0,
        limit: int = 256,
        wait_timeout_ms: int | None = None,
    ) -> ExecutionEventBatch:
        _require_observable(self.sandbox_id, self.state)
        batch = _execution_event_batch(
            self._runtime.request(
                {
                    **self._lifecycle_request("sandbox_events"),
                    **_events_request(
                        after_sequence,
                        limit,
                        wait_timeout_ms,
                    ),
                }
            )
        )
        _validate_execution_identity(
            self.sandbox_id,
            self.generation,
            batch.execution_id,
            batch.generation,
        )
        _validate_event_batch(batch, after_sequence)
        return batch

    def stream_events(
        self,
        *,
        after_sequence: int = 0,
        batch_size: int = DEFAULT_EVENT_STREAM_BATCH_ITEMS,
        wait_timeout_ms: int = DEFAULT_EVENT_STREAM_WAIT_TIMEOUT_MS,
        cancel_event: threading.Event | None = None,
    ) -> Iterator[ExecutionRuntimeEvent]:
        """Continuously yield ordered events from this exact generation.

        Breaking iteration releases the stream without a background worker.
        ``cancel_event`` is checked between bounded long polls, so cross-thread
        cancellation completes within ``wait_timeout_ms``.
        """

        _require_observable(self.sandbox_id, self.state)
        request = _event_stream_request(
            after_sequence,
            batch_size,
            wait_timeout_ms,
        )
        generation = self.generation

        def iterator() -> Iterator[ExecutionRuntimeEvent]:
            cursor = after_sequence
            while cancel_event is None or not cancel_event.is_set():
                _require_stream_generation(
                    self.sandbox_id,
                    generation,
                    self.generation,
                    self.state,
                )
                batch = _execution_event_batch(
                    self._runtime.request(
                        {
                            **self._lifecycle_request("sandbox_events"),
                            **request,
                            "generation": generation,
                            "after_sequence": cursor,
                        }
                    )
                )
                _validate_execution_identity(
                    self.sandbox_id,
                    generation,
                    batch.execution_id,
                    batch.generation,
                )
                _validate_event_batch(batch, cursor)
                cursor = batch.next_sequence
                if cancel_event is not None and cancel_event.is_set():
                    return
                yield from batch.events

        return iterator()

    def update_resources(
        self,
        update: ExecutionResourceUpdate,
        *,
        operation_id: str | None = None,
    ) -> None:
        _require_running(self.sandbox_id, self.state)
        if operation_id is not None and (
            not isinstance(operation_id, str) or not operation_id.strip()
        ):
            raise ValueError("operation_id cannot be empty")
        if not isinstance(update, ExecutionResourceUpdate):
            raise ValueError("update must be an ExecutionResourceUpdate")
        result = self._runtime.request(
            {
                **self._lifecycle_request("sandbox_update_resources"),
                "operation_id": (
                    operation_id
                    if operation_id is not None
                    else f"sdk-resource-update-{uuid.uuid4()}"
                ),
                "resources": update.bridge_value(),
            }
        )
        _validate_resource_update_response(
            self.sandbox_id,
            self.generation,
            self.isolation,
            result,
        )
        self._update_lifecycle(result, fallback_state="running")

    def create_filesystem_snapshot(
        self,
        snapshot_id: str,
    ) -> FilesystemSnapshotInfo:
        result = self._runtime.request(
            {
                **self._lifecycle_request("sandbox_snapshot_create"),
                "snapshot_id": snapshot_id,
            }
        )
        self._update_lifecycle(result, fallback_state=self.state)
        return _snapshot_info(result)

    def script(self, source: str | bytes | Script) -> ScriptBuilder:
        return self.commands.script(source)

    @classmethod
    def filesystem_snapshot_size(
        cls,
        snapshot_id: str,
        *,
        runtime: LocalRuntime | None = None,
    ) -> int | None:
        result = _ensure_compatible_runtime(runtime or A3SLocalRuntime()).request(
            {
                "operation": "filesystem_snapshot_size",
                "snapshot_id": snapshot_id,
            }
        )
        size = result.get("size_bytes")
        return None if size is None else _integer(size)

    @classmethod
    def delete_filesystem_snapshot(
        cls,
        snapshot_id: str,
        *,
        runtime: LocalRuntime | None = None,
    ) -> bool:
        result = _ensure_compatible_runtime(runtime or A3SLocalRuntime()).request(
            {
                "operation": "filesystem_snapshot_delete",
                "snapshot_id": snapshot_id,
            }
        )
        return _boolean(result["deleted"])

    def _lifecycle_request(self, operation: str) -> dict[str, object]:
        return {
            "operation": operation,
            "sandbox_id": self.sandbox_id,
            "generation": self.generation,
        }

    def _update_lifecycle(
        self,
        result: Mapping[str, Any],
        *,
        fallback_state: str,
    ) -> None:
        self.generation = _integer(
            result.get("generation", self.generation)
        )
        self.state = _string(result.get("state", fallback_state))
        if "isolation" in result:
            self.isolation = _isolation(result)

    def __enter__(self) -> Sandbox:
        return self

    def __exit__(self, *_: object) -> bool:
        self.kill()
        return False


class Commands:
    def __init__(self, sandbox: Sandbox) -> None:
        self._sandbox = sandbox

    def run(
        self,
        command: str | Sequence[str],
        *,
        timeout: float | None = None,
        envs: Mapping[str, str] | None = None,
        cwd: str | None = None,
        user: str | None = None,
        stdin: str | bytes | None = None,
    ) -> CommandResult:
        result = self._sandbox._runtime.request(
            _command_request(
                self._sandbox,
                command,
                timeout,
                envs,
                cwd,
                user,
                stdin,
            )
        )
        return _command_result(result)

    def script(self, source: str | bytes | Script) -> ScriptBuilder:
        return ScriptBuilder(self, source)

    def run_script(
        self,
        source: str | bytes | Script,
        *,
        timeout: float | None = None,
        envs: Mapping[str, str] | None = None,
        cwd: str | None = None,
        user: str | None = None,
    ) -> CommandResult:
        script = self.script(source)
        if timeout is not None:
            script.timeout(timeout)
        for key, value in (envs or {}).items():
            script.env(key, value)
        if cwd is not None:
            script.cwd(cwd)
        if user is not None:
            script.user(user)
        return script.run()


class Filesystem:
    def __init__(self, sandbox: Sandbox) -> None:
        self._sandbox = sandbox

    def write(
        self,
        path: str,
        data: str | bytes,
        *,
        user: str | None = None,
    ) -> WriteInfo:
        raw = data.encode() if isinstance(data, str) else data
        result = self._sandbox._runtime.request(
            {
                **self._request("file_write", path, user=user),
                "data_base64": base64.b64encode(raw).decode(),
            }
        )
        return WriteInfo(
            path=_string(result["path"]),
            size=_integer(result["size"]),
        )

    def read(
        self,
        path: str,
        *,
        format: Literal["text", "bytes"] = "text",
        user: str | None = None,
    ) -> str | bytes:
        data = self._read_bytes(path, user=user)
        return data if format == "bytes" else data.decode()

    def _read_bytes(
        self,
        path: str,
        *,
        user: str | None,
        max_bytes: int | None = None,
    ) -> bytes:
        request = self._request("file_read", path, user=user)
        if max_bytes is not None:
            request["max_bytes"] = max_bytes
        result = self._sandbox._runtime.request(request)
        return _decoded_file_result(result, path)

    def export(
        self,
        path: str,
        *,
        max_bytes: int = MAX_ARTIFACT_BYTES,
        destination: str | os.PathLike[str] | None = None,
        user: str | None = None,
    ) -> Artifact:
        path = _artifact_source_path(path)
        limit = _artifact_limit(max_bytes)
        host_path = _artifact_destination(destination)
        entry = self.stat(path, user=user)
        if entry.type != "file":
            raise A3SBoxError(
                f"artifact source {path!r} must be a file",
                code="invalid_request",
            )
        if entry.size < 0:
            raise A3SBoxError(
                "A3S Box bridge returned a negative artifact size",
                code="bridge_protocol_error",
            )
        if entry.size > limit:
            raise A3SBoxError(
                f"artifact source is {entry.size} bytes; max_bytes is {limit}",
                code="invalid_request",
            )
        data = self._read_bytes(path, user=user, max_bytes=limit)
        if len(data) > limit:
            raise A3SBoxError(
                f"artifact source grew beyond max_bytes ({limit}) while reading",
                code="bridge_protocol_error",
            )
        if len(data) != entry.size:
            raise A3SBoxError(
                "artifact source changed size while it was being exported",
                code="bridge_protocol_error",
            )
        if host_path is not None:
            _write_new_host_file(host_path, data)
        return Artifact(
            path=path,
            data=data,
            size=len(data),
            sha256=hashlib.sha256(data).hexdigest(),
            host_path=host_path,
        )

    def stat(self, path: str, *, user: str | None = None) -> EntryInfo:
        result = self._sandbox._runtime.request(
            self._request("filesystem_stat", path, user=user)
        )
        return _entry_info(_mapping(result["entry"]))

    def exists(self, path: str, *, user: str | None = None) -> bool:
        try:
            self.stat(path, user=user)
        except A3SBoxError as error:
            if error.code == "not_found":
                return False
            raise
        return True

    def list(
        self,
        path: str,
        *,
        depth: int = 1,
        user: str | None = None,
    ) -> list[EntryInfo]:
        result = self._sandbox._runtime.request(
            {
                **self._request("filesystem_list", path, user=user),
                "depth": depth,
            }
        )
        return [
            _entry_info(entry)
            for entry in _mapping_sequence(result["entries"])
        ]

    def make_dir(self, path: str, *, user: str | None = None) -> EntryInfo | None:
        result = self._sandbox._runtime.request(
            self._request("filesystem_make_dir", path, user=user)
        )
        entry = result.get("entry")
        return None if entry is None else _entry_info(_mapping(entry))

    def rename(
        self,
        old_path: str,
        new_path: str,
        *,
        user: str | None = None,
    ) -> EntryInfo | None:
        result = self._sandbox._runtime.request(
            {
                **self._request("filesystem_move", old_path, user=user),
                "destination": new_path,
            }
        )
        entry = result.get("entry")
        return None if entry is None else _entry_info(_mapping(entry))

    def remove(self, path: str, *, user: str | None = None) -> None:
        self._sandbox._runtime.request(
            self._request("filesystem_remove", path, user=user)
        )

    def _request(
        self,
        operation: str,
        path: str,
        *,
        user: str | None,
    ) -> dict[str, object]:
        request: dict[str, object] = {
            "operation": operation,
            "sandbox_id": self._sandbox.sandbox_id,
            "generation": self._sandbox.generation,
            "path": path,
        }
        if user is not None:
            request["user"] = user
        return request


class AsyncSandbox:
    """Async counterpart of :class:`Sandbox` for the local runtime."""

    def __init__(
        self,
        sandbox_id: str,
        generation: int,
        state: str,
        isolation: Literal["microvm", "sandbox"],
        runtime: AsyncLocalRuntime,
    ) -> None:
        self.sandbox_id = sandbox_id
        self.generation = generation
        self.state = state
        self.isolation = isolation
        self._runtime = runtime
        self.commands = AsyncCommands(self)
        self.files = AsyncFilesystem(self)

    @property
    def id(self) -> str:
        return self.sandbox_id

    @classmethod
    async def create(
        cls,
        template: str | None = None,
        *,
        command: Sequence[str] | None = None,
        entrypoint: Sequence[str] | None = None,
        timeout: int = 3600,
        envs: Mapping[str, str] | None = None,
        metadata: Mapping[str, str] | None = None,
        name: str | None = None,
        cpus: int | None = None,
        memory_mb: int | None = None,
        isolation: Literal["microvm", "sandbox"] = "microvm",
        filesystem_snapshot_id: str | None = None,
        workspace: str | None = None,
        workdir: str | None = None,
        user: str | None = None,
        hostname: str | None = None,
        mounts: Sequence[VolumeMount] | None = None,
        tmpfs: Sequence[TmpfsMount] | None = None,
        network: SandboxNetwork | None = None,
        ports: Sequence[PortMapping] | None = None,
        dns: Sequence[str] | None = None,
        host_aliases: Mapping[str, str] | None = None,
        read_only: bool = False,
        persistent: bool = False,
        auto_remove: bool = True,
        runtime: AsyncLocalRuntime | None = None,
    ) -> AsyncSandbox:
        local_runtime = _ensure_compatible_async_runtime(
            runtime or A3SAsyncLocalRuntime()
        )
        result = await local_runtime.request(
            _create_request(
                template,
                command,
                entrypoint,
                timeout,
                envs,
                metadata,
                name,
                cpus,
                memory_mb,
                isolation,
                filesystem_snapshot_id,
                workspace,
                workdir,
                user,
                hostname,
                mounts,
                tmpfs,
                network,
                ports,
                dns,
                host_aliases,
                read_only,
                persistent,
                auto_remove,
            )
        )
        return cls._from_result(result, local_runtime)

    @classmethod
    async def connect(
        cls,
        sandbox_id: str,
        *,
        runtime: AsyncLocalRuntime | None = None,
    ) -> AsyncSandbox:
        local_runtime = _ensure_compatible_async_runtime(
            runtime or A3SAsyncLocalRuntime()
        )
        result = await local_runtime.request(
            {"operation": "sandbox_inspect", "sandbox_id": sandbox_id}
        )
        return cls._from_result(result, local_runtime)

    @classmethod
    def _from_result(
        cls,
        result: Mapping[str, Any],
        runtime: AsyncLocalRuntime,
    ) -> AsyncSandbox:
        return cls(
            sandbox_id=_string(result["sandbox_id"]),
            generation=_integer(result["generation"]),
            state=_string(result["state"]),
            isolation=_isolation(result),
            runtime=runtime,
        )

    async def kill(self) -> None:
        if self.state in {"killed", "removed"}:
            return
        await self._runtime.request(self._lifecycle_request("sandbox_kill"))
        self.state = "killed"

    async def stop(self) -> None:
        if self.state in {"killed", "removed"}:
            return
        result = await self._runtime.request(
            self._lifecycle_request("sandbox_stop")
        )
        self._update_lifecycle(result, fallback_state="stopped")

    async def restart(
        self,
        *,
        operation_id: str | None = None,
        stop_timeout: int | None = None,
    ) -> None:
        if self.state in {"killed", "removed"}:
            raise ValueError(f"sandbox {self.sandbox_id} has been removed")
        if operation_id is not None and not operation_id.strip():
            raise ValueError("operation_id cannot be empty")
        if stop_timeout is not None and stop_timeout < 0:
            raise ValueError("stop_timeout cannot be negative")
        result = await self._runtime.request(
            {
                **self._lifecycle_request("sandbox_restart"),
                "operation_id": (
                    operation_id
                    if operation_id is not None
                    else f"sdk-restart-{uuid.uuid4()}"
                ),
                **(
                    {}
                    if stop_timeout is None
                    else {"stop_timeout_seconds": stop_timeout}
                ),
            }
        )
        self._update_lifecycle(result, fallback_state="running")

    async def remove(self) -> None:
        if self.state in {"removed", "killed"}:
            return
        await self._runtime.request(
            self._lifecycle_request("sandbox_remove")
        )
        self.state = "removed"

    async def pause(self, *, keep_memory: bool = True) -> None:
        result = await self._runtime.request(
            {
                **self._lifecycle_request("sandbox_pause"),
                "keep_memory": keep_memory,
            }
        )
        self._update_lifecycle(result, fallback_state="paused")

    async def resume(self) -> None:
        result = await self._runtime.request(
            self._lifecycle_request("sandbox_resume")
        )
        self._update_lifecycle(result, fallback_state="running")

    async def is_running(self) -> bool:
        try:
            result = await self._runtime.request(
                {
                    "operation": "sandbox_inspect",
                    "sandbox_id": self.sandbox_id,
                }
            )
        except A3SBoxError as error:
            if error.code == "not_found":
                return False
            raise
        self._update_lifecycle(result, fallback_state=self.state)
        return self.state == "running"

    async def logs(self, *, tail: int = 100) -> list[SandboxLogEntry]:
        if not 1 <= tail <= 10_000:
            raise ValueError("tail must be between 1 and 10000")
        if self.state in {"killed", "removed"}:
            raise ValueError(f"sandbox {self.sandbox_id} has been removed")
        result = await self._runtime.request(
            {
                **self._lifecycle_request("sandbox_logs"),
                "tail": tail,
            }
        )
        return [
            _sandbox_log_entry(item)
            for item in _mapping_sequence(result["logs"])
        ]

    async def stats(self) -> SandboxStats | None:
        if self.state in {"killed", "removed"}:
            return None
        result = await self._runtime.request(
            self._lifecycle_request("sandbox_stats")
        )
        value = result.get("stats")
        return None if value is None else _sandbox_stats(_mapping(value))

    async def processes(self) -> ExecutionProcessInventory:
        _require_observable(self.sandbox_id, self.state)
        inventory = _execution_process_inventory(
            await self._runtime.request(
                self._lifecycle_request("sandbox_processes")
            )
        )
        _validate_execution_identity(
            self.sandbox_id,
            self.generation,
            inventory.execution_id,
            inventory.generation,
        )
        _validate_process_inventory(inventory)
        return inventory

    async def runtime_stats(self) -> ExecutionStats:
        _require_observable(self.sandbox_id, self.state)
        stats = _execution_stats(
            await self._runtime.request(
                self._lifecycle_request("sandbox_runtime_stats")
            )
        )
        _validate_execution_identity(
            self.sandbox_id,
            self.generation,
            stats.execution_id,
            stats.generation,
        )
        _validate_runtime_stats(stats)
        return stats

    async def events(
        self,
        *,
        after_sequence: int = 0,
        limit: int = 256,
        wait_timeout_ms: int | None = None,
    ) -> ExecutionEventBatch:
        _require_observable(self.sandbox_id, self.state)
        batch = _execution_event_batch(
            await self._runtime.request(
                {
                    **self._lifecycle_request("sandbox_events"),
                    **_events_request(
                        after_sequence,
                        limit,
                        wait_timeout_ms,
                    ),
                }
            )
        )
        _validate_execution_identity(
            self.sandbox_id,
            self.generation,
            batch.execution_id,
            batch.generation,
        )
        _validate_event_batch(batch, after_sequence)
        return batch

    def stream_events(
        self,
        *,
        after_sequence: int = 0,
        batch_size: int = DEFAULT_EVENT_STREAM_BATCH_ITEMS,
        wait_timeout_ms: int = DEFAULT_EVENT_STREAM_WAIT_TIMEOUT_MS,
        cancel_event: threading.Event | None = None,
    ) -> AsyncIterator[ExecutionRuntimeEvent]:
        """Continuously yield ordered events from this exact generation."""

        _require_observable(self.sandbox_id, self.state)
        request = _event_stream_request(
            after_sequence,
            batch_size,
            wait_timeout_ms,
        )
        generation = self.generation

        async def iterator() -> AsyncIterator[ExecutionRuntimeEvent]:
            cursor = after_sequence
            while cancel_event is None or not cancel_event.is_set():
                _require_stream_generation(
                    self.sandbox_id,
                    generation,
                    self.generation,
                    self.state,
                )
                batch = _execution_event_batch(
                    await self._runtime.request(
                        {
                            **self._lifecycle_request("sandbox_events"),
                            **request,
                            "generation": generation,
                            "after_sequence": cursor,
                        }
                    )
                )
                _validate_execution_identity(
                    self.sandbox_id,
                    generation,
                    batch.execution_id,
                    batch.generation,
                )
                _validate_event_batch(batch, cursor)
                cursor = batch.next_sequence
                if cancel_event is not None and cancel_event.is_set():
                    return
                for event in batch.events:
                    yield event

        return iterator()

    async def update_resources(
        self,
        update: ExecutionResourceUpdate,
        *,
        operation_id: str | None = None,
    ) -> None:
        _require_running(self.sandbox_id, self.state)
        if operation_id is not None and (
            not isinstance(operation_id, str) or not operation_id.strip()
        ):
            raise ValueError("operation_id cannot be empty")
        if not isinstance(update, ExecutionResourceUpdate):
            raise ValueError("update must be an ExecutionResourceUpdate")
        result = await self._runtime.request(
            {
                **self._lifecycle_request("sandbox_update_resources"),
                "operation_id": (
                    operation_id
                    if operation_id is not None
                    else f"sdk-resource-update-{uuid.uuid4()}"
                ),
                "resources": update.bridge_value(),
            }
        )
        _validate_resource_update_response(
            self.sandbox_id,
            self.generation,
            self.isolation,
            result,
        )
        self._update_lifecycle(result, fallback_state="running")

    async def create_filesystem_snapshot(
        self,
        snapshot_id: str,
    ) -> FilesystemSnapshotInfo:
        result = await self._runtime.request(
            {
                **self._lifecycle_request("sandbox_snapshot_create"),
                "snapshot_id": snapshot_id,
            }
        )
        self._update_lifecycle(result, fallback_state=self.state)
        return _snapshot_info(result)

    def script(self, source: str | bytes | Script) -> AsyncScriptBuilder:
        return self.commands.script(source)

    @classmethod
    async def filesystem_snapshot_size(
        cls,
        snapshot_id: str,
        *,
        runtime: AsyncLocalRuntime | None = None,
    ) -> int | None:
        result = await _ensure_compatible_async_runtime(
            runtime or A3SAsyncLocalRuntime()
        ).request(
            {
                "operation": "filesystem_snapshot_size",
                "snapshot_id": snapshot_id,
            }
        )
        size = result.get("size_bytes")
        return None if size is None else _integer(size)

    @classmethod
    async def delete_filesystem_snapshot(
        cls,
        snapshot_id: str,
        *,
        runtime: AsyncLocalRuntime | None = None,
    ) -> bool:
        result = await _ensure_compatible_async_runtime(
            runtime or A3SAsyncLocalRuntime()
        ).request(
            {
                "operation": "filesystem_snapshot_delete",
                "snapshot_id": snapshot_id,
            }
        )
        return _boolean(result["deleted"])

    def _lifecycle_request(self, operation: str) -> dict[str, object]:
        return {
            "operation": operation,
            "sandbox_id": self.sandbox_id,
            "generation": self.generation,
        }

    def _update_lifecycle(
        self,
        result: Mapping[str, Any],
        *,
        fallback_state: str,
    ) -> None:
        self.generation = _integer(
            result.get("generation", self.generation)
        )
        self.state = _string(result.get("state", fallback_state))
        if "isolation" in result:
            self.isolation = _isolation(result)

    async def __aenter__(self) -> AsyncSandbox:
        return self

    async def __aexit__(self, *_: object) -> bool:
        await self.kill()
        return False


class AsyncCommands:
    def __init__(self, sandbox: AsyncSandbox) -> None:
        self._sandbox = sandbox

    async def run(
        self,
        command: str | Sequence[str],
        *,
        timeout: float | None = None,
        envs: Mapping[str, str] | None = None,
        cwd: str | None = None,
        user: str | None = None,
        stdin: str | bytes | None = None,
    ) -> CommandResult:
        result = await self._sandbox._runtime.request(
            _command_request(
                self._sandbox,
                command,
                timeout,
                envs,
                cwd,
                user,
                stdin,
            )
        )
        return _command_result(result)

    def script(self, source: str | bytes | Script) -> AsyncScriptBuilder:
        return AsyncScriptBuilder(self, source)

    async def run_script(
        self,
        source: str | bytes | Script,
        *,
        timeout: float | None = None,
        envs: Mapping[str, str] | None = None,
        cwd: str | None = None,
        user: str | None = None,
    ) -> CommandResult:
        script = self.script(source)
        if timeout is not None:
            script.timeout(timeout)
        for key, value in (envs or {}).items():
            script.env(key, value)
        if cwd is not None:
            script.cwd(cwd)
        if user is not None:
            script.user(user)
        return await script.run()


class AsyncFilesystem:
    def __init__(self, sandbox: AsyncSandbox) -> None:
        self._sandbox = sandbox

    async def write(
        self,
        path: str,
        data: str | bytes,
        *,
        user: str | None = None,
    ) -> WriteInfo:
        raw = data.encode() if isinstance(data, str) else data
        result = await self._sandbox._runtime.request(
            {
                **self._request("file_write", path, user=user),
                "data_base64": base64.b64encode(raw).decode(),
            }
        )
        return WriteInfo(
            path=_string(result["path"]),
            size=_integer(result["size"]),
        )

    async def read(
        self,
        path: str,
        *,
        format: Literal["text", "bytes"] = "text",
        user: str | None = None,
    ) -> str | bytes:
        data = await self._read_bytes(path, user=user)
        return data if format == "bytes" else data.decode()

    async def _read_bytes(
        self,
        path: str,
        *,
        user: str | None,
        max_bytes: int | None = None,
    ) -> bytes:
        request = self._request("file_read", path, user=user)
        if max_bytes is not None:
            request["max_bytes"] = max_bytes
        result = await self._sandbox._runtime.request(request)
        return _decoded_file_result(result, path)

    async def export(
        self,
        path: str,
        *,
        max_bytes: int = MAX_ARTIFACT_BYTES,
        destination: str | os.PathLike[str] | None = None,
        user: str | None = None,
    ) -> Artifact:
        path = _artifact_source_path(path)
        limit = _artifact_limit(max_bytes)
        host_path = _artifact_destination(destination)
        entry = await self.stat(path, user=user)
        if entry.type != "file":
            raise A3SBoxError(
                f"artifact source {path!r} must be a file",
                code="invalid_request",
            )
        if entry.size < 0:
            raise A3SBoxError(
                "A3S Box bridge returned a negative artifact size",
                code="bridge_protocol_error",
            )
        if entry.size > limit:
            raise A3SBoxError(
                f"artifact source is {entry.size} bytes; max_bytes is {limit}",
                code="invalid_request",
            )
        data = await self._read_bytes(path, user=user, max_bytes=limit)
        if len(data) > limit:
            raise A3SBoxError(
                f"artifact source grew beyond max_bytes ({limit}) while reading",
                code="bridge_protocol_error",
            )
        if len(data) != entry.size:
            raise A3SBoxError(
                "artifact source changed size while it was being exported",
                code="bridge_protocol_error",
            )
        if host_path is not None:
            await asyncio.to_thread(_write_new_host_file, host_path, data)
        return Artifact(
            path=path,
            data=data,
            size=len(data),
            sha256=hashlib.sha256(data).hexdigest(),
            host_path=host_path,
        )

    async def stat(self, path: str, *, user: str | None = None) -> EntryInfo:
        result = await self._sandbox._runtime.request(
            self._request("filesystem_stat", path, user=user)
        )
        return _entry_info(_mapping(result["entry"]))

    async def exists(self, path: str, *, user: str | None = None) -> bool:
        try:
            await self.stat(path, user=user)
        except A3SBoxError as error:
            if error.code == "not_found":
                return False
            raise
        return True

    async def list(
        self,
        path: str,
        *,
        depth: int = 1,
        user: str | None = None,
    ) -> list[EntryInfo]:
        result = await self._sandbox._runtime.request(
            {
                **self._request("filesystem_list", path, user=user),
                "depth": depth,
            }
        )
        return [
            _entry_info(entry)
            for entry in _mapping_sequence(result["entries"])
        ]

    async def make_dir(
        self,
        path: str,
        *,
        user: str | None = None,
    ) -> EntryInfo | None:
        result = await self._sandbox._runtime.request(
            self._request("filesystem_make_dir", path, user=user)
        )
        entry = result.get("entry")
        return None if entry is None else _entry_info(_mapping(entry))

    async def rename(
        self,
        old_path: str,
        new_path: str,
        *,
        user: str | None = None,
    ) -> EntryInfo | None:
        result = await self._sandbox._runtime.request(
            {
                **self._request("filesystem_move", old_path, user=user),
                "destination": new_path,
            }
        )
        entry = result.get("entry")
        return None if entry is None else _entry_info(_mapping(entry))

    async def remove(self, path: str, *, user: str | None = None) -> None:
        await self._sandbox._runtime.request(
            self._request("filesystem_remove", path, user=user)
        )

    def _request(
        self,
        operation: str,
        path: str,
        *,
        user: str | None,
    ) -> dict[str, object]:
        request: dict[str, object] = {
            "operation": operation,
            "sandbox_id": self._sandbox.sandbox_id,
            "generation": self._sandbox.generation,
            "path": path,
        }
        if user is not None:
            request["user"] = user
        return request
