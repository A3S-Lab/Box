#!/usr/bin/env bash
# Locate cargo-built libkrun / libkrunfw library directories for `just test-vm`
# and `just test-tee` (#575).
#
# Real layout (after `just build` / `just release` with A3S_BUILD_LIBKRUN=1):
#   target/{debug,release}/build/a3s-libkrun-sys-*/out/libkrun/{lib,lib64}
#   target/{debug,release}/build/a3s-libkrun-sys-*/out/libkrunfw/{lib,lib64}
#
# Do not quote the glob (bash must expand `*`). Do not insert an extra
# `libkrun/` segment between `out/` and the component name.
#
# Usage (cwd = crates/box/src, or pass --target-root):
#   eval "$(../scripts/find-libkrun-build-libs.sh --export)"
#   ../scripts/find-libkrun-build-libs.sh --print
#   ../scripts/find-libkrun-build-libs.sh --self-test

set -euo pipefail

TARGET_ROOT="target"
MODE="print"

usage() {
  cat <<'EOF'
Usage: find-libkrun-build-libs.sh [--target-root DIR] [--print|--export|--self-test]
EOF
}

while [ $# -gt 0 ]; do
  case "$1" in
    --target-root)
      TARGET_ROOT="${2:?--target-root requires a directory}"
      shift 2
      ;;
    --print)
      MODE="print"
      shift
      ;;
    --export)
      MODE="export"
      shift
      ;;
    --self-test)
      MODE="self-test"
      shift
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

find_libkrun_dir() {
  local component="$1"
  local root="$2"
  local path
  local profile crate libdir
  # Unquoted globs are intentional: bash must expand crate-* and not look for
  # a literal asterisk in the path (#575 reopen).
  for profile in debug release; do
    for crate in a3s-libkrun-sys libkrun-sys; do
      for libdir in lib64 lib; do
        # shellcheck disable=SC2086
        path=$(ls -td ${root}/${profile}/build/${crate}-*/out/${component}/${libdir} 2>/dev/null | head -1 || true)
        if [ -n "${path}" ]; then
          printf '%s\n' "${path}"
          return 0
        fi
      done
    done
  done
  return 1
}

run_self_test() {
  local fixture
  fixture=$(mktemp -d "${TMPDIR:-/tmp}/a3s-box-libkrun-find.XXXXXX")
  # Keep the path in a global so the EXIT trap still sees it after `local`
  # goes out of scope under `set -u`.
  A3S_BOX_LIBKRUN_FIND_FIXTURE="${fixture}"
  cleanup_self_test() {
    rm -rf "${A3S_BOX_LIBKRUN_FIND_FIXTURE:-}"
    unset A3S_BOX_LIBKRUN_FIND_FIXTURE
  }
  trap cleanup_self_test EXIT

  mkdir -p \
    "${fixture}/release/build/a3s-libkrun-sys-deadbeef/out/libkrun/lib64" \
    "${fixture}/release/build/a3s-libkrun-sys-deadbeef/out/libkrunfw/lib64"
  # Misleading nested layout from the broken recipe must not win.
  mkdir -p "${fixture}/release/build/a3s-libkrun-sys-deadbeef/out/libkrun/libkrun/lib64"

  local lib fw
  lib=$(find_libkrun_dir libkrun "${fixture}")
  fw=$(find_libkrun_dir libkrunfw "${fixture}")
  case "${lib}" in
    */out/libkrun/lib64) ;;
    *)
      echo "self-test: expected out/libkrun/lib64, got: ${lib}" >&2
      exit 1
      ;;
  esac
  case "${fw}" in
    */out/libkrunfw/lib64) ;;
    *)
      echo "self-test: expected out/libkrunfw/lib64, got: ${fw}" >&2
      exit 1
      ;;
  esac
  echo "libkrun build-lib finder self-test passed"
}

case "${MODE}" in
  self-test)
    run_self_test
    ;;
  print|export)
    LIBKRUN_LIB=$(find_libkrun_dir libkrun "${TARGET_ROOT}" || true)
    LIBKRUNFW_LIB=$(find_libkrun_dir libkrunfw "${TARGET_ROOT}" || true)
    if [ -z "${LIBKRUN_LIB}" ] || [ -z "${LIBKRUNFW_LIB}" ]; then
      echo "libkrun not found under ${TARGET_ROOT}/{debug,release}/build/(a3s-)libkrun-sys-*/out/{libkrun,libkrunfw}/{lib,lib64}" >&2
      exit 1
    fi
    if [ "${MODE}" = "export" ]; then
      printf "LIBKRUN_LIB=%q\n" "${LIBKRUN_LIB}"
      printf "LIBKRUNFW_LIB=%q\n" "${LIBKRUNFW_LIB}"
    else
      printf 'LIBKRUN_LIB=%s\n' "${LIBKRUN_LIB}"
      printf 'LIBKRUNFW_LIB=%s\n' "${LIBKRUNFW_LIB}"
    fi
    ;;
esac
