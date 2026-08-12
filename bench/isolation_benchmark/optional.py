"""Host-specific TEE, bridge, warm-pool, and snapshot-fork workloads."""

from __future__ import annotations

import os
import platform
import subprocess
import time
from pathlib import Path

from .core import BenchmarkCore


def tee_simulation(bench: BenchmarkCore) -> None:
    if bench.args.skip_tee or "microvm" not in bench.active_isolations:
        return
    runs = max(5, bench.args.lifecycle_runs // 2)
    for iteration in range(1, runs + 1):
        arguments = bench.run_args(
            "microvm",
            ["--rm", "--no-stdin", "--timeout", "180", "--tee-simulate"],
            ["true"],
        )
        if iteration == 1:
            started = time.monotonic_ns()
            exit_code = bench.command(
                arguments, "tee_simulated_lifecycle-microvm-1", timeout=240
            )
            elapsed_ms = (time.monotonic_ns() - started) / 1_000_000
            if exit_code != 0:
                bench.record(
                    "microvm",
                    "tee_simulated_lifecycle",
                    iteration,
                    "skip",
                    exit_code=exit_code,
                    note="TEE simulation unavailable; not hardware TEE evidence",
                )
                return
            bench.record(
                "microvm",
                "tee_simulated_lifecycle",
                iteration,
                "pass",
                elapsed_ms=elapsed_ms,
                work_units=1,
                work_unit="operation",
                exit_code=0,
            )
            continue
        if not bench.measure(
            "microvm", "tee_simulated_lifecycle", iteration, arguments
        ):
            break


def bridge_network(bench: BenchmarkCore) -> None:
    if (
        bench.args.skip_bridge
        or platform.system() != "Linux"
        or "microvm" not in bench.active_isolations
    ):
        return
    network = f"{bench.prefix}-bridge"
    subnet_octet = (os.getpid() % 150) + 50
    if (
        bench.command(
            [
                "network",
                "create",
                network,
                "--subnet",
                f"10.245.{subnet_octet}.0/24",
            ],
            "bridge-create",
        )
        != 0
    ):
        bench.record(
            "microvm",
            "bridge_network",
            0,
            "skip",
            note="bridge network creation unavailable",
        )
        return
    bench.networks.append(network)
    name = bench.start_persistent("microvm", "bridge", ["--network", network])
    url = bench.network_url("microvm", bridge=True)
    if not name or not url:
        return
    for iteration in range(1, bench.args.workload_runs + 1):
        bench.measure(
            "microvm",
            "bridge_host_http_64k_downloads",
            iteration,
            bench.exec_arguments(
                name,
                f"i=1; while [ \"$i\" -le {bench.args.network_mib * 16} ]; "
                f"do wget -q -T 30 -O /dev/null {url}/payload || exit 1; "
                "i=$((i + 1)); done",
            ),
            work_units=bench.args.network_mib,
            work_unit="MiB",
        )


def start_pool(
    bench: BenchmarkCore, mechanism: str, iteration: int, snapshot_fork: bool
) -> tuple[subprocess.Popen[bytes], Path, float, bool]:
    socket_path = Path(f"/tmp/{bench.prefix}-{mechanism}-{iteration}.sock")
    log_handle = (bench.log_dir / f"{mechanism}-{iteration}.log").open("wb")
    bench.pool_logs.append(log_handle)
    arguments = [
        bench.box,
        "pool",
        "start",
        "--image",
        bench.args.image,
        "--size",
        str(bench.args.pool_size),
        "--max",
        str(bench.args.pool_size),
        "--socket",
        str(socket_path),
    ]
    if snapshot_fork:
        arguments.append("--snapshot-fork")
    started = time.monotonic_ns()
    process = subprocess.Popen(
        arguments,
        env=bench.env,
        stdout=log_handle,
        stderr=subprocess.STDOUT,
    )
    bench.pool_processes.append(process)
    deadline = time.monotonic() + 240
    ready = False
    while time.monotonic() < deadline:
        if process.poll() is not None:
            break
        if socket_path.exists():
            code = bench.command(
                ["pool", "run", "--socket", str(socket_path), "--", "true"],
                f"{mechanism}-{iteration}-probe",
                timeout=30,
            )
            if code == 0:
                ready = True
                break
        time.sleep(0.5)
    elapsed_ms = (time.monotonic_ns() - started) / 1_000_000
    return process, socket_path, elapsed_ms, ready


def pool_matrix(bench: BenchmarkCore) -> None:
    if bench.args.skip_pool or "microvm" not in bench.active_isolations:
        return
    process, socket_path, fill_ms, ready = start_pool(
        bench, "pool-cold", 1, False
    )
    bench.record(
        "microvm",
        "pool_cold_fill",
        1,
        "pass" if ready else "fail",
        elapsed_ms=fill_ms,
        work_units=bench.args.pool_size,
        work_unit="VM",
        exit_code=0 if ready else 1,
    )
    if ready:
        for iteration in range(1, bench.args.pool_runs + 1):
            bench.measure(
                "microvm",
                "warm_pool_acquire",
                iteration,
                ["pool", "run", "--socket", str(socket_path), "--", "true"],
            )
    bench.stop_pool(process)

    for iteration in range(1, bench.args.fork_runs + 1):
        process, _socket_path, fill_ms, ready = start_pool(
            bench, "pool-snapshot-fork", iteration, True
        )
        bench.record(
            "microvm",
            "snapshot_fork_fill",
            iteration,
            "pass" if ready else "skip",
            elapsed_ms=fill_ms if ready else None,
            work_units=bench.args.pool_size if ready else None,
            work_unit="VM" if ready else "",
            exit_code=0 if ready else 1,
            note="" if ready else "snapshot-fork unavailable on this host",
        )
        bench.stop_pool(process)


def run_optional_workloads(bench: BenchmarkCore) -> None:
    tee_simulation(bench)
    bridge_network(bench)
    bench.stop_tracked_boxes()
    pool_matrix(bench)
