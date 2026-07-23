from __future__ import annotations

import asyncio
import base64
import unittest
from collections.abc import Mapping
from typing import Any

import a3s_box
from a3s_box import (
    A3SRemoteConnection,
    AsyncSandbox,
    Sandbox,
)
from a3s_box.code_interpreter import Sandbox as CodeInterpreter


class FakeRuntime:
    def __init__(self) -> None:
        self.requests: list[dict[str, Any]] = []

    def request(self, request: Mapping[str, object]) -> dict[str, Any]:
        payload = dict(request)
        self.requests.append(payload)
        return response_for(payload)


class AsyncFakeRuntime:
    def __init__(self) -> None:
        self.requests: list[dict[str, Any]] = []

    async def request(self, request: Mapping[str, object]) -> dict[str, Any]:
        payload = dict(request)
        self.requests.append(payload)
        await asyncio.sleep(0)
        return response_for(payload)


def response_for(request: Mapping[str, object]) -> dict[str, Any]:
    operation = request["operation"]
    if operation == "sandbox_create":
        return {
            "sandbox_id": "sandbox-local-1",
            "generation": 1,
            "state": "running",
        }
    if operation == "sandbox_inspect":
        return {
            "sandbox_id": request["sandbox_id"],
            "generation": 2,
            "state": "paused",
        }
    if operation == "command_run":
        return {
            "stdout_base64": base64.b64encode(b"42\n").decode(),
            "stderr_base64": "",
            "exit_code": 0,
            "truncated": False,
        }
    if operation == "file_write":
        return {"path": request["path"], "size": 5}
    if operation == "file_read":
        return {
            "path": request["path"],
            "data_base64": base64.b64encode(b"hello").decode(),
            "size": 5,
        }
    if operation == "filesystem_stat":
        return {
            "entry": {
                "name": "notes.txt",
                "type": "file",
                "path": request["path"],
                "size": 5,
                "mode": 420,
                "permissions": "-rw-r--r--",
                "owner": "root",
                "group": "root",
                "modified_seconds": 1,
                "modified_nanos": 0,
                "symlink_target": None,
            }
        }
    if operation == "filesystem_list":
        return {"entries": []}
    if operation in {
        "sandbox_kill",
        "sandbox_pause",
        "sandbox_resume",
        "filesystem_make_dir",
        "filesystem_move",
        "filesystem_remove",
    }:
        return {"ok": True}
    raise AssertionError(f"unexpected operation: {operation}")


class SdkTests(unittest.TestCase):
    def test_exports_native_local_clients_without_importing_e2b(self) -> None:
        self.assertIs(a3s_box.Sandbox, Sandbox)
        self.assertIs(a3s_box.AsyncSandbox, AsyncSandbox)
        self.assertEqual(a3s_box.DEFAULT_IMAGE, "alpine:3.20")

    def test_sync_sandbox_uses_local_runtime_with_e2b_like_surface(self) -> None:
        runtime = FakeRuntime()

        with Sandbox.create(
            "python:3.12-alpine",
            timeout=120,
            envs={"MODE": "test"},
            metadata={"suite": "sdk"},
            runtime=runtime,
        ) as sandbox:
            self.assertEqual(sandbox.sandbox_id, "sandbox-local-1")
            result = sandbox.commands.run(
                "python -c 'print(6 * 7)'",
                timeout=10,
                cwd="/workspace",
                envs={"REQUEST": "one"},
            )
            self.assertEqual(result.stdout, "42\n")
            self.assertEqual(result.stderr, "")
            self.assertEqual(result.exit_code, 0)

            write = sandbox.files.write("/workspace/notes.txt", "hello")
            self.assertEqual(write.size, 5)
            self.assertEqual(sandbox.files.read("/workspace/notes.txt"), "hello")
            self.assertTrue(sandbox.files.exists("/workspace/notes.txt"))

        create, command, write, read, stat, kill = runtime.requests
        self.assertEqual(create["operation"], "sandbox_create")
        self.assertEqual(create["image"], "python:3.12-alpine")
        self.assertEqual(create["timeout_seconds"], 120)
        self.assertEqual(create["env"], {"MODE": "test"})
        self.assertEqual(create["labels"], {"suite": "sdk"})
        self.assertEqual(command["argv"], ["/bin/sh", "-lc", "python -c 'print(6 * 7)'"])
        self.assertEqual(command["generation"], 1)
        self.assertEqual(write["data_base64"], base64.b64encode(b"hello").decode())
        self.assertEqual(read["path"], "/workspace/notes.txt")
        self.assertEqual(stat["operation"], "filesystem_stat")
        self.assertEqual(kill["operation"], "sandbox_kill")

    def test_connect_recovers_a_local_handle_without_credentials(self) -> None:
        runtime = FakeRuntime()

        sandbox = Sandbox.connect("existing-local", runtime=runtime)

        self.assertEqual(sandbox.sandbox_id, "existing-local")
        self.assertEqual(sandbox.generation, 2)
        self.assertEqual(sandbox.state, "paused")
        self.assertEqual(runtime.requests[0]["operation"], "sandbox_inspect")

    def test_code_interpreter_uses_the_native_local_sandbox(self) -> None:
        runtime = FakeRuntime()

        interpreter = CodeInterpreter.create(runtime=runtime)
        result = interpreter.run_code("print(6 * 7)")
        interpreter.kill()

        self.assertEqual(result.stdout, "42\n")
        self.assertEqual(runtime.requests[0]["image"], "python:3.12-alpine")
        self.assertEqual(
            runtime.requests[1]["argv"],
            ["python", "-c", "print(6 * 7)"],
        )

    def test_remote_configuration_is_explicit_and_not_used_by_local_create(self) -> None:
        connection = A3SRemoteConnection.from_environment(
            {
                "A3S_BOX_ENDPOINT": "https://api.box.example.com",
                "A3S_BOX_API_KEY": "e2b_a1b2c3",
            }
        )
        self.assertEqual(connection.domain, "box.example.com")
        self.assertEqual(
            connection.official_python_options(),
            {
                "api_url": "https://api.box.example.com",
                "domain": "box.example.com",
                "api_key": "e2b_a1b2c3",
            },
        )

        with self.assertRaisesRegex(ValueError, "A3S_BOX_ENDPOINT is required"):
            A3SRemoteConnection.from_environment({})


class AsyncSdkTests(unittest.IsolatedAsyncioTestCase):
    async def test_async_sandbox_uses_the_same_local_protocol(self) -> None:
        runtime = AsyncFakeRuntime()

        async with await AsyncSandbox.create(runtime=runtime) as sandbox:
            result = await sandbox.commands.run(["printf", "42"])
            self.assertEqual(result.stdout, "42\n")
            data = await sandbox.files.read("/workspace/notes.txt", format="bytes")
            self.assertEqual(data, b"hello")

        self.assertEqual(runtime.requests[0]["operation"], "sandbox_create")
        self.assertEqual(runtime.requests[0]["image"], "alpine:3.20")
        self.assertEqual(runtime.requests[1]["argv"], ["printf", "42"])
        self.assertEqual(runtime.requests[-1]["operation"], "sandbox_kill")


if __name__ == "__main__":
    unittest.main()
