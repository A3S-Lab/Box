#!/usr/bin/env python3
"""Exercise unchanged official Python clients against a production service."""

from __future__ import annotations

import argparse
import asyncio
import os
from typing import Any

from e2b import (
    AsyncSandbox,
    Sandbox,
    SandboxNotFoundException,
    SandboxQuery,
    SandboxState,
)
from e2b_code_interpreter import AsyncSandbox as AsyncCodeInterpreter
from e2b_code_interpreter import Sandbox as CodeInterpreter


def connection(api_url: str, domain: str) -> dict[str, Any]:
    api_key = os.environ.get("E2B_API_KEY")
    if not api_key:
        raise RuntimeError("E2B_API_KEY is required")
    return {"api_key": api_key, "api_url": api_url, "domain": domain}


def assert_listed(items: list[Any], sandbox_id: str) -> None:
    if not any(item.sandbox_id == sandbox_id for item in items):
        raise AssertionError(f"sandbox {sandbox_id} was absent from the filtered list")


def run_sync(api_url: str, domain: str, template: str) -> None:
    options = connection(api_url, domain)
    metadata = {"client": "python-sync", "suite": "production-official"}
    sandbox: Sandbox | None = None
    interpreter: CodeInterpreter | None = None
    try:
        sandbox = Sandbox.create(
            template,
            timeout=60,
            metadata=metadata,
            envs={"OFFICIAL_CLIENT": "python-sync"},
            secure=True,
            allow_internet_access=False,
            **options,
        )
        connected = Sandbox.connect(sandbox.sandbox_id, timeout=45, **options)
        if connected.sandbox_id != sandbox.sandbox_id:
            raise AssertionError("connect returned a different sandbox ID")
        if not sandbox.is_running():
            raise AssertionError("envd health reported the running sandbox as stopped")
        result = sandbox.commands.run(
            "printf 'python-sync:%s' \"$OFFICIAL_CLIENT\""
        )
        if result.stdout != "python-sync:python-sync" or result.stderr:
            raise AssertionError(f"unexpected sync command result: {result!r}")

        paginator = Sandbox.list(
            query=SandboxQuery(metadata=metadata, state=[SandboxState.RUNNING]),
            limit=20,
            **options,
        )
        assert_listed(paginator.next_items(), sandbox.sandbox_id)
        sandbox.set_timeout(30)
        if not sandbox.kill():
            raise AssertionError("kill did not terminate the production sandbox")
        if sandbox.is_running():
            raise AssertionError("envd health reported the killed sandbox as running")

        missing_id = "missing-production-python-sync"
        if Sandbox.kill(missing_id, **options):
            raise AssertionError("kill reported success for a missing sandbox")
        try:
            Sandbox.connect(missing_id, **options)
        except SandboxNotFoundException:
            pass
        else:
            raise AssertionError("missing sandbox connect did not raise not-found")

        interpreter = CodeInterpreter.create(
            timeout=60,
            metadata={"client": "python-code-interpreter"},
            **options,
        )
        if not interpreter.is_running():
            raise AssertionError("Code Interpreter envd health check failed")
        if not interpreter.kill():
            raise AssertionError("Code Interpreter lifecycle kill failed")
        if interpreter.is_running():
            raise AssertionError("Code Interpreter remained running after kill")
    finally:
        if interpreter is not None:
            Sandbox.kill(interpreter.sandbox_id, **options)
        if sandbox is not None:
            Sandbox.kill(sandbox.sandbox_id, **options)


async def run_async(api_url: str, domain: str, template: str) -> None:
    options = connection(api_url, domain)
    metadata = {"client": "python-async", "suite": "production-official"}
    sandbox: AsyncSandbox | None = None
    interpreter: AsyncCodeInterpreter | None = None
    try:
        sandbox = await AsyncSandbox.create(
            template,
            timeout=60,
            metadata=metadata,
            envs={"OFFICIAL_CLIENT": "python-async"},
            secure=True,
            allow_internet_access=False,
            **options,
        )
        connected = await AsyncSandbox.connect(
            sandbox.sandbox_id, timeout=45, **options
        )
        if connected.sandbox_id != sandbox.sandbox_id:
            raise AssertionError("connect returned a different sandbox ID")
        if not await sandbox.is_running():
            raise AssertionError("envd health reported the running sandbox as stopped")
        result = await sandbox.commands.run(
            "printf 'python-async:%s' \"$OFFICIAL_CLIENT\""
        )
        if result.stdout != "python-async:python-async" or result.stderr:
            raise AssertionError(f"unexpected async command result: {result!r}")

        paginator = AsyncSandbox.list(
            query=SandboxQuery(metadata=metadata, state=[SandboxState.RUNNING]),
            limit=20,
            **options,
        )
        assert_listed(await paginator.next_items(), sandbox.sandbox_id)
        await sandbox.set_timeout(30)
        if not await sandbox.kill():
            raise AssertionError("kill did not terminate the production sandbox")
        if await sandbox.is_running():
            raise AssertionError("envd health reported the killed sandbox as running")

        missing_id = "missing-production-python-async"
        if await AsyncSandbox.kill(missing_id, **options):
            raise AssertionError("kill reported success for a missing sandbox")
        try:
            await AsyncSandbox.connect(missing_id, **options)
        except SandboxNotFoundException:
            pass
        else:
            raise AssertionError("missing sandbox connect did not raise not-found")

        interpreter = await AsyncCodeInterpreter.create(
            timeout=60,
            metadata={"client": "python-async-code-interpreter"},
            **options,
        )
        if not await interpreter.is_running():
            raise AssertionError("async Code Interpreter envd health check failed")
        if not await interpreter.kill():
            raise AssertionError("Code Interpreter lifecycle kill failed")
        if await interpreter.is_running():
            raise AssertionError("async Code Interpreter remained running after kill")
    finally:
        if interpreter is not None:
            await AsyncSandbox.kill(interpreter.sandbox_id, **options)
        if sandbox is not None:
            await AsyncSandbox.kill(sandbox.sandbox_id, **options)


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=["sync", "async"])
    parser.add_argument("api_url")
    parser.add_argument("domain")
    parser.add_argument("template")
    args = parser.parse_args()
    if args.mode == "sync":
        run_sync(args.api_url, args.domain, args.template)
    else:
        asyncio.run(run_async(args.api_url, args.domain, args.template))


if __name__ == "__main__":
    main()
