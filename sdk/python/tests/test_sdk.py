from __future__ import annotations

import asyncio
import base64
import json
import os
import unittest
from collections.abc import Mapping
from pathlib import Path
from typing import Any
from unittest.mock import patch

import a3s_box
from a3s_box import (
    A3SAsyncBoxClient,
    A3SBoxClient,
    A3SRemoteConnection,
    AsyncSandbox,
    RegistryCredentials,
    Sandbox,
    SignaturePolicy,
    SUPPORTED_BRIDGE_OPERATIONS,
)
from a3s_box.code_interpreter import Sandbox as CodeInterpreter
from a3s_box.runtime import _resolve_binary


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
    if operation == "image_build":
        return {
            "reference": request.get("tag", "local/build:latest"),
            "digest": "sha256:build",
            "size_bytes": 8192,
            "layer_count": 3,
        }
    if operation == "image_pull":
        return image_response(str(request["reference"]))
    if operation == "image_get":
        return {"image": image_response(str(request["reference"]))}
    if operation == "image_list":
        return {"images": [image_response("alpine:3.20")]}
    if operation == "image_inspect":
        return {"image": image_inspect_response(str(request["reference"]))}
    if operation == "image_history":
        return {
            "history": [
                {
                    "created": "2026-07-23T00:00:00Z",
                    "created_by": "RUN npm test",
                    "size_bytes": 2048,
                    "comment": "ci",
                    "empty_layer": False,
                }
            ]
        }
    if operation == "image_tag":
        return image_response(str(request["target"]))
    if operation == "image_push":
        return {
            "reference": request["target"],
            "manifest_digest": "sha256:manifest",
            "config_url": "https://registry.example/config",
            "manifest_url": "https://registry.example/manifest",
        }
    if operation == "image_evict":
        return {"references": ["local/old:latest"]}
    if operation == "image_remove":
        return {"reference": request["reference"], "removed": True}
    if operation == "volume_create":
        return volume_response(str(request["name"]))
    if operation == "volume_get":
        return {"volume": volume_response(str(request["name"]))}
    if operation == "volume_list":
        return {"volumes": [volume_response("ci-cache")]}
    if operation == "volume_remove":
        return volume_response(str(request["name"]))
    if operation == "volume_prune":
        return {"names": ["old-cache"]}
    if operation == "network_create":
        return network_response(str(request["name"]), str(request["subnet"]))
    if operation == "network_get":
        return {
            "network": network_response(str(request["name"]), "10.89.0.0/24")
        }
    if operation == "network_list":
        return {"networks": [network_response("ci-net", "10.89.0.0/24")]}
    if operation == "network_remove":
        return network_response(str(request["name"]), "10.89.0.0/24")
    if operation == "network_prune":
        return {"names": ["old-network"]}
    if operation == "sdk_capabilities":
        return {
            "protocol_version": 1,
            "operations": [
                "sdk_capabilities",
                "image_get",
                "image_inspect",
                "image_history",
                "image_tag",
                "image_push",
                "image_evict",
                "volume_prune",
                "network_prune",
            ],
        }
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
    if operation == "sandbox_snapshot_create":
        return {
            "snapshot_id": request["snapshot_id"],
            "size_bytes": 4096,
            "state": "running",
            "generation": request["generation"],
        }
    if operation == "filesystem_snapshot_size":
        return {
            "snapshot_id": request["snapshot_id"],
            "size_bytes": 4096,
        }
    if operation == "filesystem_snapshot_delete":
        return {
            "snapshot_id": request["snapshot_id"],
            "deleted": True,
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


def image_response(reference: str) -> dict[str, Any]:
    return {
        "reference": reference,
        "digest": "sha256:image",
        "size_bytes": 4096,
        "pulled_at": "2026-07-23T00:00:00Z",
        "last_used": "2026-07-23T00:00:00Z",
        "path": "/tmp/image",
    }


def image_inspect_response(reference: str) -> dict[str, Any]:
    return {
        **image_response(reference),
        "manifest_digest": "sha256:manifest",
        "layer_count": 2,
        "entrypoint": ["/bin/sh"],
        "command": ["-c", "npm test"],
        "env": {"CI": "true"},
        "working_dir": "/workspace",
        "user": "1000:1000",
        "exposed_ports": ["8080/tcp"],
        "volumes": ["/cache"],
        "stop_signal": "SIGTERM",
        "health_check": {
            "test": ["CMD", "true"],
            "interval": 1_000_000_000,
            "timeout": 500_000_000,
            "retries": 3,
            "start_period": 0,
        },
        "onbuild": [],
        "labels": {"purpose": "ci"},
    }


def volume_response(name: str) -> dict[str, Any]:
    return {
        "name": name,
        "driver": "local",
        "mount_point": f"/tmp/volumes/{name}",
        "labels": {"purpose": "ci"},
        "in_use_by": [],
        "in_use": False,
        "size_limit": 4096,
        "created_at": "2026-07-23T00:00:00Z",
    }


def network_response(name: str, subnet: str) -> dict[str, Any]:
    return {
        "name": name,
        "driver": "bridge",
        "subnet": subnet,
        "gateway": "10.89.0.1",
        "labels": {"purpose": "ci"},
        "endpoints": [],
        "endpoint_count": 0,
        "isolation": "none",
        "created_at": "2026-07-23T00:00:00Z",
    }


class SdkTests(unittest.TestCase):
    def test_exports_native_local_clients_without_importing_e2b(self) -> None:
        self.assertIs(a3s_box.Sandbox, Sandbox)
        self.assertIs(a3s_box.AsyncSandbox, AsyncSandbox)
        self.assertEqual(a3s_box.DEFAULT_IMAGE, "alpine:3.20")
        inventory = json.loads(
            (
                Path(__file__).resolve().parents[2]
                / "bridge-operations.json"
            ).read_text()
        )
        self.assertEqual(list(SUPPORTED_BRIDGE_OPERATIONS), inventory)

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
        self.assertEqual(create["isolation"], "microvm")
        self.assertEqual(command["argv"], ["/bin/sh", "-lc", "python -c 'print(6 * 7)'"])
        self.assertEqual(command["generation"], 1)
        self.assertEqual(write["data_base64"], base64.b64encode(b"hello").decode())
        self.assertEqual(read["path"], "/workspace/notes.txt")
        self.assertEqual(stat["operation"], "filesystem_stat")
        self.assertEqual(kill["operation"], "sandbox_kill")

    def test_fluent_programmable_cicd_builders_share_the_e2b_sandbox(self) -> None:
        runtime = FakeRuntime()
        client = A3SBoxClient(runtime)

        image = (
            client.image("./ci")
            .dockerfile("Dockerfile.ci")
            .tag("local/ci-base:latest")
            .build_arg("NODE_VERSION", "24")
            .platform("linux/arm64")
            .build()
        )
        volume = (
            client.volume("ci-cache")
            .label("purpose", "ci")
            .size_limit(4096)
            .create()
        )
        network = (
            client.network("ci-net")
            .subnet("10.89.55.0/24")
            .label("purpose", "ci")
            .create()
        )
        sandbox = (
            client.sandbox(image.reference)
            .cpus(4)
            .memory_mb(4096)
            .mount_named(volume.name, "/cache")
            .network(network.name)
            .publish_tcp(8080, 80)
            .workdir("/workspace")
            .auto_remove(False)
            .start()
        )
        result = (
            sandbox.script("print(6 * 7)\n")
            .interpreter("python", "-")
            .env("CI", "true")
            .cwd("/workspace")
            .run()
        )
        sandbox.kill()
        client.remove_network(network.name)
        client.remove_volume(volume.name)
        client.remove_image(image.reference)

        self.assertEqual(result.stdout, "42\n")
        self.assertEqual(runtime.requests[0]["operation"], "image_build")
        self.assertEqual(runtime.requests[0]["dockerfile"], "Dockerfile.ci")
        self.assertEqual(runtime.requests[0]["platforms"], ["linux/arm64"])
        create = runtime.requests[3]
        self.assertEqual(create["operation"], "sandbox_create")
        self.assertEqual(
            create["mounts"],
            [
                {
                    "kind": "named",
                    "name": "ci-cache",
                    "target": "/cache",
                    "read_only": False,
                }
            ],
        )
        self.assertEqual(create["network"], {"mode": "bridge", "name": "ci-net"})
        self.assertEqual(
            create["ports"],
            [{"host_port": 8080, "guest_port": 80}],
        )
        self.assertFalse(create["auto_remove"])
        command = runtime.requests[4]
        self.assertEqual(command["argv"], ["python", "-"])
        self.assertEqual(
            base64.b64decode(str(command["stdin_base64"])),
            b"print(6 * 7)\n",
        )

    def test_complete_image_and_resource_management_surface(self) -> None:
        runtime = FakeRuntime()
        client = A3SBoxClient(runtime)
        credentials = RegistryCredentials("builder", "secret")
        signature = SignaturePolicy.cosign_key("/keys/cosign.pub")

        pulled = client.pull_image(
            "registry.example/ci/base:latest",
            credentials=credentials,
            signature_policy=signature,
        )
        cached = client.get_image(pulled.reference)
        inspected = client.inspect_image(pulled.reference)
        history = client.image_history(pulled.reference)
        tagged = client.tag_image(pulled.reference, "local/ci:tested")
        pushed = client.push_image(
            tagged.reference,
            "registry.example/ci/app:tested",
            credentials=credentials,
            protocol="http",
        )
        evicted = client.evict_images()
        pruned_volumes = client.prune_volumes()
        pruned_networks = client.prune_networks()
        capabilities = client.capabilities()

        self.assertEqual(cached, pulled)
        self.assertEqual(inspected.manifest_digest, "sha256:manifest")
        self.assertEqual(inspected.health_check.retries, 3)
        self.assertEqual(history[0].created_by, "RUN npm test")
        self.assertEqual(pushed.manifest_digest, "sha256:manifest")
        self.assertEqual(evicted, ["local/old:latest"])
        self.assertEqual(pruned_volumes, ["old-cache"])
        self.assertEqual(pruned_networks, ["old-network"])
        self.assertIn("image_push", capabilities.operations)

        pull_request = runtime.requests[0]
        self.assertEqual(
            pull_request["credentials"],
            {"username": "builder", "password": "secret"},
        )
        self.assertEqual(
            pull_request["signature_policy"],
            {"mode": "cosign_key", "public_key": "/keys/cosign.pub"},
        )
        push_request = runtime.requests[5]
        self.assertEqual(push_request["registry_protocol"], "http")
        self.assertEqual(push_request["credentials"]["username"], "builder")

    def test_create_explicitly_selects_shared_kernel_sandbox_isolation(self) -> None:
        runtime = FakeRuntime()

        sandbox = Sandbox.create(isolation="sandbox", runtime=runtime)
        sandbox.kill()

        self.assertEqual(runtime.requests[0]["isolation"], "sandbox")

    def test_local_binary_resolution_ignores_remote_credentials(self) -> None:
        environment = {
            "E2B_API_KEY": "must-not-be-read",
            "A3S_BOX_API_KEY": "must-not-be-read",
            "A3S_BOX_ENDPOINT": "https://must-not-be-read.invalid",
        }
        with (
            patch.dict(os.environ, environment, clear=True),
            patch("a3s_box.runtime.shutil.which", return_value="/usr/local/bin/a3s-box") as which,
        ):
            self.assertEqual(_resolve_binary(None), "/usr/local/bin/a3s-box")
            which.assert_called_once_with("a3s-box")

    def test_connect_recovers_a_local_handle_without_credentials(self) -> None:
        runtime = FakeRuntime()

        sandbox = Sandbox.connect("existing-local", runtime=runtime)

        self.assertEqual(sandbox.sandbox_id, "existing-local")
        self.assertEqual(sandbox.generation, 2)
        self.assertEqual(sandbox.state, "paused")
        self.assertEqual(runtime.requests[0]["operation"], "sandbox_inspect")

    def test_runtime_managed_filesystem_snapshot_lifecycle(self) -> None:
        runtime = FakeRuntime()

        sandbox = Sandbox.create(
            isolation="sandbox",
            filesystem_snapshot_id="ci-base-source",
            runtime=runtime,
        )
        snapshot = sandbox.create_filesystem_snapshot("ci-base-captured")
        size = Sandbox.filesystem_snapshot_size(snapshot.snapshot_id, runtime=runtime)
        deleted = Sandbox.delete_filesystem_snapshot(snapshot.snapshot_id, runtime=runtime)
        sandbox.kill()

        self.assertEqual(snapshot.snapshot_id, "ci-base-captured")
        self.assertEqual(snapshot.size_bytes, 4096)
        self.assertEqual(snapshot.state, "running")
        self.assertEqual(size, 4096)
        self.assertTrue(deleted)
        self.assertEqual(
            [request["operation"] for request in runtime.requests],
            [
                "sandbox_create",
                "sandbox_snapshot_create",
                "filesystem_snapshot_size",
                "filesystem_snapshot_delete",
                "sandbox_kill",
            ],
        )
        self.assertEqual(
            runtime.requests[0]["filesystem_snapshot_id"],
            "ci-base-source",
        )

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

    async def test_async_fluent_builders_have_resource_and_script_parity(self) -> None:
        runtime = AsyncFakeRuntime()
        client = A3SAsyncBoxClient(runtime)
        image = await client.image("./ci").tag("local/async-ci:latest").build()
        await client.volume("async-cache").create()
        sandbox = await (
            client.sandbox(image.reference)
            .mount_named("async-cache", "/cache", read_only=True)
            .disable_network()
            .start()
        )
        result = await sandbox.script("printf '42\\n'\n").run()
        await sandbox.kill()
        await client.remove_volume("async-cache")
        await client.remove_image(image.reference)

        self.assertEqual(result.stdout, "42\n")
        self.assertEqual(runtime.requests[2]["network"], {"mode": "none"})
        self.assertTrue(runtime.requests[2]["mounts"][0]["read_only"])
        self.assertEqual(runtime.requests[3]["argv"], ["/bin/sh", "-se"])

    async def test_async_resource_management_matches_sync_surface(self) -> None:
        runtime = AsyncFakeRuntime()
        client = A3SAsyncBoxClient(runtime)

        image = await client.get_image("alpine:3.20")
        inspect = await client.inspect_image("alpine:3.20")
        history = await client.image_history("alpine:3.20")
        tagged = await client.tag_image("alpine:3.20", "local/async:latest")
        pushed = await client.push_image(
            tagged.reference,
            "registry.example/async:latest",
        )
        evicted = await client.evict_images()
        volumes = await client.prune_volumes()
        networks = await client.prune_networks()
        capabilities = await client.capabilities()

        self.assertEqual(image.reference, "alpine:3.20")
        self.assertEqual(inspect.layer_count, 2)
        self.assertEqual(history[0].size_bytes, 2048)
        self.assertEqual(pushed.reference, "registry.example/async:latest")
        self.assertEqual(evicted, ["local/old:latest"])
        self.assertEqual(volumes, ["old-cache"])
        self.assertEqual(networks, ["old-network"])
        self.assertEqual(capabilities.protocol_version, 1)

    async def test_async_filesystem_snapshot_lifecycle(self) -> None:
        runtime = AsyncFakeRuntime()
        sandbox = await AsyncSandbox.create(isolation="sandbox", runtime=runtime)

        snapshot = await sandbox.create_filesystem_snapshot("ci-async")
        size = await AsyncSandbox.filesystem_snapshot_size(
            snapshot.snapshot_id,
            runtime=runtime,
        )
        deleted = await AsyncSandbox.delete_filesystem_snapshot(
            snapshot.snapshot_id,
            runtime=runtime,
        )
        await sandbox.kill()

        self.assertEqual(snapshot.size_bytes, 4096)
        self.assertEqual(size, 4096)
        self.assertTrue(deleted)


if __name__ == "__main__":
    unittest.main()
