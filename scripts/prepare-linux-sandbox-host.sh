#!/usr/bin/env bash
# Prepare a Linux host for A3S Box Sandbox (`--isolation sandbox`).
#
# Operator path (production):
#   sudo bash scripts/prepare-linux-sandbox-host.sh \
#     --install-launcher /path/to/a3s-oci
#
# Installs the setuid launcher at
# `/usr/local/libexec/a3s-box-sandbox-oci-launcher`, prepares a delegated
# cgroup v2 tree, and ensures subordinate UID/GID ranges plus userns sysctls.
#
# CI / qualification path (nosuid homes often cannot use chmod 4755):
#   sudo bash scripts/prepare-linux-sandbox-host.sh --ci
#
# CI mode prepares the same cgroup/subuid surface and writes GITHUB_ENV, but
# does not install a setuid launcher (Sandbox CI uses setpriv instead).
set -euo pipefail

SCRIPT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"
SYSTEM_LAUNCHER="/usr/local/libexec/a3s-box-sandbox-oci-launcher"
CI_MODE=0
LAUNCHER_SOURCE=""

usage() {
  cat <<'EOF'
Usage:
  prepare-linux-sandbox-host.sh [--ci] [--install-launcher ABS_PATH]

  --ci                   Qualification mode: write GITHUB_ENV, skip setuid install
  --install-launcher P   Install P (usually a3s-oci bytes) as setuid system launcher
EOF
}

while [[ $# -gt 0 ]]; do
  case "$1" in
    --ci) CI_MODE=1; shift ;;
    --install-launcher)
      LAUNCHER_SOURCE="${2:?--install-launcher requires an absolute path}"
      shift 2
      ;;
    -h|--help) usage; exit 0 ;;
    *)
      echo "unknown argument: $1" >&2
      usage >&2
      exit 2
      ;;
  esac
done

if [[ "$(uname -s)" != Linux ]]; then
  echo "Linux Sandbox host preparation requires Linux" >&2
  exit 1
fi

if [[ "${EUID}" -ne 0 ]]; then
  reexec=(bash "$SCRIPT_PATH")
  if [[ "${CI_MODE}" -eq 1 ]]; then
    reexec+=(--ci)
  fi
  if [[ -n "${LAUNCHER_SOURCE}" ]]; then
    reexec+=(--install-launcher "${LAUNCHER_SOURCE}")
  fi
  exec sudo -E -- "${reexec[@]}"
fi

identity_uid="${SUDO_UID:-}"
identity_gid="${SUDO_GID:-}"
if [[ ! "${identity_uid}" =~ ^[1-9][0-9]*$ ]]; then
  if id -u runner >/dev/null 2>&1; then
    identity_uid="$(id -u runner)"
    identity_gid="$(id -g runner)"
  else
    echo "refusing to invent a non-root Sandbox identity; invoke via sudo from a non-root user" >&2
    exit 1
  fi
fi
if [[ ! "${identity_gid}" =~ ^[1-9][0-9]*$ ]]; then
  identity_gid="${identity_uid}"
fi

if [[ "${CI_MODE}" -eq 1 ]]; then
  cgroup_root="${A3S_BOX_CI_CGROUP_ROOT:-/sys/fs/cgroup/a3s-box-ci}"
else
  cgroup_root="${A3S_BOX_SANDBOX_CGROUP_ROOT:-/sys/fs/cgroup/a3s-box-sandbox}"
fi
probe_cgroup="${cgroup_root}/probe"
delegated_cgroup="${cgroup_root}/delegated"

if [[ ! -e /sys/fs/cgroup/cgroup.controllers ]]; then
  echo "cgroup v2 is required at /sys/fs/cgroup" >&2
  exit 1
fi

enable_controllers() {
  local target="$1"
  printf '+cpu +cpuset +memory +pids' >"${target}/cgroup.subtree_control"
}

rm -rf --one-file-system -- "${cgroup_root}"
printf '+cpu +cpuset +memory +pids' >/sys/fs/cgroup/cgroup.subtree_control || true
mkdir -p "${cgroup_root}"
enable_controllers "${cgroup_root}"
mkdir -p "${probe_cgroup}" "${delegated_cgroup}"
enable_controllers "${delegated_cgroup}"
chown "${identity_uid}:${identity_gid}" \
  "${cgroup_root}" \
  "${cgroup_root}/cgroup.procs" \
  "${cgroup_root}/cgroup.subtree_control" \
  "${probe_cgroup}" \
  "${delegated_cgroup}" \
  "${probe_cgroup}/cgroup.procs" \
  "${probe_cgroup}/cgroup.subtree_control" \
  "${delegated_cgroup}/cgroup.procs" \
  "${delegated_cgroup}/cgroup.subtree_control"
