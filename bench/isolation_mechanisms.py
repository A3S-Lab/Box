#!/usr/bin/env python3
"""Benchmark A3S Box isolation levels and runtime mechanisms end to end."""

from __future__ import annotations

import argparse
import sys
from pathlib import Path

from isolation_benchmark.core import BenchmarkCore, parse_positive
from isolation_benchmark.optional import run_optional_workloads
from isolation_benchmark.sandbox import run_persistent_sandbox_workloads
from isolation_benchmark.workloads import run_standard_workloads


def parse_args() -> argparse.Namespace:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("--box", default="a3s-box", help="a3s-box binary")
    parser.add_argument("--image", default="docker.io/library/alpine:3.22")
    parser.add_argument("--image-tar", type=Path)
    parser.add_argument("--image-sha256", default="")
    parser.add_argument("--result-dir", type=Path, required=True)
    parser.add_argument("--state-dir", type=Path, required=True)
    parser.add_argument("--host-lane", required=True)
    parser.add_argument("--source-commit", default="")
    parser.add_argument("--oci-runtime-commit", default="")
    parser.add_argument(
        "--isolations",
        default="microvm,sandbox",
        help="Comma-separated: microvm,sandbox",
    )
    parser.add_argument("--host-ip", default="")
    parser.add_argument("--lifecycle-runs", type=parse_positive, default=20)
    parser.add_argument("--exec-runs", type=parse_positive, default=30)
    parser.add_argument("--workload-runs", type=parse_positive, default=5)
    parser.add_argument("--concurrency-runs", type=parse_positive, default=5)
    parser.add_argument("--concurrency", type=parse_positive, default=4)
    parser.add_argument("--cpu-mib", type=parse_positive, default=256)
    parser.add_argument("--memory-mib", type=parse_positive, default=512)
    parser.add_argument("--io-mib", type=parse_positive, default=256)
    parser.add_argument("--metadata-files", type=parse_positive, default=2000)
    parser.add_argument("--network-requests", type=parse_positive, default=50)
    parser.add_argument("--network-mib", type=parse_positive, default=64)
    parser.add_argument("--pool-size", type=parse_positive, default=4)
    parser.add_argument("--pool-runs", type=parse_positive, default=20)
    parser.add_argument("--fork-runs", type=parse_positive, default=3)
    parser.add_argument("--skip-tee", action="store_true")
    parser.add_argument("--skip-bridge", action="store_true")
    parser.add_argument("--skip-pool", action="store_true")
    parser.add_argument(
        "--sandbox-persistent",
        action="store_true",
        help=(
            "Benchmark the supported detached Sandbox lifecycle and persistent "
            "exec/storage paths; requires --isolations sandbox"
        ),
    )
    args = parser.parse_args()
    args.isolations = [
        value.strip() for value in args.isolations.split(",") if value.strip()
    ]
    unsupported = set(args.isolations) - {"microvm", "sandbox"}
    if unsupported:
        parser.error(f"unsupported isolation values: {sorted(unsupported)}")
    if args.sandbox_persistent and args.isolations != ["sandbox"]:
        parser.error("--sandbox-persistent requires --isolations sandbox")
    return args


def run(args: argparse.Namespace) -> int:
    benchmark = BenchmarkCore(args)
    metadata = benchmark.metadata()
    failure: Exception | None = None
    try:
        benchmark.prepare_image()
        if args.sandbox_persistent:
            run_persistent_sandbox_workloads(benchmark)
        else:
            benchmark.probe_isolations()
            if not benchmark.active_isolations:
                raise RuntimeError("no requested isolation level passed preflight")
            run_standard_workloads(benchmark)
            run_optional_workloads(benchmark)
    except Exception as error:
        failure = error
        benchmark.record(
            "harness",
            "benchmark_run",
            0,
            "fail",
            exit_code=1,
            note=str(error),
        )
    finally:
        benchmark.cleanup()
        result = benchmark.finish(metadata)
    if failure is not None:
        raise failure
    return result


def main() -> int:
    args = parse_args()
    try:
        return run(args)
    except Exception as error:
        print(f"benchmark failed: {error}", file=sys.stderr)
        return 1


if __name__ == "__main__":
    raise SystemExit(main())
