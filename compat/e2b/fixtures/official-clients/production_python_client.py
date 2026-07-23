#!/usr/bin/env python3
"""Exercise unchanged official Python clients against a production service."""

from __future__ import annotations

import argparse
import asyncio
import datetime
import os
from typing import Any

from e2b import (
    AsyncSandbox,
    AsyncVolume,
    Sandbox,
    SandboxException,
    SandboxNotFoundException,
    SandboxQuery,
    SandboxState,
    Volume,
    VolumeException,
)
from e2b.sandbox.commands.command_handle import PtySize
from e2b_code_interpreter import AsyncSandbox as AsyncCodeInterpreter
from e2b_code_interpreter import Sandbox as CodeInterpreter


def connection(api_url: str, domain: str) -> dict[str, Any]:
    api_key = os.environ.get("E2B_API_KEY")
    if not api_key:
        raise RuntimeError("E2B_API_KEY is required")
    return {"api_key": api_key, "api_url": api_url, "domain": domain}


def volume_connection(api_url: str) -> dict[str, Any]:
    return {"api_url": api_url}


def assert_listed(items: list[Any], sandbox_id: str) -> Any:
    for item in items:
        if item.sandbox_id == sandbox_id:
            return item
    raise AssertionError(f"sandbox {sandbox_id} was absent from the filtered list")


def assert_volume_mount(item: Any, name: str, path: str) -> None:
    mounts = item.volume_mounts or []
    if not any(
        mount.get("name") == name and mount.get("path") == path for mount in mounts
    ):
        raise AssertionError(
            f"sandbox list omitted volume mount {name}:{path}: {mounts!r}"
        )


def trace(label: str, stage: str) -> None:
    print(f"{label}:{stage}", flush=True)


