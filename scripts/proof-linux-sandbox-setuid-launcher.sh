#!/usr/bin/env bash
# Operator setuid Sandbox OCI launcher evidence gate (self-hosted / lab).
#
# Proves the production privilege boundary that CI setpriv on nosuid runners
# intentionally does not cover:
#   1) prepare-linux-sandbox-host.sh --install-launcher installs mode 4755
#      root:root at /usr/local/libexec/a3s-box-sandbox-oci-launcher
#   2) a non-root identity can spawn Sandbox Host without A3S_BOX_CI_SETPRIV_WRAPPER
#
# Observation-only. Does not flip B2, MicroVM cutover, or claim every distro.
# Fail closed when the target filesystem is nosuid (chmod 4755 ignored).
# The captured run status must be the a3s-box status. A bare `2>&1` on its own
# line inside $(...) is a successful null command and hides a failing run (#628).

set -euo pipefail

# Run a command, keep combined output, and store that command's status in
# CAPTURE_RC. `2>&1` stays on the same simple command. The function itself
# returns 0 so `set -e` cannot discard the recorded status.
capture_command_status() {
  local output
  set +e
  output="$("$@" 2>&1)"
  CAPTURE_RC=$?
  set -e
  CAPTURED_OUTPUT="${output}"
}

if [[ "${1:-}" == "--self-test" ]]; then
  capture_command_status bash -c 'echo boom >&2; exit 9'
  if [[ "${CAPTURE_RC}" -ne 9 || "${CAPTURED_OUTPUT}" != *boom* ]]; then
    echo "proof capture self-test: failing command reported rc=${CAPTURE_RC} output=${CAPTURED_OUTPUT}" >&2
    exit 1
  fi
  capture_command_status true
  if [[ "${CAPTURE_RC}" -ne 0 ]]; then
    echo "proof capture self-test: successful command reported rc=${CAPTURE_RC}" >&2
    exit 1
  fi
  echo "setuid-launcher proof capture self-test passed"
  exit 0
fi

usage() {
  cat <<'EOF'
Usage:
  proof-linux-sandbox-setuid-launcher.sh \
    --a3s-oci ABS_BIN \
    --box-bin ABS_DIR \
    --report ABS_JSON \
    [--home ABS_DIR] \
    [--image REF]

Requires sudo from a non-root shell (SUDO_UID/SUDO_GID). Unsets
A3S_BOX_CI_SETPRIV_WRAPPER for the proof run.
EOF
}

A3S_OCI=""
BOX_BIN=""
REPORT=""
HOME_DIR=""
IMAGE="alpine:3.20"
SYSTEM_LAUNCHER="/usr/local/libexec/a3s-box-sandbox-oci-launcher"
DELEGATED_CGROUP="/sys/fs/cgroup/a3s-box-sandbox/delegated"
SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

while [[ $# -gt 0 ]]; do
  case "$1" in
    --a3s-oci) A3S_OCI="${2:?}"; shift 2 ;;
    --box-bin) BOX_BIN="${2:?}"; shift 2 ;;
    --report) REPORT="${2:?}"; shift 2 ;;
    --home) HOME_DIR="${2:?}"; shift 2 ;;
    --image) IMAGE="${2:?}"; shift 2 ;;
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

require_abs "--a3s-oci" "${A3S_OCI}"
require_abs "--box-bin" "${BOX_BIN}"
require_abs "--report" "${REPORT}"

if [[ ! -x "${A3S_OCI}" ]]; then
  echo "missing executable ${A3S_OCI}" >&2
  exit 2
fi
if [[ ! -x "${BOX_BIN}/a3s-box" ]]; then
  echo "missing executable ${BOX_BIN}/a3s-box" >&2
  exit 2
fi
if [[ -e "${REPORT}" ]]; then
  echo "refusing to overwrite existing report ${REPORT}" >&2
  exit 2
fi

if [[ -z "${HOME_DIR}" ]]; then
  HOME_DIR="$(mktemp -d /tmp/a3s-box-setuid-launcher-proof.XXXXXX)"
fi
require_abs "--home" "${HOME_DIR}"

if [[ "$(id -u)" -eq 0 && -z "${SUDO_UID:-}" ]]; then
  echo "invoke via sudo from a non-root shell so SUDO_UID/SUDO_GID identify the proof user" >&2
  exit 2
fi

if [[ "$(id -u)" -ne 0 ]]; then
  if [[ ! -d "${DELEGATED_CGROUP}" ]] && ! sudo -n true >/dev/null 2>&1; then
    echo "non-root Sandbox operator path was not started: missing delegated cgroup ${DELEGATED_CGROUP}; sudo requires a password; this is not an A3S_BOX_CI_SETPRIV_WRAPPER pass" >&2
    exit 2
  fi
  echo "re-executing under sudo for setuid install + cgroup prep" >&2
  exec sudo -E env \
    "PATH=${PATH}" \
    "HOME=${HOME}" \
    bash "${BASH_SOURCE[0]}" \
    --a3s-oci "${A3S_OCI}" \
    --box-bin "${BOX_BIN}" \
    --report "${REPORT}" \
    --home "${HOME_DIR}" \
    --image "${IMAGE}"
