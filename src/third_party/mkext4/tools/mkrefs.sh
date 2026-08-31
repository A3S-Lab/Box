#!/usr/bin/env bash
# Generate mke2fs reference images for mkext4 format research.
#
# Produces, under $OUT (default build/refs):
#   img/<name>.img        raw ext4 images made by mke2fs 1.47.x
#   tree/<name>/          the source trees fed to mke2fs -d
#   extract/<name>/       text dumps (dumpe2fs, debugfs stat/htree/ex/ea_list)
#
# Every image uses the exact feature set targeted by the crate:
#   compat:   has_journal ext_attr dir_index
#   incompat: filetype extent flex_bg
#   ro:       sparse_super large_file huge_file dir_nlink extra_isize metadata_csum
# with fixed UUID + fixed htree hash seed, no lazy init, 4k blocks, 256B inodes.
#
# Gate: every image must pass `e2fsck -fn` before extraction runs.
#
# Usage: tools/mkrefs.sh [outdir]
#   SKIP_BIG=1   skip the 8 GiB and 64 GiB images (faster iteration)

set -euo pipefail

E2SBIN=${E2SBIN:-/opt/homebrew/opt/e2fsprogs/sbin}
MKE2FS=$E2SBIN/mke2fs
DEBUGFS=$E2SBIN/debugfs
DUMPE2FS=$E2SBIN/dumpe2fs
E2FSCK=$E2SBIN/e2fsck

OUT=${1:-build/refs}
UUID=d0d0caca-0000-4000-8000-000000000001
HASH_SEED=deadbeef-dead-4ead-8ead-deadbeef0000
FEATURES='^64bit,^metadata_csum_seed,^orphan_file,^resize_inode'

mkdir -p "$OUT"/{img,tree,extract}

log() { printf '\n=== %s\n' "$*" >&2; }

# --- source trees -----------------------------------------------------------

make_tree_common() {           # $1 = tree dir; the flavors every image gets
    local t=$1
    mkdir -p "$t"/sub/subsub
    printf 'hello ext4\n' > "$t/small.txt"
    : > "$t/empty"
    printf 'nested\n' > "$t/sub/inner.txt"
    ln "$t/small.txt" "$t/hardlink_to_small"
    # fast/slow symlink boundary: i_block holds 60 bytes; 59 = longest fast?
    ln -s b "$t/sym_1"
    ln -s "$(printf 'a%.0s' {1..59})" "$t/sym_59"
    ln -s "$(printf 'a%.0s' {1..60})" "$t/sym_60"
    ln -s "$(printf 'target/%.0s' {1..25})end" "$t/sym_slow"   # 182 bytes
    # sparse: 4MiB with data at head and tail, hole in the middle
    dd if=/dev/urandom of="$t/sparse_small" bs=4096 count=4 2>/dev/null
    dd if=/dev/urandom of="$t/sparse_small" bs=4096 count=4 seek=1020 conv=notrunc 2>/dev/null
    # unix socket (mke2fs -d copies S_IFSOCK)
    python3 - "$t/sock" <<'EOF'
import socket, sys
s = socket.socket(socket.AF_UNIX)
s.bind(sys.argv[1])
EOF
    chmod 4755 "$t/small.txt" 2>/dev/null || true   # setuid bit encoding
    touch -t 202001010000.00 "$t/small.txt" "$t/empty" "$t/sub"
}

make_tree_main() {             # 512MiB image: htree dir + multi-extent file
    local t=$1
    make_tree_common "$t"
    mkdir "$t/bigdir"
    python3 - "$t/bigdir" 300 <<'EOF'
import os, sys
d, n = sys.argv[1], int(sys.argv[2])
for i in range(n):
    os.close(os.open(os.path.join(d, "entry_%05d_pad" % i), os.O_CREAT | os.O_WRONLY, 0o644))
EOF
    # >32768 blocks so the extent tree needs >1 extent (contiguous alloc)
    dd if=/dev/urandom of="$t/big200" bs=1048576 count=200 2>/dev/null
    # files to receive xattrs via debugfs post-ops
    : > "$t/xattr_ibody"; : > "$t/xattr_block"; : > "$t/xattr_mixed"
}

