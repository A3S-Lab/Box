from __future__ import annotations

import asyncio
import base64
import inspect
import json
import os
import threading
import unittest
from collections.abc import Mapping
from pathlib import Path
from tempfile import TemporaryDirectory
from typing import Any
from unittest.mock import patch

import a3s_box
from a3s_box import (
    A3SBoxError,
    A3SAsyncBoxClient,
    A3SBoxClient,
    A3SBoxNotInstalledError,
    A3SAsyncLocalRuntime,
    AsyncSandbox,
    ExecutionResourceUpdate,
    MAX_ARTIFACT_BYTES,
    RegistryCredentials,
    Sandbox,
    SignaturePolicy,
    SUPPORTED_BRIDGE_OPERATIONS,
    BRIDGE_PROTOCOL_VERSION,
)
from a3s_box.code_interpreter import Sandbox as CodeInterpreter
from a3s_box.runtime import _decode_response, _resolve_binary
from a3s_box.sandbox import AsyncCommands, AsyncFilesystem, Commands, Filesystem


class FakeRuntime:
    def __init__(self) -> None:
        self.requests: list[dict[str, Any]] = []

    def request(self, request: Mapping[str, object]) -> dict[str, Any]:
        payload = dict(request)
        if payload["operation"] != "sdk_capabilities":
            self.requests.append(payload)
        return response_for(payload)


class AsyncFakeRuntime:
    def __init__(self) -> None:
        self.requests: list[dict[str, Any]] = []

    async def request(self, request: Mapping[str, object]) -> dict[str, Any]:
        payload = dict(request)
        if payload["operation"] != "sdk_capabilities":
            self.requests.append(payload)
        await asyncio.sleep(0)
        return response_for(payload)


class ArtifactRuntime(FakeRuntime):
    def __init__(
        self,
        *,
        data: bytes = b"hello",
        declared_size: int | None = None,
        stat_size: int = 5,
        entry_type: str = "file",
    ) -> None:
        super().__init__()
        self.data = data
        self.declared_size = len(data) if declared_size is None else declared_size
        self.stat_size = stat_size
        self.entry_type = entry_type

    def request(self, request: Mapping[str, object]) -> dict[str, Any]:
        payload = dict(request)
        if payload["operation"] != "sdk_capabilities":
            self.requests.append(payload)
        if payload["operation"] == "file_read":
            return {
                "path": payload["path"],
                "data_base64": base64.b64encode(self.data).decode(),
                "size": self.declared_size,
            }
        if payload["operation"] == "filesystem_stat":
            result = response_for(payload)
            result["entry"]["size"] = self.stat_size
            result["entry"]["type"] = self.entry_type
            return result
        return response_for(payload)


class AsyncArtifactRuntime(ArtifactRuntime):
    async def request(self, request: Mapping[str, object]) -> dict[str, Any]:
        await asyncio.sleep(0)
        return super().request(request)


def event_stream_response(request: Mapping[str, object]) -> dict[str, Any]:
    after_sequence = int(request["after_sequence"])
    limit = int(request["limit"])
    events = [
        {
            "sequence": sequence,
            "timestamp_unix_ns": str(1_700_000_000_000_000_000 + sequence),
            "process_id": "init" if sequence == 5 else None,
            "kind": (
                "container-started"
                if sequence == 2
                else "process-started"
                if sequence == 5
                else "resources-updated"
            ),
            "attributes": {},
        }
        for sequence in (2, 5, 9)
        if sequence > after_sequence
    ][:limit]
    return {
        "execution_id": request["sandbox_id"],
        "generation": request["generation"],
        "events": events,
        "next_sequence": (
            int(events[-1]["sequence"]) if events else after_sequence
        ),
    }


class CapabilityRuntime:
    def __init__(
        self,
        *,
        protocol_version: int = BRIDGE_PROTOCOL_VERSION,
        operations: tuple[str, ...] = SUPPORTED_BRIDGE_OPERATIONS,
    ) -> None:
        self.protocol_version = protocol_version
        self.operations = operations
        self.requests: list[dict[str, Any]] = []

    def request(self, request: Mapping[str, object]) -> dict[str, Any]:
        payload = dict(request)
        self.requests.append(payload)
        if payload["operation"] == "sdk_capabilities":
            return {
                "protocol_version": self.protocol_version,
                "operations": list(self.operations),
            }
        raise AssertionError("a mutating request ran before capability validation")


