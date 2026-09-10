#!/usr/bin/env bash
# Local Native Linux live-session gate for Box over OCI Runtime.
#
# Must run as root for cgroup/home preparation, then setpriv-execs the example
# with matched creds (euid==ruid) via scripts/run-linux-sandbox-ci.sh — the same
# shape as Sandbox CI. Owner spawn elevates through A3S_BOX_CI_SETPRIV_WRAPPER.
#
# Observation-only. Live Host-reopen is Native-Linux-driver-only today.

set -euo pipefail

usage() {
  cat <<'EOF'
Usage (as root, after prepare-linux-sandbox-ci-host.sh):
  linux-native-live-session-qualification.sh \
    --box-bin ABS_DIR \
    --a3s-oci ABS_BIN \
    --a3s-oci-agent ABS_BIN \
    --image REF \
    --report ABS_JSON \
    [--home ABS_DIR] \
    [--host-root ABS_DIR] \
    [--box-sha SHA] \
    [--oci-sha SHA]

Required environment from prepare-linux-sandbox-ci-host.sh:
  A3S_BOX_CI_SANDBOX_UID / A3S_BOX_CI_SANDBOX_GID
  A3S_BOX_SANDBOX_DELEGATED_CGROUP_ROOT
  A3S_BOX_CI_PROBE_CGROUP (recommended)

Forces A3S_OCI_NATIVE_SESSION_SUPERVISOR=1 and
A3S_BOX_CI_SETPRIV_MATCHED_CREDS=1 with
SETPRIV_WRAPPER=elevate-linux-sandbox-owner.sh (passwordless sudo for that
helper, or root).
EOF
}

if [[ "$(uname -s)" != Linux ]]; then
  echo "Native Linux live-session qualification requires Linux" >&2
  exit 1
fi
if [[ "${EUID}" -ne 0 ]]; then
  echo "this runner must start as root to prepare cgroup/home then setpriv the example" >&2
  exit 2
fi

BOX_BIN=""
A3S_OCI=""
A3S_OCI_AGENT=""
IMAGE=""
REPORT=""
HOME_DIR=""
HOST_ROOT=""
BOX_SHA=""
OCI_SHA=""

while [[ $# -gt 0 ]]; do
  case "$1" in
    --box-bin) BOX_BIN="${2:?}"; shift 2 ;;
    --a3s-oci) A3S_OCI="${2:?}"; shift 2 ;;
    --a3s-oci-agent) A3S_OCI_AGENT="${2:?}"; shift 2 ;;
    --image) IMAGE="${2:?}"; shift 2 ;;
    --report) REPORT="${2:?}"; shift 2 ;;
    --home) HOME_DIR="${2:?}"; shift 2 ;;
    --host-root) HOST_ROOT="${2:?}"; shift 2 ;;
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
require_abs "--a3s-oci-agent" "${A3S_OCI_AGENT}"
require_abs "--report" "${REPORT}"

if [[ -z "$IMAGE" ]]; then
  echo "--image is required" >&2
  exit 2
fi

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
BOX_REPO="$(cd "${SCRIPT_DIR}/.." && pwd)"
OCI_REPO_CANDIDATE="$(cd "${BOX_REPO}/../oci-runtime" 2>/dev/null && pwd || true)"
SETPRIV_WRAPPER="${SCRIPT_DIR}/elevate-linux-sandbox-owner.sh"

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
  HOME_DIR="$(mktemp -d /tmp/a3s-box-native-live-session.XXXXXX)"
fi
require_abs "--home" "${HOME_DIR}"
case "$(basename "$HOME_DIR")" in
  *native-live-session*) ;;
  *)
    echo "A3S_HOME basename must contain native-live-session" >&2
    exit 2
    ;;
esac

if [[ -z "$HOST_ROOT" ]]; then
  HOST_ROOT="${HOME_DIR}/oci-host"
fi
require_abs "--host-root" "${HOST_ROOT}"

EXAMPLE_BIN="${BOX_BIN}/linux-native-live-session-qualification"
for path in "${BOX_BIN}/a3s-box" "${EXAMPLE_BIN}" "${A3S_OCI}" "${A3S_OCI_AGENT}" "${SETPRIV_WRAPPER}"; do
  if [[ ! -x "$path" ]]; then
    echo "missing executable ${path}" >&2
    exit 2
  fi
