#!/usr/bin/env bash
#
# Exercise the zero-configuration Rust, Python, TypeScript, and Go local SDKs
# against one real A3S Box isolation backend.

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE="$REPO_ROOT/src"
ISOLATION="${1:-microvm}"
PYTHON="${PYTHON:-python3}"
GO="${GO:-go}"
A3S_BOX_BINARY="${A3S_BOX_BINARY:-$WORKSPACE/target/debug/a3s-box}"
CARGO_PROFILE="${A3S_BOX_SDK_CARGO_PROFILE:-debug}"
BINARY_DIR="$(cd "$(dirname "$A3S_BOX_BINARY")" && pwd)"
RUST_MIN_STACK="${RUST_MIN_STACK:-16777216}"

case "$ISOLATION" in
    microvm|sandbox) ;;
    *)
        echo "usage: scripts/local-sdk-smoke.sh [microvm|sandbox]" >&2
        exit 2
        ;;
esac

case "$CARGO_PROFILE" in
    debug)
        cargo_release=0
        ;;
    release)
        cargo_release=1
        ;;
    *)
        echo "A3S_BOX_SDK_CARGO_PROFILE must be debug or release" >&2
        exit 2
        ;;
esac

if [ ! -x "$A3S_BOX_BINARY" ]; then
    echo "A3S Box binary is not executable: $A3S_BOX_BINARY" >&2
    exit 1
fi

case "$(uname -m)" in
    arm64|aarch64)
        guest_target="aarch64-unknown-linux-musl"
        ;;
    x86_64|amd64)
        guest_target="x86_64-unknown-linux-musl"
        ;;
    *)
        echo "unsupported host architecture: $(uname -m)" >&2
        exit 1
        ;;
esac

A3S_BOX_SHIM_BINARY="${A3S_BOX_SHIM_BINARY:-$BINARY_DIR/a3s-box-shim}"
if [ -n "${A3S_BOX_GUEST_INIT_BINARY:-}" ]; then
    guest_init="$A3S_BOX_GUEST_INIT_BINARY"
elif [ -x "$BINARY_DIR/a3s-box-guest-init" ]; then
    guest_init="$BINARY_DIR/a3s-box-guest-init"
elif [ -x "$WORKSPACE/target/$guest_target/release/a3s-box-guest-init" ]; then
    guest_init="$WORKSPACE/target/$guest_target/release/a3s-box-guest-init"
else
    guest_init="$WORKSPACE/target/$guest_target/debug/a3s-box-guest-init"
fi

if [ ! -x "$A3S_BOX_SHIM_BINARY" ]; then
    echo "matching A3S Box shim is not executable: $A3S_BOX_SHIM_BINARY" >&2
    exit 1
fi
if [ ! -x "$guest_init" ]; then
    echo "matching Linux guest init is not executable: $guest_init" >&2
    exit 1
fi

if [ -z "${A3S_HOME:-}" ] ||
    [[ "$(basename "$A3S_HOME")" != *local-sdk-smoke* ]]; then
    echo "A3S_HOME must point to a dedicated directory whose name contains local-sdk-smoke" >&2
    exit 1
fi

mkdir -p "$A3S_HOME/bin"
install -m 755 "$A3S_BOX_SHIM_BINARY" "$A3S_HOME/bin/a3s-box-shim"
install -m 755 "$guest_init" "$A3S_HOME/bin/a3s-box-guest-init"

echo "==> Rust SDK ($ISOLATION)"
(
    cd "$WORKSPACE"
    if [ "$cargo_release" -eq 1 ]; then
        A3S_BOX_SDK_LOCAL_SMOKE=1 \
            A3S_BOX_SDK_SMOKE_ISOLATION="$ISOLATION" \
            RUST_MIN_STACK="$RUST_MIN_STACK" \
            cargo test --locked --release -p a3s-box-sdk --test local_sandbox \
            local_sandbox_exercises_real_runtime \
            -- --ignored --nocapture --test-threads=1
    else
        A3S_BOX_SDK_LOCAL_SMOKE=1 \
            A3S_BOX_SDK_SMOKE_ISOLATION="$ISOLATION" \
            RUST_MIN_STACK="$RUST_MIN_STACK" \
            cargo test --locked -p a3s-box-sdk --test local_sandbox \
            local_sandbox_exercises_real_runtime \
            -- --ignored --nocapture --test-threads=1
    fi
)

