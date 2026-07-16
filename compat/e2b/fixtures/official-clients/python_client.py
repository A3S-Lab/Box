#!/usr/bin/env python3
"""Exercise pinned official Python lifecycle clients against the recorder."""

from __future__ import annotations

import argparse
import asyncio
from typing import Any

from e2b import (
    AsyncVolume,
    AsyncSandbox,
    Sandbox,
    SandboxNotFoundException,
    SandboxQuery,
    SandboxState,
    Volume,
)
from e2b_code_interpreter import AsyncSandbox as AsyncCodeInterpreter
from e2b_code_interpreter import Sandbox as CodeInterpreter


API_KEY = "e2b_a1b2c3"
SANDBOX_ID = "fixture-sandbox"
RESTORED_SANDBOX_ID = "fixture-restored"
INTERPRETER_SANDBOX_ID = "fixture-interpreter"
MISSING_SANDBOX_ID = "missing-sandbox"


def connection(api_url: str) -> dict[str, Any]:
    return {"api_key": API_KEY, "api_url": api_url}


def create_options(api_url: str) -> dict[str, Any]:
    return {
        **connection(api_url),
        "allow_internet_access": False,
        "envs": {"BETA": "two", "ALPHA": "one"},
        "lifecycle": {
            "on_timeout": {"action": "pause", "keep_memory": False},
            "auto_resume": False,
        },
        "metadata": {"team": "alpha beta", "purpose": "fixture"},
        "secure": True,
        "timeout": 321,
    }


def run_sync(api_url: str) -> None:
    volume = Volume.create("fixture-data", **connection(api_url))
    assert volume.volume_id
    assert volume.token == "fixture-volume-token"
    connected_volume = Volume.connect(volume.volume_id, **connection(api_url))
    assert connected_volume.name == "fixture-data"
    assert any(
        item.volume_id == volume.volume_id
        for item in Volume.list(**connection(api_url))
    )
    directory = volume.make_dir(
        "/nested", force=True, mode=0o755, api_url=api_url
    )
    assert directory.path == "/nested"
    written = volume.write_file(
        "/nested/value.txt", "volume-value", mode=0o644, api_url=api_url
    )
    assert written.size == len("volume-value")
    assert volume.exists("/nested/value.txt", api_url=api_url)
    updated = volume.update_metadata(
        "/nested/value.txt", mode=0o600, api_url=api_url
    )
    assert updated.mode == 0o600
    assert len(volume.list("/", depth=2, api_url=api_url)) == 2
    assert volume.read_file("/nested/value.txt", api_url=api_url) == "volume-value"
    volume.remove("/nested", api_url=api_url)

    sandbox = Sandbox.create(
        "fixture-template",
        volume_mounts={"/mnt/data": volume},
        **create_options(api_url),
    )
    assert sandbox.sandbox_id == SANDBOX_ID

    assert sandbox.pause(keep_memory=True)
    assert not sandbox.pause(keep_memory=True)

    connected = Sandbox.connect(SANDBOX_ID, timeout=222, **connection(api_url))
    assert connected.sandbox_id == SANDBOX_ID

    paginator = Sandbox.list(
        query=SandboxQuery(
            metadata={"team": "alpha beta"},
            state=[SandboxState.RUNNING, SandboxState.PAUSED],
        ),
        limit=2,
        next_token="cursor-0",
        **connection(api_url),
    )
    listed = paginator.next_items()
    assert len(listed) == 1
    assert listed[0].volume_mounts[0]["name"] == "fixture-data"
    assert listed[0].volume_mounts[0]["path"] == "/mnt/data"

    snapshot = sandbox.create_snapshot(name="fixture-state")
    assert snapshot.snapshot_id
    assert snapshot.names == [snapshot.snapshot_id]
    snapshots = sandbox.list_snapshots(limit=1).next_items()
    assert len(snapshots) == 1
    assert snapshots[0].snapshot_id == snapshot.snapshot_id
    restored = Sandbox.create(snapshot.snapshot_id, **connection(api_url))
    assert restored.sandbox_id == RESTORED_SANDBOX_ID
    assert restored.kill()
    assert Sandbox.delete_snapshot(snapshot.snapshot_id, **connection(api_url))
    assert not Sandbox.delete_snapshot(snapshot.snapshot_id, **connection(api_url))

    sandbox.set_timeout(123)
    assert sandbox.kill()
    assert not Sandbox.kill(MISSING_SANDBOX_ID, **connection(api_url))
    try:
        Sandbox.connect(MISSING_SANDBOX_ID, **connection(api_url))
    except SandboxNotFoundException:
        pass
    else:
        raise AssertionError("missing sandbox connect must raise SandboxNotFoundException")

    interpreter = CodeInterpreter.create(**connection(api_url))
    assert interpreter.sandbox_id == INTERPRETER_SANDBOX_ID
    assert interpreter.kill()
    assert Volume.destroy(volume.volume_id, **connection(api_url))


