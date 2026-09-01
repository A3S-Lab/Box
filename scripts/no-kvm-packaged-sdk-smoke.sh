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

sentinel_marker="/tmp/a3s-box-ci-kvm-sentinel"
saved_kvm="/dev/a3s-box-ci-host-kvm"
kvm_state_owned=0
kvm_original_moved=0
kvm_test_directory_created=0

cleanup_kvm_state() {
    local cleanup_status=0

    if ((kvm_test_directory_created)); then
        if [[ -d /dev/kvm && ! -L /dev/kvm ]]; then
            sudo rmdir /dev/kvm || cleanup_status=1
        elif [[ -e /dev/kvm || -L /dev/kvm ]]; then
            echo "refusing to remove an unexpected replacement at /dev/kvm" >&2
            cleanup_status=1
        fi
    fi

    if ((kvm_original_moved)); then
        if [[ -e /dev/kvm || -L /dev/kvm ]]; then
            echo "cannot restore the host KVM device because /dev/kvm is occupied" >&2
            cleanup_status=1
        elif [[ ! -c "$saved_kvm" || -L "$saved_kvm" ]]; then
            echo "saved host KVM device is missing or has the wrong type" >&2
            cleanup_status=1
        else
            sudo mv -- "$saved_kvm" /dev/kvm || cleanup_status=1
        fi
    fi

    if ((cleanup_status == 0)); then
        if rm -f -- "$sentinel_marker"; then
            kvm_state_owned=0
            kvm_original_moved=0
            kvm_test_directory_created=0
        else
            cleanup_status=1
        fi
    fi

    return "$cleanup_status"
}

handle_exit() {
    local command_status=$?
    trap - EXIT
    if ((kvm_state_owned)) && ! cleanup_kvm_state; then
        command_status=1
    fi
    exit "$command_status"
}
trap handle_exit EXIT

prepare_kvm_states() {
    if [[ -e "$sentinel_marker" || -L "$sentinel_marker" ]]; then
        echo "refusing to replace an existing KVM ownership marker" >&2
        exit 1
    fi
    if [[ -e "$saved_kvm" || -L "$saved_kvm" ]]; then
        echo "refusing to replace a saved host KVM device" >&2
        exit 1
    fi
    if [[ -e /dev/kvm || -L /dev/kvm ]]; then
        if [[ ! -c /dev/kvm || -L /dev/kvm ]]; then
            echo "host /dev/kvm exists but is not a character device" >&2
            exit 1
        fi
    fi

    printf '%s\n' 'owned by scripts/no-kvm-packaged-sdk-smoke.sh' \
        > "$sentinel_marker"
    kvm_state_owned=1

    if [[ -e /dev/kvm || -L /dev/kvm ]]; then
        sudo mv -- /dev/kvm "$saved_kvm"
        kvm_original_moved=1
    fi
    if [[ -e /dev/kvm || -L /dev/kvm ]]; then
        echo "failed to establish the KVM-absent test state" >&2
        exit 1
    fi
}

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
            if [[ -e /dev/kvm || -L /dev/kvm ]]; then
                echo "/dev/kvm unexpectedly exists in the KVM-absent phase" >&2
                exit 1
            fi
            expected_info_pattern='KVM is not available: /dev/kvm not found'
            ;;
        inaccessible)
            [[ ! -e /dev/kvm && ! -L /dev/kvm ]]
            kvm_test_directory_created=1
            sudo install -d -m 000 /dev/kvm
            sudo python3 - <<'PY'
import errno
import os
import stat

metadata = os.lstat("/dev/kvm")
assert stat.S_ISDIR(metadata.st_mode), "/dev/kvm test path is not a directory"
try:
    descriptor = os.open("/dev/kvm", os.O_RDWR)
except OSError as error:
    assert error.errno == errno.EISDIR, error
else:
    os.close(descriptor)
    raise RuntimeError("/dev/kvm test path unexpectedly opened read/write")
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
        [[ -d /dev/kvm && ! -L /dev/kvm ]]
        cleanup_kvm_state
    else
        [[ ! -e /dev/kvm && ! -L /dev/kvm ]]
    fi
}

prepare_kvm_states
run_sdk_phase absent
run_sdk_phase inaccessible
