#!/usr/bin/env bash
# Hermetic regression tests for install-runtimeclass.sh. These tests use an
# isolated filesystem root and fake containerd/systemctl commands; they never
# modify the host's containerd configuration or system directories.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
INSTALLER="$SCRIPT_DIR/install-runtimeclass.sh"
VERSION="v3.0.12"
ARCH="x86_64"
PACKAGE_DIR="a3s-box-${VERSION}-linux-${ARCH}"
TARBALL="${PACKAGE_DIR}.tar.gz"
SHIM_ASSET="containerd-shim-a3s-box-v2-linux-${ARCH}"
CHECKSUM_ASSET="${PACKAGE_DIR}.sha256"

TEST_TMP="$(mktemp -d)"
trap 'rm -rf -- "$TEST_TMP"' EXIT

fail() {
  echo "FAIL: $*" >&2
  exit 1
}

assert_contains() {
  local file="$1"
  local expected="$2"
  grep -Fq "$expected" "$file" ||
    fail "$file does not contain: $expected"
}

make_assets() {
  local destination="$1"
  local fixture="$destination/fixture/$PACKAGE_DIR"
  mkdir -p "$fixture/lib" "$destination/artifacts"

  cat > "$fixture/a3s-box" <<'SCRIPT'
#!/usr/bin/env bash
echo "a3s-box 3.0.12"
SCRIPT
  cat > "$fixture/a3s-box-cri" <<'SCRIPT'
#!/usr/bin/env bash
exit 0
SCRIPT
  cp "$fixture/a3s-box-cri" "$fixture/a3s-box-guest-init"
  cp "$fixture/a3s-box-cri" "$fixture/a3s-box-shim"
  printf 'fake libkrun\n' > "$fixture/lib/libkrun.so"
  chmod 0755 "$fixture"/a3s-box*

  tar czf "$destination/artifacts/$TARBALL" \
    -C "$destination/fixture" "$PACKAGE_DIR"

  # Deliberately provide only the legacy, unsuffixed local shim. The installer
  # must canonicalize it to SHIM_ASSET before checking the release manifest.
  cat > "$destination/artifacts/containerd-shim-a3s-box-v2" <<'SCRIPT'
#!/usr/bin/env bash
exit 0
SCRIPT
  chmod 0755 "$destination/artifacts/containerd-shim-a3s-box-v2"

  local tar_hash shim_hash
  tar_hash="$(sha256sum "$destination/artifacts/$TARBALL" | awk '{print $1}')"
  shim_hash="$(sha256sum "$destination/artifacts/containerd-shim-a3s-box-v2" | awk '{print $1}')"
  {
    printf '%s  %s\n' "$tar_hash" "$TARBALL"
    printf '%s  %s\n' "$shim_hash" "$SHIM_ASSET"
  } > "$destination/artifacts/$CHECKSUM_ASSET"
}

