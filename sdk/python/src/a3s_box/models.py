"""Typed values returned by the native A3S Box SDK."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Literal


@dataclass(frozen=True, slots=True)
class CommandResult:
    stdout: str
    stderr: str
    exit_code: int
    truncated: bool = False


@dataclass(frozen=True, slots=True)
class WriteInfo:
    path: str
    size: int


@dataclass(frozen=True, slots=True)
class FilesystemSnapshotInfo:
    snapshot_id: str
    size_bytes: int
    state: str
    generation: int


@dataclass(frozen=True, slots=True)
class BuildImageInfo:
    reference: str
    digest: str
    size_bytes: int
    layer_count: int


@dataclass(frozen=True, slots=True)
class ImageInfo:
    reference: str
    digest: str
    size_bytes: int
    pulled_at: str
    last_used: str
    path: str


@dataclass(frozen=True, slots=True)
class RegistryCredentials:
    username: str
    password: str = field(repr=False)

    def bridge_value(self) -> dict[str, str]:
        return {
            "username": self.username,
            "password": self.password,
        }


@dataclass(frozen=True, slots=True)
class SignaturePolicy:
    mode: Literal["skip", "cosign_key", "cosign_keyless"]
    public_key: str | None = None
    issuer: str | None = None
    identity: str | None = None

    @classmethod
    def skip(cls) -> SignaturePolicy:
        return cls("skip")

    @classmethod
    def cosign_key(cls, public_key: str) -> SignaturePolicy:
        return cls("cosign_key", public_key=public_key)

    @classmethod
    def cosign_keyless(cls, issuer: str, identity: str) -> SignaturePolicy:
        return cls("cosign_keyless", issuer=issuer, identity=identity)

    def bridge_value(self) -> dict[str, str]:
        if self.mode == "skip":
            return {"mode": self.mode}
        if self.mode == "cosign_key":
            if not self.public_key:
                raise ValueError("cosign_key requires public_key")
            return {"mode": self.mode, "public_key": self.public_key}
        if not self.issuer or not self.identity:
            raise ValueError("cosign_keyless requires issuer and identity")
        return {
            "mode": self.mode,
            "issuer": self.issuer,
            "identity": self.identity,
        }


@dataclass(frozen=True, slots=True)
class ImageHealthCheckInfo:
    test: tuple[str, ...]
    interval: int | None
    timeout: int | None
    retries: int | None
    start_period: int | None


@dataclass(frozen=True, slots=True)
class ImageInspectInfo:
    reference: str
    digest: str
    size_bytes: int
    pulled_at: str
    last_used: str
    path: str
    manifest_digest: str
    layer_count: int
    entrypoint: tuple[str, ...] | None
    command: tuple[str, ...] | None
    env: dict[str, str]
    working_dir: str | None
    user: str | None
    exposed_ports: tuple[str, ...]
    volumes: tuple[str, ...]
    stop_signal: str | None
    health_check: ImageHealthCheckInfo | None
    onbuild: tuple[str, ...]
    labels: dict[str, str]


@dataclass(frozen=True, slots=True)
class ImageHistoryInfo:
    created: str | None
    created_by: str
    size_bytes: int
    comment: str
    empty_layer: bool


@dataclass(frozen=True, slots=True)
class PushImageInfo:
    reference: str
    manifest_digest: str
    config_url: str
    manifest_url: str


@dataclass(frozen=True, slots=True)
class SdkCapabilities:
    protocol_version: int
    operations: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class VolumeInfo:
    name: str
    driver: str
    mount_point: str
    labels: dict[str, str]
    in_use_by: tuple[str, ...]
    in_use: bool
    size_limit: int
    created_at: str


@dataclass(frozen=True, slots=True)
class NetworkEndpointInfo:
    box_id: str
    box_name: str
    aliases: tuple[str, ...]
    ip_address: str
    mac_address: str


@dataclass(frozen=True, slots=True)
class NetworkInfo:
    name: str
    driver: str
    subnet: str
    gateway: str
    labels: dict[str, str]
    endpoints: tuple[NetworkEndpointInfo, ...]
    endpoint_count: int
    isolation: str
    created_at: str


@dataclass(frozen=True, slots=True)
class VolumeMount:
    kind: Literal["bind", "named"]
    source: str
    target: str
    read_only: bool = False

    @classmethod
    def bind(
        cls,
        source: str,
        target: str,
        *,
        read_only: bool = False,
    ) -> VolumeMount:
        return cls("bind", source, target, read_only)

    @classmethod
    def named(
        cls,
        name: str,
        target: str,
        *,
        read_only: bool = False,
    ) -> VolumeMount:
        return cls("named", name, target, read_only)

    def bridge_value(self) -> dict[str, object]:
        source_key = "source" if self.kind == "bind" else "name"
        return {
            "kind": self.kind,
            source_key: self.source,
            "target": self.target,
            "read_only": self.read_only,
        }


@dataclass(frozen=True, slots=True)
class TmpfsMount:
    target: str
    size_bytes: int | None = None
    read_only: bool = False

    def bridge_value(self) -> dict[str, object]:
        result: dict[str, object] = {
            "target": self.target,
            "read_only": self.read_only,
        }
        if self.size_bytes is not None:
            result["size_bytes"] = self.size_bytes
        return result


@dataclass(frozen=True, slots=True)
class SandboxNetwork:
    mode: Literal["tsi", "none", "bridge"]
    name: str | None = None

    @classmethod
    def tsi(cls) -> SandboxNetwork:
        return cls("tsi")

    @classmethod
    def disabled(cls) -> SandboxNetwork:
        return cls("none")

    @classmethod
    def bridge(cls, name: str) -> SandboxNetwork:
        return cls("bridge", name)

    def bridge_value(self) -> dict[str, str]:
        result = {"mode": self.mode}
        if self.name is not None:
            result["name"] = self.name
        return result


@dataclass(frozen=True, slots=True)
class PortMapping:
    host_port: int
    guest_port: int

    @classmethod
    def tcp(cls, host_port: int, guest_port: int) -> PortMapping:
        return cls(host_port, guest_port)

    def bridge_value(self) -> dict[str, int]:
        return {
            "host_port": self.host_port,
            "guest_port": self.guest_port,
        }


@dataclass(frozen=True, slots=True)
class Script:
    source: str | bytes
    interpreter: tuple[str, ...] = ("/bin/sh", "-se")


@dataclass(frozen=True, slots=True)
class EntryInfo:
    name: str
    type: Literal["file", "directory", "unspecified"]
    path: str
    size: int
    mode: int
    permissions: str
    owner: str
    group: str
    modified_seconds: int
    modified_nanos: int
    symlink_target: str | None = None