make_tree_big() {              # 8GiB image: 2-level htree + multi-GiB sparse
    local t=$1
    make_tree_common "$t"
    mkdir "$t/hugedir"
    python3 - "$t/hugedir" 120000 <<'EOF'
import os, sys
d, n = sys.argv[1], int(sys.argv[2])
for i in range(n):
    os.close(os.open(os.path.join(d, "node_%06d_padpadpad" % i), os.O_CREAT | os.O_WRONLY, 0o644))
EOF
    # >4*32768 blocks contiguous => >4 extents => depth-1 extent tree
    dd if=/dev/urandom of="$t/big600" bs=1048576 count=600 2>/dev/null
    # 5GiB sparse file: 64MiB data at 0, at 2GiB, and at the tail
    python3 - "$t/giant_sparse" <<'EOF'
import os, sys
path = sys.argv[1]
chunk = os.urandom(1 << 20)
with open(path, "wb") as f:
    for off in (0, 2 << 30, (5 << 30) - (64 << 20)):
        f.seek(off)
        for _ in range(64):
            f.write(chunk)
    f.truncate(5 << 30)
EOF
}

# --- image build ------------------------------------------------------------

build_image() {                # $1 name, $2 size(4k blocks), $3 tree ('' = none)
    local name=$1 blocks=$2 tree=$3 img="$OUT/img/$1.img"
    log "mke2fs $name ($blocks blocks)"
    rm -f "$img"
    local dopt=()
    [ -n "$tree" ] && dopt=(-d "$tree")
    "$MKE2FS" -F -q -t ext4 -b 4096 -I 256 -L reffs \
        -O "$FEATURES" -U "$UUID" \
        -E "lazy_itable_init=0,lazy_journal_init=0,hash_seed=$HASH_SEED" \
        ${dopt[@]+"${dopt[@]}"} "$img" "$blocks"   # bash-3.2-safe empty array
}

post_ops() {                   # device nodes, fifo, xattrs via debugfs -w
    local img=$1
    log "debugfs post-ops $img"
    "$DEBUGFS" -w -f /dev/stdin "$img" >/dev/null 2>"$OUT/debugfs-postops.err" <<'EOF'
mknod dev_c_old c 5 1
mknod dev_b_old b 8 16
mknod dev_c_new c 254 300
mknod dev_b_new b 200 65535
mknod fifo p
ea_set /xattr_ibody user.small smallvalue
ea_set /xattr_block user.big 0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef
ea_set /xattr_mixed security.selinux system_u:object_r:etc_t:s0
ea_set /xattr_mixed user.alpha aaaa
ea_set /xattr_mixed user.zeta zzzz
ea_set /xattr_mixed user.beta bbbb
EOF
    if grep -v '^debugfs' "$OUT/debugfs-postops.err" | grep -q .; then
        echo "debugfs post-ops reported errors:" >&2
        cat "$OUT/debugfs-postops.err" >&2
        return 1
    fi
}

fsck_gate() {
    local img=$1
    log "e2fsck -fn $img"
    "$E2FSCK" -fn "$img" || { echo "FSCK FAILED: $img" >&2; exit 1; }
}

# mke2fs -d does NOT hash-index directories (verified: 120k-entry dirs come
# out linear). e2fsck -fD rebuilds dirs as htree via ext2fs rehash — that is
# our htree layout oracle, so derive an indexed variant of each tree image.
index_variant() {              # $1 name  ->  <name>dx.img
    local name=$1 img="$OUT/img/$1.img" dximg="$OUT/img/${1}dx.img"
    log "e2fsck -fD $name -> ${name}dx"
    cp "$img" "$dximg"
    "$E2FSCK" -fD -y "$dximg" >/dev/null 2>&1 || true   # exit 1 = "fs modified"
    fsck_gate "$dximg"
}

# --- text extraction --------------------------------------------------------

dfs() { "$DEBUGFS" -R "$1" "$2" 2>/dev/null; }

