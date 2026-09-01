#!/usr/bin/env bash
# Build a self-contained Linux qualification archive and install it exactly as a user would.
set -Eeuo pipefail

if [ "$#" -ne 5 ]; then
    echo "usage: $0 GUEST_TARGET PLATFORM OCI_REVISION OUTPUT_ROOT INSTALL_DIR" >&2
    exit 2
fi

GUEST_TARGET="$1"
PLATFORM="$2"
OCI_REVISION="$3"
OUTPUT_ROOT="$4"
INSTALL_DIR="$5"

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPOSITORY_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
BOX_WORKSPACE="$REPOSITORY_ROOT/src"
OCI_WORKSPACE="$REPOSITORY_ROOT/oci-runtime"

case "$(uname -m):$PLATFORM" in
    x86_64:linux-x86_64|amd64:linux-x86_64|aarch64:linux-arm64|arm64:linux-arm64) ;;
    *)
        echo "host architecture $(uname -m) does not match package platform $PLATFORM" >&2
        exit 1
        ;;
esac

VERSION="$({
    cargo metadata --locked --no-deps --format-version 1 \
        --manifest-path "$BOX_WORKSPACE/Cargo.toml" |
        jq -r '.packages[] | select(.name == "a3s-box-cli") | .version'
})"
TAG="v$VERSION"
PACKAGE_NAME="a3s-box-$TAG-$PLATFORM"
PACKAGE_ROOT="$OUTPUT_ROOT/$PACKAGE_NAME"
ARCHIVE="$OUTPUT_ROOT/$PACKAGE_NAME.tar.gz"

mkdir -p "$OUTPUT_ROOT"
if [ -e "$PACKAGE_ROOT" ] || [ -e "$ARCHIVE" ]; then
    echo "refusing to replace an existing qualification package in $OUTPUT_ROOT" >&2
    exit 1
fi
install -d -m 0755 "$PACKAGE_ROOT/lib"
install -m 0755 "$BOX_WORKSPACE/target/release/a3s-box" "$PACKAGE_ROOT/a3s-box"
install -m 0755 "$BOX_WORKSPACE/target/release/a3s-box-shim" \
    "$PACKAGE_ROOT/a3s-box-shim"
install -m 0755 \
    "$BOX_WORKSPACE/target/$GUEST_TARGET/release/a3s-box-guest-init" \
    "$PACKAGE_ROOT/a3s-box-guest-init"
install -m 0755 "$OCI_WORKSPACE/target/release/a3s-oci" "$PACKAGE_ROOT/a3s-oci"
install -m 0755 "$OCI_WORKSPACE/target/release/a3s-oci-agent" \
    "$PACKAGE_ROOT/a3s-oci-agent"

mapfile -t libkrun_build_dirs < <(
    find "$BOX_WORKSPACE/target/release/build" \
        -mindepth 2 -maxdepth 2 -type d -name out \
        -path '*/a3s-libkrun-sys-*/out' -print
)
if [ "${#libkrun_build_dirs[@]}" -ne 1 ]; then
    echo "expected one vendored libkrun output, found ${#libkrun_build_dirs[@]}" >&2
    exit 1
fi
libkrun_build_dir="${libkrun_build_dirs[0]}"
shopt -s nullglob
libkrun_sources=("$libkrun_build_dir/libkrun/lib64"/libkrun.so*)
libkrunfw_sources=("$libkrun_build_dir/libkrunfw/lib64"/libkrunfw.so*)
if [ "${#libkrun_sources[@]}" -eq 0 ] || [ "${#libkrunfw_sources[@]}" -eq 0 ]; then
    echo "vendored build did not produce the complete libkrun runtime" >&2
    exit 1
fi
cp -a -- "${libkrun_sources[@]}" "${libkrunfw_sources[@]}" "$PACKAGE_ROOT/lib/"

