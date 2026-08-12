"""Comparable lifecycle, compute, storage, network, and concurrency workloads."""

from __future__ import annotations

import json
import re
import time
from concurrent.futures import ThreadPoolExecutor

from .core import BenchmarkCore


def lifecycle_matrix(bench: BenchmarkCore) -> None:
    for iteration in range(1, bench.args.lifecycle_runs + 1):
        for isolation in bench.ordered_isolations(iteration):
            bench.measure(
                isolation,
                "cold_noop_lifecycle",
                iteration,
                bench.run_args(
                    isolation,
                    ["--rm", "--no-stdin", "--timeout", "180"],
                    ["true"],
                ),
            )

    init_script = bench.fixture_dir / "init.sh"
    init_script.write_text(
        "#!/bin/sh\n"
        "set -eu\n"
        'root="/state/init-${A3S_PERF_ITERATION}"\n'
        'mkdir -p "$root"\n'
        "i=1\n"
        'while [ "$i" -le 100 ]; do printf "%s\\n" "$i" >"$root/$i"; '
        "i=$((i + 1)); done\n"
        'rm -rf "$root"\n'
        'exec "$@"\n',
        encoding="utf-8",
    )
    init_script.chmod(0o755)
    init_runs = max(5, bench.args.lifecycle_runs // 2)
    init_volumes: dict[str, str] = {}
    for isolation in bench.active_isolations:
        volume = bench.create_volume(isolation, "init")
        if volume:
            init_volumes[isolation] = volume
    for iteration in range(1, init_runs + 1):
        for isolation in bench.ordered_isolations(iteration):
            volume = init_volumes.get(isolation)
            if not volume:
                continue
            options = [
                "--rm",
                "--no-stdin",
                "--timeout",
                "180",
                "-e",
                f"A3S_PERF_ITERATION={iteration}",
                "-v",
                f"{init_script}:/opt/a3s-perf/init.sh:ro",
                "-v",
                f"{volume}:/state",
                "--entrypoint",
                "/opt/a3s-perf/init.sh",
            ]
            bench.measure(
                isolation,
                "volume_backed_init",
                iteration,
                bench.run_args(isolation, options, ["true"]),
            )


def capture_idle_stats(bench: BenchmarkCore, isolation: str, name: str) -> None:
    code, output = bench.capture(
        ["stats", name, "--no-stream", "--format", "json"], timeout=20
    )
    if code != 0:
        bench.record(
            isolation,
            "idle_memory",
            1,
            "fail",
            exit_code=code,
            note="stats command failed",
        )
        return
    try:
        rows = json.loads(output)
        memory = float(rows[0]["memory_bytes"])
    except (ValueError, KeyError, IndexError, TypeError):
        bench.record(
            isolation,
            "idle_memory",
            1,
            "fail",
            exit_code=1,
            note="stats JSON did not contain memory_bytes",
        )
        return
    bench.record(
        isolation,
        "idle_memory",
        1,
        "pass",
        observed_value=memory,
        observed_unit="bytes",
        exit_code=0,
    )


def start_persistent_boxes(bench: BenchmarkCore) -> None:
    for isolation in bench.active_isolations:
        base = bench.start_persistent(isolation, "base", [])
        if base:
            bench.base_boxes[isolation] = base
            time.sleep(1)
            capture_idle_stats(bench, isolation, base)

        volume = bench.create_volume(isolation, "data")
        if not volume:
            continue
        bind_dir = bench.fixture_dir / f"bind-{isolation}"
        bind_dir.mkdir(parents=True, exist_ok=True)
        if isolation == "sandbox":
            # Sandbox root is UID-mapped and must be able to write this
            # benchmark-owned external bind fixture.
            bind_dir.chmod(0o777)
        options = [
            "-v",
            f"{bind_dir}:/bench-bind",
            "-v",
            f"{volume}:/bench-volume",
            "--tmpfs",
            "/bench-tmpfs:size=768m",
        ]
        if isolation == "microvm":
            options.extend(["--virtiofs-cache", "none"])
        storage = bench.start_persistent(isolation, "storage", options)
        if storage:
            bench.storage_boxes[isolation] = storage


def exec_compute_memory(bench: BenchmarkCore) -> None:
    scenarios = [
        ("exec_noop", "true", bench.args.exec_runs, 1, "operation"),
        (
            "cpu_sha256",
            f"dd if=/dev/zero bs=1M count={bench.args.cpu_mib} 2>/dev/null "
            "| sha256sum >/dev/null",
            bench.args.workload_runs,
            bench.args.cpu_mib,
            "MiB",
        ),
        (
            "memory_zero_copy",
            f"dd if=/dev/zero of=/dev/null bs=1M "
            f"count={bench.args.memory_mib} 2>/dev/null",
            bench.args.workload_runs,
            bench.args.memory_mib,
            "MiB",
        ),
    ]
    for mechanism, script, runs, units, unit in scenarios:
        for iteration in range(1, runs + 1):
            for isolation in bench.ordered_isolations(iteration):
                name = bench.base_boxes.get(isolation)
                if name:
                    bench.measure(
                        isolation,
                        mechanism,
                        iteration,
                        bench.exec_arguments(name, script),
                        work_units=units,
                        work_unit=unit,
                    )


def storage_workloads(bench: BenchmarkCore) -> None:
    paths = {
        "rootfs": "/tmp/a3s-perf.bin",
        "tmpfs": "/bench-tmpfs/a3s-perf.bin",
        "bind": "/bench-bind/a3s-perf.bin",
        "named_volume": "/bench-volume/a3s-perf.bin",
    }
    for label, path in paths.items():
        for iteration in range(1, bench.args.workload_runs + 1):
            for isolation in bench.ordered_isolations(iteration):
                name = bench.storage_boxes.get(isolation)
                if not name:
                    continue
                effective_path = (
                    "/var/tmp/a3s-perf.bin"
                    if label == "rootfs" and isolation == "sandbox"
                    else path
                )
                script = (
                    f"rm -f {effective_path}; "
                    f"dd if=/dev/zero of={effective_path} bs=1M "
                    f"count={bench.args.io_mib} conv=fsync 2>/dev/null"
                )
                bench.measure(
                    isolation,
                    f"{label}_write",
                    iteration,
                    bench.exec_arguments(name, script),
                    work_units=bench.args.io_mib,
                    work_unit="MiB",
                )
        for isolation, name in bench.storage_boxes.items():
            effective_path = (
                "/var/tmp/a3s-perf.bin"
                if label == "rootfs" and isolation == "sandbox"
                else path
            )
            script = f"dd if={effective_path} of=/dev/null bs=1M 2>/dev/null"
            for iteration in range(1, bench.args.workload_runs + 1):
                bench.measure(
                    isolation,
                    f"{label}_warm_read",
                    iteration,
                    bench.exec_arguments(name, script),
                    work_units=bench.args.io_mib,
                    work_unit="MiB",
                )

    metadata_script = (
        "set -eu; root=/bench-bind/meta; rm -rf \"$root\"; mkdir -p \"$root\"; "
        f"i=1; while [ \"$i\" -le {bench.args.metadata_files} ]; do "
        'printf x >"$root/$i"; i=$((i + 1)); done; rm -rf "$root"'
    )
    for iteration in range(1, bench.args.workload_runs + 1):
        for isolation in bench.ordered_isolations(iteration):
            name = bench.storage_boxes.get(isolation)
            if name:
                bench.measure(
                    isolation,
                    "bind_metadata_create_delete",
                    iteration,
                    bench.exec_arguments(name, metadata_script),
                    work_units=bench.args.metadata_files,
                    work_unit="file",
                )


def network_workloads(bench: BenchmarkCore) -> None:
    bench.start_http_fixture()
    for isolation, name in bench.base_boxes.items():
        url = bench.network_url(isolation)
        probe = (
            "i=1; while [ \"$i\" -le 3 ]; do "
            f"wget -q -T 5 -O /dev/null {url}/tiny && exit 0; "
            "i=$((i + 1)); done; exit 1"
        )
        if (
            bench.command(
                bench.exec_arguments(name, probe),
                f"network-probe-{isolation}",
                timeout=30,
            )
            != 0
        ):
            bench.record(
                isolation,
                "network_probe",
                1,
                "fail",
                exit_code=1,
                note="guest could not reach host-local HTTP fixture",
            )
            continue
        for iteration in range(1, bench.args.workload_runs + 1):
            script = (
                f"i=1; retries=0; while [ \"$i\" -le "
                f"{bench.args.network_requests} ]; do "
                f"if ! wget -q -T 5 -O /dev/null {url}/tiny; then "
                "retries=$((retries + 1)); "
                f"wget -q -T 5 -O /dev/null {url}/tiny || exit 1; fi; "
                "i=$((i + 1)); done; "
                'printf "A3S_NETWORK_FIRST_ATTEMPT_FAILURES=%s\\n" "$retries"'
            )
            passed = bench.measure(
                isolation,
                "host_http_requests",
                iteration,
                bench.exec_arguments(name, script),
                work_units=bench.args.network_requests,
                work_unit="request",
            )
            record_network_retries(
                bench, isolation, "host_http_requests", iteration, passed
            )
            transfer_script = (
                f"i=1; retries=0; while [ \"$i\" -le "
                f"{bench.args.network_mib * 16} ]; do "
                f"if ! wget -q -T 30 -O /dev/null {url}/payload; then "
                "retries=$((retries + 1)); "
                f"wget -q -T 30 -O /dev/null {url}/payload || exit 1; fi; "
                "i=$((i + 1)); done; "
                'printf "A3S_NETWORK_FIRST_ATTEMPT_FAILURES=%s\\n" "$retries"'
            )
            passed = bench.measure(
                isolation,
                "host_http_64k_downloads",
                iteration,
                bench.exec_arguments(name, transfer_script),
                work_units=bench.args.network_mib,
                work_unit="MiB",
            )
            record_network_retries(
                bench, isolation, "host_http_64k_downloads", iteration, passed
            )


def record_network_retries(
    bench: BenchmarkCore,
    isolation: str,
    mechanism: str,
    iteration: int,
    command_passed: bool,
) -> None:
    if not command_passed:
        return
    log_path = bench.log_dir / f"{mechanism}-{isolation}-{iteration}.log"
    match = re.search(
        r"^A3S_NETWORK_FIRST_ATTEMPT_FAILURES=(\d+)$",
        log_path.read_text(encoding="utf-8", errors="replace"),
        flags=re.MULTILINE,
    )
    if not match:
        bench.record(
            isolation,
            f"{mechanism}_first_attempt_failures",
            iteration,
            "fail",
            exit_code=1,
            note="network reliability marker missing",
        )
        return
    failures = int(match.group(1))
    bench.record(
        isolation,
        f"{mechanism}_first_attempt_failures",
        iteration,
        "pass",
        observed_value=failures,
        observed_unit="requests",
        exit_code=0,
    )


def concurrency_matrix(bench: BenchmarkCore) -> None:
    for iteration in range(1, bench.args.concurrency_runs + 1):
        for isolation in bench.ordered_isolations(iteration):
            arguments = bench.run_args(
                isolation,
                ["--rm", "--no-stdin", "--timeout", "180"],
                ["true"],
            )
            start = time.monotonic_ns()
            with ThreadPoolExecutor(max_workers=bench.args.concurrency) as executor:
                futures = [
                    executor.submit(
                        bench.command,
                        arguments,
                        f"parallel-{isolation}-{iteration}-{slot}",
                        240,
                    )
                    for slot in range(1, bench.args.concurrency + 1)
                ]
                exits = [future.result() for future in futures]
            elapsed_ms = (time.monotonic_ns() - start) / 1_000_000
            success = all(code == 0 for code in exits)
            bench.record(
                isolation,
                f"parallel_{bench.args.concurrency}_cold_noop",
                iteration,
                "pass" if success else "fail",
                elapsed_ms=elapsed_ms,
                work_units=bench.args.concurrency,
                work_unit="operation",
                exit_code=0 if success else 1,
                note="" if success else f"exit codes: {exits}",
            )


def run_standard_workloads(bench: BenchmarkCore) -> None:
    lifecycle_matrix(bench)
    start_persistent_boxes(bench)
    exec_compute_memory(bench)
    storage_workloads(bench)
    network_workloads(bench)
    bench.stop_tracked_boxes()
    concurrency_matrix(bench)
