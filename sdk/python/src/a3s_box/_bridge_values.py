"""Strict decoders for values returned by the local machine bridge."""

from __future__ import annotations

import base64
from collections.abc import Mapping, Sequence
from typing import Literal, cast

from .models import (
    BuildImageInfo,
    CommandResult,
    EntryInfo,
    FilesystemSnapshotInfo,
    FilesystemSnapshotSummary,
    ImageHealthCheckInfo,
    ImageHistoryInfo,
    ImageInfo,
    ImageInspectInfo,
    NetworkEndpointInfo,
    NetworkInfo,
    PushImageInfo,
    RuntimeDiagnostics,
    RuntimeDiskUsage,
    RuntimeVirtualization,
    SandboxLogEntry,
    SandboxStats,
    SandboxSummary,
    SdkCapabilities,
    VolumeInfo,
)


def build_image_info(result: Mapping[str, object]) -> BuildImageInfo:
    return BuildImageInfo(
        reference=str(result["reference"]),
        digest=str(result["digest"]),
        size_bytes=integer(result["size_bytes"]),
        layer_count=integer(result["layer_count"]),
    )


def image_info(result: Mapping[str, object]) -> ImageInfo:
    return ImageInfo(
        reference=str(result["reference"]),
        digest=str(result["digest"]),
        size_bytes=integer(result["size_bytes"]),
        pulled_at=str(result["pulled_at"]),
        last_used=str(result["last_used"]),
        path=str(result["path"]),
    )


def image_inspect_info(result: Mapping[str, object]) -> ImageInspectInfo:
    health_value = result.get("health_check")
    return ImageInspectInfo(
        reference=str(result["reference"]),
        digest=str(result["digest"]),
        size_bytes=integer(result["size_bytes"]),
        pulled_at=str(result["pulled_at"]),
        last_used=str(result["last_used"]),
        path=str(result["path"]),
        manifest_digest=str(result["manifest_digest"]),
        layer_count=integer(result["layer_count"]),
        entrypoint=optional_string_tuple(result.get("entrypoint")),
        command=optional_string_tuple(result.get("command")),
        env=string_mapping(result["env"]),
        working_dir=optional_string(result.get("working_dir")),
        user=optional_string(result.get("user")),
        exposed_ports=tuple(str(value) for value in sequence(result["exposed_ports"])),
        volumes=tuple(str(value) for value in sequence(result["volumes"])),
        stop_signal=optional_string(result.get("stop_signal")),
        health_check=(
            None
            if health_value is None
            else image_health_check_info(mapping(health_value))
        ),
        onbuild=tuple(str(value) for value in sequence(result["onbuild"])),
        labels=string_mapping(result["labels"]),
    )


def image_health_check_info(
    result: Mapping[str, object],
) -> ImageHealthCheckInfo:
    return ImageHealthCheckInfo(
        test=tuple(str(value) for value in sequence(result["test"])),
        interval=optional_int(result.get("interval")),
        timeout=optional_int(result.get("timeout")),
        retries=optional_int(result.get("retries")),
        start_period=optional_int(result.get("start_period")),
    )


def image_history_info(result: Mapping[str, object]) -> ImageHistoryInfo:
    return ImageHistoryInfo(
        created=optional_string(result.get("created")),
        created_by=str(result["created_by"]),
        size_bytes=integer(result["size_bytes"]),
        comment=str(result["comment"]),
        empty_layer=boolean(result["empty_layer"]),
    )


def push_image_info(result: Mapping[str, object]) -> PushImageInfo:
    return PushImageInfo(
        reference=str(result["reference"]),
        manifest_digest=str(result["manifest_digest"]),
        config_url=str(result["config_url"]),
        manifest_url=str(result["manifest_url"]),
    )


def sdk_capabilities(result: Mapping[str, object]) -> SdkCapabilities:
    return SdkCapabilities(
        protocol_version=integer(result["protocol_version"]),
        operations=tuple(str(value) for value in sequence(result["operations"])),
    )