class AsyncCapabilityRuntime:
    def __init__(self) -> None:
        self.requests: list[dict[str, Any]] = []

    async def request(self, request: Mapping[str, object]) -> dict[str, Any]:
        payload = dict(request)
        self.requests.append(payload)
        if payload["operation"] == "sdk_capabilities":
            await asyncio.sleep(0)
            return {
                "protocol_version": BRIDGE_PROTOCOL_VERSION,
                "operations": list(SUPPORTED_BRIDGE_OPERATIONS),
            }
        if payload["operation"] == "image_list":
            return {"images": []}
        if payload["operation"] == "volume_list":
            return {"volumes": []}
        raise AssertionError(f"unexpected operation {payload['operation']!r}")


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
    if operation == "runtime_diagnostics":
        return {
            "core_version": "3.2.2",
            "runtime_version": "3.2.2",
            "sdk_version": "3.2.2",
            "home": "/tmp/a3s",
            "virtualization": {
                "available": True,
                "backend": "hvf",
                "details": "Apple Hypervisor.framework",
            },
        }
    if operation == "runtime_disk_usage":
        return {
            "home": "/tmp/a3s",
            "total_bytes": 28,
            "boxes_bytes": 1,
            "images_bytes": 2,
            "volumes_bytes": 3,
            "snapshots_bytes": 4,
            "state_bytes": 8,
            "other_bytes": 10,
        }
    if operation == "sdk_capabilities":
        return {
            "protocol_version": BRIDGE_PROTOCOL_VERSION,
            "operations": list(SUPPORTED_BRIDGE_OPERATIONS),
        }
    if operation == "sandbox_list":
        return {"sandboxes": [sandbox_summary_response("sandbox-local-1")]}
    if operation == "sandbox_get":
        return {"sandbox": sandbox_summary_response(str(request["query"]))}
    if operation == "sandbox_create":
        return {
            "sandbox_id": "sandbox-local-1",
            "generation": 1,
            "state": "running",
            "isolation": request.get("isolation", "microvm"),
        }
    if operation == "sandbox_inspect":
        return {
            "sandbox_id": request["sandbox_id"],
            "generation": 2,
            "state": "paused",
            "isolation": "sandbox",
        }
    if operation == "sandbox_stop":
        return {
            "sandbox_id": request["sandbox_id"],
            "generation": request["generation"],
            "state": "stopped",
        }
    if operation == "sandbox_restart":
        return {
            "sandbox_id": request["sandbox_id"],
            "generation": int(request["generation"]) + 1,
            "state": "running",
        }
    if operation == "sandbox_remove":
        return {
            "sandbox_id": request["sandbox_id"],
            "generation": request["generation"],
            "state": "removed",
        }
    if operation == "sandbox_logs":
        return {
            "logs": [
                {
                    "stream": "stdout",
                    "log": "sdk-log\n",
                    "time": "2026-07-23T00:00:00Z",
                }
            ]
        }
    if operation == "sandbox_stats":
        return {"stats": sandbox_stats_response(str(request["sandbox_id"]))}
    if operation == "sandbox_processes":
        return {
            "execution_id": request["sandbox_id"],
            "generation": request["generation"],
            "processes": [
                {"process_id": "init", "pid": 1234, "terminal": False},
                {"process_id": "exec-1", "pid": None, "terminal": True},
            ],
        }
    if operation == "sandbox_runtime_stats":
        return {
            "execution_id": request["sandbox_id"],
            "generation": request["generation"],
            "timestamp_unix_ns": "1700000000000000123",
            "cpu": {
                "usage_ns": 300,
                "user_ns": 200,
                "system_ns": 100,
                "throttled_ns": 5,
            },
            "memory": {
                "usage_bytes": 1024,
                "limit_bytes": 2048,
                "peak_bytes": 1536,
            },
            "process_count": 2,
            "metrics": {"io.read_bytes": 64},
        }
    if operation == "sandbox_events":
        return {
            "execution_id": request["sandbox_id"],
            "generation": request["generation"],
            "events": [
                {
                    "sequence": int(request["after_sequence"]) + 1,
                    "timestamp_unix_ns": "1700000000000000124",
                    "process_id": "exec-1",
                    "kind": "process-exited",
                    "attributes": {"exit_code": "0"},
                }
            ],
            "next_sequence": int(request["after_sequence"]) + 1,
        }
    if operation == "sandbox_update_resources":
        return {
            "sandbox_id": request["sandbox_id"],
            "generation": request["generation"],
            "state": "running",
            "isolation": "microvm",
        }
    if operation == "sandbox_snapshot_create":
        return {
            "snapshot_id": request["snapshot_id"],
            "size_bytes": 4096,
            "state": "running",
            "generation": request["generation"],
        }
    if operation == "filesystem_snapshot_list":
        return {"snapshots": [filesystem_snapshot_response("ci-base")]}
    if operation == "filesystem_snapshot_get":
        return {
            "snapshot": filesystem_snapshot_response(str(request["snapshot_id"]))
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


def sandbox_summary_response(sandbox_id: str) -> dict[str, Any]:
    return {
        "id": sandbox_id,
        "short_id": "sandboxlocal",
        "name": "ci-box",
        "image": "alpine:3.20",
        "isolation": "microvm",
        "status": "running",
        "status_summary": "running",
        "active": True,
        "pid": 1234,
        "cpus": 2,
        "memory_mb": 512,
        "ports": ["8080:80"],
        "command": ["sh"],
        "health": "none",
        "labels": {"purpose": "ci"},
        "created_at": "2026-07-23T00:00:00Z",
        "started_at": "2026-07-23T00:00:01Z",
        "network_name": "ci-net",
        "volume_names": ["ci-cache"],
    }


def sandbox_stats_response(sandbox_id: str) -> dict[str, Any]:
    return {
        "id": sandbox_id,
        "short_id": "sandboxlocal",
        "name": "ci-box",
        "status": "running",
        "pid": 1234,
        "cpus": 2,
        "cpu_percent": 1.5,
        "cpu_percent_scaled": 3.0,
        "memory_bytes": 1024,
        "memory_limit_bytes": 2048,
        "memory_percent": 50.0,
        "network_rx_bytes": 10,
        "network_tx_bytes": 20,
        "block_read_bytes": 30,
        "block_write_bytes": 40,
    }


def filesystem_snapshot_response(snapshot_id: str) -> dict[str, Any]:
    return {
        "id": snapshot_id,
        "name": "CI base",
        "source_box_id": "sandbox-local-1",
        "image": "alpine:3.20",
        "vcpus": 2,
        "memory_mb": 512,
        "volumes": ["/cache"],
        "command": ["sh"],
        "port_map": ["8080:80"],
        "labels": {"purpose": "ci"},
        "network_mode": "tsi",
        "size_bytes": 4096,
        "created_at": "2026-07-23T00:00:00Z",
        "description": "Warm CI base",
    }


class SdkTests(unittest.TestCase):
    def test_sync_and_async_public_surfaces_stay_aligned(self) -> None:
        pairs = (
            (A3SBoxClient, A3SAsyncBoxClient),
            (Sandbox, AsyncSandbox),
            (Commands, AsyncCommands),
            (Filesystem, AsyncFilesystem),
        )

        def methods(cls: type[object]) -> dict[str, object]:
            return {
                name: value
                for name, value in cls.__dict__.items()
                if not name.startswith("_")
                and name != "id"
                and (
                    inspect.isfunction(value)
                    or isinstance(value, (classmethod, staticmethod))
                )
            }

        def parameter_shape(value: object) -> tuple[tuple[object, ...], ...]:
            if isinstance(value, (classmethod, staticmethod)):
                value = value.__func__
            parameters = list(inspect.signature(value).parameters.values())[1:]
            return tuple(
                (parameter.name, parameter.kind, parameter.default)
                for parameter in parameters
            )

        for sync_type, async_type in pairs:
            with self.subTest(sync=sync_type.__name__, async_=async_type.__name__):
                sync_methods = methods(sync_type)
                async_methods = methods(async_type)
                self.assertEqual(sync_methods.keys(), async_methods.keys())
                for name, sync_method in sync_methods.items():
                    self.assertEqual(
                        parameter_shape(sync_method),
                        parameter_shape(async_methods[name]),
                        name,
                    )

    def test_missing_binary_uses_the_cross_sdk_error_code(self) -> None:
        error = A3SBoxNotInstalledError("/missing/a3s-box")
        self.assertEqual(error.code, "binary_not_found")

    def test_malformed_typed_result_is_a_protocol_error(self) -> None:
        class MalformedRuntime(FakeRuntime):
            def request(
                self,
                request: Mapping[str, object],
            ) -> dict[str, Any]:
                if request["operation"] == "sdk_capabilities":
                    return response_for(request)
                if request["operation"] == "image_list":
                    return {
                        "images": [
                            {
                                "reference": 42,
                                "digest": "sha256:bad",
                                "size_bytes": 1,
                                "pulled_at": "2026-07-28T00:00:00Z",
                                "last_used": "2026-07-28T00:00:00Z",
                                "path": "/tmp/image",
                            }
                        ]
                    }
                return super().request(request)

        with self.assertRaises(A3SBoxError) as raised:
            A3SBoxClient(MalformedRuntime()).list_images()

        self.assertEqual(raised.exception.code, "bridge_protocol_error")

    def test_public_api_exercises_every_rust_bridge_operation(self) -> None:
        runtime = FakeRuntime()
        client = A3SBoxClient(runtime)

        client.runtime_diagnostics()
        client.runtime_disk_usage()
        client.image(".").tag("local/test:latest").build()
        client.pull_image("alpine:3.20")
        client.get_image("alpine:3.20")
        client.list_images()
        client.inspect_image("alpine:3.20")
        client.image_history("alpine:3.20")
        client.tag_image("alpine:3.20", "local/alpine:latest")
        client.push_image("local/alpine:latest", "registry/alpine:latest")
        client.remove_image("local/alpine:latest")
        client.evict_images()

        client.volume("cache").create()
        client.get_volume("cache")
        client.list_volumes()
        client.remove_volume("cache", force=True)
        client.prune_volumes()
        client.network("ci-net").create()
        client.get_network("ci-net")
        client.list_networks()
        client.remove_network("ci-net")
        client.prune_networks()
        client.list_sandboxes()
        client.get_sandbox("sandbox-local-1")

        sandbox = client.sandbox().start()
        sandbox.stop()
        sandbox.restart(operation_id="coverage-restart")
        sandbox.pause()
        sandbox.resume()
        sandbox.logs(tail=10)
        sandbox.stats()
        sandbox.processes()
        sandbox.runtime_stats()
        sandbox.events(after_sequence=4, limit=16, wait_timeout_ms=25)
        sandbox.update_resources(
            ExecutionResourceUpdate(cpu_shares=512),
            operation_id="coverage-resources",
        )
        sandbox.create_filesystem_snapshot("snap-1")
        sandbox.commands.run(["true"])
        sandbox.files.write("/tmp/value", "value")
        sandbox.files.read("/tmp/value")
        sandbox.files.stat("/tmp/value")
        sandbox.files.export("/tmp/value")
        sandbox.files.list("/tmp", depth=1)
        sandbox.files.make_dir("/tmp/dir")
        sandbox.files.rename("/tmp/dir", "/tmp/moved")
        sandbox.files.remove("/tmp/moved")
        sandbox.kill()

        Sandbox.connect("sandbox-remove", runtime=runtime).remove()
        client.list_filesystem_snapshots()
        client.get_filesystem_snapshot("snap-1")
        Sandbox.filesystem_snapshot_size("snap-1", runtime=runtime)
        Sandbox.delete_filesystem_snapshot("snap-1", runtime=runtime)
        client.capabilities()

        called = {str(request["operation"]) for request in runtime.requests}
        expected = set(SUPPORTED_BRIDGE_OPERATIONS) - {"sdk_capabilities"}
        self.assertEqual(called, expected)

    def test_client_fails_closed_before_an_unsupported_runtime_mutation(self) -> None:
        operations = tuple(
            operation
            for operation in SUPPORTED_BRIDGE_OPERATIONS
            if operation != "image_list"
        )
        runtime = CapabilityRuntime(operations=operations)

        with self.assertRaises(A3SBoxError) as raised:
            A3SBoxClient(runtime).list_images()

        self.assertEqual(raised.exception.code, "unavailable")
        self.assertIn("image_list", str(raised.exception))
        self.assertEqual(
            [request["operation"] for request in runtime.requests],
            ["sdk_capabilities"],
        )

    def test_protocol_mismatch_is_a_stable_sdk_error(self) -> None:
        envelope = json.dumps(
            {
                "protocol_version": BRIDGE_PROTOCOL_VERSION + 1,
                "ok": True,
                "result": {},
            }
        )

        with self.assertRaises(A3SBoxError) as raised:
            _decode_response(envelope, "", 0)

        self.assertEqual(raised.exception.code, "bridge_protocol_error")

    def test_exports_native_local_clients(self) -> None:
        self.assertIs(a3s_box.Sandbox, Sandbox)
        self.assertIs(a3s_box.AsyncSandbox, AsyncSandbox)
        self.assertEqual(a3s_box.DEFAULT_IMAGE, "alpine:3.20")
        self.assertEqual(a3s_box.MAX_ARTIFACT_BYTES, 8 * 1024 * 1024)
        for exported_type in (
            "Artifact",
            "FilesystemSnapshotSummary",
            "RuntimeDiagnostics",
            "RuntimeDiskUsage",
            "RuntimeVirtualization",
            "SandboxLogEntry",
            "SandboxStats",
            "SandboxSummary",
        ):
            self.assertTrue(hasattr(a3s_box, exported_type), exported_type)

    def test_operation_inventory_matches_rust_contract(self) -> None:
        contract_root = Path(__file__).resolve().parents[2]
        inventory_path = contract_root / "bridge-operations.json"
        protocol_path = contract_root / "bridge-protocol.json"
        if not inventory_path.exists() or not protocol_path.exists():
            self.skipTest(
                "repository bridge contract is not included in the Python package"
            )
        inventory = json.loads(
            inventory_path.read_text()
        )
        self.assertEqual(list(SUPPORTED_BRIDGE_OPERATIONS), inventory)
        protocol = json.loads(protocol_path.read_text())
        self.assertEqual(BRIDGE_PROTOCOL_VERSION, protocol["version"])

    def test_sync_sandbox_uses_local_runtime_surface(self) -> None:
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

    def test_artifact_export_hashes_and_exclusively_writes_host_file(self) -> None:
        runtime = FakeRuntime()
        sandbox = Sandbox.create(runtime=runtime)

        with TemporaryDirectory() as temporary:
            destination = Path(temporary) / "note.txt"
            artifact = sandbox.files.export(
                "/workspace/notes.txt",
                max_bytes=5,
                destination=destination,
                user="1000",
            )

            self.assertEqual(artifact.path, "/workspace/notes.txt")
            self.assertEqual(artifact.data, b"hello")
            self.assertEqual(artifact.size, 5)
            self.assertEqual(
                artifact.sha256,
                "2cf24dba5fb0a30e26e83b2ac5b9e29e"
                "1b161e5c1fa7425e73043362938b9824",
            )
            self.assertEqual(artifact.host_path, str(destination))
            self.assertEqual(destination.read_bytes(), b"hello")

            destination.write_bytes(b"keep")
            with self.assertRaises(A3SBoxError) as raised:
                sandbox.files.export(
                    "/workspace/notes.txt",
                    destination=destination,
                )
            self.assertEqual(raised.exception.code, "runtime_error")
            self.assertEqual(destination.read_bytes(), b"keep")

        artifact_requests = [
            request
            for request in runtime.requests
            if request["operation"] in {"filesystem_stat", "file_read"}
        ]
        self.assertEqual(artifact_requests[0]["user"], "1000")
        self.assertEqual(artifact_requests[1]["user"], "1000")
        self.assertEqual(artifact_requests[1]["max_bytes"], 5)

    def test_artifact_export_rejects_invalid_limits_and_non_files(self) -> None:
        runtime = ArtifactRuntime(entry_type="directory", stat_size=0)
        sandbox = Sandbox.create(runtime=runtime)

        for max_bytes in (0, MAX_ARTIFACT_BYTES + 1):
            with self.assertRaises(A3SBoxError) as raised:
                sandbox.files.export("/workspace/output", max_bytes=max_bytes)
            self.assertEqual(raised.exception.code, "invalid_request")
        with self.assertRaises(A3SBoxError) as raised:
            sandbox.files.export("  ")
        self.assertEqual(raised.exception.code, "invalid_request")
        with self.assertRaises(A3SBoxError) as raised:
            sandbox.files.export("/workspace/output", destination="  ")
        self.assertEqual(raised.exception.code, "invalid_request")

        self.assertFalse(
            any(
                request["operation"] == "filesystem_stat"
                for request in runtime.requests
            )
        )
        with self.assertRaises(A3SBoxError) as raised:
            sandbox.files.export("/workspace/output")
        self.assertEqual(raised.exception.code, "invalid_request")
        self.assertFalse(
            any(request["operation"] == "file_read" for request in runtime.requests)
        )

        oversized = ArtifactRuntime(stat_size=6)
        oversized_sandbox = Sandbox.create(runtime=oversized)
        with self.assertRaises(A3SBoxError) as raised:
            oversized_sandbox.files.export("/workspace/output", max_bytes=5)
        self.assertEqual(raised.exception.code, "invalid_request")
        self.assertFalse(
            any(request["operation"] == "file_read" for request in oversized.requests)
        )

    def test_artifact_export_rejects_malformed_and_racing_reads(self) -> None:
        malformed = ArtifactRuntime(declared_size=6)
        sandbox = Sandbox.create(runtime=malformed)
        with self.assertRaises(A3SBoxError) as raised:
            sandbox.files.export("/workspace/output")
        self.assertEqual(raised.exception.code, "bridge_protocol_error")

    def test_malformed_runtime_control_results_fail_closed(self) -> None:
        cases = (
            (
                "sandbox_processes",
                {
                    "execution_id": "sandbox-local-1",
                    "generation": 1,
                    "processes": [
                        {"process_id": "init", "pid": 1, "terminal": False},
                        {"process_id": "init", "pid": 2, "terminal": False},
                    ],
                },
                lambda sandbox: sandbox.processes(),
            ),
            (
                "sandbox_runtime_stats",
                {
                    "execution_id": "sandbox-local-1",
                    "generation": 1,
                    "timestamp_unix_ns": "0",
                    "cpu": {
                        "usage_ns": 1,
                        "user_ns": 1,
                        "system_ns": 0,
                        "throttled_ns": 0,
                    },
                    "memory": {
                        "usage_bytes": 0,
                        "limit_bytes": None,
                        "peak_bytes": None,
                    },
                    "process_count": 1,
                    "metrics": {},
                },
                lambda sandbox: sandbox.runtime_stats(),
            ),
            (
                "sandbox_events",
                {
                    "execution_id": "sandbox-local-1",
                    "generation": 1,
                    "events": [
                        {
                            "sequence": 7,
                            "timestamp_unix_ns": "1",
                            "process_id": None,
                            "kind": "runtime-warning",
                            "attributes": {},
                        }
                    ],
                    "next_sequence": 7,
                },
                lambda sandbox: sandbox.events(after_sequence=7),
            ),
            (
                "sandbox_update_resources",
                {
                    "sandbox_id": "sandbox-local-1",
                    "generation": 2,
                    "state": "running",
                    "isolation": "microvm",
                },
                lambda sandbox: sandbox.update_resources(
                    ExecutionResourceUpdate(cpu_shares=512),
                    operation_id="malformed-update",
                ),
            ),
        )

        for operation, result, invoke in cases:
            class MalformedRuntime(FakeRuntime):
                def request(
                    self,
                    request: Mapping[str, object],
                ) -> dict[str, Any]:
                    if request["operation"] == operation:
                        self.requests.append(dict(request))
                        return result
                    return super().request(request)

            with self.subTest(operation=operation):
                sandbox = Sandbox.create(runtime=MalformedRuntime())
                with self.assertRaises(A3SBoxError) as raised:
                    invoke(sandbox)
                self.assertEqual(
                    raised.exception.code,
                    "bridge_protocol_error",
                )

        racing = ArtifactRuntime(data=b"four", stat_size=5)
        sandbox = Sandbox.create(runtime=racing)
        with self.assertRaises(A3SBoxError) as raised:
            sandbox.files.export("/workspace/output")
        self.assertEqual(raised.exception.code, "bridge_protocol_error")

    def test_lifecycle_logs_and_stats_preserve_request_identity(self) -> None:
        runtime = FakeRuntime()
        sandbox = Sandbox.create(runtime=runtime)

        sandbox.stop()
        self.assertEqual(sandbox.state, "stopped")
        with self.assertRaisesRegex(ValueError, "operation_id cannot be empty"):
            sandbox.restart(operation_id="")
        with self.assertRaisesRegex(ValueError, "tail must be between"):
            sandbox.logs(tail=0)
        sandbox.restart(operation_id="python-restart-1", stop_timeout=7)
        self.assertEqual(sandbox.generation, 2)
        self.assertEqual(sandbox.state, "running")
        logs = sandbox.logs(tail=1)
        stats = sandbox.stats()
        processes = sandbox.processes()
        runtime_stats = sandbox.runtime_stats()
        events = sandbox.events(
            after_sequence=7,
            limit=8,
            wait_timeout_ms=50,
        )
        sandbox.update_resources(
            ExecutionResourceUpdate(
                cpu_shares=512,
                cpuset_cpus="0-1",
            ),
            operation_id="python-resources-1",
        )
        sandbox.stop()
        sandbox.remove()
        sandbox.kill()

        self.assertEqual(logs[0].message, "sdk-log\n")
        self.assertEqual(logs[0].stream, "stdout")
        self.assertEqual(stats.memory_percent, 50.0)
        self.assertEqual(processes.processes[0].process_id, "init")
        self.assertEqual(runtime_stats.timestamp_unix_ns, 1_700_000_000_000_000_123)
        self.assertEqual(runtime_stats.cpu.usage_ns, 300)
        self.assertEqual(runtime_stats.metrics["io.read_bytes"], 64)
        self.assertEqual(events.events[0].kind, "process-exited")
        self.assertEqual(events.events[0].timestamp_unix_ns, 1_700_000_000_000_000_124)
        self.assertEqual(events.next_sequence, 8)
        self.assertEqual(sandbox.state, "removed")
        self.assertEqual(
            [request["operation"] for request in runtime.requests],
            [
                "sandbox_create",
                "sandbox_stop",
                "sandbox_restart",
                "sandbox_logs",
                "sandbox_stats",
                "sandbox_processes",
                "sandbox_runtime_stats",
                "sandbox_events",
                "sandbox_update_resources",
                "sandbox_stop",
                "sandbox_remove",
            ],
        )
        restart = runtime.requests[2]
        self.assertEqual(restart["generation"], 1)
        self.assertEqual(restart["operation_id"], "python-restart-1")
        self.assertEqual(restart["stop_timeout_seconds"], 7)
        self.assertEqual(runtime.requests[3]["generation"], 2)
        self.assertEqual(runtime.requests[3]["tail"], 1)
        self.assertEqual(runtime.requests[7]["after_sequence"], 7)
        self.assertEqual(runtime.requests[7]["limit"], 8)
        self.assertEqual(runtime.requests[7]["wait_timeout_ms"], 50)
        self.assertEqual(
            runtime.requests[8]["resources"],
            {"cpu_shares": 512, "cpuset_cpus": "0-1"},
        )
        self.assertEqual(
            runtime.requests[8]["operation_id"],
            "python-resources-1",
        )
        self.assertEqual(runtime.requests[-1]["generation"], 2)

    def test_runtime_control_validates_before_bridge_mutation(self) -> None:
        runtime = FakeRuntime()
        sandbox = Sandbox.create(runtime=runtime)
        baseline = len(runtime.requests)

        for arguments in (
            {"limit": 0},
            {"limit": 4_097},
            {"after_sequence": -1},
            {"wait_timeout_ms": -1},
        ):
            with self.subTest(arguments=arguments):
                with self.assertRaises(ValueError):
                    sandbox.events(**arguments)
        for update in (
            ExecutionResourceUpdate(),
            ExecutionResourceUpdate(cpu_shares=1),
            ExecutionResourceUpdate(pids_limit=0),
            ExecutionResourceUpdate(memory_swap=-2),
            ExecutionResourceUpdate(cpuset_cpus="2-1"),
        ):
            with self.subTest(update=update):
                with self.assertRaises(ValueError):
                    sandbox.update_resources(update)
        with self.assertRaisesRegex(ValueError, "operation_id cannot be empty"):
            sandbox.update_resources(
                ExecutionResourceUpdate(cpu_period=100_000),
                operation_id=" ",
            )

        self.assertEqual(len(runtime.requests), baseline)

    def test_event_stream_is_backpressured_paused_and_generation_fenced(
        self,
    ) -> None:
        class StreamRuntime(FakeRuntime):
            def request(
                self,
                request: Mapping[str, object],
            ) -> dict[str, Any]:
                if request["operation"] == "sandbox_events":
                    self.requests.append(dict(request))
                    return event_stream_response(request)
                return super().request(request)

        runtime = StreamRuntime()
        sandbox = Sandbox.create(runtime=runtime)
        stream = sandbox.stream_events(batch_size=2, wait_timeout_ms=1)
        events = [next(stream), next(stream), next(stream)]
        self.assertEqual([event.sequence for event in events], [2, 5, 9])
        polls = [
            request
            for request in runtime.requests
            if request["operation"] == "sandbox_events"
        ]
        self.assertEqual(
            [request["after_sequence"] for request in polls],
            [0, 5],
        )
        self.assertTrue(all(request["generation"] == 1 for request in polls))

        sandbox.pause()
        paused = sandbox.events(
            after_sequence=9,
            limit=1,
            wait_timeout_ms=1,
        )
        self.assertEqual(paused.events, ())
        sandbox.resume()

        fenced = sandbox.stream_events(batch_size=1, wait_timeout_ms=1)
        self.assertEqual(next(fenced).sequence, 2)
        sandbox.restart(operation_id="python-event-stream-restart")
        with self.assertRaises(A3SBoxError) as raised:
            next(fenced)
        self.assertEqual(raised.exception.code, "conflict")

        cancel = threading.Event()
        cancel.set()
        self.assertEqual(list(sandbox.stream_events(cancel_event=cancel)), [])

    def test_event_stream_validates_options_before_runtime_access(self) -> None:
        runtime = FakeRuntime()
        sandbox = Sandbox.create(runtime=runtime)
        baseline = len(runtime.requests)
        for arguments in (
            {"batch_size": 0},
            {"batch_size": 4_097},
            {"wait_timeout_ms": 0},
            {"after_sequence": -1},
        ):
            with self.subTest(arguments=arguments):
                with self.assertRaises(ValueError):
                    sandbox.stream_events(**arguments)
        self.assertEqual(len(runtime.requests), baseline)

    def test_management_inspection_and_snapshot_queries_are_typed(self) -> None:
        runtime = FakeRuntime()
        client = A3SBoxClient(runtime)

        sandboxes = client.list_sandboxes(all=False)
        sandbox = client.get_sandbox("sandbox-local-1")
        diagnostics = client.runtime_diagnostics()
        disk = client.runtime_disk_usage()
        snapshots = client.list_filesystem_snapshots()
        snapshot = client.get_filesystem_snapshot("ci-base")

        self.assertEqual(sandboxes[0].name, "ci-box")
        self.assertEqual(sandbox.id, "sandbox-local-1")
        self.assertEqual(diagnostics.virtualization.backend, "hvf")
        self.assertEqual(disk.total_bytes, 28)
        self.assertEqual(snapshots[0].source_sandbox_id, "sandbox-local-1")
        self.assertEqual(snapshot.description, "Warm CI base")
        self.assertEqual(runtime.requests[0], {
            "operation": "sandbox_list",
            "all": False,
        })
        self.assertEqual(
            [request["operation"] for request in runtime.requests],
            [
                "sandbox_list",
                "sandbox_get",
                "runtime_diagnostics",
                "runtime_disk_usage",
                "filesystem_snapshot_list",
                "filesystem_snapshot_get",
            ],
        )

    def test_fluent_programmable_cicd_builders_share_the_local_sandbox(self) -> None:
        runtime = FakeRuntime()
        client = A3SBoxClient(runtime)

        image = (
            client.image("./ci")
            .dockerfile("Dockerfile.ci")
            .tag("local/ci-base:latest")
            .build_arg("NODE_VERSION", "24")
            .quiet(False)
            .platform("linux/arm64")
            .target("test")
            .no_cache()
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
            .timeout(90)
            .env("CI", "true")
            .metadata("job", "test")
            .name("python-test")
            .cpus(4)
            .memory_mb(4096)
            .isolation("sandbox")
            .filesystem_snapshot("base-snapshot")
            .workspace("/workspace")
            .entrypoint("/usr/bin/env", "sh")
            .command("-c", "python -m pytest && sleep 3600")
            .mount_named(volume.name, "/cache", read_only=True)
            .mount_bind("./src", "/workspace/src")
            .tmpfs("/scratch", size_bytes=1024, read_only=True)
            .network(network.name)
            .publish_tcp(8080, 80)
            .dns_server("1.1.1.1")
            .host_alias("registry.local", "10.89.55.2")
            .workdir("/workspace/src")
            .user("1000:1000")
            .hostname("python-ci")
            .read_only()
            .persistent()
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
        self.assertFalse(runtime.requests[0]["quiet"])
        self.assertEqual(runtime.requests[0]["target"], "test")
        self.assertTrue(runtime.requests[0]["no_cache"])
        create = runtime.requests[3]
        self.assertEqual(create["operation"], "sandbox_create")
        self.assertEqual(create["timeout_seconds"], 90)
        self.assertEqual(create["env"], {"CI": "true"})
        self.assertEqual(create["labels"], {"job": "test"})
        self.assertEqual(create["name"], "python-test")
        self.assertEqual(create["cpus"], 4)
        self.assertEqual(create["memory_mb"], 4096)
        self.assertEqual(create["isolation"], "sandbox")
        self.assertEqual(create["filesystem_snapshot_id"], "base-snapshot")
        self.assertEqual(create["workspace"], "/workspace")
        self.assertEqual(create["entrypoint"], ["/usr/bin/env", "sh"])
        self.assertEqual(
            create["command"], ["-c", "python -m pytest && sleep 3600"]
        )
        self.assertEqual(create["workdir"], "/workspace/src")
        self.assertEqual(create["user"], "1000:1000")
        self.assertEqual(create["hostname"], "python-ci")
        self.assertEqual(
            create["mounts"],
            [
                {
                    "kind": "named",
                    "name": "ci-cache",
                    "target": "/cache",
                    "read_only": True,
                },
                {
                    "kind": "bind",
                    "source": "./src",
                    "target": "/workspace/src",
                    "read_only": False,
                },
            ],
        )
        self.assertEqual(
            create["tmpfs"],
            [{"target": "/scratch", "size_bytes": 1024, "read_only": True}],
        )
        self.assertEqual(create["network"], {"mode": "bridge", "name": "ci-net"})
        self.assertEqual(
            create["ports"],
            [{"host_port": 8080, "guest_port": 80}],
        )
        self.assertEqual(create["dns"], ["1.1.1.1"])
        self.assertEqual(
            create["host_aliases"], {"registry.local": "10.89.55.2"}
        )
        self.assertTrue(create["read_only"])
        self.assertTrue(create["persistent"])
        self.assertFalse(create["auto_remove"])
        command = runtime.requests[4]
        self.assertEqual(command["argv"], ["python", "-"])
        self.assertEqual(
            base64.b64decode(str(command["stdin_base64"])),
            b"print(6 * 7)\n",
        )

    def test_initial_process_configuration_validates_before_runtime_access(
        self,
    ) -> None:
        runtime = FakeRuntime()
        client = A3SBoxClient(runtime)

        with self.assertRaisesRegex(ValueError, "initial command cannot be empty"):
            client.sandbox("alpine:3.20").command().start()
        with self.assertRaisesRegex(ValueError, "initial entrypoint.*blank"):
            client.sandbox("alpine:3.20").entrypoint(" ").start()
        with self.assertRaisesRegex(ValueError, "sequence of strings"):
            Sandbox.create(
                "alpine:3.20",
                command="echo split-into-characters",  # type: ignore[arg-type]
                runtime=runtime,
            )

        self.assertEqual(runtime.requests, [])

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

        self.assertEqual(sandbox.isolation, "sandbox")
        self.assertEqual(runtime.requests[0]["isolation"], "sandbox")

    def test_local_binary_resolution_uses_path_without_override(self) -> None:
        with (
            patch.dict(os.environ, {}, clear=True),
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

class AsyncSdkTests(unittest.IsolatedAsyncioTestCase):
    async def test_async_client_verifies_capabilities_once_under_concurrency(
        self,
    ) -> None:
        runtime = AsyncCapabilityRuntime()
        client = A3SAsyncBoxClient(runtime)

        images, volumes = await asyncio.gather(
            client.list_images(),
            client.list_volumes(),
        )

        self.assertEqual(images, [])
        self.assertEqual(volumes, [])
        operations = [request["operation"] for request in runtime.requests]
        self.assertEqual(operations.count("sdk_capabilities"), 1)
        self.assertEqual(
            sorted(operation for operation in operations if operation != "sdk_capabilities"),
            ["image_list", "volume_list"],
        )

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

    async def test_async_artifact_export_matches_sync_contract(self) -> None:
        runtime = AsyncArtifactRuntime()
        sandbox = await AsyncSandbox.create(runtime=runtime)

        with TemporaryDirectory() as temporary:
            destination = Path(temporary) / "note.txt"
            artifact = await sandbox.files.export(
                "/workspace/notes.txt",
                max_bytes=5,
                destination=destination,
                user="1000",
            )

            self.assertEqual(artifact.data, b"hello")
            self.assertEqual(artifact.size, 5)
            self.assertEqual(
                artifact.sha256,
                "2cf24dba5fb0a30e26e83b2ac5b9e29e"
                "1b161e5c1fa7425e73043362938b9824",
            )
            self.assertEqual(artifact.host_path, str(destination))
            self.assertEqual(destination.read_bytes(), b"hello")

        operations = [request["operation"] for request in runtime.requests]
        self.assertEqual(
            operations[-2:],
            ["filesystem_stat", "file_read"],
        )

    async def test_async_artifact_export_rejects_stat_read_races(self) -> None:
        runtime = AsyncArtifactRuntime(data=b"four", stat_size=5)
        sandbox = await AsyncSandbox.create(runtime=runtime)

        with self.assertRaises(A3SBoxError) as raised:
            await sandbox.files.export("/workspace/output")
        self.assertEqual(raised.exception.code, "bridge_protocol_error")

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

    async def test_async_lifecycle_and_management_match_sync_surface(self) -> None:
        runtime = AsyncFakeRuntime()
        client = A3SAsyncBoxClient(runtime)
        sandbox = await AsyncSandbox.create(runtime=runtime)

        await sandbox.stop()
        await sandbox.restart(
            operation_id="python-async-restart-1",
            stop_timeout=9,
        )
        logs = await sandbox.logs(tail=1)
        stats = await sandbox.stats()
        processes = await sandbox.processes()
        runtime_stats = await sandbox.runtime_stats()
        events = await sandbox.events(
            after_sequence=10,
            limit=32,
            wait_timeout_ms=75,
        )
        await sandbox.update_resources(
            ExecutionResourceUpdate(pids_limit=64),
            operation_id="python-async-resources-1",
        )
        sandboxes = await client.list_sandboxes(all=False)
        snapshot = await client.get_filesystem_snapshot("ci-async")
        diagnostics = await client.runtime_diagnostics()
        disk = await client.runtime_disk_usage()
        await sandbox.stop()
        await sandbox.remove()

        self.assertEqual(logs[0].message, "sdk-log\n")
        self.assertEqual(stats.block_write_bytes, 40)
        self.assertEqual(processes.processes[1].terminal, True)
        self.assertEqual(runtime_stats.timestamp_unix_ns, 1_700_000_000_000_000_123)
        self.assertEqual(runtime_stats.memory.limit_bytes, 2048)
        self.assertEqual(events.next_sequence, 11)
        self.assertEqual(sandboxes[0].id, "sandbox-local-1")
        self.assertEqual(snapshot.id, "ci-async")
        self.assertEqual(diagnostics.sdk_version, "3.2.2")
        self.assertEqual(disk.snapshots_bytes, 4)
        restart = next(
            request
            for request in runtime.requests
            if request["operation"] == "sandbox_restart"
        )
        self.assertEqual(restart["operation_id"], "python-async-restart-1")
        self.assertEqual(restart["stop_timeout_seconds"], 9)
        resource_update = next(
            request
            for request in runtime.requests
            if request["operation"] == "sandbox_update_resources"
        )
        self.assertEqual(resource_update["operation_id"], "python-async-resources-1")
        self.assertEqual(resource_update["resources"], {"pids_limit": 64})

    async def test_async_event_stream_is_backpressured_and_generation_fenced(
        self,
    ) -> None:
        class AsyncStreamRuntime(AsyncFakeRuntime):
            async def request(
                self,
                request: Mapping[str, object],
            ) -> dict[str, Any]:
                if request["operation"] == "sandbox_events":
                    self.requests.append(dict(request))
                    await asyncio.sleep(0)
                    return event_stream_response(request)
                return await super().request(request)

        runtime = AsyncStreamRuntime()
        sandbox = await AsyncSandbox.create(runtime=runtime)
        stream = sandbox.stream_events(batch_size=2, wait_timeout_ms=1)
        events = [await anext(stream), await anext(stream), await anext(stream)]
        self.assertEqual([event.sequence for event in events], [2, 5, 9])
        polls = [
            request
            for request in runtime.requests
            if request["operation"] == "sandbox_events"
        ]
        self.assertEqual(
            [request["after_sequence"] for request in polls],
            [0, 5],
        )

        fenced = sandbox.stream_events(batch_size=1, wait_timeout_ms=1)
        self.assertEqual((await anext(fenced)).sequence, 2)
        await sandbox.restart(operation_id="python-async-event-stream-restart")
        with self.assertRaises(A3SBoxError) as raised:
            await anext(fenced)
        self.assertEqual(raised.exception.code, "conflict")
        await stream.aclose()
        await fenced.aclose()

    async def test_async_event_stream_cancellation_reaches_runtime(self) -> None:
        class BlockingRuntime(AsyncFakeRuntime):
            def __init__(self) -> None:
                super().__init__()
                self.started = asyncio.Event()
                self.cancelled = False

            async def request(
                self,
                request: Mapping[str, object],
            ) -> dict[str, Any]:
                if request["operation"] != "sandbox_events":
                    return await super().request(request)
                self.requests.append(dict(request))
                self.started.set()
                try:
                    await asyncio.Future()
                except asyncio.CancelledError:
                    self.cancelled = True
                    raise
                raise AssertionError("unreachable event stream response")

        runtime = BlockingRuntime()
        sandbox = await AsyncSandbox.create(runtime=runtime)
        stream = sandbox.stream_events(wait_timeout_ms=1)
        pending = asyncio.create_task(anext(stream))
        await asyncio.wait_for(runtime.started.wait(), timeout=1)
        pending.cancel()
        with self.assertRaises(asyncio.CancelledError):
            await pending
        self.assertTrue(runtime.cancelled)
        await stream.aclose()

    async def test_async_local_runtime_cancellation_reaps_bridge_process(
        self,
    ) -> None:
        class BlockingProcess:
            def __init__(self) -> None:
                self.returncode: int | None = None
                self.started = asyncio.Event()
                self.killed = False
                self.waited = False

            async def communicate(self, _payload: bytes) -> tuple[bytes, bytes]:
                self.started.set()
                await asyncio.Future()
                raise AssertionError("unreachable bridge response")

            def kill(self) -> None:
                self.killed = True
                self.returncode = -9

            async def wait(self) -> int:
                self.waited = True
                return self.returncode or 0

        process = BlockingProcess()
        with TemporaryDirectory() as temporary:
            binary = Path(temporary) / "a3s-box"
            binary.write_bytes(b"fixture")
            runtime = A3SAsyncLocalRuntime(binary_path=str(binary))
            with patch(
                "a3s_box.runtime.asyncio.create_subprocess_exec",
                return_value=process,
            ):
                request = asyncio.create_task(
                    runtime.request({"operation": "sandbox_events"})
                )
                await asyncio.wait_for(process.started.wait(), timeout=1)
                request.cancel()
                with self.assertRaises(asyncio.CancelledError):
                    await request
        self.assertTrue(process.killed)
        self.assertTrue(process.waited)

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
        self.assertEqual(
            capabilities.protocol_version,
            BRIDGE_PROTOCOL_VERSION,
        )

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