fi

PROOF_UID="${SUDO_UID}"
PROOF_GID="${SUDO_GID:-${PROOF_UID}}"
if [[ ! "${PROOF_UID}" =~ ^[1-9][0-9]*$ ]]; then
  echo "setuid proof requires a non-root SUDO_UID" >&2
  exit 2
fi

bash "${SCRIPT_DIR}/prepare-linux-sandbox-host.sh" \
  --install-launcher "${A3S_OCI}"

mode="$(stat -c '%a' "${SYSTEM_LAUNCHER}")"
owner_uid="$(stat -c '%u' "${SYSTEM_LAUNCHER}")"
owner_gid="$(stat -c '%g' "${SYSTEM_LAUNCHER}")"
digest="$(sha256sum "${SYSTEM_LAUNCHER}" | awk '{print $1}')"

if [[ "${mode}" != 4755 || "${owner_uid}" != 0 ]]; then
  echo "setuid launcher evidence incomplete: ${SYSTEM_LAUNCHER} mode=${mode} uid=${owner_uid}" >&2
  exit 1
fi
if [[ ! -d "${DELEGATED_CGROUP}" ]]; then
  echo "missing delegated cgroup ${DELEGATED_CGROUP} after host prep" >&2
  exit 1
fi

mkdir -p "${HOME_DIR}"
chown "${PROOF_UID}:${PROOF_GID}" "${HOME_DIR}"

STATUS="failed"
ERROR=""
capture_command_status \
  sudo -u "#${PROOF_UID}" -g "#${PROOF_GID}" \
  env -u A3S_BOX_CI_SETPRIV_WRAPPER -u A3S_BOX_CI_SETPRIV_MATCHED_CREDS -u A3S_BOX_OCI_MIGRATION \
  "PATH=${BOX_BIN}:${PATH}" \
  "HOME=${HOME_DIR}" \
  "A3S_HOME=${HOME_DIR}" \
  "A3S_BOX_SANDBOX_OCI_LAUNCHER=${SYSTEM_LAUNCHER}" \
  "A3S_BOX_SANDBOX_DELEGATED_CGROUP_ROOT=${DELEGATED_CGROUP}" \
  "${BOX_BIN}/a3s-box" run --rm --isolation sandbox "${IMAGE}" -- /bin/sh -c 'printf setuid-launcher-ok; exit 0'
RC="${CAPTURE_RC}"
PROOF_OUTPUT="${CAPTURED_OUTPUT}"
if [[ "${RC}" -eq 0 ]] && grep -q 'setuid-launcher-ok' <<<"${PROOF_OUTPUT}"; then
  STATUS="passed"
else
  ERROR="non-root Sandbox run without SETPRIV_WRAPPER failed (rc=${RC}): ${PROOF_OUTPUT}"
fi

mkdir -p "$(dirname "${REPORT}")"
python3 - "${REPORT}" "${STATUS}" "${ERROR}" "${SYSTEM_LAUNCHER}" "${mode}" "${owner_uid}" "${owner_gid}" "${digest}" "${PROOF_UID}" <<'PY'
import json, sys

(
    report_path,
    status,
    error,
    launcher,
    mode,
    owner_uid,
    owner_gid,
    digest,
    proof_uid,
) = sys.argv[1:]
payload = {
    "schema_version": "a3s.box.linux-sandbox-setuid-launcher-proof.v1",
    "status": status,
    "launcher_path": launcher,
    "launcher_mode": mode,
    "launcher_uid": int(owner_uid),
    "launcher_gid": int(owner_gid),
    "launcher_sha256": digest,
    "proof_uid": int(proof_uid),
    "setpriv_wrapper_unset": True,
    "ci_setpriv_not_claimed_as_setuid": True,
    "b2_process_session_recovery_closed": False,
    "microvm_cutover_claimed": False,
}
if error:
    payload["error"] = error
with open(report_path, "w", encoding="utf-8") as handle:
    json.dump(payload, handle, indent=2)
    handle.write("\n")
raise SystemExit(0 if status == "passed" else 1)
PY

if [[ "${STATUS}" != "passed" ]]; then
  echo "${ERROR}" >&2
  exit 1
fi

python3 "${SCRIPT_DIR}/verify-linux-sandbox-setuid-launcher-proof.py" "${REPORT}"

echo "setuid launcher proof passed: ${SYSTEM_LAUNCHER} mode ${mode} root:root"
echo "  report=${REPORT}"