def volume_info(result: Mapping[str, object]) -> VolumeInfo:
    return VolumeInfo(
        name=str(result["name"]),
        driver=str(result["driver"]),
        mount_point=str(result["mount_point"]),
        labels=string_mapping(result["labels"]),
        in_use_by=tuple(str(value) for value in sequence(result["in_use_by"])),
        in_use=boolean(result["in_use"]),
        size_limit=integer(result["size_limit"]),
        created_at=str(result["created_at"]),
    )


def network_info(result: Mapping[str, object]) -> NetworkInfo:
    return NetworkInfo(
        name=str(result["name"]),
        driver=str(result["driver"]),
        subnet=str(result["subnet"]),
        gateway=str(result["gateway"]),
        labels=string_mapping(result["labels"]),
        endpoints=tuple(
            network_endpoint(item)
            for item in mapping_sequence(result["endpoints"])
        ),
        endpoint_count=integer(result["endpoint_count"]),
        isolation=str(result["isolation"]),
        created_at=str(result["created_at"]),
    )


def network_endpoint(result: Mapping[str, object]) -> NetworkEndpointInfo:
    return NetworkEndpointInfo(
        box_id=str(result["box_id"]),
        box_name=str(result["box_name"]),
        aliases=tuple(str(value) for value in sequence(result["aliases"])),
        ip_address=str(result["ip_address"]),
        mac_address=str(result["mac_address"]),
    )


def sandbox_summary(result: Mapping[str, object]) -> SandboxSummary:
    return SandboxSummary(
        id=str(result["id"]),
        short_id=str(result["short_id"]),
        name=str(result["name"]),
        image=str(result["image"]),
        isolation=str(result["isolation"]),
        status=str(result["status"]),
        status_summary=str(result["status_summary"]),
        active=boolean(result["active"]),
        pid=optional_int(result.get("pid")),
        cpus=integer(result["cpus"]),
        memory_mb=integer(result["memory_mb"]),
        ports=tuple(str(value) for value in sequence(result["ports"])),
        command=tuple(str(value) for value in sequence(result["command"])),
        health=str(result["health"]),
        labels=string_mapping(result["labels"]),
        created_at=str(result["created_at"]),
        started_at=optional_string(result.get("started_at")),
        network_name=optional_string(result.get("network_name")),
        volume_names=tuple(str(value) for value in sequence(result["volume_names"])),
    )


def sandbox_log_entry(result: Mapping[str, object]) -> SandboxLogEntry:
    return SandboxLogEntry(
        stream=str(result["stream"]),
        message=str(result["log"]),
        timestamp=optional_string(result.get("time")),
    )


def sandbox_stats(result: Mapping[str, object]) -> SandboxStats:
    return SandboxStats(
        id=str(result["id"]),
        short_id=str(result["short_id"]),
        name=str(result["name"]),
        status=str(result["status"]),
        pid=integer(result["pid"]),
        cpus=integer(result["cpus"]),
        cpu_percent=number(result["cpu_percent"]),
        cpu_percent_scaled=number(result["cpu_percent_scaled"]),
        memory_bytes=integer(result["memory_bytes"]),
        memory_limit_bytes=integer(result["memory_limit_bytes"]),
        memory_percent=number(result["memory_percent"]),
        network_rx_bytes=integer(result["network_rx_bytes"]),
        network_tx_bytes=integer(result["network_tx_bytes"]),
        block_read_bytes=integer(result["block_read_bytes"]),
        block_write_bytes=integer(result["block_write_bytes"]),
    )


def runtime_diagnostics(result: Mapping[str, object]) -> RuntimeDiagnostics:
    virtualization = mapping(result["virtualization"])
    return RuntimeDiagnostics(
        core_version=str(result["core_version"]),
        runtime_version=str(result["runtime_version"]),
        sdk_version=str(result["sdk_version"]),
        home=str(result["home"]),
        virtualization=RuntimeVirtualization(
            available=boolean(virtualization["available"]),
            backend=optional_string(virtualization.get("backend")),
            details=str(virtualization["details"]),
        ),
    )