async def run_async(api_url: str) -> None:
    volume = await AsyncVolume.create("fixture-data", **connection(api_url))
    assert volume.volume_id
    assert volume.token == "fixture-volume-token"
    connected_volume = await AsyncVolume.connect(
        volume.volume_id, **connection(api_url)
    )
    assert connected_volume.name == "fixture-data"
    assert any(
        item.volume_id == volume.volume_id
        for item in await AsyncVolume.list(**connection(api_url))
    )
    directory = await volume.make_dir(
        "/nested", force=True, mode=0o755, api_url=api_url
    )
    assert directory.path == "/nested"
    written = await volume.write_file(
        "/nested/value.txt", "volume-value", mode=0o644, api_url=api_url
    )
    assert written.size == len("volume-value")
    assert await volume.exists("/nested/value.txt", api_url=api_url)
    updated = await volume.update_metadata(
        "/nested/value.txt", mode=0o600, api_url=api_url
    )
    assert updated.mode == 0o600
    assert len(await volume.list("/", depth=2, api_url=api_url)) == 2
    assert (
        await volume.read_file("/nested/value.txt", api_url=api_url)
        == "volume-value"
    )
    await volume.remove("/nested", api_url=api_url)

    sandbox = await AsyncSandbox.create(
        "fixture-template",
        volume_mounts={"/mnt/data": volume},
        **create_options(api_url),
    )
    assert sandbox.sandbox_id == SANDBOX_ID

    assert await sandbox.pause(keep_memory=True)
    assert not await sandbox.pause(keep_memory=True)

    connected = await AsyncSandbox.connect(
        SANDBOX_ID, timeout=222, **connection(api_url)
    )
    assert connected.sandbox_id == SANDBOX_ID

    paginator = AsyncSandbox.list(
        query=SandboxQuery(
            metadata={"team": "alpha beta"},
            state=[SandboxState.RUNNING, SandboxState.PAUSED],
        ),
        limit=2,
        next_token="cursor-0",
        **connection(api_url),
    )
    listed = await paginator.next_items()
    assert len(listed) == 1
    assert listed[0].volume_mounts[0]["name"] == "fixture-data"
    assert listed[0].volume_mounts[0]["path"] == "/mnt/data"

    snapshot = await sandbox.create_snapshot(name="fixture-state")
    assert snapshot.snapshot_id
    assert snapshot.names == [snapshot.snapshot_id]
    snapshots = await sandbox.list_snapshots(limit=1).next_items()
    assert len(snapshots) == 1
    assert snapshots[0].snapshot_id == snapshot.snapshot_id
    restored = await AsyncSandbox.create(snapshot.snapshot_id, **connection(api_url))
    assert restored.sandbox_id == RESTORED_SANDBOX_ID
    assert await restored.kill()
    assert await AsyncSandbox.delete_snapshot(
        snapshot.snapshot_id, **connection(api_url)
    )
    assert not await AsyncSandbox.delete_snapshot(
        snapshot.snapshot_id, **connection(api_url)
    )

    await sandbox.set_timeout(123)
    assert await sandbox.kill()
    assert not await AsyncSandbox.kill(MISSING_SANDBOX_ID, **connection(api_url))
    try:
        await AsyncSandbox.connect(MISSING_SANDBOX_ID, **connection(api_url))
    except SandboxNotFoundException:
        pass
    else:
        raise AssertionError("missing sandbox connect must raise SandboxNotFoundException")

    interpreter = await AsyncCodeInterpreter.create(**connection(api_url))
    assert interpreter.sandbox_id == INTERPRETER_SANDBOX_ID
    assert await interpreter.kill()
    assert await AsyncVolume.destroy(volume.volume_id, **connection(api_url))


def main() -> None:
    parser = argparse.ArgumentParser()
    parser.add_argument("mode", choices=["sync", "async"])
    parser.add_argument("api_url")
    args = parser.parse_args()
    if args.mode == "sync":
        run_sync(args.api_url)
    else:
        asyncio.run(run_async(args.api_url))


if __name__ == "__main__":
    main()
