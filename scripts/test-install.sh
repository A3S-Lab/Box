#!/usr/bin/env bash
# Hermetic regression tests for the Linux/macOS one-click installer.
set -euo pipefail

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPOSITORY_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
INSTALLER="$REPOSITORY_ROOT/install.sh"
VERSION="9.8.7"
TAG="v$VERSION"

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

sha256_file() {
  if command -v sha256sum >/dev/null 2>&1; then
    sha256sum "$1" | awk '{ print $1 }'
  else
    shasum -a 256 "$1" | awk '{ print $1 }'
  fi
}

kernel="$(uname -s)"
machine="$(uname -m)"
INSTALL_PATH="$PATH"
case "$kernel:$machine" in
  Linux:x86_64|Linux:amd64)
    PLATFORM="linux-x86_64"
    ;;
  Linux:aarch64|Linux:arm64)
    PLATFORM="linux-arm64"
    ;;
  Darwin:arm64|Darwin:aarch64)
    PLATFORM="macos-arm64"
    ;;
  MINGW*:x86_64|MSYS*:x86_64)
    # This path lets contributors run the POSIX suite from Git for Windows.
    PLATFORM="linux-x86_64"
    mkdir -p "$TEST_TMP/fake-bin"
    cat > "$TEST_TMP/fake-bin/uname" <<'EOF'
#!/bin/sh
case "${1:-}" in
  -s) printf '%s\n' Linux ;;
  -m) printf '%s\n' x86_64 ;;
  *) printf '%s\n' Linux ;;
esac
EOF
    chmod 0755 "$TEST_TMP/fake-bin/uname"
    INSTALL_PATH="$TEST_TMP/fake-bin:$PATH"
    ;;
  *)
    fail "unsupported test host: $kernel $machine"
    ;;
esac

PACKAGE_DIR="a3s-box-$TAG-$PLATFORM"
ARCHIVE="$TEST_TMP/$PACKAGE_DIR.tar.gz"

make_fixture_archive() {
  local fixture_root="$TEST_TMP/fixture/$PACKAGE_DIR"
  mkdir -p "$fixture_root/lib"

  cat > "$fixture_root/a3s-box" <<EOF
#!/bin/sh
printf '%s\n' 'a3s-box $VERSION'
EOF
  cat > "$fixture_root/a3s-box-shim" <<'EOF'
#!/bin/sh
exit 0
EOF
  cp "$fixture_root/a3s-box-shim" "$fixture_root/a3s-box-guest-init"
  printf '%s\n' 'fixture runtime library' > "$fixture_root/lib/runtime.fixture"

  case "$PLATFORM" in
    linux-*)
      cp "$fixture_root/a3s-box-shim" "$fixture_root/a3s-oci"
      cp "$fixture_root/a3s-box-shim" "$fixture_root/a3s-oci-agent"
      printf '%s\n' 'fixture-revision' > "$fixture_root/OCI-RUNTIME-REVISION"
      ;;
  esac

  chmod 0755 "$fixture_root/a3s-box" \
    "$fixture_root/a3s-box-shim" \
    "$fixture_root/a3s-box-guest-init"
  case "$PLATFORM" in
    linux-*) chmod 0755 "$fixture_root/a3s-oci" "$fixture_root/a3s-oci-agent" ;;
  esac

  tar czf "$ARCHIVE" -C "$TEST_TMP/fixture" "$PACKAGE_DIR"
}

run_installer() {
  env PATH="$INSTALL_PATH" SHELL=/bin/sh \
    sh "$INSTALLER" \
      --version "$TAG" \
      --archive "$ARCHIVE" \
      --sha256 "$(sha256_file "$ARCHIVE")" \
      "$@"
}

make_fixture_archive

happy_install="$TEST_TMP/happy-install"
run_installer --install-dir "$happy_install" --no-modify-path \
  > "$TEST_TMP/happy.log"
assert_contains "$happy_install/.a3s-box-install.json" "\"version\": \"$TAG\""
assert_contains "$happy_install/.a3s-box-install.json" "\"platform\": \"$PLATFORM\""
test "$("$happy_install/a3s-box" --version)" = "a3s-box $VERSION" ||
  fail "installed fixture did not run"
test -f "$happy_install/lib/runtime.fixture" ||
  fail "installer did not preserve the package-relative library directory"

# A managed reinstall must replace the distribution, rather than retaining
# stale files from an older version.
printf '%s\n' stale > "$happy_install/stale.file"
run_installer --install-dir "$happy_install" --no-modify-path \
  > "$TEST_TMP/reinstall.log"
test ! -e "$happy_install/stale.file" ||
  fail "managed reinstall retained a stale file"

