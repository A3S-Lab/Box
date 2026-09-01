#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPOSITORY_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"

read_workflow_pin() {
    local workflow="$1"
    local match
    match="$(
        sed -nE \
            's/^[[:space:]]*A3S_OCI_RUNTIME_REV:[[:space:]]*([0-9a-f]{40})[[:space:]]*$/\1/p' \
            "$workflow"
    )"
    if [[ -z "$match" || "$match" == *$'\n'* ]]; then
        echo "$workflow must contain exactly one 40-character OCI revision" >&2
        return 1
    fi
    printf '%s\n' "$match"
}

read_sdk_pin() {
    local manifest="$1"
    local match
    match="$(
        sed -nE \
            's/^[[:space:]]*a3s-oci-sdk[[:space:]]*=.*rev[[:space:]]*=[[:space:]]*"([0-9a-f]{40})".*$/\1/p' \
            "$manifest"
    )"
    if [[ -z "$match" || "$match" == *$'\n'* ]]; then
        echo "$manifest must contain exactly one 40-character OCI SDK revision" >&2
        return 1
    fi
    printf '%s\n' "$match"
}

read_box_recovery_schema() {
    local smoke_script="$1"
    local match
    match="$(
        sed -nE \
            's/^[[:space:]]*assert recovery\["schemaVersion"\] == "(a3s\.oci\.native-linux-recovery\.v[0-9]+)"([[:space:]]*,.*)?$/\1/p' \
            "$smoke_script"
    )"
    if [[ -z "$match" || "$match" == *$'\n'* ]]; then
        echo "$smoke_script must assert exactly one native Linux recovery schema" >&2
        return 1
    fi
    printf '%s\n' "$match"
}

read_runtime_recovery_schema() {
    local runtime_root="$1"
    local source="$runtime_root/crates/agent/src/executor/recovery.rs"
    local match
    if [[ ! -f "$source" ]]; then
        echo "pinned OCI Runtime recovery source is missing: $source" >&2
        return 1
    fi
    match="$(
        sed -nE \
            's/^const CONTAINER_SCHEMA_VERSION: &str = "(a3s\.oci\.native-linux-recovery\.v[0-9]+)";[[:space:]]*$/\1/p' \
            "$source"
    )"
    if [[ -z "$match" || "$match" == *$'\n'* ]]; then
        echo "$source must declare exactly one current recovery schema" >&2
        return 1
    fi
    printf '%s\n' "$match"
}

ci_pin="$(read_workflow_pin "$REPOSITORY_ROOT/.github/workflows/ci.yml")"
release_pin="$(read_workflow_pin "$REPOSITORY_ROOT/.github/workflows/release.yml")"
sdk_pin="$(read_sdk_pin "$REPOSITORY_ROOT/src/Cargo.toml")"

if [[ "$ci_pin" != "$release_pin" || "$ci_pin" != "$sdk_pin" ]]; then
    echo "OCI revisions differ: CI=$ci_pin release=$release_pin SDK=$sdk_pin" >&2
    exit 1
fi

printf 'OCI SDK and artifact revision: %s\n' "$ci_pin"

if (($# > 1)); then
    echo "usage: $0 [pinned-oci-runtime-checkout]" >&2
    exit 2
fi

if (($# == 1)); then
    runtime_root="$1"
    runtime_revision="$(git -C "$runtime_root" rev-parse HEAD)"
    if [[ "$runtime_revision" != "$ci_pin" ]]; then
        echo "OCI checkout revision differs: expected=$ci_pin checkout=$runtime_revision" >&2
        exit 1
    fi

    box_recovery_schema="$(
        read_box_recovery_schema "$REPOSITORY_ROOT/scripts/local-sdk-smoke.sh"
    )"
    runtime_recovery_schema="$(read_runtime_recovery_schema "$runtime_root")"
    if [[ "$box_recovery_schema" != "$runtime_recovery_schema" ]]; then
        echo "OCI recovery schemas differ: Box=$box_recovery_schema Runtime=$runtime_recovery_schema" >&2
        exit 1
    fi
    printf 'OCI native Linux recovery schema: %s\n' "$runtime_recovery_schema"
fi
