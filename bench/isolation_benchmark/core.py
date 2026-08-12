"""Benchmark process control, evidence recording, and cleanup."""

from __future__ import annotations

import csv
import hashlib
import json
import os
import platform
import shutil
import socket
import subprocess
import threading
import time
from datetime import datetime, timezone
from functools import partial
from http.server import SimpleHTTPRequestHandler, ThreadingHTTPServer
from pathlib import Path
from typing import IO, Any, Sequence

from isolation_report import write_summary


CSV_FIELDS = [
    "host_lane",
    "isolation",
    "mechanism",
    "iteration",
    "status",
    "elapsed_ms",
    "work_units",
    "work_unit",
    "rate_per_second",
    "rate_unit",
    "observed_value",
    "observed_unit",
    "exit_code",
    "note",
]


def parse_positive(value: str) -> int:
    parsed = int(value)
    if parsed <= 0:
        raise ValueError("value must be positive")
    return parsed


def sha256_file(path: Path) -> str:
    digest = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            digest.update(chunk)
    return digest.hexdigest()


def host_memory_bytes() -> int | None:
    if Path("/proc/meminfo").exists():
        for line in Path("/proc/meminfo").read_text(encoding="utf-8").splitlines():
            if line.startswith("MemTotal:"):
                return int(line.split()[1]) * 1024
    value = sysctl_value("hw.memsize")
    if value and value.isdigit():
        return int(value)
    return None


def sysctl_value(name: str) -> str:
    try:
        completed = subprocess.run(
            ["sysctl", "-n", name],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            timeout=5,
            check=False,
        )
    except (FileNotFoundError, subprocess.TimeoutExpired):
        return ""
    return completed.stdout.strip() if completed.returncode == 0 else ""


def host_cpu_model() -> str:
    cpuinfo = Path("/proc/cpuinfo")
    if cpuinfo.exists():
        for line in cpuinfo.read_text(encoding="utf-8", errors="replace").splitlines():
            if line.lower().startswith(("model name", "hardware")):
                return line.split(":", 1)[-1].strip()
    return (
        sysctl_value("machdep.cpu.brand_string")
        or sysctl_value("hw.model")
        or "unknown"
    )


def load_average() -> list[float]:
    try:
        return [round(value, 3) for value in os.getloadavg()]
    except OSError:
        return []


def cpu_affinity() -> list[int]:
    get_affinity = getattr(os, "sched_getaffinity", None)
    if get_affinity is None:
        return []
    try:
        return sorted(get_affinity(0))
    except OSError:
        return []


def process_nice() -> int | None:
    try:
        return os.getpriority(os.PRIO_PROCESS, 0)
    except (AttributeError, OSError):
        return None


def is_box_shim_argv0(value: str) -> bool:
    """Return whether an argv[0] path names the Box shim executable."""
    return Path(value).name == "a3s-box-shim"


def box_shim_process_count() -> int:
    """Count shims by argv[0], which remains stable after libkrun renames comm."""
    proc = Path("/proc")
    if proc.is_dir():
        count = 0
        for entry in proc.iterdir():
            if not entry.name.isdigit():
                continue
            try:
                argv0 = (entry / "cmdline").read_bytes().split(b"\0", 1)[0]
                value = os.fsdecode(argv0)
            except (FileNotFoundError, PermissionError, ProcessLookupError, OSError):
                continue
            if is_box_shim_argv0(value):
                count += 1
        return count

    try:
        completed = subprocess.run(
            ["ps", "-A", "-o", "pid=", "-o", "command="],
            stdout=subprocess.PIPE,
            stderr=subprocess.DEVNULL,
            text=True,
            check=False,
        )
    except FileNotFoundError:
        return 0
    if completed.returncode != 0:
        return 0
    count = 0
    for line in completed.stdout.splitlines():
        fields = line.strip().split(maxsplit=1)
        if len(fields) != 2:
            continue
        argv0 = fields[1].split(maxsplit=1)[0]
        if is_box_shim_argv0(argv0):
            count += 1
    return count


def primary_ipv4() -> str:
    probe = socket.socket(socket.AF_INET, socket.SOCK_DGRAM)
    try:
        probe.connect(("1.1.1.1", 53))
        return str(probe.getsockname()[0])
    finally:
        probe.close()


class QuietHandler(SimpleHTTPRequestHandler):
    def log_message(self, _format: str, *_args: object) -> None:
        return


