"""Persistent-lifecycle workloads for the shared-kernel Sandbox backend."""

from __future__ import annotations

import time
from concurrent.futures import ThreadPoolExecutor
from typing import Sequence

from .core import BenchmarkCore
from .workloads import exec_compute_memory, start_persistent_boxes, storage_workloads


def start_named(
    bench: BenchmarkCore,
    name: str,
    mechanism: str,
    iteration: int,
    *,
    options: Sequence[str] = (),
    command: Sequence[str] = ("sleep", "3600"),
) -> bool:
    """Start and track one detached Sandbox while recording start latency."""
    bench.boxes.append(name)
    return bench.measure(
        "sandbox",
        mechanism,
        iteration,
        bench.run_args(
            "sandbox",
            [
                "-d",
                "--name",
                name,
                "--cpus",
                "2",
                "--memory",
                "1g",
                *options,
            ],
            command,
        ),
    )


def remove_named(
    bench: BenchmarkCore,
    name: str,
    mechanism: str,
    iteration: int,
) -> bool:
    """Remove one tracked Sandbox and stop tracking it after confirmed cleanup."""
    removed = bench.measure(
        "sandbox",
        mechanism,
        iteration,
        ["rm", "-f", name],
    )
    if removed and name in bench.boxes:
        bench.boxes.remove(name)
    return removed


def persistent_preflight(bench: BenchmarkCore) -> None:
    name = f"{bench.prefix}-sandbox-preflight"
    started = start_named(
        bench,
        name,
        "persistent_preflight_start",
        1,
    )
    removed = (
        remove_named(bench, name, "persistent_preflight_remove", 1)
        if started
        else False
    )
    if not started or not removed:
        raise RuntimeError("persistent Sandbox lifecycle failed preflight")
    bench.active_isolations.append("sandbox")


def persistent_lifecycle_matrix(bench: BenchmarkCore) -> None:
    for iteration in range(1, bench.args.lifecycle_runs + 1):
        name = f"{bench.prefix}-sandbox-cycle-{iteration}"
        if start_named(
            bench,
            name,
            "detached_start",
            iteration,
        ):
            remove_named(bench, name, "detached_remove", iteration)


def persistent_init_matrix(bench: BenchmarkCore) -> None:
    init_script = bench.fixture_dir / "sandbox-init.sh"
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
    volume = bench.create_volume("sandbox", "persistent-init")
    if not volume:
        return
    runs = max(5, bench.args.lifecycle_runs // 2)
    for iteration in range(1, runs + 1):
        name = f"{bench.prefix}-sandbox-init-{iteration}"
        options = [
            "-e",
            f"A3S_PERF_ITERATION={iteration}",
            "-v",
            f"{init_script}:/opt/a3s-perf/init.sh:ro",
            "-v",
            f"{volume}:/state",
            "--entrypoint",
            "/opt/a3s-perf/init.sh",
        ]
        if start_named(
            bench,
            name,
            "volume_backed_init_start",
            iteration,
            options=options,
        ):
            remove_named(
                bench,
                name,
                "volume_backed_init_remove",
                iteration,
            )


def persistent_concurrency_matrix(bench: BenchmarkCore) -> None:
    for iteration in range(1, bench.args.concurrency_runs + 1):
        names = [
            f"{bench.prefix}-sandbox-parallel-{iteration}-{slot}"
            for slot in range(1, bench.args.concurrency + 1)
        ]
        arguments = [
            bench.run_args(
                "sandbox",
                [
                    "-d",
                    "--name",
                    name,
                    "--cpus",
                    "2",
                    "--memory",
                    "1g",
                ],
                ["sleep", "3600"],
            )
            for name in names
        ]
        bench.boxes.extend(names)
        started_at = time.monotonic_ns()
        with ThreadPoolExecutor(max_workers=bench.args.concurrency) as executor:
            futures = [
                executor.submit(
                    bench.command,
                    argument,
                    f"parallel-detached-start-sandbox-{iteration}-{slot}",
                    240,
                )
                for slot, argument in enumerate(arguments, start=1)
            ]
            exits = [future.result() for future in futures]
        elapsed_ms = (time.monotonic_ns() - started_at) / 1_000_000
        started_names = [
            name for name, exit_code in zip(names, exits) if exit_code == 0
        ]
        bench.record(
            "sandbox",
            f"parallel_{bench.args.concurrency}_detached_start",
            iteration,
            "pass" if len(started_names) == len(names) else "fail",
            elapsed_ms=elapsed_ms,
            work_units=len(started_names),
            work_unit="operation",
            exit_code=0 if len(started_names) == len(names) else 1,
            note="" if len(started_names) == len(names) else f"exit codes: {exits}",
        )
        if not started_names:
            bench.record(
                "sandbox",
                f"parallel_{bench.args.concurrency}_detached_remove",
                iteration,
                "skip",
                note="no detached Sandbox started",
            )
            continue

        removed_at = time.monotonic_ns()
        with ThreadPoolExecutor(max_workers=bench.args.concurrency) as executor:
            futures = [
                executor.submit(
                    bench.command,
                    ["rm", "-f", name],
                    f"parallel-detached-remove-sandbox-{iteration}-{slot}",
                    120,
                )
                for slot, name in enumerate(started_names, start=1)
            ]
            remove_exits = [future.result() for future in futures]
        remove_elapsed_ms = (time.monotonic_ns() - removed_at) / 1_000_000
        removed_names = [
            name
            for name, exit_code in zip(started_names, remove_exits)
            if exit_code == 0
        ]
        for name in removed_names:
            if name in bench.boxes:
                bench.boxes.remove(name)
        bench.record(
            "sandbox",
            f"parallel_{bench.args.concurrency}_detached_remove",
            iteration,
            "pass" if len(removed_names) == len(started_names) else "fail",
            elapsed_ms=remove_elapsed_ms,
            work_units=len(removed_names),
            work_unit="operation",
            exit_code=0 if len(removed_names) == len(started_names) else 1,
            note=(
                ""
                if len(removed_names) == len(started_names)
                else f"exit codes: {remove_exits}"
            ),
        )


def record_network_boundary(bench: BenchmarkCore) -> None:
    bench.record(
        "sandbox",
        "host_network",
        0,
        "skip",
        note=(
            "current Sandbox networking exposes loopback only; TSI and named "
            "bridge networking are MicroVM-only"
        ),
    )


def run_persistent_sandbox_workloads(bench: BenchmarkCore) -> None:
    """Run only Sandbox mechanisms supported by a detached persistent box."""
    persistent_preflight(bench)
    persistent_lifecycle_matrix(bench)
    persistent_init_matrix(bench)
    start_persistent_boxes(bench)
    exec_compute_memory(bench)
    storage_workloads(bench)
    record_network_boundary(bench)
    bench.stop_tracked_boxes()
    persistent_concurrency_matrix(bench)
