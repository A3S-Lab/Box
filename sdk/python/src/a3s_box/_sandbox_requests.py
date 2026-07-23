"""Validated bridge request builders for the local Sandbox facade."""

from __future__ import annotations

import base64
from collections.abc import Mapping, Sequence
from typing import Protocol

from .models import (
    PortMapping,
    SandboxNetwork,
    TmpfsMount,
    VolumeMount,
)

DEFAULT_IMAGE = "alpine:3.20"


class SandboxIdentity(Protocol):
    sandbox_id: str
    generation: int


def create_request(
    template: str | None,
    timeout: int,
    envs: Mapping[str, str] | None,
    metadata: Mapping[str, str] | None,
    name: str | None,
    cpus: int | None,
    memory_mb: int | None,
    isolation: str,
    filesystem_snapshot_id: str | None,
    workspace: str | None,
    workdir: str | None,
    user: str | None,
    hostname: str | None,
    mounts: Sequence[VolumeMount] | None,
    tmpfs: Sequence[TmpfsMount] | None,
    network: SandboxNetwork | None,
    ports: Sequence[PortMapping] | None,
    dns: Sequence[str] | None,
    host_aliases: Mapping[str, str] | None,
    read_only: bool,
    persistent: bool,
    auto_remove: bool,
) -> dict[str, object]:
    if timeout <= 0:
        raise ValueError("timeout must be greater than zero")
    request: dict[str, object] = {
        "operation": "sandbox_create",
        "image": template or DEFAULT_IMAGE,
        "timeout_seconds": timeout,
        "env": dict(envs or {}),
        "labels": dict(metadata or {}),
        "isolation": isolation,
        "mounts": [mount.bridge_value() for mount in mounts or ()],
        "tmpfs": [mount.bridge_value() for mount in tmpfs or ()],
        "network": (network or SandboxNetwork.tsi()).bridge_value(),
        "ports": [port.bridge_value() for port in ports or ()],
        "dns": list(dns or ()),
        "host_aliases": dict(host_aliases or {}),
        "read_only": read_only,
        "persistent": persistent,
        "auto_remove": auto_remove,
    }
    if name is not None:
        request["name"] = name
    if cpus is not None:
        request["cpus"] = cpus
    if memory_mb is not None:
        request["memory_mb"] = memory_mb
    if filesystem_snapshot_id is not None:
        request["filesystem_snapshot_id"] = filesystem_snapshot_id
    if workspace is not None:
        request["workspace"] = workspace
    if workdir is not None:
        request["workdir"] = workdir
    if user is not None:
        request["user"] = user
    if hostname is not None:
        request["hostname"] = hostname
    return request


def command_request(
    sandbox: SandboxIdentity,
    command: str | Sequence[str],
    timeout: float | None,
    envs: Mapping[str, str] | None,
    cwd: str | None,
    user: str | None,
    stdin: str | bytes | None,
) -> dict[str, object]:
    argv = ["/bin/sh", "-lc", command] if isinstance(command, str) else list(command)
    if not argv:
        raise ValueError("command cannot be empty")
    request: dict[str, object] = {
        "operation": "command_run",
        "sandbox_id": sandbox.sandbox_id,
        "generation": sandbox.generation,
        "argv": argv,
        "env": dict(envs or {}),
    }
    if timeout is not None:
        if timeout <= 0:
            raise ValueError("timeout must be greater than zero")
        request["timeout_ms"] = int(timeout * 1000)
    if cwd is not None:
        request["cwd"] = cwd
    if user is not None:
        request["user"] = user
    if stdin is not None:
        raw = stdin.encode() if isinstance(stdin, str) else stdin
        request["stdin_base64"] = base64.b64encode(raw).decode()
    return request