echo "==> Python SDK ($ISOLATION)"
A3S_BOX_BINARY="$A3S_BOX_BINARY" \
    A3S_BOX_SDK_SMOKE_ISOLATION="$ISOLATION" \
    PYTHONPATH="$REPO_ROOT/sdk/python/src" \
    "$PYTHON" - <<'PY'
import asyncio
import json
import os
import shutil
import signal
import stat
import time
from pathlib import Path

from a3s_box import A3SBoxClient, AsyncSandbox, ExecutionResourceUpdate, Sandbox


def read_private_json(path: Path) -> dict:
    metadata = path.lstat()
    assert stat.S_ISREG(metadata.st_mode), f"{path} is not a regular file"
    assert not path.is_symlink(), f"{path} is a symlink"
    assert metadata.st_uid == 0, f"{path} is not root-owned"
    assert stat.S_IMODE(metadata.st_mode) == 0o600, f"{path} is not mode 0600"
    payload = path.read_bytes()
    assert len(payload) <= 64 * 1024, f"{path} exceeds the evidence bound"
    value = json.loads(payload)
    assert isinstance(value, dict), f"{path} does not contain a JSON object"
    return value


def process_start_time(pid: int) -> int | None:
    try:
        raw = Path(f"/proc/{pid}/stat").read_text(encoding="utf-8")
    except FileNotFoundError:
        return None
    closing = raw.rfind(") ")
    assert closing >= 0, f"process {pid} has malformed stat evidence"
    fields = raw[closing + 2 :].split()
    assert len(fields) > 19, f"process {pid} has incomplete stat evidence"
    assert len(fields[0]) == 1, f"process {pid} has invalid state evidence"
    if fields[0] in {"Z", "X", "x"}:
        return None
    return int(fields[19])


def require_live_identity(label: str, identity: dict) -> tuple[int, int]:
    pid = int(identity["pid"])
    start_time = int(identity["startTimeTicks"])
    assert pid > 0 and start_time > 0, f"{label} has an invalid process identity"
    assert process_start_time(pid) == start_time, f"{label} is not alive with its recorded identity"
    return pid, start_time


def wait_identity_gone(label: str, identity: tuple[int, int]) -> None:
    deadline = time.monotonic() + 10
    while time.monotonic() < deadline:
        observed = process_start_time(identity[0])
        if observed is None or observed != identity[1]:
            return
        time.sleep(0.025)
    raise AssertionError(f"{label} remained alive after OCI owner SIGKILL")


def load_owner_record(host_root: Path) -> dict:
    root_metadata = host_root.lstat()
    assert stat.S_ISDIR(root_metadata.st_mode), f"{host_root} is not a directory"
    assert root_metadata.st_uid == 0, f"{host_root} is not root-owned"
    assert stat.S_IMODE(root_metadata.st_mode) == 0o700, f"{host_root} is not mode 0700"
    record = read_private_json(host_root / "box-owner.json")
    assert set(record) == {
        "schema",
        "pid",
        "pid_start_time",
        "runtime_path",
        "runtime_sha256",
        "agent_path",
        "agent_sha256",
        "socket_path",
    }
    assert record["schema"] == "a3s.box.native-linux-oci-owner.v1"
    assert Path(record["runtime_path"]).resolve() == Path(os.environ["A3S_BOX_OCI_RUNTIME_PATH"]).resolve()
    assert Path(record["agent_path"]).resolve() == Path(os.environ["A3S_BOX_OCI_AGENT_PATH"]).resolve()
    assert Path(record["socket_path"]) == host_root / "runtime.sock"
    for field in ("runtime_sha256", "agent_sha256"):
        digest = record[field]
        assert len(digest) == 64 and all(character in "0123456789abcdef" for character in digest)
    owner_identity = {
        "pid": record["pid"],
        "startTimeTicks": record["pid_start_time"],
    }
    require_live_identity("OCI owner", owner_identity)
    socket_metadata = (host_root / "runtime.sock").lstat()
    assert stat.S_ISSOCK(socket_metadata.st_mode), "OCI endpoint is not a Unix socket"
    assert socket_metadata.st_uid == 0, "OCI endpoint is not root-owned"
    return record