done
if [[ -e "${REPORT}" ]]; then
  echo "refusing to overwrite existing report ${REPORT}" >&2
  exit 2
fi

uid="${A3S_BOX_CI_SANDBOX_UID:-}"
gid="${A3S_BOX_CI_SANDBOX_GID:-}"
delegated="${A3S_BOX_SANDBOX_DELEGATED_CGROUP_ROOT:-}"
probe="${A3S_BOX_CI_PROBE_CGROUP:-}"
if [[ ! "${uid}" =~ ^[1-9][0-9]*$ || ! "${gid}" =~ ^[1-9][0-9]*$ ]]; then
  echo "A3S_BOX_CI_SANDBOX_UID/GID required (run scripts/prepare-linux-sandbox-ci-host.sh via sudo)" >&2
  exit 2
fi
if [[ -z "${delegated}" || ! -d "${delegated}" ]]; then
  echo "A3S_BOX_SANDBOX_DELEGATED_CGROUP_ROOT must be a prepared directory" >&2
  exit 2
fi

mkdir -p "${HOME_DIR}/bin"
mkdir -m 0700 -p "${HOST_ROOT}"
chown -R "${uid}:${gid}" "${HOME_DIR}"

if [[ -x "${BOX_BIN}/a3s-box-shim" ]]; then
  install -o "${uid}" -g "${gid}" -m 755 "${BOX_BIN}/a3s-box-shim" "${HOME_DIR}/bin/a3s-box-shim"
elif [[ ! -x "${HOME_DIR}/bin/a3s-box-shim" ]]; then
  echo "missing ${BOX_BIN}/a3s-box-shim" >&2
  exit 2
fi
if [[ -x "${BOX_BIN}/a3s-box-guest-init" ]]; then
  install -o "${uid}" -g "${gid}" -m 755 "${BOX_BIN}/a3s-box-guest-init" "${HOME_DIR}/bin/a3s-box-guest-init"
fi
install -o "${uid}" -g "${gid}" -m 755 "${A3S_OCI}" "${HOME_DIR}/bin/a3s-oci"
install -o "${uid}" -g "${gid}" -m 755 "${A3S_OCI_AGENT}" "${HOME_DIR}/bin/a3s-oci-agent"

export PATH="${BOX_BIN}:${PATH}"
export A3S_HOME="${HOME_DIR}"
export A3S_BOX_OCI_HOST_ROOT="${HOST_ROOT}"
export A3S_BOX_OCI_RUNTIME_PATH="${HOME_DIR}/bin/a3s-oci"
export A3S_BOX_OCI_AGENT_PATH="${HOME_DIR}/bin/a3s-oci-agent"
export A3S_BOX_NATIVE_LIVE_SESSION_IMAGE="${IMAGE}"
export A3S_BOX_NATIVE_LIVE_SESSION_REPORT="${REPORT}"
export A3S_BOX_NATIVE_LIVE_SESSION_BOX_SHA="${BOX_SHA}"
export A3S_BOX_NATIVE_LIVE_SESSION_OCI_SHA="${OCI_SHA}"
export A3S_BOX_NATIVE_LIVE_SESSION_QUALIFICATION=1
export A3S_OCI_NATIVE_SESSION_SUPERVISOR=1
export A3S_BOX_SANDBOX_DELEGATED_CGROUP_ROOT="${delegated}"
export A3S_BOX_CI_PROBE_CGROUP="${probe}"
export A3S_BOX_CI_SANDBOX_UID="${uid}"
export A3S_BOX_CI_SANDBOX_GID="${gid}"
export A3S_BOX_CI_SETPRIV_MATCHED_CREDS=1
export A3S_BOX_CI_SETPRIV_WRAPPER="${SETPRIV_WRAPPER}"

echo "running Native Linux live-session qualification v1"
echo "  home=${A3S_HOME}"
echo "  host-root=${HOST_ROOT}"
echo "  image=${IMAGE}"
echo "  box-sha=${BOX_SHA}"
echo "  oci-sha=${OCI_SHA}"
echo "  report=${REPORT}"
echo "  matched-creds setpriv + SETPRIV_WRAPPER=${SETPRIV_WRAPPER}"
echo "  note: Live Host-reopen is Native Linux only; KVM MicroVM Live is not claimed"

exec bash "${SETPRIV_WRAPPER}" "${EXAMPLE_BIN}"
