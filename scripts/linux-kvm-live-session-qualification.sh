#!/usr/bin/env bash
# Local Linux KVM MicroVM live-session gate for Box over OCI Runtime.
#
# Starts box-kvm-qualification-service with A3S_OCI_KVM_SESSION_OWNER=1, then
# runs the destructive live-session qualification: retained streaming handle
# continuity across Host Service SIGKILL/restart, filesystem continuity via
# transfer_file (upload before kill, download after reattach), keyed captured
# exec, inventory, stats, and Live kill without inventing exit status.
#
# Observation-only. Does not claim fresh-host or AArch64 promotion.

set -euo pipefail

usage() {
  cat <<'EOF'
Usage:
  linux-kvm-live-session-qualification.sh \
    --box-bin DIR \
    --a3s-oci ABS_BIN \
    --service-root ABS_DIR \
    --shim ABS_BIN \
    --system-image-manifest ABS_JSON \
    --image REF \
    --report ABS_JSON \
    [--home ABS_DIR] \
    [--box-sha SHA] \
    [--oci-sha SHA]

Environment:
  LD_LIBRARY_PATH may need the directory that contains libkrun.so.1.
  Forces A3S_OCI_KVM_SESSION_OWNER=1 for durable session-owner create.
EOF
}

BOX_BIN=""
A3S_OCI=""
SERVICE_ROOT=""
SHIM=""
MANIFEST=""
IMAGE=""
REPORT=""
HOME_DIR=""
BOX_SHA=""
OCI_SHA=""
SERVICE_PID=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --box-bin) BOX_BIN="${2:?}"; shift 2 ;;
    --a3s-oci) A3S_OCI="${2:?}"; shift 2 ;;
    --service-root) SERVICE_ROOT="${2:?}"; shift 2 ;;
    --shim) SHIM="${2:?}"; shift 2 ;;
    --system-image-manifest) MANIFEST="${2:?}"; shift 2 ;;
    --image) IMAGE="${2:?}"; shift 2 ;;
    --report) REPORT="${2:?}"; shift 2 ;;
    --home) HOME_DIR="${2:?}"; shift 2 ;;
    --box-sha) BOX_SHA="${2:?}"; shift 2 ;;
    --oci-sha) OCI_SHA="${2:?}"; shift 2 ;;
    -h|--help) usage; exit 0 ;;
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
require_abs "--a3s-oci" "${A3S_OCI}"
require_abs "--service-root" "${SERVICE_ROOT}"
require_abs "--shim" "${SHIM}"
require_abs "--system-image-manifest" "${MANIFEST}"
require_abs "--report" "${REPORT}"

if [[ -z "$IMAGE" ]]; then
  echo "--image is required" >&2
  exit 2
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BOX_REPO="$(cd "${SCRIPT_DIR}/.." && pwd)"
OCI_REPO_CANDIDATE="$(cd "${BOX_REPO}/../oci-runtime" 2>/dev/null && pwd || true)"

if [[ -z "$BOX_SHA" ]]; then
  BOX_SHA="$(git -C "${BOX_REPO}" rev-parse HEAD)"
fi
if [[ -z "$OCI_SHA" ]]; then
  if [[ -n "${OCI_REPO_CANDIDATE}" && -d "${OCI_REPO_CANDIDATE}/.git" ]]; then
    OCI_SHA="$(git -C "${OCI_REPO_CANDIDATE}" rev-parse HEAD)"
  else
    echo "--oci-sha is required when ../oci-runtime is unavailable" >&2
    exit 2
  fi
fi

if [[ -z "$HOME_DIR" ]]; then
  HOME_DIR="$(mktemp -d /tmp/a3s-box-kvm-live-session.XXXXXX)"
fi
require_abs "--home" "${HOME_DIR}"
case "$(basename "$HOME_DIR")" in
  *kvm-live-session*) ;;
  *)
    echo "A3S_HOME basename must contain kvm-live-session" >&2
    exit 2
    ;;
esac

EXAMPLE_BIN="${BOX_BIN}/linux-kvm-live-session-qualification"
for path in "${BOX_BIN}/a3s-box" "${EXAMPLE_BIN}" "${A3S_OCI}" "${SHIM}"; do
  if [[ ! -x "$path" ]]; then
    echo "missing executable ${path}" >&2
    if [[ "$path" == "${EXAMPLE_BIN}" ]]; then
      echo "  cargo build -p a3s-box-runtime --example linux-kvm-live-session-qualification --release" >&2
    fi
    exit 2
  fi