printf '%s\n' "$OCI_REVISION" > "$PACKAGE_ROOT/OCI-RUNTIME-REVISION"
install -m 0644 "$REPOSITORY_ROOT/LICENSE" "$PACKAGE_ROOT/LICENSE"
install -m 0644 "$REPOSITORY_ROOT/README.md" "$PACKAGE_ROOT/README.md"
install -m 0644 "$OCI_WORKSPACE/LICENSE" "$PACKAGE_ROOT/OCI-RUNTIME-LICENSE"

while IFS= read -r -d '' library; do
    patchelf --set-rpath '$ORIGIN' "$library"
    test "$(patchelf --print-rpath "$library")" = '$ORIGIN'
    soname="$(patchelf --print-soname "$library")"
    test -n "$soname"
    test -e "$PACKAGE_ROOT/lib/$soname"
done < <(
    find "$PACKAGE_ROOT/lib" -maxdepth 1 -type f \
        \( -name 'libkrun.so*' -o -name 'libkrunfw.so*' \) -print0
)

mapfile -t packaged_libkrun < <(
    find "$PACKAGE_ROOT/lib" -maxdepth 1 -type f -name 'libkrun.so*' -print
)
mapfile -t packaged_libkrunfw < <(
    find "$PACKAGE_ROOT/lib" -maxdepth 1 -type f -name 'libkrunfw.so*' -print
)
if [ "${#packaged_libkrun[@]}" -eq 0 ] || [ "${#packaged_libkrunfw[@]}" -eq 0 ]; then
    echo "qualification package is missing a regular libkrun runtime file" >&2
    exit 1
fi
libkrunfw_soname="$(patchelf --print-soname "${packaged_libkrunfw[0]}")"
for library in "${packaged_libkrunfw[@]}"; do
    test "$(patchelf --print-soname "$library")" = "$libkrunfw_soname"
done
found_libkrunfw_load_name=0
for library in "${packaged_libkrun[@]}"; do
    if grep -aF -- "$libkrunfw_soname" "$library" >/dev/null; then
        found_libkrunfw_load_name=1
    fi
done
if [ "$found_libkrunfw_load_name" -ne 1 ]; then
    echo "packaged libkrun does not name $libkrunfw_soname" >&2
    exit 1
fi

for binary in a3s-box a3s-box-shim; do
    patchelf --set-rpath '$ORIGIN/lib' "$PACKAGE_ROOT/$binary"
    test "$(patchelf --print-rpath "$PACKAGE_ROOT/$binary")" = '$ORIGIN/lib'
    ldd_output="$(LD_LIBRARY_PATH= ldd "$PACKAGE_ROOT/$binary")"
    printf '%s\n' "$ldd_output"
    ! grep -q 'not found' <<< "$ldd_output"
done
shim_ldd_output="$(LD_LIBRARY_PATH= ldd "$PACKAGE_ROOT/a3s-box-shim")"
grep -F "$PACKAGE_ROOT/lib/libkrun.so" <<< "$shim_ldd_output"

tar czf "$ARCHIVE" -C "$OUTPUT_ROOT" "$PACKAGE_NAME"
DIGEST="$(sha256sum "$ARCHIVE" | awk '{ print $1 }')"
sh "$REPOSITORY_ROOT/install.sh" \
    --version "$TAG" \
    --archive "$ARCHIVE" \
    --sha256 "$DIGEST" \
    --install-dir "$INSTALL_DIR" \
    --no-modify-path

test "$("$INSTALL_DIR/a3s-box" --version)" = "a3s-box $VERSION"
test "$(cat "$INSTALL_DIR/OCI-RUNTIME-REVISION")" = "$OCI_REVISION"
jq --exit-status \
    --arg version "$TAG" \
    --arg platform "$PLATFORM" \
    '.version == $version and .platform == $platform' \
    "$INSTALL_DIR/.a3s-box-install.json" >/dev/null
for artifact in a3s-box a3s-box-shim a3s-box-guest-init a3s-oci a3s-oci-agent; do
    test -x "$INSTALL_DIR/$artifact"
done

printf 'Installed %s from %s (sha256:%s)\n' "$PACKAGE_NAME" "$ARCHIVE" "$DIGEST"
