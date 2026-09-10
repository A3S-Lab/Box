#!/usr/bin/env bash
# Elevate Native Linux OCI owner spawn for matched-cred Sandbox CI.
# Invoked as A3S_BOX_CI_SETPRIV_WRAPPER with MATCHED_CREDS cleared.
set -euo pipefail

SELF="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)/$(basename "${BASH_SOURCE[0]}")"

if [[ "${EUID}" -ne 0 ]]; then
  exec sudo -n -- "$SELF" "$@"
fi

# Prefer the invoking Sandbox identity (sudo preserves SUDO_UID/GID). Fall back
# to explicit CI exports when already root (direct root test harnesses).
uid="${SUDO_UID:-${A3S_BOX_CI_SANDBOX_UID:-}}"
gid="${SUDO_GID:-${A3S_BOX_CI_SANDBOX_GID:-}}"
if [[ ! "${uid}" =~ ^[1-9][0-9]*$ || ! "${gid}" =~ ^[1-9][0-9]*$ ]]; then
  echo "A3S_BOX_CI_SANDBOX_UID/GID or SUDO_UID/GID required" >&2
  exit 2
fi

# Do not migrate the owner into the harness probe cgroup.
unset A3S_BOX_CI_PROBE_CGROUP || true
unset A3S_BOX_CI_SETPRIV_MATCHED_CREDS || true
# sudo env_reset drops harness exports; Live qualification requires supervised
# create on the owner process itself.
export A3S_OCI_NATIVE_SESSION_SUPERVISOR=1

exec setpriv \
  --ruid="${uid}" --euid=0 \
  --rgid="${gid}" --egid=0 \
  --clear-groups -- "$@"
