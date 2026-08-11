"""Typed values returned by the native A3S Box SDK."""

from __future__ import annotations

from dataclasses import dataclass, field
from typing import Literal, TypeAlias


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
class Artifact:
    path: str
    data: bytes
    size: int
    sha256: str
    host_path: str | None = None


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
class SandboxSummary:
    id: str
    short_id: str
    name: str
    image: str
    isolation: str
    status: str
    status_summary: str
    active: bool
    pid: int | None
    cpus: int
    memory_mb: int
    ports: tuple[str, ...]
    command: tuple[str, ...]
    health: str
    labels: dict[str, str]
    created_at: str
    started_at: str | None
    network_name: str | None
    volume_names: tuple[str, ...]


@dataclass(frozen=True, slots=True)
class SandboxLogEntry:
    stream: str
    message: str
    timestamp: str | None


@dataclass(frozen=True, slots=True)
class SandboxStats:
    id: str
    short_id: str
    name: str
    status: str
    pid: int
    cpus: int
    cpu_percent: float
    cpu_percent_scaled: float
    memory_bytes: int
    memory_limit_bytes: int
    memory_percent: float
    network_rx_bytes: int
    network_tx_bytes: int
    block_read_bytes: int
    block_write_bytes: int


@dataclass(frozen=True, slots=True)
class ExecutionProcessInfo:
    process_id: str
    pid: int | None
    terminal: bool


@dataclass(frozen=True, slots=True)
class ExecutionProcessInventory:
    execution_id: str
    generation: int
    processes: tuple[ExecutionProcessInfo, ...]


@dataclass(frozen=True, slots=True)
class ExecutionCpuStats:
    usage_ns: int
    user_ns: int
    system_ns: int
    throttled_ns: int


@dataclass(frozen=True, slots=True)
class ExecutionMemoryStats:
    usage_bytes: int
    limit_bytes: int | None
    peak_bytes: int | None


@dataclass(frozen=True, slots=True)
class ExecutionStats:
    execution_id: str
    generation: int
    timestamp_unix_ns: int
    cpu: ExecutionCpuStats
    memory: ExecutionMemoryStats
    process_count: int
    metrics: dict[str, int]


ExecutionEventKind: TypeAlias = Literal[
    "container-creating",
    "container-created",
    "container-started",
    "container-stopped",
    "container-deleted",
    "container-paused",
    "container-resumed",
    "resources-updated",
    "process-created",
    "process-started",
    "process-exited",
    "output-dropped",
    "runtime-warning",
]


@dataclass(frozen=True, slots=True)
class ExecutionRuntimeEvent:
    sequence: int
    timestamp_unix_ns: int
    process_id: str | None
    kind: ExecutionEventKind
    attributes: dict[str, str]


@dataclass(frozen=True, slots=True)
class ExecutionEventBatch:
    execution_id: str
    generation: int
    events: tuple[ExecutionRuntimeEvent, ...]
    next_sequence: int


@dataclass(frozen=True, slots=True)
class ExecutionResourceUpdate:
    memory_reservation: int | None = None
    memory_swap: int | None = None
    pids_limit: int | None = None
    cpu_shares: int | None = None
    cpu_quota: int | None = None
    cpu_period: int | None = None
    cpuset_cpus: str | None = None

    def bridge_value(self) -> dict[str, object]:
        values = {
            name: value
            for name, value in (
                ("memory_reservation", self.memory_reservation),
                ("memory_swap", self.memory_swap),
                ("pids_limit", self.pids_limit),
                ("cpu_shares", self.cpu_shares),
                ("cpu_quota", self.cpu_quota),
                ("cpu_period", self.cpu_period),
                ("cpuset_cpus", self.cpuset_cpus),
            )
            if value is not None
        }
        if not values:
            raise ValueError(
                "resource update must change at least one supported field"
            )
        _non_negative_integer(
            "memory_reservation",
            self.memory_reservation,
        )
        _minimum_integer(
            "memory_swap",
            self.memory_swap,
            -1,
            (1 << 63) - 1,
        )
        _minimum_integer("pids_limit", self.pids_limit, 1)
        if self.cpu_shares is not None:
            _minimum_integer("cpu_shares", self.cpu_shares, 2)
            if self.cpu_shares > 262_144:
                raise ValueError("cpu_shares must be at most 262144")
        _minimum_integer(
            "cpu_quota",
            self.cpu_quota,
            1,
            (1 << 63) - 1,
        )
        _minimum_integer("cpu_period", self.cpu_period, 1)
        if self.cpuset_cpus is not None and not _valid_cpuset(
            self.cpuset_cpus
        ):
            raise ValueError(
                "cpuset_cpus must be a comma-separated list of indices "
                "or ascending ranges"
            )
        return values


def _non_negative_integer(name: str, value: int | None) -> None:
    _minimum_integer(name, value, 0, (1 << 64) - 1)


def _minimum_integer(
    name: str,
    value: int | None,
    minimum: int,
    maximum: int = (1 << 64) - 1,
) -> None:
    if value is None:
        return
    if isinstance(value, bool) or not isinstance(value, int):
        raise ValueError(f"{name} must be an integer")
    if value < minimum:
        raise ValueError(f"{name} must be at least {minimum}")
    if value > maximum:
        raise ValueError(f"{name} must be at most {maximum}")


def _valid_cpuset(value: object) -> bool:
    if not isinstance(value, str) or not value.strip():
        return False

    def index(part: str) -> int | None:
        if not part or not part.isascii() or not part.isdigit():
            return None
        result = int(part)
        return result if result <= (1 << 32) - 1 else None

    for item in value.strip().split(","):
        item = item.strip()
        if "-" not in item:
            if index(item) is None:
                return False
            continue
        lower_text, upper_text = item.split("-", 1)
        lower = index(lower_text)
        upper = index(upper_text)
        if lower is None or upper is None or lower > upper:
            return False
    return True


@dataclass(frozen=True, slots=True)
class RuntimeVirtualization:
    available: bool
    backend: str | None
    details: str


@dataclass(frozen=True, slots=True)
class RuntimeDiagnostics:
    core_version: str
    runtime_version: str
    sdk_version: str
    home: str
    virtualization: RuntimeVirtualization


@dataclass(frozen=True, slots=True)
class RuntimeDiskUsage:
    home: str
    total_bytes: int
    boxes_bytes: int
    images_bytes: int
    volumes_bytes: int
    snapshots_bytes: int
    state_bytes: int
    other_bytes: int


@dataclass(frozen=True, slots=True)
class FilesystemSnapshotSummary:
    id: str
    name: str
    source_sandbox_id: str
    image: str
    vcpus: int
    memory_mb: int
    volumes: tuple[str, ...]
    command: tuple[str, ...]
    ports: tuple[str, ...]
    labels: dict[str, str]
    network_mode: str | None
    size_bytes: int
    created_at: str
    description: str


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
