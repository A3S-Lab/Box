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
    if NATIVE_SDK:
        return A3SConnectionConfig.from_environment().python_options()  # type: ignore[name-defined]
    api_key = os.environ.get("E2B_API_KEY")
    if not api_key:
        raise RuntimeError("E2B_API_KEY is required")
    return {"api_key": api_key, "api_url": api_url, "domain": domain}


def assert_listed(items: list[Any], sandbox_id: str) -> None:
    if not any(item.sandbox_id == sandbox_id for item in items):
        raise AssertionError(f"sandbox {sandbox_id} was absent from the filtered list")


def trace(label: str, stage: str) -> None:
    print(f"{label}:{stage}", flush=True)


def exercise_sync_data_plane(sandbox: Sandbox, label: str) -> None:
    root = f"a3s-runtime-{label}"
    original = f"{root}/nested/original.txt"
    renamed = f"{root}/nested/renamed.txt"
    content = f"{label}-filesystem"

    trace(label, "filesystem.remove-initial")
    sandbox.files.remove(root)
    trace(label, "filesystem.make-dir")
    if not sandbox.files.make_dir(f"{root}/nested"):
        raise AssertionError("fresh nested directory was reported as pre-existing")
    trace(label, "filesystem.write")
    written = sandbox.files.write(original, content)
    if written.path != f"/home/user/{original}":
        raise AssertionError(f"unexpected written path: {written.path}")
    trace(label, "filesystem.read")
    if sandbox.files.read(original) != content:
        raise AssertionError("filesystem read did not return the written content")
    trace(label, "filesystem.get-info")
    info = sandbox.files.get_info(original)
    if info.name != "original.txt" or info.path != f"/home/user/{original}":
        raise AssertionError(f"unexpected filesystem stat result: {info!r}")
    trace(label, "filesystem.list")
    entries = sandbox.files.list(root, depth=2)
    if not any(entry.path == f"/home/user/{original}" for entry in entries):
        raise AssertionError("filesystem list omitted the written file")
    trace(label, "filesystem.rename")
    moved = sandbox.files.rename(original, renamed)
    if moved.path != f"/home/user/{renamed}":
        raise AssertionError(f"unexpected renamed path: {moved.path}")
    trace(label, "filesystem.exists-renamed")
    if sandbox.files.exists(original) or not sandbox.files.exists(renamed):
        raise AssertionError("filesystem rename did not move the file")
    trace(label, "filesystem.remove-final")
    sandbox.files.remove(root)
    trace(label, "filesystem.exists-final")
    if sandbox.files.exists(root):
        raise AssertionError("filesystem remove left the directory behind")

    payload = f"{label}-stdin"
    trace(label, "process.start-background")
    command = sandbox.commands.run("cat", background=True, stdin=True, timeout=20)
    trace(label, "process.list")
    if not any(process.pid == command.pid for process in sandbox.commands.list()):
        raise AssertionError("background command was absent from process list")
    trace(label, "process.send-stdin")
    command.send_stdin(payload)
    trace(label, "process.close-stdin")
    command.close_stdin()
    trace(label, "process.wait")
    result = command.wait()
    if result.exit_code != 0 or result.stdout != payload or result.stderr:
        raise AssertionError(f"unexpected background command result: {result!r}")

    output: list[bytes] = []
    trace(label, "pty.create")
    terminal = sandbox.pty.create(PtySize(cols=80, rows=24), timeout=20)
    trace(label, "pty.resize")
    sandbox.pty.resize(terminal.pid, PtySize(cols=100, rows=30))
    trace(label, "pty.send-stdin")
    sandbox.pty.send_stdin(
        terminal.pid,
        f"printf '{label}-pty:'; stty size; exit\n".encode(),
    )
    trace(label, "pty.wait")
    terminal_result = terminal.wait(on_pty=output.append)
    terminal_output = b"".join(output).decode("utf-8", errors="replace")
    if terminal_result.exit_code != 0 or f"{label}-pty:" not in terminal_output:
        raise AssertionError(f"unexpected PTY output: {terminal_output!r}")
    if "30 100" not in terminal_output:
        raise AssertionError(f"PTY resize was not observable: {terminal_output!r}")
    trace(label, "data-plane.complete")