# If moving the previous installation fails, rollback must not mistake that
# still-active directory for a newly activated one and delete it.
failure_bin="$TEST_TMP/failure-bin"
mkdir "$failure_bin"
cat > "$failure_bin/mv" <<'EOF'
#!/bin/sh
if [ "${1:-}" = "${A3S_FAIL_MOVE_FROM:-}" ]; then
  exit 73
fi
exec "$A3S_REAL_MV" "$@"
EOF
chmod 0755 "$failure_bin/mv"
printf '%s\n' preserve > "$happy_install/preserve.file"
if env \
  PATH="$failure_bin:$INSTALL_PATH" \
  SHELL=/bin/sh \
  A3S_FAIL_MOVE_FROM="$happy_install" \
  A3S_REAL_MV="$(command -v mv)" \
  sh "$INSTALLER" \
    --version "$TAG" \
    --archive "$ARCHIVE" \
    --sha256 "$(sha256_file "$ARCHIVE")" \
    --install-dir "$happy_install" \
    --no-modify-path > "$TEST_TMP/move-failure.log" 2>&1; then
  fail "installer unexpectedly succeeded when activation backup failed"
fi
assert_contains "$TEST_TMP/move-failure.log" \
  "could not move the previous installation out of the way"
test -f "$happy_install/preserve.file" ||
  fail "backup-move failure deleted the active installation"

tampered_archive="$TEST_TMP/tampered.tar.gz"
cp "$ARCHIVE" "$tampered_archive"
printf '%s\n' tampered >> "$tampered_archive"
tampered_install="$TEST_TMP/tampered-install"
if env PATH="$INSTALL_PATH" SHELL=/bin/sh \
  sh "$INSTALLER" \
    --version "$TAG" \
    --archive "$tampered_archive" \
    --sha256 "$(sha256_file "$ARCHIVE")" \
    --install-dir "$tampered_install" \
    --no-modify-path > "$TEST_TMP/tampered.log" 2>&1; then
  fail "installer accepted an archive with the wrong digest"
fi
assert_contains "$TEST_TMP/tampered.log" "SHA-256 mismatch"
test ! -e "$tampered_install" ||
  fail "digest failure modified the installation destination"

unmanaged_install="$TEST_TMP/unmanaged-install"
mkdir "$unmanaged_install"
printf '%s\n' keep > "$unmanaged_install/unmanaged.file"
if run_installer --install-dir "$unmanaged_install" --no-modify-path \
  > "$TEST_TMP/unmanaged.log" 2>&1; then
  fail "installer replaced an unmanaged directory without --force"
fi
assert_contains "$TEST_TMP/unmanaged.log" "is not managed by this installer"
test -f "$unmanaged_install/unmanaged.file" ||
  fail "unmanaged directory changed after a refused install"
run_installer --install-dir "$unmanaged_install" --no-modify-path --force \
  > "$TEST_TMP/forced.log"
test ! -e "$unmanaged_install/unmanaged.file" ||
  fail "--force retained an unmanaged file"

wrong_root="$TEST_TMP/wrong-root"
mkdir "$wrong_root"
printf '%s\n' wrong > "$wrong_root/file"
wrong_archive="$TEST_TMP/wrong-root.tar.gz"
tar czf "$wrong_archive" -C "$TEST_TMP" wrong-root
wrong_install="$TEST_TMP/wrong-install"
if env PATH="$INSTALL_PATH" SHELL=/bin/sh \
  sh "$INSTALLER" \
    --version "$TAG" \
    --archive "$wrong_archive" \
    --sha256 "$(sha256_file "$wrong_archive")" \
    --install-dir "$wrong_install" \
    --no-modify-path > "$TEST_TMP/wrong-root.log" 2>&1; then
  fail "installer accepted an archive with an unexpected root"
fi
assert_contains "$TEST_TMP/wrong-root.log" "archive contains an unexpected path"
test ! -e "$wrong_install" ||
  fail "layout failure modified the installation destination"

if env PATH="$INSTALL_PATH" SHELL=/bin/sh \
  sh "$INSTALLER" \
    --version "$TAG" \
    --archive "$ARCHIVE" \
    --sha256 "$(sha256_file "$ARCHIVE")" \
    --install-dir /usr \
    --no-modify-path \
    --force > "$TEST_TMP/protected.log" 2>&1; then
  fail "installer accepted a protected system directory"
fi
assert_contains "$TEST_TMP/protected.log" "protected system directory"

profile="$TEST_TMP/profile"
path_install="$TEST_TMP/path install"
run_installer --install-dir "$path_install" --profile "$profile" \
  > "$TEST_TMP/path-first.log"
run_installer --install-dir "$path_install" --profile "$profile" \
  > "$TEST_TMP/path-second.log"
test "$(grep -Fc '# >>> a3s-box installer >>>' "$profile")" -eq 1 ||
  fail "PATH profile block is not idempotent"
assert_contains "$profile" "export PATH='$path_install':\"\$PATH\""

echo "install.sh tests passed for $PLATFORM"
