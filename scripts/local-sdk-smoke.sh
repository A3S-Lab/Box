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

from a3s_box import Sandbox

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

with Sandbox.create(
    "alpine:3.20",
    isolation=os.environ["A3S_BOX_SDK_SMOKE_ISOLATION"],
) as sandbox:
    result = sandbox.commands.run("printf 'python-sdk-ok'")
    assert result.exit_code == 0
    assert result.stdout == "python-sdk-ok"
    sandbox.files.write("/tmp/a3s-python-sdk-smoke.txt", "hello")
    assert sandbox.files.read("/tmp/a3s-python-sdk-smoke.txt") == "hello"
    sandbox.files.remove("/tmp/a3s-python-sdk-smoke.txt")
PY

echo "==> TypeScript SDK ($ISOLATION)"
npm --prefix "$REPO_ROOT/sdk/typescript" ci
npm --prefix "$REPO_ROOT/sdk/typescript" run build
"${clean_env[@]}" \
    A3S_BOX_BINARY="$A3S_BOX_BINARY" \
    A3S_BOX_SDK_SMOKE_ISOLATION="$ISOLATION" \
    node --input-type=module <<'JS'
import { Sandbox } from './sdk/typescript/dist/index.js'

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

const sandbox = await Sandbox.create('alpine:3.20', {
  isolation: process.env.A3S_BOX_SDK_SMOKE_ISOLATION,
})
try {
  const result = await sandbox.commands.run("printf 'typescript-sdk-ok'")
  if (result.exitCode !== 0 || result.stdout !== 'typescript-sdk-ok') {
    throw new Error('TypeScript SDK command returned an unexpected result')
  }
  await sandbox.files.write('/tmp/a3s-typescript-sdk-smoke.txt', 'hello')
  if (await sandbox.files.read('/tmp/a3s-typescript-sdk-smoke.txt') !== 'hello') {
    throw new Error('TypeScript SDK file read returned unexpected data')
  }
  await sandbox.files.remove('/tmp/a3s-typescript-sdk-smoke.txt')
} finally {
  await sandbox.kill()
}
JS

echo "All local SDK smokes passed for isolation=$ISOLATION"