extract_common() {             # $1 name
    local name=$1 img="$OUT/img/$1.img" x="$OUT/extract/$1"
    log "extract $name"
    mkdir -p "$x"
    "$DUMPE2FS" -h "$img" > "$x/sb.txt" 2>&1
    "$DUMPE2FS" "$img" > "$x/full.txt" 2>&1
    for i in 1 2 3 4 5 6 7 8 9 10 11; do
        dfs "stat <$i>" "$img" > "$x/inode_$i.txt"
    done
}

extract_files() {              # $1 name — path-level dumps for tree images
    local name=$1 img="$OUT/img/$1.img" x="$OUT/extract/$1"
    for p in small.txt empty sub sym_1 sym_59 sym_60 sym_slow sparse_small \
             hardlink_to_small sock dev_c_old dev_b_old dev_c_new dev_b_new fifo; do
        dfs "stat /$p" "$img" > "$x/stat_${p//\//_}.txt" 2>/dev/null || true
    done
    for p in xattr_ibody xattr_block xattr_mixed; do
        dfs "stat /$p" "$img" > "$x/stat_$p.txt" 2>/dev/null || true
        dfs "ea_list /$p" "$img" > "$x/ea_$p.txt" 2>/dev/null || true
    done
}

# --- run --------------------------------------------------------------------

log "building source trees"
rm -rf "$OUT/tree"; mkdir -p "$OUT/tree"
make_tree_common "$OUT/tree/t16"
make_tree_main   "$OUT/tree/t512"
[ "${SKIP_BIG:-0}" = 1 ] || make_tree_big "$OUT/tree/t8g"

build_image ref16 4096 "$OUT/tree/t16"
fsck_gate "$OUT/img/ref16.img"

# truncated last group (305000 = 9 full groups + 10088-block group 9),
# all inside one partial flex span - exercises edge geometry + bitmap padding
build_image refodd 305000 "$OUT/tree/t16"
fsck_gate "$OUT/img/refodd.img"

build_image ref512 131072 "$OUT/tree/t512"
post_ops "$OUT/img/ref512.img"
fsck_gate "$OUT/img/ref512.img"

if [ "${SKIP_BIG:-0}" != 1 ]; then
    build_image ref8g 2097152 "$OUT/tree/t8g"
    fsck_gate "$OUT/img/ref8g.img"
    build_image ref64g 16777216 ""     # geometry/journal-tier oracle only
    fsck_gate "$OUT/img/ref64g.img"
fi

index_variant ref512

extract_common ref16
extract_files  ref16
extract_common refodd
extract_common ref512
extract_files  ref512
dfs "ex /big200"    "$OUT/img/ref512.img" > "$OUT/extract/ref512/ex_big200.txt"
dfs "ex /bigdir"    "$OUT/img/ref512.img" > "$OUT/extract/ref512/ex_bigdir.txt"
dfs "htree /bigdir" "$OUT/img/ref512dx.img" > "$OUT/extract/ref512/htree_bigdir_dx.txt"
dfs "stat /bigdir"  "$OUT/img/ref512dx.img" > "$OUT/extract/ref512/stat_bigdir_dx.txt"

if [ "${SKIP_BIG:-0}" != 1 ]; then
    index_variant ref8g
    extract_common ref8g
    dfs "ex /giant_sparse" "$OUT/img/ref8g.img" > "$OUT/extract/ref8g/ex_giant_sparse.txt"
    dfs "ex /big600"       "$OUT/img/ref8g.img" > "$OUT/extract/ref8g/ex_big600.txt"
    dfs "stat /big600"     "$OUT/img/ref8g.img" > "$OUT/extract/ref8g/stat_big600.txt"
    dfs "stat /giant_sparse" "$OUT/img/ref8g.img" > "$OUT/extract/ref8g/stat_giant_sparse.txt"
    dfs "htree /hugedir"   "$OUT/img/ref8gdx.img" > "$OUT/extract/ref8g/htree_hugedir_dx.txt"
    dfs "stat /hugedir"    "$OUT/img/ref8gdx.img" > "$OUT/extract/ref8g/stat_hugedir_dx.txt"
    extract_common ref64g
fi

log "done: $OUT"