def executor_root(host_root: Path, owner: dict) -> Path:
    return host_root / "executor" / (
        f"a3s-oci-agent-{int(owner['pid'])}-{int(owner['pid_start_time']):016x}"
    )


def load_runtime_record(host_root: Path, container_id: str) -> tuple[Path, dict]:
    path = host_root / "state" / "containers" / container_id / "record.json"
    record = read_private_json(path)
    assert record["schemaVersion"] == "a3s.oci.container-record.v1"
    assert record["id"] == container_id
    assert record["record"]["state"]["id"] == container_id
    return path, record


def load_recovery_record(host_root: Path, owner: dict, container_id: str) -> tuple[Path, dict]:
    root = executor_root(host_root, owner)
    candidates = list(root.glob("c-*/recovery.json"))
    assert len(candidates) == 1, f"expected one live recovery record below {root}, found {len(candidates)}"
    recovery = read_private_json(candidates[0])
    assert recovery["schemaVersion"] == "a3s.oci.native-linux-recovery.v1"
    assert recovery["target"]["id"] == container_id
    assert int(recovery["owner"]["pid"]) == int(owner["pid"])
    assert int(recovery["owner"]["startTimeTicks"]) == int(owner["pid_start_time"])
    return candidates[0], recovery


def exercise_owner_death_recovery(sandbox: Sandbox) -> None:
    host_root = Path(os.environ["A3S_BOX_OCI_HOST_ROOT"])
    container_id = f"a3s-box-{sandbox.id}"
    old_box_generation = sandbox.generation
    old_owner = load_owner_record(host_root)
    old_owner_identity = (int(old_owner["pid"]), int(old_owner["pid_start_time"]))
    old_socket_descriptor = os.open(
        host_root / "runtime.sock",
        os.O_PATH | os.O_CLOEXEC | os.O_NOFOLLOW,
    )
    old_socket_metadata = os.fstat(old_socket_descriptor)
    old_socket_identity = (old_socket_metadata.st_dev, old_socket_metadata.st_ino)
    old_executor_root = executor_root(host_root, old_owner)
    runtime_path, runtime = load_runtime_record(host_root, container_id)
    recovery_path, recovery = load_recovery_record(host_root, old_owner, container_id)
    old_runtime_generation = int(runtime["record"]["generation"])
    assert runtime["record"]["state"]["status"] == "running"
    assert int(runtime["record"]["state"]["pid"]) == int(recovery["init"]["pid"])
    assert int(recovery["target"]["generation"]) == old_runtime_generation
    old_launcher = require_live_identity("old OCI launcher", recovery["launcher"])
    old_init = require_live_identity("old OCI init", recovery["init"])
    assert recovery_path.is_file()

    os.kill(old_owner_identity[0], signal.SIGKILL)
    wait_identity_gone("old OCI owner", old_owner_identity)
    wait_identity_gone("old OCI launcher", old_launcher)
    wait_identity_gone("old OCI init", old_init)

    # Every synchronous SDK request launches a distinct `a3s-box sdk-bridge`
    # process. This inspection therefore forces a fresh Box process to reclaim
    # the stale endpoint and reconcile the stopped OCI tombstone.
    assert not sandbox.is_running(), "owner-death reconciliation still reports the Sandbox running"
    assert sandbox.state == "stopped", "owner-death reconciliation did not persist stopped"
    assert sandbox.generation == old_box_generation, "owner-death changed the Box generation"

    new_owner = load_owner_record(host_root)
    new_owner_identity = (int(new_owner["pid"]), int(new_owner["pid_start_time"]))
    assert new_owner_identity != old_owner_identity, "replacement OCI owner reused the old identity"
    for field in ("runtime_path", "runtime_sha256", "agent_path", "agent_sha256", "socket_path"):
        assert new_owner[field] == old_owner[field], f"replacement OCI owner changed {field}"
    new_socket_metadata = (host_root / "runtime.sock").stat()
    new_socket_identity = (new_socket_metadata.st_dev, new_socket_metadata.st_ino)
    assert new_socket_identity != old_socket_identity, "stale OCI socket was not rebound"
    os.close(old_socket_descriptor)
    assert not runtime_path.exists(), "stopped OCI generation remained after Box reconciliation"
    assert not old_executor_root.exists(), "old OCI executor root remained after stopped-only delete"

    # A second fresh Box process must build exactly the next Box and OCI
    # generations rather than replaying the terminated workload.
    sandbox.restart(
        operation_id=f"python-owner-death-restart-{sandbox.id}",
        stop_timeout=5,
    )
    assert sandbox.generation == old_box_generation + 1
    assert sandbox.is_running(), "replacement Sandbox generation is not running"
    new_runtime_path, new_runtime = load_runtime_record(host_root, container_id)
    _, new_recovery = load_recovery_record(host_root, new_owner, container_id)
    new_runtime_generation = int(new_runtime["record"]["generation"])
    assert new_runtime["record"]["state"]["status"] == "running"
    assert new_runtime_generation == old_runtime_generation + 1
    assert int(new_recovery["target"]["generation"]) == new_runtime_generation
    new_init = require_live_identity("replacement OCI init", new_recovery["init"])
    assert new_init != old_init, "replacement Sandbox reused the terminated init identity"
    assert new_runtime_path.is_file()

    report = {
        "schema_version": "a3s.box.native-linux-owner-recovery.v1",
        "status": "available",
        "platform": "linux",
        "sandbox_id": sandbox.id,
        "runtime_container_id": container_id,
        "old_box_generation": old_box_generation,
        "new_box_generation": sandbox.generation,
        "old_runtime_generation": old_runtime_generation,
        "new_runtime_generation": new_runtime_generation,
        "old_owner": {"pid": old_owner_identity[0], "start_time_ticks": old_owner_identity[1]},
        "new_owner": {"pid": new_owner_identity[0], "start_time_ticks": new_owner_identity[1]},
        "old_init": {"pid": old_init[0], "start_time_ticks": old_init[1]},
        "new_init": {"pid": new_init[0], "start_time_ticks": new_init[1]},
        "old_owner_gone": True,
        "old_launcher_gone": True,
        "old_init_gone": True,
        "socket_rebound": True,
        "stopped_without_invented_exit_status": True,
        "old_generation_deleted": True,
        "old_executor_root_removed": True,
        "replacement_generation_running": True,
    }
    report_path = os.environ.get("A3S_BOX_OCI_OWNER_RECOVERY_REPORT")
    if report_path:
        destination = Path(report_path)
        destination.parent.mkdir(parents=True, exist_ok=True)
        destination.write_text(json.dumps(report, indent=2) + "\n", encoding="utf-8")
    print(
        "owner-recovery "
        f"owner={old_owner_identity[0]}->{new_owner_identity[0]} "
        f"box-generation={old_box_generation}->{sandbox.generation} "
        f"runtime-generation={old_runtime_generation}->{new_runtime_generation}"
    )


