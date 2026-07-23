#!/usr/bin/env bash
#
# Exercise the zero-configuration Rust, Python, and TypeScript local SDKs
# against one real A3S Box isolation backend.

set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
WORKSPACE="$REPO_ROOT/src"
ISOLATION="${1:-microvm}"
PYTHON="${PYTHON:-python3}"
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

remote_variables=(
    E2B_API_KEY
    E2B_API_URL
    E2B_DOMAIN
    A3S_BOX_API_KEY
    A3S_BOX_ENDPOINT
    A3S_BOX_DOMAIN
    A3S_BOX_SANDBOX_URL
)
clean_env=(env)
for variable in "${remote_variables[@]}"; do
    clean_env+=(-u "$variable")
done

echo "==> Rust SDK ($ISOLATION)"
(
    cd "$WORKSPACE"
    if [ "$cargo_release" -eq 1 ]; then
        "${clean_env[@]}" \
            A3S_BOX_SDK_LOCAL_SMOKE=1 \
            A3S_BOX_SDK_SMOKE_ISOLATION="$ISOLATION" \
            RUST_MIN_STACK="$RUST_MIN_STACK" \
            cargo test --locked --release -p a3s-box-sdk --test local_sandbox \
            e2b_style_local_sandbox_runs_without_remote_credentials \
            -- --ignored --nocapture --test-threads=1
    else
        "${clean_env[@]}" \
            A3S_BOX_SDK_LOCAL_SMOKE=1 \
            A3S_BOX_SDK_SMOKE_ISOLATION="$ISOLATION" \
            RUST_MIN_STACK="$RUST_MIN_STACK" \
            cargo test --locked -p a3s-box-sdk --test local_sandbox \
            e2b_style_local_sandbox_runs_without_remote_credentials \
            -- --ignored --nocapture --test-threads=1
    fi
)

echo "==> Python SDK ($ISOLATION)"
"${clean_env[@]}" \
    A3S_BOX_BINARY="$A3S_BOX_BINARY" \
    A3S_BOX_SDK_SMOKE_ISOLATION="$ISOLATION" \
    PYTHONPATH="$REPO_ROOT/sdk/python/src" \
    "$PYTHON" - <<'PY'
import os
import shutil
from pathlib import Path

from a3s_box import A3SBoxClient, Sandbox

for name in (
    "E2B_API_KEY",
    "E2B_API_URL",
    "E2B_DOMAIN",
    "A3S_BOX_API_KEY",
    "A3S_BOX_ENDPOINT",
    "A3S_BOX_DOMAIN",
    "A3S_BOX_SANDBOX_URL",
):
    assert name not in os.environ

client = A3SBoxClient()
isolation = os.environ["A3S_BOX_SDK_SMOKE_ISOLATION"]
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
        sandbox.files.remove("/tmp/a3s-python-sdk-smoke.txt")
        if isolation == "sandbox":
            # `/tmp` is an ephemeral tmpfs and is intentionally excluded from
            # rootfs snapshots.
            marker = "/a3s-python-sdk-snapshot.txt"
            snapshot_id = f"python_sdk_{sandbox.id.replace('-', '_')}"
            sandbox.files.write(marker, "snapshot-ok")
            snapshot = sandbox.create_filesystem_snapshot(snapshot_id)
            assert Sandbox.filesystem_snapshot_size(snapshot.snapshot_id) == snapshot.size_bytes
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
"${clean_env[@]}" \
    A3S_BOX_BINARY="$A3S_BOX_BINARY" \
    A3S_BOX_SDK_SMOKE_ISOLATION="$ISOLATION" \
node --input-type=module <<'JS'
import { mkdir, rm, writeFile } from 'node:fs/promises'
import { join } from 'node:path'

import { A3SBoxClient, Sandbox } from './sdk/typescript/dist/index.js'

for (const name of [
  'E2B_API_KEY',
  'E2B_API_URL',
  'E2B_DOMAIN',
  'A3S_BOX_API_KEY',
  'A3S_BOX_ENDPOINT',
  'A3S_BOX_DOMAIN',
  'A3S_BOX_SANDBOX_URL',
]) {
  if (name in process.env) throw new Error(`${name} must be unset`)
}

const client = new A3SBoxClient()
const isolation = process.env.A3S_BOX_SDK_SMOKE_ISOLATION
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
    await sandbox.files.remove('/tmp/a3s-typescript-sdk-smoke.txt')
    if (isolation === 'sandbox') {
      // `/tmp` is an ephemeral tmpfs and is intentionally excluded from
      // rootfs snapshots.
      const marker = '/a3s-typescript-sdk-snapshot.txt'
      const snapshotId = `typescript_sdk_${sandbox.id.replaceAll('-', '_')}`
      await sandbox.files.write(marker, 'snapshot-ok')
      const snapshot = await sandbox.createFilesystemSnapshot(snapshotId)
      if (await Sandbox.filesystemSnapshotSize(snapshot.snapshotId) !== snapshot.sizeBytes) {
        throw new Error('snapshot size lookup returned an unexpected value')
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

echo "All local SDK smokes passed for isolation=$ISOLATION"
