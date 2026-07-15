#!/usr/bin/env python3
"""Exercise unchanged official Python clients against a production service."""

from __future__ import annotations

import argparse
import asyncio
import os
from typing import Any

from e2b.sandbox.commands.command_handle import PtySize

NATIVE_SDK = os.environ.get("A3S_BOX_NATIVE_SDK") == "1"

if NATIVE_SDK:
    from a3s_box import (  # type: ignore[import-not-found]
        A3SConnectionConfig,
        AsyncSandbox,
        Sandbox,
        SandboxNotFoundException,
        SandboxQuery,
        SandboxState,
    )
    from a3s_box.code_interpreter import (  # type: ignore[import-not-found]
        AsyncSandbox as AsyncCodeInterpreter,
    )
    from a3s_box.code_interpreter import (  # type: ignore[import-not-found]
        Sandbox as CodeInterpreter,
    )
else:
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
    if NATIVE_SDK:
        return A3SConnectionConfig(  # type: ignore[name-defined]
            api_url=api_url,
            domain=domain,
            api_key=api_key,
        ).python_options()
    return {"api_key": api_key, "api_url": api_url, "domain": domain}


def assert_listed(items: list[Any], sandbox_id: str) -> None:
    if not any(item.sandbox_id == sandbox_id for item in items):
        raise AssertionError(f"sandbox {sandbox_id} was absent from the filtered list")


def exercise_sync_data_plane(sandbox: Sandbox, label: str) -> None:
    root = f"a3s-runtime-{label}"
    original = f"{root}/nested/original.txt"
    renamed = f"{root}/nested/renamed.txt"
    content = f"{label}-filesystem"

    sandbox.files.remove(root)
    if not sandbox.files.make_dir(f"{root}/nested"):
        raise AssertionError("fresh nested directory was reported as pre-existing")
    written = sandbox.files.write(original, content)
    if written.path != f"/home/user/{original}":
        raise AssertionError(f"unexpected written path: {written.path}")
    if sandbox.files.read(original) != content:
        raise AssertionError("filesystem read did not return the written content")
    info = sandbox.files.get_info(original)
    if info.name != "original.txt" or info.path != f"/home/user/{original}":
        raise AssertionError(f"unexpected filesystem stat result: {info!r}")
    entries = sandbox.files.list(root, depth=2)
    if not any(entry.path == f"/home/user/{original}" for entry in entries):
        raise AssertionError("filesystem list omitted the written file")
    moved = sandbox.files.rename(original, renamed)
    if moved.path != f"/home/user/{renamed}":
        raise AssertionError(f"unexpected renamed path: {moved.path}")
    if sandbox.files.exists(original) or not sandbox.files.exists(renamed):
        raise AssertionError("filesystem rename did not move the file")
    sandbox.files.remove(root)
    if sandbox.files.exists(root):
        raise AssertionError("filesystem remove left the directory behind")

    payload = f"{label}-stdin"
    command = sandbox.commands.run("cat", background=True, stdin=True, timeout=20)
    if not any(process.pid == command.pid for process in sandbox.commands.list()):
        raise AssertionError("background command was absent from process list")
    command.send_stdin(payload)
    command.close_stdin()
    result = command.wait()
    if result.exit_code != 0 or result.stdout != payload or result.stderr:
        raise AssertionError(f"unexpected background command result: {result!r}")

    output: list[bytes] = []
    terminal = sandbox.pty.create(PtySize(cols=80, rows=24), timeout=20)
    sandbox.pty.resize(terminal.pid, PtySize(cols=100, rows=30))
    sandbox.pty.send_stdin(
        terminal.pid,
        f"printf '{label}-pty:'; stty size; exit\n".encode(),
    )
    terminal_result = terminal.wait(on_pty=output.append)
    terminal_output = b"".join(output).decode("utf-8", errors="replace")
    if terminal_result.exit_code != 0 or f"{label}-pty:" not in terminal_output:
        raise AssertionError(f"unexpected PTY output: {terminal_output!r}")
    if "30 100" not in terminal_output:
        raise AssertionError(f"PTY resize was not observable: {terminal_output!r}")