async def exercise_async_runtime_controls(sandbox_id: str) -> None:
    sandbox = await AsyncSandbox.connect(sandbox_id)
    inventory = await sandbox.processes()
    assert inventory.execution_id == sandbox.id
    assert inventory.generation == sandbox.generation
    assert inventory.processes
    runtime_stats = await sandbox.runtime_stats()
    assert runtime_stats.execution_id == sandbox.id
    assert runtime_stats.generation == sandbox.generation
    assert runtime_stats.timestamp_unix_ns > 0
    assert runtime_stats.process_count > 0
    event_cursor = (
        await sandbox.events(limit=256, wait_timeout_ms=0)
    ).next_sequence
    event_stream = sandbox.stream_events(
        after_sequence=event_cursor,
        batch_size=256,
        wait_timeout_ms=1000,
    )
    update = ExecutionResourceUpdate(cpu_shares=768)
    update_operation = f"python-async-smoke-resources-{sandbox.id}"
    await sandbox.update_resources(update, operation_id=update_operation)
    await sandbox.update_resources(update, operation_id=update_operation)
    streamed_event = await anext(event_stream)
    assert streamed_event.kind == "resources-updated"
    await event_stream.aclose()
    events = await sandbox.events(
        after_sequence=event_cursor,
        limit=256,
        wait_timeout_ms=0,
    )
    assert events.execution_id == sandbox.id
    assert events.generation == sandbox.generation
    assert sum(
        event.kind == "resources-updated"
        for event in events.events
    ) == 1

