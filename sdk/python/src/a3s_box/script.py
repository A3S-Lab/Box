"""Fluent stdin-backed script builders."""

from __future__ import annotations

from collections.abc import Mapping, Sequence
from typing import Protocol

from .models import CommandResult, Script


class _SyncCommands(Protocol):
    def run(
        self,
        command: Sequence[str],
        *,
        timeout: float | None = None,
        envs: Mapping[str, str] | None = None,
        cwd: str | None = None,
        user: str | None = None,
        stdin: str | bytes | None = None,
    ) -> CommandResult:
        ...


class _AsyncCommands(Protocol):
    async def run(
        self,
        command: Sequence[str],
        *,
        timeout: float | None = None,
        envs: Mapping[str, str] | None = None,
        cwd: str | None = None,
        user: str | None = None,
        stdin: str | bytes | None = None,
    ) -> CommandResult:
        ...


class ScriptBuilder:
    """Fluent, stdin-backed script execution builder."""

    def __init__(
        self,
        commands: _SyncCommands,
        script: str | bytes | Script,
    ) -> None:
        self._commands = commands
        if isinstance(script, Script):
            self._source = script.source
            self._interpreter = list(script.interpreter)
        else:
            self._source = script
            self._interpreter = ["/bin/sh", "-se"]
        self._timeout: float | None = None
        self._envs: dict[str, str] = {}
        self._cwd: str | None = None
        self._user: str | None = None

    def interpreter(self, executable: str, *args: str) -> ScriptBuilder:
        self._interpreter = [executable, *args]
        return self

    def timeout(self, seconds: float) -> ScriptBuilder:
        self._timeout = seconds
        return self

    def env(self, key: str, value: str) -> ScriptBuilder:
        self._envs[key] = value
        return self

    def cwd(self, path: str) -> ScriptBuilder:
        self._cwd = path
        return self

    def user(self, user: str) -> ScriptBuilder:
        self._user = user
        return self

    def run(self) -> CommandResult:
        _validate_script(self._source, self._interpreter)
        return self._commands.run(
            self._interpreter,
            timeout=self._timeout,
            envs=self._envs,
            cwd=self._cwd,
            user=self._user,
            stdin=self._source,
        )


class AsyncScriptBuilder:
    """Async counterpart of :class:`ScriptBuilder`."""

    def __init__(
        self,
        commands: _AsyncCommands,
        script: str | bytes | Script,
    ) -> None:
        self._commands = commands
        if isinstance(script, Script):
            self._source = script.source
            self._interpreter = list(script.interpreter)
        else:
            self._source = script
            self._interpreter = ["/bin/sh", "-se"]
        self._timeout: float | None = None
        self._envs: dict[str, str] = {}
        self._cwd: str | None = None
        self._user: str | None = None

    def interpreter(self, executable: str, *args: str) -> AsyncScriptBuilder:
        self._interpreter = [executable, *args]
        return self

    def timeout(self, seconds: float) -> AsyncScriptBuilder:
        self._timeout = seconds
        return self

    def env(self, key: str, value: str) -> AsyncScriptBuilder:
        self._envs[key] = value
        return self

    def cwd(self, path: str) -> AsyncScriptBuilder:
        self._cwd = path
        return self

    def user(self, user: str) -> AsyncScriptBuilder:
        self._user = user
        return self

    async def run(self) -> CommandResult:
        _validate_script(self._source, self._interpreter)
        return await self._commands.run(
            self._interpreter,
            timeout=self._timeout,
            envs=self._envs,
            cwd=self._cwd,
            user=self._user,
            stdin=self._source,
        )


def _validate_script(source: str | bytes, interpreter: Sequence[str]) -> None:
    if not source:
        raise ValueError("script source cannot be empty")
    if not interpreter:
        raise ValueError("script interpreter cannot be empty")
