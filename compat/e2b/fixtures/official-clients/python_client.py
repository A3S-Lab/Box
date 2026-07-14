#!/usr/bin/env python3
"""Exercise pinned official Python lifecycle clients against the recorder."""

from __future__ import annotations

import argparse
import asyncio
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


API_KEY = "e2b_a1b2c3"
SANDBOX_ID = "fixture-sandbox"
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
    sandbox = Sandbox.create("fixture-template", **create_options(api_url))
    assert sandbox.sandbox_id == SANDBOX_ID

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
    assert len(paginator.next_items()) == 1

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
    assert interpreter.kill()


async def run_async(api_url: str) -> None:
    sandbox = await AsyncSandbox.create("fixture-template", **create_options(api_url))
    assert sandbox.sandbox_id == SANDBOX_ID

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
    assert len(await paginator.next_items()) == 1

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
    assert await interpreter.kill()


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
