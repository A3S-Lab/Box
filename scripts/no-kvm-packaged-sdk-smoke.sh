#!/usr/bin/env bash
# Run every local SDK against one installed product with KVM missing and inaccessible.
set -Eeuo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

required_variables=(
    A3S_BOX_INSTALL_DIR
    A3S_BOX_BINARY
    A3S_BOX_SHIM_BINARY
    A3S_BOX_GUEST_INIT_BINARY
    A3S_BOX_OCI_RUNTIME_PATH
    A3S_BOX_OCI_AGENT_PATH
    A3S_BOX_OCI_HOST_ROOT
    A3S_HOME
)
for variable in "${required_variables[@]}"; do
    if [ -z "${!variable:-}" ]; then
        echo "$variable is required" >&2
        exit 2
    fi
done

for artifact in \
    "$A3S_BOX_BINARY" \
    "$A3S_BOX_SHIM_BINARY" \
    "$A3S_BOX_GUEST_INIT_BINARY" \
    "$A3S_BOX_OCI_RUNTIME_PATH" \
    "$A3S_BOX_OCI_AGENT_PATH"; do
    resolved="$(realpath "$artifact")"
    case "$resolved" in
        "$A3S_BOX_INSTALL_DIR"/*) ;;
        *)
            echo "installed-product gate escaped its install root: $resolved" >&2
            exit 1
            ;;
    esac
done

sentinel_created=0
sentinel_marker="/tmp/a3s-box-ci-kvm-sentinel"
cleanup_kvm_sentinel() {
    if ((sentinel_created)); then
        sudo rm -f /dev/kvm
        rm -f -- "$sentinel_marker"
        sentinel_created=0
    fi
}
trap cleanup_kvm_sentinel EXIT

validate_recovery_report() {
    local report="$1"
    test -s "$report"
    jq --exit-status \
        '.schema_version == "a3s.box.native-linux-owner-recovery.v1"
         and .status == "available" and .platform == "linux"
         and (.sandbox_id | length > 0)
         and (.runtime_container_id == ("a3s-box-" + .sandbox_id))
         and (.new_box_generation == (.old_box_generation + 1))
         and (.new_runtime_generation == (.old_runtime_generation + 1))
         and (.old_owner.pid > 0) and (.new_owner.pid > 0)
         and (.old_init.pid > 0) and (.new_init.pid > 0)
         and (.old_owner != .new_owner) and (.old_init != .new_init)
         and .old_owner_gone and .old_launcher_gone and .old_init_gone
         and .socket_rebound and .stopped_without_invented_exit_status
         and .old_generation_deleted and .old_executor_root_removed
         and .replacement_generation_running' \
        "$report" >/dev/null
}

run_sdk_phase() {
    local phase="$1"
    local expected_info_pattern info report
    report="/tmp/a3s-box-native-owner-recovery-$phase.json"
    rm -f -- "$report"

    case "$phase" in
        absent)
            if [ -e /dev/kvm ]; then
                echo "/dev/kvm unexpectedly exists on the no-KVM runner" >&2
                exit 1
            fi
            expected_info_pattern='KVM is not available: /dev/kvm not found'
            ;;
        inaccessible)
            test ! -e /dev/kvm
            test ! -e "$sentinel_marker"
            printf '%s\n' 'owned by scripts/no-kvm-packaged-sdk-smoke.sh' \
                > "$sentinel_marker"
            sentinel_created=1
            sudo mknod -m 000 /dev/kvm c 10 232
            sudo python3 - <<'PY'
import os
import stat

metadata = os.lstat("/dev/kvm")
assert stat.S_ISCHR(metadata.st_mode), "/dev/kvm sentinel is not a character device"
assert os.major(metadata.st_rdev) == 10, "/dev/kvm sentinel has the wrong major number"
assert os.minor(metadata.st_rdev) == 232, "/dev/kvm sentinel has the wrong minor number"
try:
    descriptor = os.open("/dev/kvm", os.O_RDWR)
except OSError:
    pass
else:
    os.close(descriptor)
    raise RuntimeError("/dev/kvm sentinel unexpectedly opened read/write")
PY
            expected_info_pattern='KVM access denied|Failed to access /dev/kvm'
            ;;
        *)
            echo "unknown no-KVM phase: $phase" >&2
            exit 2
            ;;
    esac

    info="$(
        sudo env \
            PATH="$A3S_BOX_INSTALL_DIR:$PATH" \
            HOME="$HOME" \
            A3S_HOME="$A3S_HOME" \
            "$A3S_BOX_BINARY" info
    )"
    grep -E "$expected_info_pattern" <<< "$info"

    sudo env \
        PATH="$A3S_BOX_INSTALL_DIR:$PATH" \
        HOME="$HOME" \
        RUSTUP_HOME="${RUSTUP_HOME:-$HOME/.rustup}" \
        CARGO_HOME="${CARGO_HOME:-$HOME/.cargo}" \
        A3S_DEPS_STUB=1 \
        A3S_HOME="$A3S_HOME" \
        A3S_BOX_OCI_MIGRATION=sandbox \
        A3S_BOX_OCI_HOST_ROOT="$A3S_BOX_OCI_HOST_ROOT" \
        A3S_BOX_OCI_RUNTIME_PATH="$A3S_BOX_OCI_RUNTIME_PATH" \
        A3S_BOX_OCI_AGENT_PATH="$A3S_BOX_OCI_AGENT_PATH" \
        A3S_BOX_OCI_OWNER_RECOVERY_REPORT="$report" \
        A3S_BOX_BINARY="$A3S_BOX_BINARY" \
        A3S_BOX_SHIM_BINARY="$A3S_BOX_SHIM_BINARY" \
        A3S_BOX_GUEST_INIT_BINARY="$A3S_BOX_GUEST_INIT_BINARY" \
        RUST_MIN_STACK="${RUST_MIN_STACK:-16777216}" \
        PYTHON="${PYTHON:-python3}" \
        GO="${GO:-go}" \
        "$SCRIPT_DIR/local-sdk-smoke.sh" sandbox
    validate_recovery_report "$report"

    if [ "$phase" = inaccessible ]; then
        test -c /dev/kvm
        cleanup_kvm_sentinel
    else
        test ! -e /dev/kvm
    fi
}

run_sdk_phase absent
run_sdk_phase inaccessible
