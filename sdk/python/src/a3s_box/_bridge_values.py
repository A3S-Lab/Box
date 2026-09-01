"""Strict decoders for values returned by the local machine bridge."""

from __future__ import annotations

import base64
import binascii
from collections.abc import Mapping, Sequence
from typing import Literal, NoReturn, cast

from .exceptions import A3SBoxError
from .models import (
    BuildImageInfo,
    CommandResult,
    EntryInfo,
    ExecutionCpuStats,
    ExecutionEventBatch,
    ExecutionEventKind,
    ExecutionMemoryStats,
    ExecutionProcessInfo,
    ExecutionProcessInventory,
    ExecutionRuntimeEvent,
    ExecutionStats,
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
        reference=string(result["reference"]),
        digest=string(result["digest"]),
        size_bytes=integer(result["size_bytes"]),
        layer_count=integer(result["layer_count"]),
    )


def image_info(result: Mapping[str, object]) -> ImageInfo:
    return ImageInfo(
        reference=string(result["reference"]),
        digest=string(result["digest"]),
        size_bytes=integer(result["size_bytes"]),
        pulled_at=string(result["pulled_at"]),
        last_used=string(result["last_used"]),
        path=string(result["path"]),
    )


def image_inspect_info(result: Mapping[str, object]) -> ImageInspectInfo:
    health_value = result.get("health_check")
    return ImageInspectInfo(
        reference=string(result["reference"]),
        digest=string(result["digest"]),
        size_bytes=integer(result["size_bytes"]),
        pulled_at=string(result["pulled_at"]),
        last_used=string(result["last_used"]),
        path=string(result["path"]),
        manifest_digest=string(result["manifest_digest"]),
        layer_count=integer(result["layer_count"]),
        entrypoint=optional_string_tuple(result.get("entrypoint")),
        command=optional_string_tuple(result.get("command")),
        env=string_mapping(result["env"]),
        working_dir=optional_string(result.get("working_dir")),
        user=optional_string(result.get("user")),
        exposed_ports=string_tuple(result["exposed_ports"]),
        volumes=string_tuple(result["volumes"]),
        stop_signal=optional_string(result.get("stop_signal")),
        health_check=(
            None
            if health_value is None
            else image_health_check_info(mapping(health_value))
        ),
        onbuild=string_tuple(result["onbuild"]),
        labels=string_mapping(result["labels"]),
    )


def image_health_check_info(
    result: Mapping[str, object],
) -> ImageHealthCheckInfo:
    return ImageHealthCheckInfo(
        test=string_tuple(result["test"]),
        interval=optional_int(result.get("interval")),
        timeout=optional_int(result.get("timeout")),
        retries=optional_int(result.get("retries")),
        start_period=optional_int(result.get("start_period")),
    )


def image_history_info(result: Mapping[str, object]) -> ImageHistoryInfo:
    return ImageHistoryInfo(
        created=optional_string(result.get("created")),
        created_by=string(result["created_by"]),
        size_bytes=integer(result["size_bytes"]),
        comment=string(result["comment"]),
        empty_layer=boolean(result["empty_layer"]),
    )


def push_image_info(result: Mapping[str, object]) -> PushImageInfo:
    return PushImageInfo(
        reference=string(result["reference"]),
        manifest_digest=string(result["manifest_digest"]),
        config_url=string(result["config_url"]),
        manifest_url=string(result["manifest_url"]),
    )


def sdk_capabilities(result: Mapping[str, object]) -> SdkCapabilities:
    return SdkCapabilities(
        protocol_version=integer(result["protocol_version"]),
        operations=string_tuple(result["operations"]),
    )


def volume_info(result: Mapping[str, object]) -> VolumeInfo:
    return VolumeInfo(
        name=string(result["name"]),
        driver=string(result["driver"]),
        mount_point=string(result["mount_point"]),
        labels=string_mapping(result["labels"]),
        in_use_by=string_tuple(result["in_use_by"]),
        in_use=boolean(result["in_use"]),
        size_limit=integer(result["size_limit"]),
        created_at=string(result["created_at"]),
    )


def network_info(result: Mapping[str, object]) -> NetworkInfo:
    return NetworkInfo(
        name=string(result["name"]),
        driver=string(result["driver"]),
        subnet=string(result["subnet"]),
        gateway=string(result["gateway"]),
        labels=string_mapping(result["labels"]),
        endpoints=tuple(
            network_endpoint(item)
            for item in mapping_sequence(result["endpoints"])
        ),
        endpoint_count=integer(result["endpoint_count"]),
        isolation=string(result["isolation"]),
        created_at=string(result["created_at"]),
    )


def network_endpoint(result: Mapping[str, object]) -> NetworkEndpointInfo:
    return NetworkEndpointInfo(
        box_id=string(result["box_id"]),
        box_name=string(result["box_name"]),
        aliases=string_tuple(result["aliases"]),
        ip_address=string(result["ip_address"]),
        mac_address=string(result["mac_address"]),
    )