make_fake_commands() {
  local fake_bin="$1"
  mkdir -p "$fake_bin"

  cat > "$fake_bin/uname" <<'SCRIPT'
#!/usr/bin/env bash
[ "${1:-}" = "-m" ] || exit 2
echo x86_64
SCRIPT
  cat > "$fake_bin/containerd" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
if [ "$#" -eq 2 ] && [ "$1" = "config" ] && [ "$2" = "dump" ]; then
  if [ -e "$FAKE_CONTAINERD_RESTARTED" ]; then
    cat "$FAKE_CONTAINERD_POST_DUMP"
  else
    cat "$FAKE_CONTAINERD_PRE_DUMP"
  fi
  exit 0
fi
echo "unexpected fake containerd invocation: $*" >&2
exit 2
SCRIPT
  cat > "$fake_bin/systemctl" <<'SCRIPT'
#!/usr/bin/env bash
set -euo pipefail
if [ "$#" -eq 2 ] && [ "$1" = "restart" ] && [ "$2" = "containerd" ]; then
  : > "$FAKE_CONTAINERD_RESTARTED"
  exit 0
fi
if [ "$#" -eq 3 ] && [ "$1" = "is-active" ] && [ "$2" = "--quiet" ] && [ "$3" = "containerd" ]; then
  [ -e "$FAKE_CONTAINERD_RESTARTED" ]
  exit
fi
echo "unexpected fake systemctl invocation: $*" >&2
exit 2
SCRIPT
  chmod 0755 "$fake_bin"/*
}

prepare_case_root() {
  local case_dir="$1"
  mkdir -p "$case_dir/root/dev" "$case_dir/root/etc/containerd"
  : > "$case_dir/root/dev/kvm"
}

run_installer() {
  local case_dir="$1"
  env \
    PATH="$FAKE_BIN:$PATH" \
    A3S_INSTALL_ROOT="$case_dir/root" \
    A3S_INSTALL_RESTART_DELAY=0 \
    FAKE_CONTAINERD_RESTARTED="$case_dir/containerd-restarted" \
    FAKE_CONTAINERD_PRE_DUMP="$case_dir/pre-dump.toml" \
    FAKE_CONTAINERD_POST_DUMP="$case_dir/post-dump.toml" \
    bash "$INSTALLER" \
      --version "$VERSION" \
      --from-dir "$case_dir/artifacts" \
      --no-warmup
}

test_v2_main_config() {
  local case_dir="$TEST_TMP/v2"
  make_assets "$case_dir"
  prepare_case_root "$case_dir"
  printf 'version = 2\n' > "$case_dir/root/etc/containerd/config.toml"
  printf 'version = 2\n' > "$case_dir/pre-dump.toml"
  cat > "$case_dir/post-dump.toml" <<'TOML'
version = 2
[plugins."io.containerd.grpc.v1.cri".containerd.runtimes.a3s-box]
  runtime_type = "io.containerd.a3s-box.v2"
TOML

  run_installer "$case_dir" > "$case_dir/output.log" 2>&1

  assert_contains "$case_dir/root/etc/containerd/config.toml" \
    "[plugins.'io.containerd.grpc.v1.cri'.containerd.runtimes.a3s-box]"
  assert_contains "$case_dir/root/etc/containerd/config.toml" \
    "runtime_type = 'io.containerd.a3s-box.v2'"
  cmp "$case_dir/artifacts/containerd-shim-a3s-box-v2" \
    "$case_dir/root/usr/local/bin/containerd-shim-a3s-box-v2" ||
    fail "the unsuffixed local shim was not installed verbatim"
}

test_v3_drop_in() {
  local case_dir="$TEST_TMP/v3"
  make_assets "$case_dir"
  prepare_case_root "$case_dir"
  cat > "$case_dir/root/etc/containerd/config.toml" <<'TOML'
version = 3
imports = ["/etc/containerd/conf.d/*.toml"]
TOML
  printf 'version = 3\n' > "$case_dir/pre-dump.toml"
  cat > "$case_dir/post-dump.toml" <<'TOML'
version = 3
[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.a3s-box]
  runtime_type = 'io.containerd.a3s-box.v2'
TOML

  run_installer "$case_dir" > "$case_dir/output.log" 2>&1

  local drop_in="$case_dir/root/etc/containerd/conf.d/a3s-box.toml"
  assert_contains "$drop_in" "version = 3"
  assert_contains "$drop_in" \
    "[plugins.'io.containerd.cri.v1.runtime'.containerd.runtimes.a3s-box]"
  if grep -Fq "runtimes.a3s-box" "$case_dir/root/etc/containerd/config.toml"; then
    fail "v3 runtime block was appended to the main config instead of the drop-in"
  fi
}

test_tampered_local_asset() {
  local case_dir="$TEST_TMP/tampered"
  make_assets "$case_dir"
  prepare_case_root "$case_dir"
  printf 'tampered\n' >> "$case_dir/artifacts/containerd-shim-a3s-box-v2"
  printf 'version = 3\n' > "$case_dir/pre-dump.toml"
  printf 'version = 3\n' > "$case_dir/post-dump.toml"

  if run_installer "$case_dir" > "$case_dir/output.log" 2>&1; then
    fail "installer accepted a tampered local shim"
  fi
  assert_contains "$case_dir/output.log" "release checksum verification failed"
  [ ! -e "$case_dir/root/usr/local/bin/a3s-box" ] ||
    fail "installer modified the target root after checksum verification failed"
}

test_ambiguous_unsuffixed_manifest_entry() {
  local case_dir="$TEST_TMP/ambiguous-manifest"
  make_assets "$case_dir"
  prepare_case_root "$case_dir"
  local tar_hash shim_hash
  tar_hash="$(sha256sum "$case_dir/artifacts/$TARBALL" | awk '{print $1}')"
  shim_hash="$(sha256sum "$case_dir/artifacts/containerd-shim-a3s-box-v2" | awk '{print $1}')"
  {
    printf '%s  %s\n' "$tar_hash" "$TARBALL"
    printf '%s  %s\n' "$shim_hash" "containerd-shim-a3s-box-v2"
  } > "$case_dir/artifacts/$CHECKSUM_ASSET"
  printf 'version = 3\n' > "$case_dir/pre-dump.toml"
  printf 'version = 3\n' > "$case_dir/post-dump.toml"

  if run_installer "$case_dir" > "$case_dir/output.log" 2>&1; then
    fail "installer accepted an ambiguous unsuffixed checksum entry"
  fi
  assert_contains "$case_dir/output.log" \
    "$CHECKSUM_ASSET contains unexpected path: containerd-shim-a3s-box-v2"
}

test_missing_post_restart_handler() {
  local case_dir="$TEST_TMP/missing-handler"
  make_assets "$case_dir"
  prepare_case_root "$case_dir"
  printf 'version = 3\n' > "$case_dir/root/etc/containerd/config.toml"
  printf 'version = 3\n' > "$case_dir/pre-dump.toml"
  printf 'version = 3\n' > "$case_dir/post-dump.toml"

  if run_installer "$case_dir" > "$case_dir/output.log" 2>&1; then
    fail "installer accepted a post-restart config without the runtime handler"
  fi
  assert_contains "$case_dir/output.log" \
    "containerd config dump does not contain the a3s-box handler in io.containerd.cri.v1.runtime"
}

FAKE_BIN="$TEST_TMP/fake-bin"
make_fake_commands "$FAKE_BIN"
test_v2_main_config
test_v3_drop_in
test_tampered_local_asset
test_ambiguous_unsuffixed_manifest_entry
test_missing_post_restart_handler

echo "install-runtimeclass.sh tests passed"