test -z "$(cat "${delegated_cgroup}/cgroup.procs")"

identity_name="$(getent passwd "${identity_uid}" | cut -d: -f1 || true)"
ensure_subordinate_range() {
  local database="$1"
  local name="$2"
  local start="$3"
  if [[ -z "${name}" ]]; then
    return 0
  fi
  if ! grep -q "^${name}:" "${database}"; then
    printf '%s:%s:65536\n' "${name}" "${start}" >>"${database}"
  fi
}
ensure_subordinate_range /etc/subuid root 100000
ensure_subordinate_range /etc/subgid root 100000
ensure_subordinate_range /etc/subuid "${identity_name}" 200000
ensure_subordinate_range /etc/subgid "${identity_name}" 200000

if [[ -f /proc/sys/kernel/unprivileged_userns_clone ]]; then
  if [[ "$(cat /proc/sys/kernel/unprivileged_userns_clone)" == 0 ]]; then
    sysctl -w kernel.unprivileged_userns_clone=1 >/dev/null
  fi
fi
if [[ -f /proc/sys/kernel/apparmor_restrict_unprivileged_userns ]]; then
  if [[ "$(cat /proc/sys/kernel/apparmor_restrict_unprivileged_userns)" == 1 ]]; then
    sysctl -w kernel.apparmor_restrict_unprivileged_userns=0 >/dev/null
  fi
fi

if [[ "${CI_MODE}" -eq 1 ]]; then
  if [[ -n "${GITHUB_ENV:-}" ]]; then
    {
      echo "A3S_BOX_CI_CGROUP_ROOT=${cgroup_root}"
      echo "A3S_BOX_CI_PROBE_CGROUP=${probe_cgroup}"
      echo "A3S_BOX_SANDBOX_DELEGATED_CGROUP_ROOT=${delegated_cgroup}"
      echo "A3S_BOX_CI_SANDBOX_UID=${identity_uid}"
      echo "A3S_BOX_CI_SANDBOX_GID=${identity_gid}"
    } >>"${GITHUB_ENV}"
  fi
  printf 'Prepared Sandbox CI cgroup tree at %s (identity %s:%s)\n' \
    "${cgroup_root}" "${identity_uid}" "${identity_gid}"
  printf 'A3S_BOX_SANDBOX_DELEGATED_CGROUP_ROOT=%s\n' "${delegated_cgroup}"
  exit 0
fi

# Operator mode: export delegated root for the calling shell when possible, and
# optionally install the system setuid launcher on a non-nosuid filesystem.
if [[ -n "${LAUNCHER_SOURCE}" ]]; then
  if [[ "${LAUNCHER_SOURCE}" != /* || "${LAUNCHER_SOURCE}" == *"/./"* || "${LAUNCHER_SOURCE}" == *"/../"* ]]; then
    echo "--install-launcher must be an absolute normalized path" >&2
    exit 1
  fi
  if [[ ! -f "${LAUNCHER_SOURCE}" || ! -x "${LAUNCHER_SOURCE}" ]]; then
    echo "launcher source is missing or not executable: ${LAUNCHER_SOURCE}" >&2
    exit 1
  fi
  install -d -m 0755 /usr/local/libexec
  install -m 0755 "${LAUNCHER_SOURCE}" "${SYSTEM_LAUNCHER}"
  # Setuid is required so non-root callers can enter the Sandbox namespaces.
  chmod 4755 "${SYSTEM_LAUNCHER}"
  chown root:root "${SYSTEM_LAUNCHER}"
  printf 'Installed setuid Sandbox OCI launcher at %s\n' "${SYSTEM_LAUNCHER}"
else
  printf 'Skipped setuid launcher install (pass --install-launcher ABS_PATH).\n'
  printf 'Discovery still checks %s when present.\n' "${SYSTEM_LAUNCHER}"
fi

printf 'Prepared Sandbox operator cgroup tree at %s (identity %s:%s)\n' \
  "${cgroup_root}" "${identity_uid}" "${identity_gid}"
printf 'Export for this host:\n'
printf '  export A3S_BOX_SANDBOX_DELEGATED_CGROUP_ROOT=%s\n' "${delegated_cgroup}"
printf 'Then run without A3S_BOX_OCI_MIGRATION (Sandbox GA default):\n'
printf '  a3s-box run --rm --isolation sandbox alpine:3.20 -- sleep 5\n'