async def exercise_async_data_plane(sandbox: AsyncSandbox, label: str) -> None:
    root = f"a3s-runtime-{label}"
    original = f"{root}/nested/original.txt"
    renamed = f"{root}/nested/renamed.txt"
    content = f"{label}-filesystem"

    trace(label, "filesystem.remove-initial")
    await sandbox.files.remove(root)
    trace(label, "filesystem.make-dir")
    if not await sandbox.files.make_dir(f"{root}/nested"):
        raise AssertionError("fresh nested directory was reported as pre-existing")
    trace(label, "filesystem.write")
    written = await sandbox.files.write(original, content)
    if written.path != f"/home/user/{original}":
        raise AssertionError(f"unexpected written path: {written.path}")
    trace(label, "filesystem.read")
    if await sandbox.files.read(original) != content:
        raise AssertionError("filesystem read did not return the written content")
    trace(label, "filesystem.get-info")
    info = await sandbox.files.get_info(original)
    if info.name != "original.txt" or info.path != f"/home/user/{original}":
        raise AssertionError(f"unexpected filesystem stat result: {info!r}")
    trace(label, "filesystem.list")
    entries = await sandbox.files.list(root, depth=2)
    if not any(entry.path == f"/home/user/{original}" for entry in entries):
        raise AssertionError("filesystem list omitted the written file")
    trace(label, "filesystem.rename")
    moved = await sandbox.files.rename(original, renamed)
    if moved.path != f"/home/user/{renamed}":
        raise AssertionError(f"unexpected renamed path: {moved.path}")
    trace(label, "filesystem.exists-renamed")
    if await sandbox.files.exists(original) or not await sandbox.files.exists(renamed):
        raise AssertionError("filesystem rename did not move the file")
    trace(label, "filesystem.remove-final")
    await sandbox.files.remove(root)
    trace(label, "filesystem.exists-final")
    if await sandbox.files.exists(root):
        raise AssertionError("filesystem remove left the directory behind")

    payload = f"{label}-stdin"
    trace(label, "process.start-background")
    command = await sandbox.commands.run(
        "cat", background=True, stdin=True, timeout=20
    )
    trace(label, "process.list")
    if not any(
        process.pid == command.pid for process in await sandbox.commands.list()
    ):
        raise AssertionError("background command was absent from process list")
    trace(label, "process.send-stdin")
    await command.send_stdin(payload)
    trace(label, "process.close-stdin")
    await command.close_stdin()
    trace(label, "process.wait")
    result = await command.wait()
    if result.exit_code != 0 or result.stdout != payload or result.stderr:
        raise AssertionError(f"unexpected background command result: {result!r}")

    output: list[bytes] = []
    trace(label, "pty.create")
    terminal = await sandbox.pty.create(
        PtySize(cols=80, rows=24), on_data=output.append, timeout=20
    )
    trace(label, "pty.resize")
    await sandbox.pty.resize(terminal.pid, PtySize(cols=100, rows=30))
    trace(label, "pty.send-stdin")
    await sandbox.pty.send_stdin(
        terminal.pid,
        f"printf '{label}-pty:'; stty size; exit\n".encode(),
    )
    trace(label, "pty.wait")
    terminal_result = await terminal.wait()
    terminal_output = b"".join(output).decode("utf-8", errors="replace")
    if terminal_result.exit_code != 0 or f"{label}-pty:" not in terminal_output:
        raise AssertionError(f"unexpected PTY output: {terminal_output!r}")
    if "30 100" not in terminal_output:
        raise AssertionError(f"PTY resize was not observable: {terminal_output!r}")
    trace(label, "data-plane.complete")


def exercise_sync_interpreter(interpreter: CodeInterpreter, label: str) -> None:
    trace(label, "interpreter.run")
    execution = interpreter.run_code(f"print('{label}-code')\n6 * 7")
    if execution.text != "42" or not any(
        f"{label}-code" in line for line in execution.logs.stdout
    ):
        raise AssertionError(f"unexpected Code Interpreter result: {execution!r}")

    trace(label, "interpreter.context-create")
    context = interpreter.create_code_context(language="python")
    trace(label, "interpreter.context-list")
    if not any(item.id == context.id for item in interpreter.list_code_contexts()):
        raise AssertionError("created Code Interpreter context was not listed")
    trace(label, "interpreter.context-run")
    contextual = interpreter.run_code("value = 41\nvalue + 1", context=context)
    if contextual.text != "42":
        raise AssertionError(f"unexpected contextual execution: {contextual!r}")
    trace(label, "interpreter.context-restart")
    interpreter.restart_code_context(context.id)
    trace(label, "interpreter.context-run-restarted")
    restarted = interpreter.run_code("value", context=context)
    if restarted.error is None or restarted.error.name != "NameError":
        raise AssertionError("restarted context retained its previous variables")
    trace(label, "interpreter.context-remove")
    interpreter.remove_code_context(context.id)
    trace(label, "interpreter.context-list-removed")
    if any(item.id == context.id for item in interpreter.list_code_contexts()):
        raise AssertionError("removed Code Interpreter context remained listed")
    trace(label, "interpreter.complete")