def sandbox_summary(result: Mapping[str, object]) -> SandboxSummary:
    return SandboxSummary(
        id=string(result["id"]),
        short_id=string(result["short_id"]),
        name=string(result["name"]),
        image=string(result["image"]),
        isolation=string(result["isolation"]),
        status=string(result["status"]),
        status_summary=string(result["status_summary"]),
        active=boolean(result["active"]),
        pid=optional_int(result.get("pid")),
        cpus=integer(result["cpus"]),
        memory_mb=integer(result["memory_mb"]),
        ports=string_tuple(result["ports"]),
        command=string_tuple(result["command"]),
        health=string(result["health"]),
        labels=string_mapping(result["labels"]),
        created_at=string(result["created_at"]),
        started_at=optional_string(result.get("started_at")),
        network_name=optional_string(result.get("network_name")),
        volume_names=string_tuple(result["volume_names"]),
    )


def sandbox_log_entry(result: Mapping[str, object]) -> SandboxLogEntry:
    return SandboxLogEntry(
        stream=string(result["stream"]),
        message=string(result["log"]),
        timestamp=optional_string(result.get("time")),
    )


def sandbox_stats(result: Mapping[str, object]) -> SandboxStats:
    return SandboxStats(
        id=string(result["id"]),
        short_id=string(result["short_id"]),
        name=string(result["name"]),
        status=string(result["status"]),
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


def execution_process_inventory(
    result: Mapping[str, object],
) -> ExecutionProcessInventory:
    return ExecutionProcessInventory(
        execution_id=string(result["execution_id"]),
        generation=integer(result["generation"]),
        processes=tuple(
            ExecutionProcessInfo(
                process_id=string(item["process_id"]),
                pid=optional_unsigned_integer(item.get("pid")),
                terminal=boolean(item["terminal"]),
            )
            for item in mapping_sequence(result["processes"])
        ),
    )


def execution_stats(result: Mapping[str, object]) -> ExecutionStats:
    cpu = mapping(result["cpu"])
    memory = mapping(result["memory"])
    return ExecutionStats(
        execution_id=string(result["execution_id"]),
        generation=integer(result["generation"]),
        timestamp_unix_ns=unsigned_decimal(result["timestamp_unix_ns"]),
        cpu=ExecutionCpuStats(
            usage_ns=unsigned_integer(cpu["usage_ns"]),
            user_ns=unsigned_integer(cpu["user_ns"]),
            system_ns=unsigned_integer(cpu["system_ns"]),
            throttled_ns=unsigned_integer(cpu["throttled_ns"]),
        ),
        memory=ExecutionMemoryStats(
            usage_bytes=unsigned_integer(memory["usage_bytes"]),
            limit_bytes=optional_unsigned_integer(memory.get("limit_bytes")),
            peak_bytes=optional_unsigned_integer(memory.get("peak_bytes")),
        ),
        process_count=unsigned_integer(result["process_count"]),
        metrics=integer_mapping(result["metrics"]),
    )


def execution_event_batch(
    result: Mapping[str, object],
) -> ExecutionEventBatch:
    return ExecutionEventBatch(
        execution_id=string(result["execution_id"]),
        generation=integer(result["generation"]),
        events=tuple(
            ExecutionRuntimeEvent(
                sequence=unsigned_integer(item["sequence"]),
                timestamp_unix_ns=unsigned_decimal(item["timestamp_unix_ns"]),
                process_id=optional_string(item.get("process_id")),
                kind=execution_event_kind(item["kind"]),
                attributes=string_mapping(item["attributes"]),
            )
            for item in mapping_sequence(result["events"])
        ),
        next_sequence=unsigned_integer(result["next_sequence"]),
    )


def execution_event_kind(value: object) -> ExecutionEventKind:
    kind = string(value)
    if kind not in {
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
    }:
        protocol_error("an invalid execution event kind")
    return cast(ExecutionEventKind, kind)


def runtime_diagnostics(result: Mapping[str, object]) -> RuntimeDiagnostics:
    virtualization = mapping(result["virtualization"])
    return RuntimeDiagnostics(
        core_version=string(result["core_version"]),
        runtime_version=string(result["runtime_version"]),
        sdk_version=string(result["sdk_version"]),
        home=string(result["home"]),
        virtualization=RuntimeVirtualization(
            available=boolean(virtualization["available"]),
            backend=optional_string(virtualization.get("backend")),
            details=string(virtualization["details"]),
        ),
    )


def runtime_disk_usage(result: Mapping[str, object]) -> RuntimeDiskUsage:
    return RuntimeDiskUsage(
        home=string(result["home"]),
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
        id=string(result["id"]),
        name=string(result["name"]),
        source_sandbox_id=string(result["source_box_id"]),
        image=string(result["image"]),
        vcpus=integer(result["vcpus"]),
        memory_mb=integer(result["memory_mb"]),
        volumes=string_tuple(result["volumes"]),
        command=string_tuple(result["command"]),
        ports=string_tuple(result["port_map"]),
        labels=string_mapping(result["labels"]),
        network_mode=optional_string(result.get("network_mode")),
        size_bytes=integer(result["size_bytes"]),
        created_at=string(result["created_at"]),
        description=string(result["description"]),
    )


def filesystem_snapshot_info(
    result: Mapping[str, object],
) -> FilesystemSnapshotInfo:
    return FilesystemSnapshotInfo(
        snapshot_id=string(result["snapshot_id"]),
        size_bytes=integer(result["size_bytes"]),
        state=string(result["state"]),
        generation=integer(result["generation"]),
    )


def command_result(result: Mapping[str, object]) -> CommandResult:
    stdout = decoded_base64(result.get("stdout_base64", ""), "stdout_base64")
    stderr = decoded_base64(result.get("stderr_base64", ""), "stderr_base64")
    return CommandResult(
        stdout=stdout.decode(errors="replace"),
        stderr=stderr.decode(errors="replace"),
        exit_code=integer(result["exit_code"]),
        truncated=boolean(result.get("truncated", False)),
    )


def entry_info(entry: Mapping[str, object]) -> EntryInfo:
    entry_type = string(entry["type"])
    if entry_type not in {"file", "directory", "unspecified"}:
        protocol_error("an invalid entry type")
    return EntryInfo(
        name=string(entry["name"]),
        type=cast(
            Literal["file", "directory", "unspecified"],
            entry_type,
        ),
        path=string(entry["path"]),
        size=integer(entry["size"]),
        mode=integer(entry["mode"]),
        permissions=string(entry["permissions"]),
        owner=string(entry["owner"]),
        group=string(entry["group"]),
        modified_seconds=integer(entry["modified_seconds"]),
        modified_nanos=integer(entry["modified_nanos"]),
        symlink_target=optional_string(entry.get("symlink_target")),
    )


def mapping(value: object) -> Mapping[str, object]:
    if not isinstance(value, Mapping):
        protocol_error("a non-object value")
    return cast(Mapping[str, object], value)


def sequence(value: object) -> Sequence[object]:
    if not isinstance(value, Sequence) or isinstance(
        value,
        (str, bytes, bytearray),
    ):
        protocol_error("a non-array value")
    return cast(Sequence[object], value)


def mapping_sequence(value: object) -> list[Mapping[str, object]]:
    return [mapping(item) for item in sequence(value)]


def mapping_list(
    result: Mapping[str, object],
    key: str,
) -> list[Mapping[str, object]]:
    return mapping_sequence(result[key])


def string_mapping(value: object) -> dict[str, str]:
    result = mapping(value)
    if any(
        not isinstance(key, str) or not isinstance(item, str)
        for key, item in result.items()
    ):
        protocol_error("a non-string mapping")
    return dict(cast(Mapping[str, str], result))


def integer_mapping(value: object) -> dict[str, int]:
    result = mapping(value)
    if any(
        not isinstance(key, str)
        or isinstance(item, bool)
        or not isinstance(item, int)
        or item < 0
        for key, item in result.items()
    ):
        protocol_error("a non-integer mapping")
    return dict(cast(Mapping[str, int], result))


def string(value: object) -> str:
    if not isinstance(value, str):
        protocol_error("a non-string value")
    return value


def string_tuple(value: object) -> tuple[str, ...]:
    values = sequence(value)
    if any(not isinstance(item, str) for item in values):
        protocol_error("a non-string array")
    return tuple(cast(Sequence[str], values))


def optional_string(value: object) -> str | None:
    return None if value is None else string(value)


def optional_string_tuple(value: object) -> tuple[str, ...] | None:
    return None if value is None else string_tuple(value)


def integer(value: object) -> int:
    if isinstance(value, bool) or not isinstance(value, int):
        protocol_error("a non-integer value")
    return value


def unsigned_integer(value: object) -> int:
    result = integer(value)
    if result < 0 or result > (1 << 64) - 1:
        protocol_error("an out-of-range unsigned integer")
    return result


def unsigned_decimal(value: object) -> int:
    encoded = string(value)
    if (
        not encoded
        or len(encoded) > 20
        or (len(encoded) > 1 and encoded.startswith("0"))
        or any(character not in "0123456789" for character in encoded)
    ):
        protocol_error("an invalid unsigned decimal string")
    result = int(encoded)
    if result > (1 << 64) - 1:
        protocol_error("an out-of-range unsigned decimal string")
    return result


def optional_unsigned_integer(value: object) -> int | None:
    return None if value is None else unsigned_integer(value)


def optional_int(value: object) -> int | None:
    return None if value is None else integer(value)


def number(value: object) -> float:
    if isinstance(value, bool) or not isinstance(value, (int, float)):
        protocol_error("a non-number value")
    return float(value)


def boolean(value: object) -> bool:
    if not isinstance(value, bool):
        protocol_error("a non-boolean value")
    return value


def decoded_base64(value: object, field: str) -> bytes:
    encoded = string(value)
    try:
        return base64.b64decode(encoded, validate=True)
    except (binascii.Error, ValueError) as error:
        raise A3SBoxError(
            f"A3S Box bridge returned invalid {field}",
            code="bridge_protocol_error",
        ) from error


def protocol_error(detail: str) -> NoReturn:
    raise A3SBoxError(
        f"A3S Box bridge returned {detail}",
        code="bridge_protocol_error",
    )
