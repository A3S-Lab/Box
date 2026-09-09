#!/usr/bin/env bash
# Local Linux KVM MicroVM vertical-slice gate for Box over OCI Runtime.
#
# Prerequisites (operator-owned):
#   - usable /dev/kvm
#   - running `a3s-oci box-kvm-qualification-service` with absolute --root,
#     --shim, and --system-image-manifest on a Linux-native filesystem
#   - release `a3s-box` / `a3s-box-shim` able to resolve libkrun
#
# This script does not start the Host Service and does not claim fresh-host
# promotion. It only proves the public Box create → manager reopen → start →
# exact exit → delete slice against an already-running qualification service.

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  linux-kvm-oci-qualification.sh \
    --box-bin DIR \
    --runtime-root ABS_PATH \
    --kvm-endpoint ABS_SOCK \
    --image REF \
    --report ABS_JSON \
    [--home ABS_DIR]

Environment:
  LD_LIBRARY_PATH may need the directory that contains libkrun.so.1.
EOF
}

BOX_BIN=""
RUNTIME_ROOT=""
KVM_ENDPOINT=""
IMAGE=""
REPORT=""
HOME_DIR=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --box-bin)
      BOX_BIN="${2:?}"
      shift 2
      ;;
    --runtime-root)
      RUNTIME_ROOT="${2:?}"
      shift 2
      ;;
    --kvm-endpoint)
      KVM_ENDPOINT="${2:?}"
      shift 2
      ;;
    --image)
      IMAGE="${2:?}"
      shift 2
      ;;
    --report)
      REPORT="${2:?}"
      shift 2
      ;;
    --home)
      HOME_DIR="${2:?}"
      shift 2
      ;;
    -h|--help)
      usage
      exit 0
      ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

require_abs() {
  local name="$1"
  local value="$2"
  if [[ -z "$value" || "$value" != /* || "$value" == *"/./"* || "$value" == *"/../"* ]]; then
    echo "$name must be an absolute normalized path" >&2
    exit 2
  fi
}

require_abs "--box-bin" "${BOX_BIN}"
require_abs "--runtime-root" "${RUNTIME_ROOT}"
require_abs "--kvm-endpoint" "${KVM_ENDPOINT}"
require_abs "--report" "${REPORT}"

if [[ -z "$IMAGE" ]]; then
  echo "--image is required" >&2
  exit 2
fi

if [[ -z "$HOME_DIR" ]]; then
  HOME_DIR="$(mktemp -d /tmp/a3s-box-kvm-oci-qualification.XXXXXX)"
fi
require_abs "--home" "${HOME_DIR}"
case "$(basename "$HOME_DIR")" in
  *kvm-oci-qualification*) ;;
  *)
    echo "A3S_HOME basename must contain kvm-oci-qualification" >&2
    exit 2
    ;;
esac

if [[ ! -x "${BOX_BIN}/a3s-box" ]]; then
  echo "missing executable ${BOX_BIN}/a3s-box" >&2
  exit 2
fi
if [[ ! -S "${KVM_ENDPOINT}" ]]; then
  echo "missing Unix socket ${KVM_ENDPOINT}" >&2
  exit 2
fi
if [[ ! -d "${RUNTIME_ROOT}" ]]; then
  echo "missing runtime root ${RUNTIME_ROOT}" >&2
  exit 2
fi
if [[ -e "${REPORT}" ]]; then
  echo "refusing to overwrite existing report ${REPORT}" >&2
  exit 2
fi

EXAMPLE_BIN="${BOX_BIN}/linux-kvm-oci-qualification"
if [[ ! -x "${EXAMPLE_BIN}" ]]; then
  echo "missing executable ${EXAMPLE_BIN}; build with:" >&2
  echo "  cargo build -p a3s-box-runtime --example linux-kvm-oci-qualification --release" >&2
  exit 2
fi

mkdir -p "${HOME_DIR}"
export PATH="${BOX_BIN}:${PATH}"
export A3S_HOME="${HOME_DIR}"
export A3S_BOX_OCI_HOST_ROOT="${RUNTIME_ROOT}"
export A3S_BOX_OCI_KVM_ENDPOINT="${KVM_ENDPOINT}"
export A3S_BOX_KVM_OCI_IMAGE="${IMAGE}"
export A3S_BOX_KVM_OCI_REPORT="${REPORT}"
export A3S_BOX_KVM_OCI_QUALIFICATION=1

echo "running Linux KVM OCI qualification"
echo "  home=${A3S_HOME}"
echo "  runtime-root=${A3S_BOX_OCI_HOST_ROOT}"
echo "  endpoint=${A3S_BOX_OCI_KVM_ENDPOINT}"
echo "  image=${A3S_BOX_KVM_OCI_IMAGE}"
echo "  report=${A3S_BOX_KVM_OCI_REPORT}"

"${EXAMPLE_BIN}"
