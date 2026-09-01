#!/usr/bin/env bash
set -euo pipefail

SCRIPT_DIR="$(cd -- "$(dirname -- "${BASH_SOURCE[0]}")" && pwd)"
REPOSITORY_ROOT="$(cd -- "$SCRIPT_DIR/.." && pwd)"

read_workflow_pin() {
    local workflow="$1"
    local matches
    mapfile -t matches < <(
        sed -nE \
            's/^[[:space:]]*A3S_OCI_RUNTIME_REV:[[:space:]]*([0-9a-f]{40})[[:space:]]*$/\1/p' \
            "$workflow"
    )
    if [[ "${#matches[@]}" -ne 1 ]]; then
        echo "$workflow must contain exactly one 40-character OCI revision" >&2
        return 1
    fi
    printf '%s\n' "${matches[0]}"
}

read_sdk_pin() {
    local manifest="$1"
    local matches
    mapfile -t matches < <(
        sed -nE \
            's/^[[:space:]]*a3s-oci-sdk[[:space:]]*=.*rev[[:space:]]*=[[:space:]]*"([0-9a-f]{40})".*$/\1/p' \
            "$manifest"
    )
    if [[ "${#matches[@]}" -ne 1 ]]; then
        echo "$manifest must contain exactly one 40-character OCI SDK revision" >&2
        return 1
    fi
    printf '%s\n' "${matches[0]}"
}

ci_pin="$(read_workflow_pin "$REPOSITORY_ROOT/.github/workflows/ci.yml")"
release_pin="$(read_workflow_pin "$REPOSITORY_ROOT/.github/workflows/release.yml")"
sdk_pin="$(read_sdk_pin "$REPOSITORY_ROOT/src/Cargo.toml")"

if [[ "$ci_pin" != "$release_pin" || "$ci_pin" != "$sdk_pin" ]]; then
    echo "OCI revisions differ: CI=$ci_pin release=$release_pin SDK=$sdk_pin" >&2
    exit 1
fi

printf 'OCI SDK and artifact revision: %s\n' "$ci_pin"
