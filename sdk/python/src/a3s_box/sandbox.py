"""E2B-like Python API backed by the local A3S Box runtime."""

from __future__ import annotations

import base64
import uuid
from collections.abc import Mapping, Sequence
from typing import Any, Literal, cast

from ._bridge_values import (
    command_result as _command_result,
    entry_info as _entry_info,
    filesystem_snapshot_info as _snapshot_info,
    mapping as _mapping,
    mapping_sequence as _mapping_sequence,
    sandbox_log_entry as _sandbox_log_entry,
    sandbox_stats as _sandbox_stats,
)
from .exceptions import A3SBoxError
from ._sandbox_requests import (
    DEFAULT_IMAGE,
    command_request as _command_request,
    create_request as _create_request,
)
from .models import (
    CommandResult,
    EntryInfo,
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
)
from .script import AsyncScriptBuilder, ScriptBuilder


class Sandbox:
    """A local A3S Box Sandbox with familiar E2B-style namespaces."""

    def __init__(
        self,
        sandbox_id: str,
        generation: int,
        state: str,
        runtime: LocalRuntime,
    ) -> None:
        self.sandbox_id = sandbox_id
        self.generation = generation
        self.state = state
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
        local_runtime = runtime or A3SLocalRuntime()
        result = local_runtime.request(
            _create_request(
                template,
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
        local_runtime = runtime or A3SLocalRuntime()
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
            sandbox_id=str(result["sandbox_id"]),
            generation=int(result["generation"]),
            state=str(result["state"]),
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
        result = (runtime or A3SLocalRuntime()).request(
            {
                "operation": "filesystem_snapshot_size",
                "snapshot_id": snapshot_id,
            }
        )
        size = result.get("size_bytes")
        return None if size is None else int(size)

    @classmethod
    def delete_filesystem_snapshot(
        cls,
        snapshot_id: str,
        *,
        runtime: LocalRuntime | None = None,
    ) -> bool:
        result = (runtime or A3SLocalRuntime()).request(
            {
                "operation": "filesystem_snapshot_delete",
                "snapshot_id": snapshot_id,
            }
        )
        return bool(result["deleted"])

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
        self.generation = int(result.get("generation", self.generation))
        self.state = str(result.get("state", fallback_state))

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
        return WriteInfo(path=str(result["path"]), size=int(result["size"]))

    def read(
        self,
        path: str,
        *,
        format: Literal["text", "bytes"] = "text",
        user: str | None = None,
    ) -> str | bytes:
        result = self._sandbox._runtime.request(
            self._request("file_read", path, user=user)
        )
        data = base64.b64decode(str(result["data_base64"]), validate=True)
        return data if format == "bytes" else data.decode()

    def stat(self, path: str, *, user: str | None = None) -> EntryInfo:
        result = self._sandbox._runtime.request(
            self._request("filesystem_stat", path, user=user)
        )
        return _entry_info(cast(Mapping[str, Any], result["entry"]))

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
            _entry_info(cast(Mapping[str, Any], entry))
            for entry in cast(list[object], result["entries"])
        ]

    def make_dir(self, path: str, *, user: str | None = None) -> EntryInfo | None:
        result = self._sandbox._runtime.request(
            self._request("filesystem_make_dir", path, user=user)
        )
        entry = result.get("entry")
        return _entry_info(cast(Mapping[str, Any], entry)) if entry else None

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
        return _entry_info(cast(Mapping[str, Any], entry)) if entry else None

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
        runtime: AsyncLocalRuntime,
    ) -> None:
        self.sandbox_id = sandbox_id
        self.generation = generation
        self.state = state
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
        local_runtime = runtime or A3SAsyncLocalRuntime()
        result = await local_runtime.request(
            _create_request(
                template,
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
        local_runtime = runtime or A3SAsyncLocalRuntime()
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
            sandbox_id=str(result["sandbox_id"]),
            generation=int(result["generation"]),
            state=str(result["state"]),
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
        result = await (runtime or A3SAsyncLocalRuntime()).request(
            {
                "operation": "filesystem_snapshot_size",
                "snapshot_id": snapshot_id,
            }
        )
        size = result.get("size_bytes")
        return None if size is None else int(size)

    @classmethod
    async def delete_filesystem_snapshot(
        cls,
        snapshot_id: str,
        *,
        runtime: AsyncLocalRuntime | None = None,
    ) -> bool:
        result = await (runtime or A3SAsyncLocalRuntime()).request(
            {
                "operation": "filesystem_snapshot_delete",
                "snapshot_id": snapshot_id,
            }
        )
        return bool(result["deleted"])

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
        self.generation = int(result.get("generation", self.generation))
        self.state = str(result.get("state", fallback_state))

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
        return WriteInfo(path=str(result["path"]), size=int(result["size"]))

    async def read(
        self,
        path: str,
        *,
        format: Literal["text", "bytes"] = "text",
        user: str | None = None,
    ) -> str | bytes:
        result = await self._sandbox._runtime.request(
            self._request("file_read", path, user=user)
        )
        data = base64.b64decode(str(result["data_base64"]), validate=True)
        return data if format == "bytes" else data.decode()

    async def stat(self, path: str, *, user: str | None = None) -> EntryInfo:
        result = await self._sandbox._runtime.request(
            self._request("filesystem_stat", path, user=user)
        )
        return _entry_info(cast(Mapping[str, Any], result["entry"]))

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
            _entry_info(cast(Mapping[str, Any], entry))
            for entry in cast(list[object], result["entries"])
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
        return _entry_info(cast(Mapping[str, Any], entry)) if entry else None

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
        return _entry_info(cast(Mapping[str, Any], entry)) if entry else None

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