async def exercise_async_interpreter(
    interpreter: AsyncCodeInterpreter, label: str
) -> None:
    trace(label, "interpreter.run")
    execution = await interpreter.run_code(f"print('{label}-code')\n6 * 7")
    if execution.text != "42" or not any(
        f"{label}-code" in line for line in execution.logs.stdout
    ):
        raise AssertionError(f"unexpected Code Interpreter result: {execution!r}")

    trace(label, "interpreter.context-create")
    context = await interpreter.create_code_context(language="python")
    trace(label, "interpreter.context-list")
    if not any(
        item.id == context.id for item in await interpreter.list_code_contexts()
    ):
        raise AssertionError("created Code Interpreter context was not listed")
    trace(label, "interpreter.context-run")
    contextual = await interpreter.run_code("value = 41\nvalue + 1", context=context)
    if contextual.text != "42":
        raise AssertionError(f"unexpected contextual execution: {contextual!r}")
    trace(label, "interpreter.context-restart")
    await interpreter.restart_code_context(context.id)
    trace(label, "interpreter.context-run-restarted")
    restarted = await interpreter.run_code("value", context=context)
    if restarted.error is None or restarted.error.name != "NameError":
        raise AssertionError("restarted context retained its previous variables")
    trace(label, "interpreter.context-remove")
    await interpreter.remove_code_context(context.id)
    trace(label, "interpreter.context-list-removed")
    if any(item.id == context.id for item in await interpreter.list_code_contexts()):
        raise AssertionError("removed Code Interpreter context remained listed")
    trace(label, "interpreter.complete")


def run_sync(api_url: str, domain: str, template: str) -> None:
    label = "python-sync"
    options = connection(api_url, domain)
    metadata = {"client": "python-sync", "suite": "production-official"}
    sandbox: Sandbox | None = None
    interpreter: CodeInterpreter | None = None
    try:
        trace(label, "sandbox.create")
        sandbox = Sandbox.create(
            template,
            timeout=60,
            metadata=metadata,
            envs={"OFFICIAL_CLIENT": "python-sync"},
            secure=True,
            allow_internet_access=False,
            **options,
        )
        trace(label, "sandbox.connect")
        connected = Sandbox.connect(sandbox.sandbox_id, timeout=45, **options)
        if connected.sandbox_id != sandbox.sandbox_id:
            raise AssertionError("connect returned a different sandbox ID")
        trace(label, "sandbox.health")
        if not sandbox.is_running():
            raise AssertionError("envd health reported the running sandbox as stopped")
        trace(label, "process.foreground")
        result = sandbox.commands.run(
            "printf 'python-sync:%s' \"$OFFICIAL_CLIENT\""
        )
        if result.stdout != "python-sync:python-sync" or result.stderr:
            raise AssertionError(f"unexpected sync command result: {result!r}")
        trace(label, "process.foreground.complete")
        exercise_sync_data_plane(sandbox, label)

        trace(label, "sandbox.list")
        paginator = Sandbox.list(
            query=SandboxQuery(metadata=metadata, state=[SandboxState.RUNNING]),
            limit=20,
            **options,
        )
        assert_listed(paginator.next_items(), sandbox.sandbox_id)
        trace(label, "sandbox.set-timeout")
        sandbox.set_timeout(30)
        trace(label, "sandbox.kill")
        if not sandbox.kill():
            raise AssertionError("kill did not terminate the production sandbox")
        trace(label, "sandbox.health-killed")
        if sandbox.is_running():
            raise AssertionError("envd health reported the killed sandbox as running")

        missing_id = "missing-production-python-sync"
        trace(label, "sandbox.kill-missing")
        if Sandbox.kill(missing_id, **options):
            raise AssertionError("kill reported success for a missing sandbox")
        trace(label, "sandbox.connect-missing")
        try:
            Sandbox.connect(missing_id, **options)
        except SandboxNotFoundException:
            pass
        else:
            raise AssertionError("missing sandbox connect did not raise not-found")

        trace(label, "interpreter.create")
        interpreter = CodeInterpreter.create(
            timeout=60,
            metadata={"client": "python-code-interpreter"},
            **options,
        )
        trace(label, "interpreter.health")
        if not interpreter.is_running():
            raise AssertionError("Code Interpreter envd health check failed")
        exercise_sync_interpreter(interpreter, label)
        trace(label, "interpreter.kill")
        if not interpreter.kill():
            raise AssertionError("Code Interpreter lifecycle kill failed")
        trace(label, "interpreter.health-killed")
        if interpreter.is_running():
            raise AssertionError("Code Interpreter remained running after kill")
        trace(label, "complete")
    finally:
        if interpreter is not None:
            Sandbox.kill(interpreter.sandbox_id, **options)
        if sandbox is not None:
            Sandbox.kill(sandbox.sandbox_id, **options)