class BenchmarkCore:
    """Own benchmark state and expose safe command/measurement primitives."""

    def __init__(self, args: Any) -> None:
        self.args = args
        self.result_dir = args.result_dir.resolve()
        self.log_dir = self.result_dir / "logs"
        self.fixture_dir = self.result_dir / "fixtures"
        self.state_dir = args.state_dir.resolve()
        self.result_dir.mkdir(parents=True, exist_ok=True)
        self.log_dir.mkdir(parents=True, exist_ok=True)
        self.fixture_dir.mkdir(parents=True, exist_ok=True)
        self.state_dir.mkdir(parents=True, exist_ok=True)
        sandbox_socket = (
            self.state_dir
            / "run"
            / "a3s-oci"
            / ("0" * 36)
            / "runtime.sock"
        )
        if (
            "sandbox" in args.isolations
            and platform.system() == "Linux"
            and len(os.fsencode(sandbox_socket)) >= 108
        ):
            raise RuntimeError(
                "Sandbox benchmark state path is too long for a Unix socket; "
                "use a short --state-dir below /tmp"
            )
        self.box = self.resolve_binary(args.box)
        self.env = os.environ.copy()
        self.env["A3S_HOME"] = str(self.state_dir)
        self.run_id = datetime.now(timezone.utc).strftime("%Y%m%dT%H%M%SZ")
        self.prefix = f"a3s-perf-{self.run_id.lower()}"
        self.csv_path = self.result_dir / "samples.csv"
        self.csv_stream: IO[str] = self.csv_path.open(
            "w", newline="", encoding="utf-8"
        )
        self.writer = csv.DictWriter(self.csv_stream, fieldnames=CSV_FIELDS)
        self.writer.writeheader()
        self.boxes: list[str] = []
        self.volumes: list[str] = []
        self.networks: list[str] = []
        self.pool_processes: list[subprocess.Popen[bytes]] = []
        self.pool_logs: list[IO[bytes]] = []
        self.active_isolations: list[str] = []
        self.base_boxes: dict[str, str] = {}
        self.storage_boxes: dict[str, str] = {}
        self.http_servers: list[ThreadingHTTPServer] = []
        self.http_threads: list[threading.Thread] = []
        self.http_urls: dict[str, str] = {}
        self.unexpected_failures = 0
        self.start_resources = self.resource_counts()

    @staticmethod
    def resolve_binary(value: str) -> str:
        candidate = shutil.which(value)
        if candidate:
            return candidate
        path = Path(value).expanduser().resolve()
        if not path.is_file() or not os.access(path, os.X_OK):
            raise RuntimeError(f"a3s-box binary is not executable: {value}")
        return str(path)

    def command(
        self, arguments: Sequence[str], log_name: str, timeout: int = 240
    ) -> int:
        log_path = self.log_dir / f"{log_name}.log"
        with log_path.open("wb") as log:
            try:
                completed = subprocess.run(
                    [self.box, *arguments],
                    env=self.env,
                    stdout=log,
                    stderr=subprocess.STDOUT,
                    timeout=timeout,
                    check=False,
                )
                return completed.returncode
            except subprocess.TimeoutExpired:
                log.write(f"\nbenchmark timeout after {timeout}s\n".encode())
                return 124

    def capture(self, arguments: Sequence[str], timeout: int = 30) -> tuple[int, str]:
        try:
            completed = subprocess.run(
                [self.box, *arguments],
                env=self.env,
                stdout=subprocess.PIPE,
                stderr=subprocess.STDOUT,
                text=True,
                timeout=timeout,
                check=False,
            )
            return completed.returncode, completed.stdout
        except subprocess.TimeoutExpired:
            return 124, f"benchmark timeout after {timeout}s"

    def record(
        self,
        isolation: str,
        mechanism: str,
        iteration: int,
        status: str,
        *,
        elapsed_ms: float | None = None,
        work_units: float | None = None,
        work_unit: str = "",
        observed_value: float | None = None,
        observed_unit: str = "",
        exit_code: int | None = None,
        note: str = "",
    ) -> None:
        rate: float | None = None
        rate_unit = ""
        if elapsed_ms and work_units is not None and work_unit:
            rate = work_units / (elapsed_ms / 1000)
            rate_unit = {
                "operation": "operations/s",
                "MiB": "MiB/s",
                "request": "requests/s",
                "file": "files/s",
                "VM": "VMs/s",
            }.get(work_unit, f"{work_unit}/s")
        self.writer.writerow(
            {
                "host_lane": self.args.host_lane,
                "isolation": isolation,
                "mechanism": mechanism,
                "iteration": iteration,
                "status": status,
                "elapsed_ms": "" if elapsed_ms is None else f"{elapsed_ms:.3f}",
                "work_units": "" if work_units is None else f"{work_units:.3f}",
                "work_unit": work_unit,
                "rate_per_second": "" if rate is None else f"{rate:.6f}",
                "rate_unit": rate_unit,
                "observed_value": (
                    "" if observed_value is None else f"{observed_value:.3f}"
                ),
                "observed_unit": observed_unit,
                "exit_code": "" if exit_code is None else exit_code,
                "note": note,
            }
        )
        self.csv_stream.flush()
        if status == "fail":
            self.unexpected_failures += 1

    def measure(
        self,
        isolation: str,
        mechanism: str,
        iteration: int,
        arguments: Sequence[str],
        *,
        work_units: float = 1,
        work_unit: str = "operation",
        timeout: int = 240,
    ) -> bool:
        start = time.monotonic_ns()
        exit_code = self.command(
            arguments, f"{mechanism}-{isolation}-{iteration}", timeout
        )
        elapsed_ms = (time.monotonic_ns() - start) / 1_000_000
        status = "pass" if exit_code == 0 else "fail"
        self.record(
            isolation,
            mechanism,
            iteration,
            status,
            elapsed_ms=elapsed_ms,
            work_units=work_units,
            work_unit=work_unit,
            exit_code=exit_code,
            note="" if exit_code == 0 else "command failed; inspect sample log",
        )
        return exit_code == 0

    def isolation_args(self, isolation: str) -> list[str]:
        return ["--isolation", "sandbox"] if isolation == "sandbox" else []

    def run_args(
        self, isolation: str, options: Sequence[str], command: Sequence[str]
    ) -> list[str]:
        return [
            "run",
            *self.isolation_args(isolation),
            *options,
            self.args.image,
            "--",
            *command,
        ]

    def ordered_isolations(self, iteration: int) -> list[str]:
        ordered = list(self.active_isolations)
        if iteration % 2 == 0:
            ordered.reverse()
        return ordered

    def prepare_image(self) -> None:
        if self.args.image_tar:
            code = self.command(
                [
                    "load",
                    "--input",
                    str(self.args.image_tar.resolve()),
                    "--tag",
                    self.args.image,
                ],
                "image-load",
                timeout=300,
            )
        else:
            code = self.command(["pull", self.args.image], "image-pull", timeout=300)
        if code != 0:
            raise RuntimeError("failed to prepare benchmark image")

    def probe_isolations(self) -> None:
        for isolation in self.args.isolations:
            ok = self.measure(
                isolation,
                "preflight_noop",
                1,
                self.run_args(
                    isolation,
                    ["--rm", "--no-stdin", "--timeout", "180"],
                    ["true"],
                ),
            )
            if ok:
                self.active_isolations.append(isolation)

    def create_volume(self, isolation: str, role: str) -> str | None:
        volume = f"{self.prefix}-{isolation}-{role}"
        if self.command(["volume", "create", volume], f"create-{volume}") != 0:
            self.record(
                isolation,
                f"{role}_setup",
                0,
                "fail",
                exit_code=1,
                note="failed to create named volume",
            )
            return None
        self.volumes.append(volume)
        return volume

    def start_persistent(
        self, isolation: str, role: str, options: Sequence[str]
    ) -> str | None:
        name = f"{self.prefix}-{isolation}-{role}"
        self.boxes.append(name)
        arguments = self.run_args(
            isolation,
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
            ["sleep", "3600"],
        )
        if self.command(arguments, f"start-{name}", timeout=240) != 0:
            self.record(
                isolation,
                f"{role}_setup",
                0,
                "fail",
                exit_code=1,
                note="persistent box failed to start",
            )
            return None
        return name

    def exec_arguments(self, name: str, script: str) -> list[str]:
        return ["exec", "--timeout", "180", name, "--", "sh", "-c", script]

    def start_http_fixture(self) -> None:
        network_dir = self.fixture_dir / "network"
        network_dir.mkdir(parents=True, exist_ok=True)
        (network_dir / "tiny").write_bytes(b"ok\n")
        large = network_dir / "payload"
        large.write_bytes(b"\0" * 64 * 1024)
        handler = partial(QuietHandler, directory=str(network_dir))
        addresses = {
            "tsi": "127.0.0.1",
            "routed": self.args.host_ip or primary_ipv4(),
        }
        for role, address in addresses.items():
            if role == "routed" and address == addresses["tsi"]:
                self.http_urls[role] = self.http_urls["tsi"]
                continue
            server = ThreadingHTTPServer((address, 0), handler)
            server.daemon_threads = True
            thread = threading.Thread(
                target=server.serve_forever,
                name=f"a3s-perf-http-{role}",
                daemon=True,
            )
            thread.start()
            self.http_servers.append(server)
            self.http_threads.append(thread)
            self.http_urls[role] = (
                f"http://{address}:{server.server_address[1]}"
            )

    def network_url(self, isolation: str, *, bridge: bool = False) -> str:
        if isolation == "microvm" and not bridge:
            return self.http_urls.get("tsi", "")
        return self.http_urls.get("routed", "")

    def stop_tracked_boxes(self) -> None:
        for name in reversed(self.boxes):
            self.command(["rm", "-f", name], f"cleanup-box-{name}", timeout=60)
        self.boxes.clear()
        self.base_boxes.clear()
        self.storage_boxes.clear()

    def resource_counts(self) -> dict[str, int]:
        boxes = self.state_dir / "boxes"
        box_dirs = len(list(boxes.iterdir())) if boxes.is_dir() else 0
        return {"shims": box_shim_process_count(), "box_dirs": box_dirs}

    def metadata(self) -> dict[str, Any]:
        version_code, version = self.capture(["--version"])
        image_hash = (
            sha256_file(self.args.image_tar.resolve())
            if self.args.image_tar
            else self.args.image_sha256
        )
        runtime_artifacts: dict[str, dict[str, str]] = {}
        for role, environment_key in (
            ("runtime", "A3S_BOX_OCI_RUNTIME_PATH"),
            ("agent", "A3S_BOX_OCI_AGENT_PATH"),
        ):
            configured = self.env.get(environment_key, "")
            artifact = Path(configured).expanduser() if configured else None
            if artifact and artifact.is_file():
                runtime_artifacts[role] = {
                    "path": str(artifact.resolve()),
                    "sha256": sha256_file(artifact),
                }
        return {
            "schema": "a3s.box.isolation-mechanisms.v1",
            "run_id": self.run_id,
            "started_at": datetime.now(timezone.utc).isoformat(),
            "host_lane": self.args.host_lane,
            "host": {
                "system": platform.system(),
                "release": platform.release(),
                "machine": platform.machine(),
                "cpu_model": host_cpu_model(),
                "cpu_count": os.cpu_count(),
                "cpu_affinity": cpu_affinity(),
                "memory_bytes": host_memory_bytes(),
                "load_average": load_average(),
                "process_nice": process_nice(),
            },
            "box": {
                "binary": self.box,
                "sha256": sha256_file(Path(self.box)),
                "version": version.strip() if version_code == 0 else "unknown",
                "source_commit": self.args.source_commit,
            },
            "oci_runtime_commit": self.args.oci_runtime_commit,
            "oci_runtime_artifacts": runtime_artifacts,
            "image": self.args.image,
            "image_sha256": image_hash,
            "isolations_requested": self.args.isolations,
            "benchmark_mode": (
                "sandbox_persistent"
                if getattr(self.args, "sandbox_persistent", False)
                else "standard"
            ),
            "parameters": {
                key: getattr(self.args, key)
                for key in (
                    "lifecycle_runs",
                    "exec_runs",
                    "workload_runs",
                    "concurrency_runs",
                    "concurrency",
                    "cpu_mib",
                    "memory_mib",
                    "io_mib",
                    "metadata_files",
                    "network_requests",
                    "network_mib",
                    "pool_size",
                    "pool_runs",
                    "fork_runs",
                )
            },
            "start_resources": self.start_resources,
        }

    def stop_pool(self, process: subprocess.Popen[bytes]) -> None:
        if process.poll() is None:
            process.terminate()
            try:
                process.wait(timeout=30)
            except subprocess.TimeoutExpired:
                process.kill()
                process.wait(timeout=10)

    def cleanup(self) -> None:
        for process in self.pool_processes:
            self.stop_pool(process)
        self.stop_tracked_boxes()
        for network in reversed(self.networks):
            self.command(
                ["network", "rm", "-f", network],
                f"cleanup-network-{network}",
                timeout=60,
            )
        for volume in reversed(self.volumes):
            self.command(
                ["volume", "rm", "-f", volume],
                f"cleanup-volume-{volume}",
                timeout=60,
            )
        for server in self.http_servers:
            server.shutdown()
            server.server_close()
        for handle in self.pool_logs:
            handle.close()

    def finish(self, metadata: dict[str, Any]) -> int:
        self.csv_stream.close()
        metadata["finished_at"] = datetime.now(timezone.utc).isoformat()
        metadata["finished_load_average"] = load_average()
        metadata["active_isolations"] = self.active_isolations
        metadata["unexpected_failures"] = self.unexpected_failures
        metadata["final_resources"] = self.resource_counts()
        metadata["resource_delta"] = {
            key: metadata["final_resources"][key] - self.start_resources[key]
            for key in self.start_resources
        }
        (self.result_dir / "metadata.json").write_text(
            json.dumps(metadata, indent=2, sort_keys=True) + "\n",
            encoding="utf-8",
        )
        write_summary(
            [self.csv_path],
            self.result_dir / "summary.json",
            self.result_dir / "summary.md",
        )
        leaked = any(delta > 0 for delta in metadata["resource_delta"].values())
        return 1 if self.unexpected_failures or leaked else 0