def assert_metrics(metrics: list[Any], label: str) -> None:
    if not metrics:
        raise AssertionError(f"{label} metrics were empty")
    metric = metrics[0]
    for field in (
        "timestamp",
        "cpu_count",
        "cpu_used_pct",
        "mem_used",
        "mem_total",
        "disk_used",
        "disk_total",
    ):
        if getattr(metric, field, None) is None:
            raise AssertionError(f"{label} metric omitted {field}: {metric!r}")


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
    volume_options = volume_connection(api_url)
    volume_name = f"official-{label}-volume"
    metadata = {"client": "python-sync", "suite": "production-official"}
    sandbox: Sandbox | None = None
    restored: Sandbox | None = None
    interpreter: CodeInterpreter | None = None
    volume: Volume | None = None
    snapshot_id: str | None = None
    try:
        trace(label, "volume.create")
        volume = Volume.create(volume_name, **options)
        if volume.name != volume_name or not volume.volume_id or not volume.token:
            raise AssertionError(f"unexpected created Volume: {volume!r}")
        trace(label, "volume.connect")
        connected_volume = Volume.connect(volume.volume_id, **options)
        if connected_volume.name != volume_name:
            raise AssertionError(f"unexpected connected Volume: {connected_volume!r}")
        trace(label, "volume.list")
        if not any(item.volume_id == volume.volume_id for item in Volume.list(**options)):
            raise AssertionError("created Volume was absent from the owner-scoped list")
        trace(label, "volume.make-dir")
        volume.make_dir(
            "/shared",
            uid=1000,
            gid=1000,
            mode=0o777,
            force=True,
            **volume_options,
        )
        api_content = f"{label}-api-to-sandbox"
        trace(label, "volume.api-write")
        volume.write_file(
            "/shared/from-api.txt",
            api_content,
            uid=1000,
            gid=1000,
            mode=0o644,
            **volume_options,
        )

        trace(label, "sandbox.create")
        sandbox = Sandbox.create(
            template,
            timeout=60,
            metadata=metadata,
            envs={"OFFICIAL_CLIENT": "python-sync"},
            secure=True,
            allow_internet_access=False,
            volume_mounts={"/mnt/data": volume},
            **options,
        )
        trace(label, "sandbox.connect")
        connected = Sandbox.connect(sandbox.sandbox_id, timeout=45, **options)
        if connected.sandbox_id != sandbox.sandbox_id:
            raise AssertionError("connect returned a different sandbox ID")
        trace(label, "sandbox.health")
        if not sandbox.is_running():
            raise AssertionError("envd health reported the running sandbox as stopped")
        trace(label, "sandbox.metrics")
        assert_metrics(sandbox.get_metrics(), label)
        trace(label, "sandbox.metrics-past-range")
        if sandbox.get_metrics(
            start=datetime.datetime(1970, 1, 1, tzinfo=datetime.timezone.utc),
            end=datetime.datetime(1970, 1, 2, tzinfo=datetime.timezone.utc),
        ):
            raise AssertionError("past metrics range returned current samples")
        trace(label, "process.foreground")
        result = sandbox.commands.run(
            "printf 'python-sync:%s' \"$OFFICIAL_CLIENT\""
        )
        if result.stdout != "python-sync:python-sync" or result.stderr:
            raise AssertionError(f"unexpected sync command result: {result!r}")
        trace(label, "process.foreground.complete")
        trace(label, "volume.sandbox-read")
        mounted = sandbox.commands.run("cat /mnt/data/shared/from-api.txt")
        if mounted.stdout != api_content or mounted.stderr:
            raise AssertionError(f"Sandbox did not read API Volume content: {mounted!r}")
        trace(label, "volume.sandbox-stat")
        ownership = sandbox.commands.run(
            "stat -c '%u:%g' /mnt/data/shared/from-api.txt"
        )
        if ownership.stdout.strip() != "1000:1000":
            raise AssertionError(f"API Volume ownership was not mapped: {ownership!r}")
        identity = sandbox.commands.run("printf '%s:%s' \"$(id -u)\" \"$(id -g)\"")
        sandbox_uid, sandbox_gid = (int(value) for value in identity.stdout.split(":"))
        sandbox_content = f"{label}-sandbox-to-api"
        trace(label, "volume.sandbox-write")
        sandbox.commands.run(
            f"printf '%s' '{sandbox_content}' > /mnt/data/shared/from-sandbox.txt"
        )
        trace(label, "volume.api-read")
        if (
            volume.read_file("/shared/from-sandbox.txt", **volume_options)
            != sandbox_content
        ):
            raise AssertionError("Volume API did not read Sandbox-written content")
        sandbox_entry = volume.get_info(
            "/shared/from-sandbox.txt", **volume_options
        )
        if sandbox_entry.uid != sandbox_uid or sandbox_entry.gid != sandbox_gid:
            raise AssertionError(
                "Sandbox Volume ownership did not round trip through the API: "
                f"{sandbox_entry!r} versus {sandbox_uid}:{sandbox_gid}"
            )
        trace(label, "volume.destroy-in-use")
        try:
            Volume.destroy(volume.volume_id, **options)
        except VolumeException as error:
            if "in use" not in str(error).lower():
                raise AssertionError(f"unexpected in-use Volume error: {error}") from error
        else:
            raise AssertionError("mounted Volume was destroyed while Sandbox was running")
        exercise_sync_data_plane(sandbox, label)

        trace(label, "sandbox.pause-process-start")
        survivor = sandbox.commands.run("cat", background=True, stdin=True, timeout=20)
        trace(label, "sandbox.pause")
        if not sandbox.pause(keep_memory=True):
            raise AssertionError("running sandbox was reported as already paused")
        trace(label, "sandbox.pause-idempotent")
        if sandbox.pause(keep_memory=True):
            raise AssertionError("second pause did not report the paused state")
        trace(label, "sandbox.list-paused")
        paused = Sandbox.list(
            query=SandboxQuery(metadata=metadata, state=[SandboxState.PAUSED]),
            limit=20,
            **options,
        )
        assert_listed(paused.next_items(), sandbox.sandbox_id)
        trace(label, "sandbox.resume-connect")
        resumed = sandbox.connect(timeout=45)
        if resumed.sandbox_id != sandbox.sandbox_id:
            raise AssertionError("resume returned a different sandbox ID")
        trace(label, "sandbox.pause-process-survived")
        survivor.send_stdin(f"{label}-pause")
        survivor.close_stdin()
        survivor_result = survivor.wait()
        if survivor_result.exit_code != 0 or survivor_result.stdout != f"{label}-pause":
            raise AssertionError(
                f"memory-preserving pause lost the running process: {survivor_result!r}"
            )

        trace(label, "sandbox.list")
        paginator = Sandbox.list(
            query=SandboxQuery(metadata=metadata, state=[SandboxState.RUNNING]),
            limit=20,
            **options,
        )
        listed = assert_listed(paginator.next_items(), sandbox.sandbox_id)
        assert_volume_mount(listed, volume_name, "/mnt/data")

        snapshot_content = f"official-{label}-snapshot"
        trace(label, "snapshot.write-state")
        sandbox.files.write("a3s-snapshot-state.txt", snapshot_content)
        snapshot_metadata = sandbox.commands.run(
            "stat -c '%u:%g:%a' /home/user/a3s-snapshot-state.txt"
        ).stdout.strip()
        trace(label, "snapshot.create")
        snapshot = sandbox.create_snapshot(
            name=f"official-{label}-state"
        )
        snapshot_id = snapshot.snapshot_id
        if not snapshot_id or snapshot.names != [snapshot_id]:
            raise AssertionError(f"unexpected created Snapshot: {snapshot!r}")
        trace(label, "snapshot.list")
        snapshots = sandbox.list_snapshots(limit=20).next_items()
        if not any(item.snapshot_id == snapshot_id for item in snapshots):
            raise AssertionError("created Snapshot was absent from the source-scoped list")
        trace(label, "snapshot.source-running")
        if not sandbox.is_running():
            raise AssertionError("Snapshot did not restore the running source state")

        cold_pause_content = f"{label}-cold-pause"
        trace(label, "sandbox.cold-pause-write-state")
        sandbox.files.write("a3s-cold-pause-state.txt", cold_pause_content)
        trace(label, "sandbox.cold-pause-process-start")
        cold_process = sandbox.commands.run(
            "sleep 300", background=True, timeout=310
        )
        trace(label, "sandbox.cold-pause")
        if not sandbox.pause(keep_memory=False):
            raise AssertionError("filesystem-only pause did not pause a running sandbox")
        trace(label, "sandbox.cold-pause-connect")
        cold_resumed = sandbox.connect(timeout=60)
        if cold_resumed.sandbox_id != sandbox.sandbox_id:
            raise AssertionError("filesystem-only resume returned a different sandbox ID")
        trace(label, "sandbox.cold-pause-read-state")
        if sandbox.files.read("a3s-cold-pause-state.txt") != cold_pause_content:
            raise AssertionError("filesystem-only pause lost rootfs state")
        trace(label, "sandbox.cold-pause-process-gone")
        if any(process.pid == cold_process.pid for process in sandbox.commands.list()):
            raise AssertionError("filesystem-only pause preserved an old runtime process")
        trace(label, "sandbox.cold-pause-environment")
        cold_environment = sandbox.commands.run("printf '%s' \"$OFFICIAL_CLIENT\"")
        if cold_environment.stdout != label or cold_environment.stderr:
            raise AssertionError(
                f"filesystem-only resume lost the Sandbox environment: {cold_environment!r}"
            )
        trace(label, "sandbox.cold-pause-volume")
        cold_mounted = sandbox.commands.run("cat /mnt/data/shared/from-api.txt")
        if cold_mounted.stdout != api_content or cold_mounted.stderr:
            raise AssertionError("filesystem-only resume did not remount the Volume")

        trace(label, "sandbox.set-timeout")
        sandbox.set_timeout(30)
        trace(label, "sandbox.kill")
        if not sandbox.kill():
            raise AssertionError("kill did not terminate the production sandbox")
        trace(label, "sandbox.health-killed")
        if sandbox.is_running():
            raise AssertionError("envd health reported the killed sandbox as running")

        trace(label, "snapshot.restore-after-source-kill")
        restored = Sandbox.create(snapshot_id, timeout=60, **options)
        trace(label, "snapshot.read-restored-state")
        if restored.files.read("a3s-snapshot-state.txt") != snapshot_content:
            raise AssertionError("restored Sandbox lost the captured filesystem state")
        restored_metadata = restored.commands.run(
            "stat -c '%u:%g:%a' /home/user/a3s-snapshot-state.txt"
        ).stdout.strip()
        if restored_metadata != snapshot_metadata:
            raise AssertionError(
                "restored Snapshot changed file ownership or mode: "
                f"{snapshot_metadata!r} -> {restored_metadata!r}"
            )
        restored.commands.run(
            "printf '%s' '-writable' >> /home/user/a3s-snapshot-state.txt"
        )
        trace(label, "snapshot.delete-in-use")
        try:
            Sandbox.delete_snapshot(snapshot_id, **options)
        except SandboxException as error:
            if "409" not in str(error):
                raise AssertionError(
                    f"unexpected in-use Snapshot error: {error}"
                ) from error
        else:
            raise AssertionError("Snapshot was deleted while a restored Sandbox used it")
        trace(label, "snapshot.restored-kill")
        if not restored.kill():
            raise AssertionError("restored Sandbox did not terminate")
        restored = None
        trace(label, "snapshot.delete")
        if not Sandbox.delete_snapshot(snapshot_id, **options):
            raise AssertionError("detached Snapshot was not deleted")
        trace(label, "snapshot.delete-missing")
        if Sandbox.delete_snapshot(snapshot_id, **options):
            raise AssertionError("missing Snapshot deletion reported success")
        snapshot_id = None

        trace(label, "volume.destroy")
        if not Volume.destroy(volume.volume_id, **options):
            raise AssertionError("detached Volume was not destroyed")
        volume = None

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
        try:
            if interpreter is not None:
                Sandbox.kill(interpreter.sandbox_id, **options)
            if restored is not None:
                Sandbox.kill(restored.sandbox_id, **options)
            if sandbox is not None:
                Sandbox.kill(sandbox.sandbox_id, **options)
            if snapshot_id is not None:
                Sandbox.delete_snapshot(snapshot_id, **options)
        finally:
            if volume is not None:
                Volume.destroy(volume.volume_id, **options)


