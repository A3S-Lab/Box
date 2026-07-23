"""E2B-like Python API backed by the local A3S Box runtime."""

from __future__ import annotations

import base64
from collections.abc import Mapping, Sequence
from typing import Any, Literal, cast

from .exceptions import A3SBoxError
from .models import CommandResult, EntryInfo, WriteInfo
from .runtime import (
    A3SAsyncLocalRuntime,
    A3SLocalRuntime,
    AsyncLocalRuntime,
    LocalRuntime,
)

DEFAULT_IMAGE = "alpine:3.20"


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
        if self.state == "killed":
            return
        self._runtime.request(self._lifecycle_request("sandbox_kill"))
        self.state = "killed"

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
        if self.state == "killed":
            return
        await self._runtime.request(self._lifecycle_request("sandbox_kill"))
        self.state = "killed"

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


def _create_request(
    template: str | None,
    timeout: int,
    envs: Mapping[str, str] | None,
    metadata: Mapping[str, str] | None,
    name: str | None,
    cpus: int | None,
    memory_mb: int | None,
    isolation: str,
) -> dict[str, object]:
    if timeout <= 0:
        raise ValueError("timeout must be greater than zero")
    request: dict[str, object] = {
        "operation": "sandbox_create",
        "image": template or DEFAULT_IMAGE,
        "timeout_seconds": timeout,
        "env": dict(envs or {}),
        "labels": dict(metadata or {}),
        "isolation": isolation,
    }
    if name is not None:
        request["name"] = name
    if cpus is not None:
        request["cpus"] = cpus
    if memory_mb is not None:
        request["memory_mb"] = memory_mb
    return request


def _command_request(
    sandbox: Sandbox | AsyncSandbox,
    command: str | Sequence[str],
    timeout: float | None,
    envs: Mapping[str, str] | None,
    cwd: str | None,
    user: str | None,
    stdin: str | bytes | None,
) -> dict[str, object]:
    argv = ["/bin/sh", "-lc", command] if isinstance(command, str) else list(command)
    if not argv:
        raise ValueError("command cannot be empty")
    request: dict[str, object] = {
        "operation": "command_run",
        "sandbox_id": sandbox.sandbox_id,
        "generation": sandbox.generation,
        "argv": argv,
        "env": dict(envs or {}),
    }
    if timeout is not None:
        if timeout <= 0:
            raise ValueError("timeout must be greater than zero")
        request["timeout_ms"] = int(timeout * 1000)
    if cwd is not None:
        request["cwd"] = cwd
    if user is not None:
        request["user"] = user
    if stdin is not None:
        raw = stdin.encode() if isinstance(stdin, str) else stdin
        request["stdin_base64"] = base64.b64encode(raw).decode()
    return request


def _command_result(result: Mapping[str, Any]) -> CommandResult:
    stdout = base64.b64decode(str(result.get("stdout_base64", "")), validate=True)
    stderr = base64.b64decode(str(result.get("stderr_base64", "")), validate=True)
    return CommandResult(
        stdout=stdout.decode(errors="replace"),
        stderr=stderr.decode(errors="replace"),
        exit_code=int(result["exit_code"]),
        truncated=bool(result.get("truncated", False)),
    )


def _entry_info(entry: Mapping[str, Any]) -> EntryInfo:
    return EntryInfo(
        name=str(entry["name"]),
        type=cast(
            Literal["file", "directory", "unspecified"],
            str(entry["type"]),
        ),
        path=str(entry["path"]),
        size=int(entry["size"]),
        mode=int(entry["mode"]),
        permissions=str(entry["permissions"]),
        owner=str(entry["owner"]),
        group=str(entry["group"]),
        modified_seconds=int(entry["modified_seconds"]),
        modified_nanos=int(entry["modified_nanos"]),
        symlink_target=(
            None
            if entry.get("symlink_target") is None
            else str(entry["symlink_target"])
        ),
    )