client = A3SBoxClient()
isolation = os.environ["A3S_BOX_SDK_SMOKE_ISOLATION"]
diagnostics = client.runtime_diagnostics()
assert diagnostics.home == os.environ["A3S_HOME"]
assert diagnostics.runtime_version
assert client.runtime_disk_usage().total_bytes >= 0
context = Path(os.environ["A3S_HOME"]) / "python-sdk-build-context"
context.mkdir(parents=True, exist_ok=True)
(context / "Dockerfile").write_text(
    "FROM alpine:3.20\nENV A3S_SDK_BASE=ready\nWORKDIR /workspace\n"
)
image = None
volume = None
network = None
try:
    image = (
        client.image(str(context))
        .tag("local/a3s-sdk-smoke-python:latest")
        .build()
    )
    assert "image_push" in client.capabilities().operations
    assert client.get_image(image.reference) is not None
    assert client.inspect_image(image.reference) is not None
    assert client.image_history(image.reference) is not None
    tagged = client.tag_image(
        image.reference,
        "local/a3s-sdk-smoke-python:tested",
    )
    client.remove_image(tagged.reference)
    prune_volume = client.volume("python-sdk-prune-cache").create()
    assert prune_volume.name in client.prune_volumes()
    prune_network = (
        client.network("python-sdk-prune-network")
        .subnet("10.89.95.0/24")
        .create()
    )
    assert prune_network.name in client.prune_networks()
    volume = client.volume("python-sdk-cache").label("purpose", "sdk-smoke").create()
    builder = (
        client.sandbox(image.reference)
        .isolation(isolation)
        .mount_named(volume.name, "/cache")
        .workdir("/workspace")
    )
    if isolation == "microvm":
        network = (
            client.network("python-sdk-network")
            .subnet("10.89.92.0/24")
            .create()
        )
        builder = builder.network(network.name).publish_tcp(0, 8080)
    else:
        builder = builder.disable_network()

    with builder.start() as sandbox:
        assert any(item.id == sandbox.id for item in client.list_sandboxes())
        assert client.get_sandbox(sandbox.id) is not None
        result = sandbox.commands.run("printf 'python-sdk-ok'")
        assert result.exit_code == 0
        assert result.stdout == "python-sdk-ok"
        script = sandbox.script("printf 'python-script-ok'\n").env("CI", "true").run()
        assert script.exit_code == 0
        assert script.stdout == "python-script-ok"
        sandbox.files.write("/cache/marker.txt", "cache-ok")
        assert sandbox.files.read("/cache/marker.txt") == "cache-ok"
        sandbox.files.write("/tmp/a3s-python-sdk-smoke.txt", "hello")
        assert sandbox.files.read("/tmp/a3s-python-sdk-smoke.txt") == "hello"
        artifact = sandbox.files.export(
            "/tmp/a3s-python-sdk-smoke.txt",
            max_bytes=5,
        )
        assert artifact.data == b"hello"
        assert artifact.size == 5
        assert artifact.sha256 == "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        sandbox.files.remove("/tmp/a3s-python-sdk-smoke.txt")
        assert len(sandbox.logs(tail=20)) <= 20
        assert sandbox.stats() is not None
        if isolation == "sandbox":
            inventory = sandbox.processes()
            assert inventory.execution_id == sandbox.id
            assert inventory.generation == sandbox.generation
            assert inventory.processes
            runtime_stats = sandbox.runtime_stats()
            assert runtime_stats.execution_id == sandbox.id
            assert runtime_stats.generation == sandbox.generation
            assert runtime_stats.timestamp_unix_ns > 0
            assert runtime_stats.process_count > 0
            event_cursor = sandbox.events(
                limit=256,
                wait_timeout_ms=0,
            ).next_sequence
            event_stream = sandbox.stream_events(
                after_sequence=event_cursor,
                batch_size=256,
                wait_timeout_ms=1000,
            )
            update = ExecutionResourceUpdate(cpu_shares=1024)
            update_operation = f"python-smoke-resources-{sandbox.id}"
            sandbox.update_resources(update, operation_id=update_operation)
            sandbox.update_resources(update, operation_id=update_operation)
            streamed_event = next(event_stream)
            assert streamed_event.kind == "resources-updated"
            event_stream.close()
            events = sandbox.events(
                after_sequence=event_cursor,
                limit=256,
                wait_timeout_ms=0,
            )
            assert events.execution_id == sandbox.id
            assert events.generation == sandbox.generation
            assert sum(
                event.kind == "resources-updated"
                for event in events.events
            ) == 1
            asyncio.run(exercise_async_runtime_controls(sandbox.id))
            # `/tmp` is an ephemeral tmpfs and is intentionally excluded from
            # rootfs snapshots.
            marker = "/a3s-python-sdk-snapshot.txt"
            snapshot_id = f"python_sdk_{sandbox.id.replace('-', '_')}"
            sandbox.files.write(marker, "snapshot-ok")
            snapshot = sandbox.create_filesystem_snapshot(snapshot_id)
            assert Sandbox.filesystem_snapshot_size(snapshot.snapshot_id) == snapshot.size_bytes
            assert any(
                item.id == snapshot.snapshot_id
                for item in client.list_filesystem_snapshots()
            )
            assert client.get_filesystem_snapshot(snapshot.snapshot_id) is not None
            with Sandbox.create(
                image.reference,
                isolation="sandbox",
                filesystem_snapshot_id=snapshot.snapshot_id,
            ) as restored:
                assert restored.files.read(marker) == "snapshot-ok"
                try:
                    Sandbox.delete_filesystem_snapshot(snapshot.snapshot_id)
                except Exception:
                    pass
                else:
                    raise AssertionError("active restored Sandbox did not fence snapshot deletion")
            assert Sandbox.delete_filesystem_snapshot(snapshot.snapshot_id)
            assert Sandbox.filesystem_snapshot_size(snapshot.snapshot_id) is None
        if isolation == "sandbox":
            exercise_owner_death_recovery(sandbox)
        previous_generation = sandbox.generation
        sandbox.stop()
        assert not sandbox.is_running()
        sandbox.restart(
            operation_id=f"python-smoke-restart-{sandbox.id}",
            stop_timeout=5,
        )
        assert sandbox.generation == previous_generation + 1
        assert sandbox.is_running()
        sandbox.stop()
        sandbox.remove()
        assert client.get_sandbox(sandbox.id) is None