done
if [[ ! -f "${MANIFEST}" ]]; then
  echo "missing system-image manifest ${MANIFEST}" >&2
  exit 2
fi
if [[ -e "${REPORT}" ]]; then
  echo "refusing to overwrite existing report ${REPORT}" >&2
  exit 2
fi

mkdir -p "${HOME_DIR}"
mkdir -m 0700 -p "${SERVICE_ROOT}"
chmod 0700 "${SERVICE_ROOT}"

RUNTIME_ROOT="${SERVICE_ROOT}/runtime"
KVM_ENDPOINT="${SERVICE_ROOT}/runtime.sock"
SERVICE_LOG="${SERVICE_ROOT}/qualification-service.log"

if [[ -S "${KVM_ENDPOINT}" ]]; then
  echo "refusing to reuse an already-bound endpoint ${KVM_ENDPOINT}" >&2
  exit 2
fi

cleanup() {
  local status=$?
  if [[ -f "${SERVICE_ROOT}/qualification-service.pid" ]]; then
    SERVICE_PID="$(tr -d '[:space:]' <"${SERVICE_ROOT}/qualification-service.pid" || true)"
  fi
  if [[ -n "${SERVICE_PID:-}" ]] && kill -0 "${SERVICE_PID}" 2>/dev/null; then
    kill -TERM "${SERVICE_PID}" 2>/dev/null || true
    wait "${SERVICE_PID}" 2>/dev/null || true
  fi
  exit "$status"
}
trap cleanup EXIT

export A3S_OCI_KVM_SESSION_OWNER=1
nohup env A3S_OCI_KVM_SESSION_OWNER=1 "${A3S_OCI}" box-kvm-qualification-service \
  --root "${SERVICE_ROOT}" \
  --shim "${SHIM}" \
  --system-image-manifest "${MANIFEST}" \
  >"${SERVICE_LOG}" 2>&1 &
SERVICE_PID=$!
echo "${SERVICE_PID}" >"${SERVICE_ROOT}/qualification-service.pid"

for _ in $(seq 1 120); do
  if [[ -S "${KVM_ENDPOINT}" ]]; then
    break
  fi
  if ! kill -0 "${SERVICE_PID}" 2>/dev/null; then
    echo "Host Service exited before readiness; log: ${SERVICE_LOG}" >&2
    tail -40 "${SERVICE_LOG}" >&2 || true
    exit 1
  fi
  sleep 0.25
done
if [[ ! -S "${KVM_ENDPOINT}" ]]; then
  echo "Host Service did not publish ${KVM_ENDPOINT}" >&2
  exit 1
fi

export PATH="${BOX_BIN}:${PATH}"
export A3S_HOME="${HOME_DIR}"
export A3S_BOX_OCI_HOST_ROOT="${RUNTIME_ROOT}"
export A3S_BOX_OCI_KVM_ENDPOINT="${KVM_ENDPOINT}"
export A3S_BOX_KVM_LIVE_SESSION_IMAGE="${IMAGE}"
export A3S_BOX_KVM_LIVE_SESSION_REPORT="${REPORT}"
export A3S_BOX_KVM_LIVE_SESSION_BOX_SHA="${BOX_SHA}"
export A3S_BOX_KVM_LIVE_SESSION_OCI_SHA="${OCI_SHA}"
export A3S_BOX_KVM_LIVE_SESSION_QUALIFICATION=1
export A3S_BOX_KVM_LIVE_SESSION_SERVICE_PID="${SERVICE_PID}"
export A3S_BOX_KVM_LIVE_SESSION_SERVICE_BIN="${A3S_OCI}"
export A3S_BOX_KVM_LIVE_SESSION_SERVICE_ROOT="${SERVICE_ROOT}"
export A3S_BOX_KVM_LIVE_SESSION_SERVICE_SHIM="${SHIM}"
export A3S_BOX_KVM_LIVE_SESSION_SERVICE_MANIFEST="${MANIFEST}"
export A3S_BOX_KVM_LIVE_SESSION_SERVICE_LOG="${SERVICE_LOG}"

echo "running Linux KVM live-session qualification v2"
echo "  home=${A3S_HOME}"
echo "  service-root=${SERVICE_ROOT}"
echo "  service-pid=${SERVICE_PID}"
echo "  image=${IMAGE}"
echo "  box-sha=${BOX_SHA}"
echo "  oci-sha=${OCI_SHA}"
echo "  report=${REPORT}"
echo "  session-owner=${A3S_OCI_KVM_SESSION_OWNER}"

"${EXAMPLE_BIN}"
