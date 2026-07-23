"""Small local Code Interpreter facade built on the native Sandbox API."""

from __future__ import annotations

from collections.abc import Mapping

from .models import CommandResult
from .runtime import AsyncLocalRuntime, LocalRuntime
from .sandbox import AsyncSandbox as BaseAsyncSandbox
from .sandbox import Sandbox as BaseSandbox

DEFAULT_CODE_INTERPRETER_IMAGE = "python:3.12-alpine"


class Sandbox(BaseSandbox):
    @classmethod
    def create(
        cls,
        template: str | None = None,
        *,
        timeout: int = 3600,
        envs: Mapping[str, str] | None = None,
        metadata: Mapping[str, str] | None = None,
        runtime: LocalRuntime | None = None,
    ) -> Sandbox:
        return super().create(
            template or DEFAULT_CODE_INTERPRETER_IMAGE,
            timeout=timeout,
            envs=envs,
            metadata=metadata,
            runtime=runtime,
        )

    def run_code(
        self,
        code: str,
        *,
        language: str = "python",
        timeout: float | None = None,
    ) -> CommandResult:
        if language != "python":
            raise ValueError("the local Code Interpreter currently supports Python only")
        return self.commands.run(["python", "-c", code], timeout=timeout)


class AsyncSandbox(BaseAsyncSandbox):
    @classmethod
    async def create(
        cls,
        template: str | None = None,
        *,
        timeout: int = 3600,
        envs: Mapping[str, str] | None = None,
        metadata: Mapping[str, str] | None = None,
        runtime: AsyncLocalRuntime | None = None,
    ) -> AsyncSandbox:
        return await super().create(
            template or DEFAULT_CODE_INTERPRETER_IMAGE,
            timeout=timeout,
            envs=envs,
            metadata=metadata,
            runtime=runtime,
        )

    async def run_code(
        self,
        code: str,
        *,
        language: str = "python",
        timeout: float | None = None,
    ) -> CommandResult:
        if language != "python":
            raise ValueError("the local Code Interpreter currently supports Python only")
        return await self.commands.run(["python", "-c", code], timeout=timeout)


__all__ = [
    "AsyncSandbox",
    "DEFAULT_CODE_INTERPRETER_IMAGE",
    "Sandbox",
]