async def run_async(api_url: str, domain: str, template: str) -> None:
    label = "python-async"
    options = connection(api_url, domain)
    volume_options = volume_connection(api_url)
    volume_name = f"official-{label}-volume"
    metadata = {"client": "python-async", "suite": "production-official"}
    sandbox: AsyncSandbox | None = None
    restored: AsyncSandbox | None = None
    interpreter: AsyncCodeInterpreter | None = None
    volume: AsyncVolume | None = None
    snapshot_id: str | None = None
    try:
        trace(label, "volume.create")
        volume = await AsyncVolume.create(volume_name, **options)
        if volume.name != volume_name or not volume.volume_id or not volume.token:
            raise AssertionError(f"unexpected created Volume: {volume!r}")
        trace(label, "volume.connect")
        connected_volume = await AsyncVolume.connect(volume.volume_id, **options)
        if connected_volume.name != volume_name:
            raise AssertionError(f"unexpected connected Volume: {connected_volume!r}")
        trace(label, "volume.list")
        if not any(
            item.volume_id == volume.volume_id
            for item in await AsyncVolume.list(**options)
        ):
            raise AssertionError("created Volume was absent from the owner-scoped list")
        trace(label, "volume.make-dir")
        await volume.make_dir(
            "/shared",
            uid=1000,
            gid=1000,
            mode=0o777,
            force=True,
            **volume_options,
        )
        api_content = f"{label}-api-to-sandbox"
        trace(label, "volume.api-write")
        await volume.write_file(
            "/shared/from-api.txt",
            api_content,
            uid=1000,
            gid=1000,
            mode=0o644,
            **volume_options,
        )

        trace(label, "sandbox.create")
        sandbox = await AsyncSandbox.create(
            template,
            timeout=60,
            metadata=metadata,
            envs={"OFFICIAL_CLIENT": "python-async"},
            secure=True,
            allow_internet_access=False,
            volume_mounts={"/mnt/data": volume},
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
        trace(label, "sandbox.metrics")
        assert_metrics(await sandbox.get_metrics(), label)
        trace(label, "sandbox.metrics-past-range")
        if await sandbox.get_metrics(
            start=datetime.datetime(1970, 1, 1, tzinfo=datetime.timezone.utc),
            end=datetime.datetime(1970, 1, 2, tzinfo=datetime.timezone.utc),
        ):
            raise AssertionError("past metrics range returned current samples")
        trace(label, "process.foreground")
        result = await sandbox.commands.run(
            "printf 'python-async:%s' \"$OFFICIAL_CLIENT\""
        )
        if result.stdout != "python-async:python-async" or result.stderr:
            raise AssertionError(f"unexpected async command result: {result!r}")
        trace(label, "process.foreground.complete")
        trace(label, "volume.sandbox-read")
        mounted = await sandbox.commands.run("cat /mnt/data/shared/from-api.txt")
        if mounted.stdout != api_content or mounted.stderr:
            raise AssertionError(f"Sandbox did not read API Volume content: {mounted!r}")
        trace(label, "volume.sandbox-stat")
        ownership = await sandbox.commands.run(
            "stat -c '%u:%g' /mnt/data/shared/from-api.txt"
        )
        if ownership.stdout.strip() != "1000:1000":
            raise AssertionError(f"API Volume ownership was not mapped: {ownership!r}")
        identity = await sandbox.commands.run(
            "printf '%s:%s' \"$(id -u)\" \"$(id -g)\""
        )
        sandbox_uid, sandbox_gid = (int(value) for value in identity.stdout.split(":"))
        sandbox_content = f"{label}-sandbox-to-api"
        trace(label, "volume.sandbox-write")
        await sandbox.commands.run(
            f"printf '%s' '{sandbox_content}' > /mnt/data/shared/from-sandbox.txt"
        )
        trace(label, "volume.api-read")
        if (
            await volume.read_file("/shared/from-sandbox.txt", **volume_options)
            != sandbox_content
        ):
            raise AssertionError("Volume API did not read Sandbox-written content")
        sandbox_entry = await volume.get_info(
            "/shared/from-sandbox.txt", **volume_options
        )
        if sandbox_entry.uid != sandbox_uid or sandbox_entry.gid != sandbox_gid:
            raise AssertionError(
                "Sandbox Volume ownership did not round trip through the API: "
                f"{sandbox_entry!r} versus {sandbox_uid}:{sandbox_gid}"
            )
        trace(label, "volume.destroy-in-use")
        try:
            await AsyncVolume.destroy(volume.volume_id, **options)
        except VolumeException as error:
            if "in use" not in str(error).lower():
                raise AssertionError(f"unexpected in-use Volume error: {error}") from error
        else:
            raise AssertionError("mounted Volume was destroyed while Sandbox was running")
        await exercise_async_data_plane(sandbox, label)

        trace(label, "sandbox.pause-process-start")
        survivor = await sandbox.commands.run(
            "cat", background=True, stdin=True, timeout=20
        )
        trace(label, "sandbox.pause")
        if not await sandbox.pause(keep_memory=True):
            raise AssertionError("running sandbox was reported as already paused")
        trace(label, "sandbox.pause-idempotent")
        if await sandbox.pause(keep_memory=True):
            raise AssertionError("second pause did not report the paused state")
        trace(label, "sandbox.list-paused")
        paused = AsyncSandbox.list(
            query=SandboxQuery(metadata=metadata, state=[SandboxState.PAUSED]),
            limit=20,
            **options,
        )
        assert_listed(await paused.next_items(), sandbox.sandbox_id)
        trace(label, "sandbox.resume-connect")
        resumed = await sandbox.connect(timeout=45)
        if resumed.sandbox_id != sandbox.sandbox_id:
            raise AssertionError("resume returned a different sandbox ID")
        trace(label, "sandbox.pause-process-survived")
        await survivor.send_stdin(f"{label}-pause")
        await survivor.close_stdin()
        survivor_result = await survivor.wait()
        if survivor_result.exit_code != 0 or survivor_result.stdout != f"{label}-pause":
            raise AssertionError(
                f"memory-preserving pause lost the running process: {survivor_result!r}"
            )

        trace(label, "sandbox.list")
        paginator = AsyncSandbox.list(
            query=SandboxQuery(metadata=metadata, state=[SandboxState.RUNNING]),
            limit=20,
            **options,
        )
        listed = assert_listed(await paginator.next_items(), sandbox.sandbox_id)
        assert_volume_mount(listed, volume_name, "/mnt/data")

        snapshot_content = f"official-{label}-snapshot"
        trace(label, "snapshot.write-state")
        await sandbox.files.write("a3s-snapshot-state.txt", snapshot_content)
        snapshot_metadata = (
            await sandbox.commands.run(
                "stat -c '%u:%g:%a' /home/user/a3s-snapshot-state.txt"
            )
        ).stdout.strip()
        trace(label, "snapshot.create")
        snapshot = await sandbox.create_snapshot(
            name=f"official-{label}-state"
        )
        snapshot_id = snapshot.snapshot_id
        if not snapshot_id or snapshot.names != [snapshot_id]:
            raise AssertionError(f"unexpected created Snapshot: {snapshot!r}")
        trace(label, "snapshot.list")
        snapshots = await sandbox.list_snapshots(limit=20).next_items()
        if not any(item.snapshot_id == snapshot_id for item in snapshots):
            raise AssertionError("created Snapshot was absent from the source-scoped list")
        trace(label, "snapshot.source-running")
        if not await sandbox.is_running():
            raise AssertionError("Snapshot did not restore the running source state")

        cold_pause_content = f"{label}-cold-pause"
        trace(label, "sandbox.cold-pause-write-state")
        await sandbox.files.write("a3s-cold-pause-state.txt", cold_pause_content)
        trace(label, "sandbox.cold-pause-process-start")
        cold_process = await sandbox.commands.run(
            "sleep 300", background=True, timeout=310
        )
        trace(label, "sandbox.cold-pause")
        if not await sandbox.pause(keep_memory=False):
            raise AssertionError("filesystem-only pause did not pause a running sandbox")
        trace(label, "sandbox.cold-pause-connect")
        cold_resumed = await sandbox.connect(timeout=60)
        if cold_resumed.sandbox_id != sandbox.sandbox_id:
            raise AssertionError("filesystem-only resume returned a different sandbox ID")
        trace(label, "sandbox.cold-pause-read-state")
        if (
            await sandbox.files.read("a3s-cold-pause-state.txt")
            != cold_pause_content
        ):
            raise AssertionError("filesystem-only pause lost rootfs state")
        trace(label, "sandbox.cold-pause-process-gone")
        if any(
            process.pid == cold_process.pid
            for process in await sandbox.commands.list()
        ):
            raise AssertionError("filesystem-only pause preserved an old runtime process")
        trace(label, "sandbox.cold-pause-environment")
        cold_environment = await sandbox.commands.run(
            "printf '%s' \"$OFFICIAL_CLIENT\""
        )
        if cold_environment.stdout != label or cold_environment.stderr:
            raise AssertionError(
                "filesystem-only resume lost the Sandbox environment: "
                f"{cold_environment!r}"
            )
        trace(label, "sandbox.cold-pause-volume")
        cold_mounted = await sandbox.commands.run("cat /mnt/data/shared/from-api.txt")
        if cold_mounted.stdout != api_content or cold_mounted.stderr:
            raise AssertionError("filesystem-only resume did not remount the Volume")

        trace(label, "sandbox.set-timeout")
        await sandbox.set_timeout(30)
        trace(label, "sandbox.kill")
        if not await sandbox.kill():
            raise AssertionError("kill did not terminate the production sandbox")
        trace(label, "sandbox.health-killed")
        if await sandbox.is_running():
            raise AssertionError("envd health reported the killed sandbox as running")

        trace(label, "snapshot.restore-after-source-kill")
        restored = await AsyncSandbox.create(snapshot_id, timeout=60, **options)
        trace(label, "snapshot.read-restored-state")
        if await restored.files.read("a3s-snapshot-state.txt") != snapshot_content:
            raise AssertionError("restored Sandbox lost the captured filesystem state")
        restored_metadata = (
            await restored.commands.run(
                "stat -c '%u:%g:%a' /home/user/a3s-snapshot-state.txt"
            )
        ).stdout.strip()
        if restored_metadata != snapshot_metadata:
            raise AssertionError(
                "restored Snapshot changed file ownership or mode: "
                f"{snapshot_metadata!r} -> {restored_metadata!r}"
            )
        await restored.commands.run(
            "printf '%s' '-writable' >> /home/user/a3s-snapshot-state.txt"
        )
        trace(label, "snapshot.delete-in-use")
        try:
            await AsyncSandbox.delete_snapshot(snapshot_id, **options)
        except SandboxException as error:
            if "409" not in str(error):
                raise AssertionError(
                    f"unexpected in-use Snapshot error: {error}"
                ) from error
        else:
            raise AssertionError("Snapshot was deleted while a restored Sandbox used it")
        trace(label, "snapshot.restored-kill")
        if not await restored.kill():
            raise AssertionError("restored Sandbox did not terminate")
        restored = None
        trace(label, "snapshot.delete")
        if not await AsyncSandbox.delete_snapshot(snapshot_id, **options):
            raise AssertionError("detached Snapshot was not deleted")
        trace(label, "snapshot.delete-missing")
        if await AsyncSandbox.delete_snapshot(snapshot_id, **options):
            raise AssertionError("missing Snapshot deletion reported success")
        snapshot_id = None

        trace(label, "volume.destroy")
        if not await AsyncVolume.destroy(volume.volume_id, **options):
            raise AssertionError("detached Volume was not destroyed")
        volume = None

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
        try:
            if interpreter is not None:
                await AsyncSandbox.kill(interpreter.sandbox_id, **options)
            if restored is not None:
                await AsyncSandbox.kill(restored.sandbox_id, **options)
            if sandbox is not None:
                await AsyncSandbox.kill(sandbox.sandbox_id, **options)
            if snapshot_id is not None:
                await AsyncSandbox.delete_snapshot(snapshot_id, **options)
        finally:
            if volume is not None:
                await AsyncVolume.destroy(volume.volume_id, **options)


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
