#!/usr/bin/env bash
# Compile a cargo test harness as the normal CI user, then execute it under
# run-linux-sandbox-ci.sh (setpriv Sandbox identity).
#
# setpriv with ruid != euid enables AT_SECURE, which ignores LD_LIBRARY_PATH.
# rustup's rustc wrapper needs LD_LIBRARY_PATH for librustc_driver, so cargo
# must compile outside setpriv. The resulting test binary only needs libc and
# can run under the Sandbox identity for rootless device-policy bootstrap.
#
# Usage:
#   export A3S_HOME=... A3S_BOX_*=...   # identity-sensitive env for the harness
#   bash scripts/run-linux-sandbox-cargo-test.sh \
#     -p a3s-box-runtime --lib box_runtime_passes_all_advertised_profiles \
#     -- --ignored --nocapture --exact box_runtime_passes_all_advertised_profiles --test-threads=1
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"

if [[ "$#" -lt 1 ]]; then
  echo "usage: $0 <cargo test args...> -- <harness args...>" >&2
  exit 2
fi

cargo_args=()
harness_args=()
seeing_harness=0
for arg in "$@"; do
  if [[ "${seeing_harness}" -eq 0 && "${arg}" == "--" ]]; then
    seeing_harness=1
    continue
  fi
  if [[ "${seeing_harness}" -eq 0 ]]; then
    cargo_args+=("${arg}")
  else
    harness_args+=("${arg}")
  fi
done

if [[ "${#cargo_args[@]}" -eq 0 || "${#harness_args[@]}" -eq 0 ]]; then
  echo "both cargo args and harness args (after --) are required" >&2
  exit 2
fi

before="$(mktemp)"
after="$(mktemp)"
trap 'rm -f -- "${before}" "${after}"' EXIT
find target/debug/deps -maxdepth 1 -type f -executable ! -name '*.d' -printf '%p\n' 2>/dev/null \
  | sort >"${before}" || true

cargo test --no-run "${cargo_args[@]}"

find target/debug/deps -maxdepth 1 -type f -executable ! -name '*.d' -printf '%p\n' \
  | sort >"${after}"

mapfile -t new_bins < <(comm -13 "${before}" "${after}")
if [[ "${#new_bins[@]}" -eq 0 ]]; then
  # Rebuild with identical fingerprint still refreshes mtime; fall back to newest.
  mapfile -t new_bins < <(
    find target/debug/deps -maxdepth 1 -type f -executable ! -name '*.d' -printf '%T@\t%p\n' \
      | sort -nr \
      | head -n 5 \
      | cut -f2-
  )
fi
if [[ "${#new_bins[@]}" -eq 0 ]]; then
  echo "no cargo test harness found under target/debug/deps" >&2
  exit 1
fi

prefer=""
for ((i = 0; i < ${#cargo_args[@]}; i++)); do
  case "${cargo_args[$i]}" in
    --test)
      prefer="${cargo_args[$((i + 1))]-}"
      ;;
    -p|--package)
      pkg="${cargo_args[$((i + 1))]-}"
      prefer="${pkg//-/_}"
      ;;
  esac
done

test_bin=""
if [[ -n "${prefer}" ]]; then
  for candidate in "${new_bins[@]}"; do
    base="$(basename "${candidate}")"
    if [[ "${base}" == "${prefer}-"* ]]; then
      test_bin="${candidate}"
      break
    fi
  done
fi
if [[ -z "${test_bin}" ]]; then
  test_bin="${new_bins[0]}"
fi

echo "Running Sandbox-identity harness: ${test_bin}" >&2
# Preserve the caller's environment (A3S_*, RUST_MIN_STACK, PATH, 鈥?.
exec bash "${SCRIPT_DIR}/run-linux-sandbox-ci.sh" "${test_bin}" "${harness_args[@]}"