finally:
    if network is not None:
        client.remove_network(network.name)
    if volume is not None:
        client.remove_volume(volume.name)
    if image is not None:
        client.remove_image(image.reference)
    client.evict_images()
    shutil.rmtree(context, ignore_errors=True)
PY

echo "==> TypeScript SDK ($ISOLATION)"
npm --prefix "$REPO_ROOT/sdk/typescript" ci
npm --prefix "$REPO_ROOT/sdk/typescript" run build
A3S_BOX_BINARY="$A3S_BOX_BINARY" \
    A3S_BOX_SDK_SMOKE_ISOLATION="$ISOLATION" \
node --input-type=module <<'JS'
import { mkdir, rm, writeFile } from 'node:fs/promises'
import { join } from 'node:path'

import { A3SBoxClient, Sandbox } from './sdk/typescript/dist/index.js'

const client = new A3SBoxClient()
const isolation = process.env.A3S_BOX_SDK_SMOKE_ISOLATION
const diagnostics = await client.runtimeDiagnostics()
if (diagnostics.home !== process.env.A3S_HOME || !diagnostics.runtimeVersion) {
  throw new Error('runtime diagnostics returned an unexpected identity')
}
if ((await client.runtimeDiskUsage()).totalBytes < 0) {
  throw new Error('runtime disk usage returned an invalid total')
}
const context = join(process.env.A3S_HOME, 'typescript-sdk-build-context')
await mkdir(context, { recursive: true })
await writeFile(
  join(context, 'Dockerfile'),
  'FROM alpine:3.20\nENV A3S_SDK_BASE=ready\nWORKDIR /workspace\n'
)
let image
let volume
let network
try {
  image = await client
    .image(context)
    .tag('local/a3s-sdk-smoke-typescript:latest')
    .build()
  if (!(await client.capabilities()).operations.includes('image_push')) {
    throw new Error('SDK capability inventory did not include image_push')
  }
  if ((await client.getImage(image.reference)) === undefined) {
    throw new Error('built image was not gettable through the TypeScript SDK')
  }
  if ((await client.inspectImage(image.reference)) === undefined) {
    throw new Error('built image was not inspectable through the TypeScript SDK')
  }
  if ((await client.imageHistory(image.reference)) === undefined) {
    throw new Error('built image history was not available through the TypeScript SDK')
  }
  const tagged = await client.tagImage(
    image.reference,
    'local/a3s-sdk-smoke-typescript:tested'
  )
  await client.removeImage(tagged.reference)
  const pruneVolume = await client.volume('typescript-sdk-prune-cache').create()
  if (!(await client.pruneVolumes()).includes(pruneVolume.name)) {
    throw new Error('volume prune did not remove an unused TypeScript SDK volume')
  }
  const pruneNetwork = await client
    .network('typescript-sdk-prune-network')
    .subnet('10.89.96.0/24')
    .create()
  if (!(await client.pruneNetworks()).includes(pruneNetwork.name)) {
    throw new Error('network prune did not remove an unused TypeScript SDK network')
  }
  volume = await client
    .volume('typescript-sdk-cache')
    .label('purpose', 'sdk-smoke')
    .create()
  let builder = client
    .sandbox(image.reference)
    .isolation(isolation)
    .mountNamed(volume.name, '/cache')
    .workdir('/workspace')
  if (isolation === 'microvm') {
    network = await client
      .network('typescript-sdk-network')
      .subnet('10.89.93.0/24')
      .create()
    builder = builder.network(network.name).publishTcp(0, 8080)
  } else {
    builder = builder.disableNetwork()
  }
  const sandbox = await builder.start()
  try {
    if (!(await client.listSandboxes()).some((item) => item.id === sandbox.id)) {
      throw new Error('created Sandbox was absent from the management inventory')
    }
    if ((await client.getSandbox(sandbox.id)) === undefined) {
      throw new Error('created Sandbox was not gettable through the management client')
    }
    const result = await sandbox.commands.run("printf 'typescript-sdk-ok'")
    if (result.exitCode !== 0 || result.stdout !== 'typescript-sdk-ok') {
      throw new Error('TypeScript SDK command returned an unexpected result')
    }
    const script = await sandbox
      .script("printf 'typescript-script-ok'\n")
      .env('CI', 'true')
      .run()
    if (script.exitCode !== 0 || script.stdout !== 'typescript-script-ok') {
      throw new Error('TypeScript SDK script returned an unexpected result')
    }
    await sandbox.files.write('/cache/marker.txt', 'cache-ok')
    if (await sandbox.files.read('/cache/marker.txt') !== 'cache-ok') {
      throw new Error('TypeScript SDK named volume returned unexpected data')
    }
    await sandbox.files.write('/tmp/a3s-typescript-sdk-smoke.txt', 'hello')
    if (await sandbox.files.read('/tmp/a3s-typescript-sdk-smoke.txt') !== 'hello') {
      throw new Error('TypeScript SDK file read returned unexpected data')
    }
    const artifact = await sandbox.files.export(
      '/tmp/a3s-typescript-sdk-smoke.txt',
      { maxBytes: 5 }
    )
    if (
      Buffer.from(artifact.data).toString('utf8') !== 'hello' ||
      artifact.size !== 5 ||
      artifact.sha256 !== '2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824'
    ) {
      throw new Error('TypeScript SDK artifact export returned unexpected content or metadata')
    }
    await sandbox.files.remove('/tmp/a3s-typescript-sdk-smoke.txt')
    if ((await sandbox.logs({ tail: 20 })).length > 20) {
      throw new Error('Sandbox logs exceeded the requested tail')
    }
    if ((await sandbox.stats()) === undefined) {
      throw new Error('running Sandbox did not expose a stats snapshot')
    }
    if (isolation === 'sandbox') {
      const inventory = await sandbox.processes()
      if (
        inventory.executionId !== sandbox.id ||
        inventory.generation !== sandbox.generation ||
        inventory.processes.length === 0
      ) {
        throw new Error('runtime process inventory returned an invalid Sandbox generation')
      }
      const runtimeStats = await sandbox.runtimeStats()
      if (
        runtimeStats.executionId !== sandbox.id ||
        runtimeStats.generation !== sandbox.generation ||
        runtimeStats.timestampUnixNs <= 0 ||
        runtimeStats.processCount <= 0
      ) {
        throw new Error('runtime stats returned an invalid Sandbox snapshot')
      }
      const eventCursor = (await sandbox.events({ limit: 256, waitTimeoutMs: 0 }))
        .nextSequence
      const eventStream = sandbox
        .streamEvents({
          afterSequence: eventCursor,
          batchSize: 256,
          waitTimeoutMs: 1000,
        })
        [Symbol.asyncIterator]()
      const update = { cpuShares: 1024 }
      const updateOptions = {
        operationId: `typescript-smoke-resources-${sandbox.id}`,
      }
      await sandbox.updateResources(update, updateOptions)
      await sandbox.updateResources(update, updateOptions)
      const streamedEvent = (await eventStream.next()).value
      if (streamedEvent?.kind !== 'resources-updated') {
        throw new Error('runtime event stream returned the wrong event')
      }
      await eventStream.return()
      const events = await sandbox.events({
        afterSequence: eventCursor,
        limit: 256,
        waitTimeoutMs: 0,
      })
      if (
        events.executionId !== sandbox.id ||
        events.generation !== sandbox.generation ||
        events.events.filter((event) => event.kind === 'resources-updated').length !== 1
      ) {
        throw new Error('runtime resource-update replay did not publish exactly one event')
      }
      // `/tmp` is an ephemeral tmpfs and is intentionally excluded from
      // rootfs snapshots.
      const marker = '/a3s-typescript-sdk-snapshot.txt'
      const snapshotId = `typescript_sdk_${sandbox.id.replaceAll('-', '_')}`
      await sandbox.files.write(marker, 'snapshot-ok')
      const snapshot = await sandbox.createFilesystemSnapshot(snapshotId)
      if (await Sandbox.filesystemSnapshotSize(snapshot.snapshotId) !== snapshot.sizeBytes) {
        throw new Error('snapshot size lookup returned an unexpected value')
      }
      if (!(await client.listFilesystemSnapshots()).some(
        (item) => item.id === snapshot.snapshotId
      )) {
        throw new Error('captured snapshot was absent from the management inventory')
      }
      if ((await client.getFilesystemSnapshot(snapshot.snapshotId)) === undefined) {
        throw new Error('captured snapshot was not gettable through the management client')
      }
      const restored = await Sandbox.create(image.reference, {
        isolation: 'sandbox',
        filesystemSnapshotId: snapshot.snapshotId,
      })
      try {
        if (await restored.files.read(marker) !== 'snapshot-ok') {
          throw new Error('restored Sandbox did not contain the captured file')
        }
        let fenced = false
        try {
          await Sandbox.deleteFilesystemSnapshot(snapshot.snapshotId)
        } catch {
          fenced = true
        }
        if (!fenced) throw new Error('active restored Sandbox did not fence snapshot deletion')
      } finally {
        await restored.kill()
      }
      if (!(await Sandbox.deleteFilesystemSnapshot(snapshot.snapshotId))) {
        throw new Error('snapshot was not deleted after restored Sandbox cleanup')
      }
      if (await Sandbox.filesystemSnapshotSize(snapshot.snapshotId) !== undefined) {
        throw new Error('deleted snapshot still reported a size')
      }
    }
    const previousGeneration = sandbox.generation
    await sandbox.stop()
    if (await sandbox.isRunning()) {
      throw new Error('stopped Sandbox reports itself running')
    }
    await sandbox.restart({
      operationId: `typescript-smoke-restart-${sandbox.id}`,
      stopTimeoutSeconds: 5,
    })
    if (sandbox.generation !== previousGeneration + 1 || !(await sandbox.isRunning())) {
      throw new Error('restarted Sandbox did not expose its new running generation')
    }
    await sandbox.stop()
    await sandbox.remove()
    if ((await client.getSandbox(sandbox.id)) !== undefined) {
      throw new Error('removed Sandbox remained in the management inventory')
    }
  } finally {
    await sandbox.kill()
  }
} finally {
  if (network !== undefined) await client.removeNetwork(network.name)
  if (volume !== undefined) await client.removeVolume(volume.name)
  if (image !== undefined) await client.removeImage(image.reference)
  await client.evictImages()
  await rm(context, { recursive: true, force: true })
}
JS

echo "==> Go SDK ($ISOLATION)"
if ! command -v "$GO" >/dev/null 2>&1; then
    echo "Go executable is not available: $GO" >&2
    exit 1
fi
(
    cd "$REPO_ROOT/sdk/go"
    A3S_BOX_BINARY="$A3S_BOX_BINARY" \
        "$GO" run ./cmd/local-sdk-smoke "$ISOLATION"
)

echo "All local SDK smokes passed for isolation=$ISOLATION"
