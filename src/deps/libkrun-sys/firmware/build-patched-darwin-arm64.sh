#!/bin/sh
set -eu

root=$(CDPATH= cd -- "$(dirname -- "$0")/../../.." && pwd)
firmware_dir="$root/deps/libkrun-sys/firmware"
work_dir=${A3S_LIBKRUNFW_WORK_DIR:-"$root/target/libkrunfw-a3s"}
source_dir="$work_dir/source"
builder_image=${A3S_LIBKRUNFW_BUILDER_IMAGE:-fedora:42}
upstream=https://github.com/boxlite-ai/libkrunfw.git
revision=v5.3.0

mkdir -p "$work_dir"
if [ ! -d "$source_dir/.git" ]; then
    git clone --depth 1 --branch "$revision" "$upstream" "$source_dir"
fi

if [ ! -d "$source_dir/linux-6.12.76" ]; then
    docker run --rm --platform linux/arm64 \
        -v "$source_dir:/src" -w /src "$builder_image" \
        /bin/bash -lc 'dnf install -y make patch curl xz && make linux-6.12.76'
fi

patch_file="$firmware_dir/patches/0001-tsi-nonblocking-accept-probe-local-vsock-first.patch"
if patch --dry-run -N -p1 -d "$source_dir/linux-6.12.76" < "$patch_file" >/dev/null; then
    patch -N -p1 -d "$source_dir/linux-6.12.76" < "$patch_file"
elif ! patch --dry-run -R -p1 -d "$source_dir/linux-6.12.76" < "$patch_file" >/dev/null; then
    echo "firmware patch does not apply cleanly" >&2
    exit 1
fi
rm -f "$source_dir/kernel.c" "$source_dir/linux-6.12.76/arch/arm64/boot/Image"

docker run --rm --platform linux/arm64 \
    -v "$source_dir:/src" -w /src "$builder_image" \
    /bin/bash -lc 'dnf install -y make gcc bc bison flex elfutils-libelf-devel openssl-devel python3 python3-pyelftools cpio diffutils perl && make -j"$(nproc)" kernel.c'

make -C "$source_dir" libkrunfw.5.dylib
cp "$source_dir/libkrunfw.5.dylib" "$work_dir/libkrunfw.5.dylib"
codesign --force --sign - "$work_dir/libkrunfw.5.dylib"
shasum -a 256 "$work_dir/libkrunfw.5.dylib"
printf '%s\n' "$work_dir/libkrunfw.5.dylib"
