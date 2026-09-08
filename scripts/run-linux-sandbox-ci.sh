#!/usr/bin/env bash
# Run a Sandbox CI command with non-root real UID/GID and effective root.
#
# prepare-linux-sandbox-ci-host.sh must have exported:
#   A3S_BOX_CI_SANDBOX_UID / A3S_BOX_CI_SANDBOX_GID
#   A3S_BOX_SANDBOX_DELEGATED_CGROUP_ROOT
#   A3S_BOX_CI_PROBE_CGROUP (optional; migrates this process before setpriv)
#
# Mirrors OCI Runtime native-linux-smoke rootless setpriv identity: chmod 4755
# under /tmp is ignored on nosuid mounts, so CI cannot rely on the packaged
# launcher alone.
set -euo pipefail

SCRIPT_PATH="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"

if [[ "$(uname -s)" != Linux ]]; then
  echo "Linux Sandbox CI runner requires Linux" >&2
  exit 1
fi

if [[ "${EUID}" -ne 0 ]]; then
  # Absolute path + bash: sudo does not resolve relative paths, and the helper
  # may not be marked executable in the git tree.
  exec sudo -E -- bash "$SCRIPT_PATH" "$@"
fi

if [[ "$#" -lt 1 ]]; then
  echo "usage: $0 <command> [args...]" >&2
  exit 2
fi

uid="${A3S_BOX_CI_SANDBOX_UID:-}"
gid="${A3S_BOX_CI_SANDBOX_GID:-}"
delegated="${A3S_BOX_SANDBOX_DELEGATED_CGROUP_ROOT:-}"
probe="${A3S_BOX_CI_PROBE_CGROUP:-}"

if [[ ! "${uid}" =~ ^[1-9][0-9]*$ ]]; then
  echo "A3S_BOX_CI_SANDBOX_UID is required (run prepare-linux-sandbox-ci-host.sh first)" >&2
  exit 2
fi
if [[ ! "${gid}" =~ ^[1-9][0-9]*$ ]]; then
  echo "A3S_BOX_CI_SANDBOX_GID is required (run prepare-linux-sandbox-ci-host.sh first)" >&2
  exit 2
fi
if [[ -z "${delegated}" || ! -d "${delegated}" ]]; then
  echo "A3S_BOX_SANDBOX_DELEGATED_CGROUP_ROOT must be a prepared cgroup directory" >&2
  exit 2
fi

if [[ -n "${probe}" ]]; then
  if [[ ! -w "${probe}/cgroup.procs" ]]; then
    echo "A3S_BOX_CI_PROBE_CGROUP is not writable: ${probe}" >&2
    exit 2
  fi
  # cgroup v2: writing 0 migrates the current task into the probe cgroup.
  printf 0 >"${probe}/cgroup.procs"
fi

export A3S_BOX_SANDBOX_DELEGATED_CGROUP_ROOT="${delegated}"

exec setpriv \
  --ruid="${uid}" --euid=0 \
  --rgid="${gid}" --egid=0 \
  --clear-groups -- "$@"