async def run_async(api_url: str, domain: str, template: str) -> None:
    label = "python-async"
    options = connection(api_url, domain)
    metadata = {"client": "python-async", "suite": "production-official"}
    sandbox: AsyncSandbox | None = None
    interpreter: AsyncCodeInterpreter | None = None
    try:
        trace(label, "sandbox.create")
        sandbox = await AsyncSandbox.create(
            template,
            timeout=60,
            metadata=metadata,
            envs={"OFFICIAL_CLIENT": "python-async"},
            secure=True,
            allow_internet_access=False,
            **options,
        )
        trace(label, "sandbox.connect")
        connected = await AsyncSandbox.connect(
            sandbox.sandbox_id, timeout=45, **options
        )
        if connected.sandbox_id != sandbox.sandbox_id:
            raise AssertionError("connect returned a different sandbox ID")
        trace(label, "sandbox.health")
        if not await sandbox.is_running():
            raise AssertionError("envd health reported the running sandbox as stopped")
        trace(label, "process.foreground")
        result = await sandbox.commands.run(
            "printf 'python-async:%s' \"$OFFICIAL_CLIENT\""
        )
        if result.stdout != "python-async:python-async" or result.stderr:
            raise AssertionError(f"unexpected async command result: {result!r}")
        trace(label, "process.foreground.complete")
        await exercise_async_data_plane(sandbox, label)

        trace(label, "sandbox.list")
        paginator = AsyncSandbox.list(
            query=SandboxQuery(metadata=metadata, state=[SandboxState.RUNNING]),
            limit=20,
            **options,
        )
        assert_listed(await paginator.next_items(), sandbox.sandbox_id)
        trace(label, "sandbox.set-timeout")
        await sandbox.set_timeout(30)
        trace(label, "sandbox.kill")
        if not await sandbox.kill():
            raise AssertionError("kill did not terminate the production sandbox")
        trace(label, "sandbox.health-killed")
        if await sandbox.is_running():
            raise AssertionError("envd health reported the killed sandbox as running")

        missing_id = "missing-production-python-async"
        trace(label, "sandbox.kill-missing")
        if await AsyncSandbox.kill(missing_id, **options):
            raise AssertionError("kill reported success for a missing sandbox")
        trace(label, "sandbox.connect-missing")
        try:
            await AsyncSandbox.connect(missing_id, **options)
        except SandboxNotFoundException:
            pass
        else:
            raise AssertionError("missing sandbox connect did not raise not-found")

        trace(label, "interpreter.create")
        interpreter = await AsyncCodeInterpreter.create(
            timeout=60,
            metadata={"client": "python-async-code-interpreter"},
            **options,
        )
        trace(label, "interpreter.health")
        if not await interpreter.is_running():
            raise AssertionError("async Code Interpreter envd health check failed")
        await exercise_async_interpreter(interpreter, label)
        trace(label, "interpreter.kill")
        if not await interpreter.kill():
            raise AssertionError("Code Interpreter lifecycle kill failed")
        trace(label, "interpreter.health-killed")
        if await interpreter.is_running():
            raise AssertionError("async Code Interpreter remained running after kill")
        trace(label, "complete")
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