async def exercise_async_data_plane(sandbox: AsyncSandbox, label: str) -> None:
    root = f"a3s-runtime-{label}"
    original = f"{root}/nested/original.txt"
    renamed = f"{root}/nested/renamed.txt"
    content = f"{label}-filesystem"

    await sandbox.files.remove(root)
    if not await sandbox.files.make_dir(f"{root}/nested"):
        raise AssertionError("fresh nested directory was reported as pre-existing")
    written = await sandbox.files.write(original, content)
    if written.path != f"/home/user/{original}":
        raise AssertionError(f"unexpected written path: {written.path}")
    if await sandbox.files.read(original) != content:
        raise AssertionError("filesystem read did not return the written content")
    info = await sandbox.files.get_info(original)
    if info.name != "original.txt" or info.path != f"/home/user/{original}":
        raise AssertionError(f"unexpected filesystem stat result: {info!r}")
    entries = await sandbox.files.list(root, depth=2)
    if not any(entry.path == f"/home/user/{original}" for entry in entries):
        raise AssertionError("filesystem list omitted the written file")
    moved = await sandbox.files.rename(original, renamed)
    if moved.path != f"/home/user/{renamed}":
        raise AssertionError(f"unexpected renamed path: {moved.path}")
    if await sandbox.files.exists(original) or not await sandbox.files.exists(renamed):
        raise AssertionError("filesystem rename did not move the file")
    await sandbox.files.remove(root)
    if await sandbox.files.exists(root):
        raise AssertionError("filesystem remove left the directory behind")

    payload = f"{label}-stdin"
    command = await sandbox.commands.run(
        "cat", background=True, stdin=True, timeout=20
    )
    if not any(
        process.pid == command.pid for process in await sandbox.commands.list()
    ):
        raise AssertionError("background command was absent from process list")
    await command.send_stdin(payload)
    await command.close_stdin()
    result = await command.wait()
    if result.exit_code != 0 or result.stdout != payload or result.stderr:
        raise AssertionError(f"unexpected background command result: {result!r}")

    output: list[bytes] = []
    terminal = await sandbox.pty.create(
        PtySize(cols=80, rows=24), on_data=output.append, timeout=20
    )
    await sandbox.pty.resize(terminal.pid, PtySize(cols=100, rows=30))
    await sandbox.pty.send_stdin(
        terminal.pid,
        f"printf '{label}-pty:'; stty size; exit\n".encode(),
    )
    terminal_result = await terminal.wait()
    terminal_output = b"".join(output).decode("utf-8", errors="replace")
    if terminal_result.exit_code != 0 or f"{label}-pty:" not in terminal_output:
        raise AssertionError(f"unexpected PTY output: {terminal_output!r}")
    if "30 100" not in terminal_output:
        raise AssertionError(f"PTY resize was not observable: {terminal_output!r}")


def exercise_sync_interpreter(interpreter: CodeInterpreter, label: str) -> None:
    execution = interpreter.run_code(f"print('{label}-code')\n6 * 7")
    if execution.text != "42" or not any(
        f"{label}-code" in line for line in execution.logs.stdout
    ):
        raise AssertionError(f"unexpected Code Interpreter result: {execution!r}")

    context = interpreter.create_code_context(language="python")
    if not any(item.id == context.id for item in interpreter.list_code_contexts()):
        raise AssertionError("created Code Interpreter context was not listed")
    contextual = interpreter.run_code("value = 41\nvalue + 1", context=context)
    if contextual.text != "42":
        raise AssertionError(f"unexpected contextual execution: {contextual!r}")
    interpreter.restart_code_context(context.id)
    restarted = interpreter.run_code("value", context=context)
    if restarted.error is None or restarted.error.name != "NameError":
        raise AssertionError("restarted context retained its previous variables")
    interpreter.remove_code_context(context.id)
    if any(item.id == context.id for item in interpreter.list_code_contexts()):
        raise AssertionError("removed Code Interpreter context remained listed")


async def exercise_async_interpreter(
    interpreter: AsyncCodeInterpreter, label: str
) -> None:
    execution = await interpreter.run_code(f"print('{label}-code')\n6 * 7")
    if execution.text != "42" or not any(
        f"{label}-code" in line for line in execution.logs.stdout
    ):
        raise AssertionError(f"unexpected Code Interpreter result: {execution!r}")

    context = await interpreter.create_code_context(language="python")
    if not any(
        item.id == context.id for item in await interpreter.list_code_contexts()
    ):
        raise AssertionError("created Code Interpreter context was not listed")
    contextual = await interpreter.run_code("value = 41\nvalue + 1", context=context)
    if contextual.text != "42":
        raise AssertionError(f"unexpected contextual execution: {contextual!r}")
    await interpreter.restart_code_context(context.id)
    restarted = await interpreter.run_code("value", context=context)
    if restarted.error is None or restarted.error.name != "NameError":
        raise AssertionError("restarted context retained its previous variables")
    await interpreter.remove_code_context(context.id)
    if any(item.id == context.id for item in await interpreter.list_code_contexts()):
        raise AssertionError("removed Code Interpreter context remained listed")


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
        exercise_sync_data_plane(sandbox, "python-sync")

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
        exercise_sync_interpreter(interpreter, "python-sync")
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
        await exercise_async_data_plane(sandbox, "python-async")

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
        await exercise_async_interpreter(interpreter, "python-async")
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