def runtime_disk_usage(result: Mapping[str, object]) -> RuntimeDiskUsage:
    return RuntimeDiskUsage(
        home=str(result["home"]),
        total_bytes=integer(result["total_bytes"]),
        boxes_bytes=integer(result["boxes_bytes"]),
        images_bytes=integer(result["images_bytes"]),
        volumes_bytes=integer(result["volumes_bytes"]),
        snapshots_bytes=integer(result["snapshots_bytes"]),
        state_bytes=integer(result["state_bytes"]),
        other_bytes=integer(result["other_bytes"]),
    )


def filesystem_snapshot_summary(
    result: Mapping[str, object],
) -> FilesystemSnapshotSummary:
    return FilesystemSnapshotSummary(
        id=str(result["id"]),
        name=str(result["name"]),
        source_sandbox_id=str(result["source_box_id"]),
        image=str(result["image"]),
        vcpus=integer(result["vcpus"]),
        memory_mb=integer(result["memory_mb"]),
        volumes=tuple(str(value) for value in sequence(result["volumes"])),
        command=tuple(str(value) for value in sequence(result["command"])),
        ports=tuple(str(value) for value in sequence(result["port_map"])),
        labels=string_mapping(result["labels"]),
        network_mode=optional_string(result.get("network_mode")),
        size_bytes=integer(result["size_bytes"]),
        created_at=str(result["created_at"]),
        description=str(result["description"]),
    )


def filesystem_snapshot_info(
    result: Mapping[str, object],
) -> FilesystemSnapshotInfo:
    return FilesystemSnapshotInfo(
        snapshot_id=str(result["snapshot_id"]),
        size_bytes=integer(result["size_bytes"]),
        state=str(result["state"]),
        generation=integer(result["generation"]),
    )


def command_result(result: Mapping[str, object]) -> CommandResult:
    stdout = base64.b64decode(str(result.get("stdout_base64", "")), validate=True)
    stderr = base64.b64decode(str(result.get("stderr_base64", "")), validate=True)
    return CommandResult(
        stdout=stdout.decode(errors="replace"),
        stderr=stderr.decode(errors="replace"),
        exit_code=integer(result["exit_code"]),
        truncated=boolean(result.get("truncated", False)),
    )


def entry_info(entry: Mapping[str, object]) -> EntryInfo:
    return EntryInfo(
        name=str(entry["name"]),
        type=cast(
            Literal["file", "directory", "unspecified"],
            str(entry["type"]),
        ),
        path=str(entry["path"]),
        size=integer(entry["size"]),
        mode=integer(entry["mode"]),
        permissions=str(entry["permissions"]),
        owner=str(entry["owner"]),
        group=str(entry["group"]),
        modified_seconds=integer(entry["modified_seconds"]),
        modified_nanos=integer(entry["modified_nanos"]),
        symlink_target=optional_string(entry.get("symlink_target")),
    )


def mapping(value: object) -> Mapping[str, object]:
    if not isinstance(value, Mapping):
        raise TypeError("A3S Box bridge returned a non-object value")
    return cast(Mapping[str, object], value)


def sequence(value: object) -> Sequence[object]:
    if not isinstance(value, Sequence) or isinstance(value, (str, bytes)):
        raise TypeError("A3S Box bridge returned a non-array value")
    return cast(Sequence[object], value)


def mapping_sequence(value: object) -> list[Mapping[str, object]]:
    return [mapping(item) for item in sequence(value)]


def mapping_list(
    result: Mapping[str, object],
    key: str,
) -> list[Mapping[str, object]]:
    return mapping_sequence(result[key])


def string_mapping(value: object) -> dict[str, str]:
    return {str(key): str(item) for key, item in mapping(value).items()}


def optional_string(value: object) -> str | None:
    return None if value is None else str(value)


def optional_string_tuple(value: object) -> tuple[str, ...] | None:
    return (
        None
        if value is None
        else tuple(str(item) for item in sequence(value))
    )


def integer(value: object) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        raise TypeError("A3S Box bridge returned a non-integer value")
    return value


def optional_int(value: object) -> int | None:
    return None if value is None else integer(value)


def number(value: object) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        raise TypeError("A3S Box bridge returned a non-number value")
    return float(value)


def boolean(value: object) -> bool:
    if not isinstance(value, bool):
        raise TypeError("A3S Box bridge returned a non-boolean value")
    return value
