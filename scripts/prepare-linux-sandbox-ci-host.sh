#!/usr/bin/env bash
# Prepare a GitHub Actions / qualification host for Sandbox OCI owners that
# require rootless device-policy bootstrap (non-root real UID/GID + effective
# root) and an explicit delegated cgroup v2 root.
#
# The packaged launcher under $A3S_HOME/bin often lives on a nosuid filesystem
# (/tmp), so CI mirrors the OCI Runtime setpriv fixture instead of relying on
# chmod 4755 there. Operators install the setuid launcher under libexec.
set -euo pipefail

SCRIPT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"

if [[ "$(uname -s)" != Linux ]]; then
  echo "Linux Sandbox CI host preparation requires Linux" >&2
  exit 1
fi

if [[ "${EUID}" -ne 0 ]]; then
  # Absolute path + bash: sudo does not resolve relative paths, and the helper
  # may not be marked executable in the git tree.
  exec sudo -E -- bash "$SCRIPT_PATH" "$@"
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

cgroup_root="${A3S_BOX_CI_CGROUP_ROOT:-/sys/fs/cgroup/a3s-box-ci}"
probe_cgroup="${cgroup_root}/probe"
delegated_cgroup="${cgroup_root}/delegated"
controllers="+cpu +cpuset +memory +pids"

if [[ ! -e /sys/fs/cgroup/cgroup.controllers ]]; then
  echo "cgroup v2 is required at /sys/fs/cgroup" >&2
  exit 1
fi

enable_controllers() {
  local target="$1"
  # shellcheck disable=SC2086 # controllers is a fixed token list.
  printf '%s' ${controllers} >"${target}/cgroup.subtree_control"
}

rm -rf --one-file-system -- "${cgroup_root}"
mkdir -p "${cgroup_root}"
enable_controllers /sys/fs/cgroup || true
enable_controllers "${cgroup_root}"
mkdir -p "${probe_cgroup}" "${delegated_cgroup}"
enable_controllers "${delegated_cgroup}"
chown "${identity_uid}:${identity_gid}" \
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
