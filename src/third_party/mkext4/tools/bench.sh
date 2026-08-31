#!/usr/bin/env bash
# Benchmark mkext4 against mke2fs -d (DESIGN.md §19 gate 10).
#
#   (a) small-files: ~120k small files across nested dirs (node_modules-like)
#   (b) big-files:   ~4 GiB tree containing multi-GiB files
#   (c) flat-dir:    FLAT_ENTRIES files in ONE directory (pnpm-store shape;
#                    the case the htree requirement exists for)
#
# Usage: tools/bench.sh [outdir]
#   E2SBIN         e2fsprogs sbin dir (default: brew keg or PATH)
#   BENCH_SMALL=0  skip the small-files benchmark
#   BENCH_BIG=0    skip the big-files benchmark
#   BENCH_FLAT=0   skip the flat-dir benchmark
#   FLAT_ENTRIES=N flat-dir size (default 20000 for the recurring gate:
#                  mke2fs is QUADRATIC on flat directories — at 120k
#                  entries a single mke2fs run takes ~8 minutes, so the
#                  full-size case is for dedicated headline runs only)
#   ASSERT_FASTER=1  exit nonzero unless mkext4 wins every case
#
# Requires hyperfine.

set -euo pipefail

OUT=${1:-build/bench}
E2SBIN=${E2SBIN:-${E2FSPROGS_SBIN:-/opt/homebrew/opt/e2fsprogs/sbin}}
[ -x "$E2SBIN/mke2fs" ] || E2SBIN=$(dirname "$(command -v mke2fs)")
MKFS_OPTS="-F -q -t ext4 -b 4096 -I 256 -O ^64bit,^metadata_csum_seed,^orphan_file,^resize_inode -E lazy_itable_init=0,lazy_journal_init=0"

command -v hyperfine >/dev/null || { echo "hyperfine required" >&2; exit 1; }
mkdir -p "$OUT"

cargo build --release --example mkfs
MKFS_RS=target/release/examples/mkfs

make_small_tree() {   # ~120k files, nested (node_modules-like)
    local t=$1
    [ -d "$t" ] && return
    echo "generating small-files tree..." >&2
    python3 - "$t" <<'EOF'
import os, sys
root = sys.argv[1]
n = 0
for pkg in range(400):
    for sub in ("lib", "src", "dist"):
        d = os.path.join(root, "node_modules", "pkg-%03d" % pkg, sub)
        os.makedirs(d, exist_ok=True)
        for f in range(100):
            with open(os.path.join(d, "mod_%03d.js" % f), "wb") as fh:
                fh.write(b"x" * (200 + (n * 37) % 4000))
            n += 1
print(n, "files", file=sys.stderr)
EOF
}

make_flat_tree() {    # $2 small files in a single directory
    local t=$1 n=$2
    [ -d "$t" ] && return
    echo "generating flat-dir tree ($n files)..." >&2
    python3 - "$t" "$n" <<'PYEOF2'
import os, sys
d = os.path.join(sys.argv[1], "flat")
os.makedirs(d, exist_ok=True)
for i in range(int(sys.argv[2])):
    with open(os.path.join(d, "entry_%06d_pad" % i), "wb") as fh:
        fh.write(b"x" * (100 + (i * 37) % 2000))
PYEOF2
}

make_big_tree() {     # ~4 GiB: two multi-GiB files + some medium ones
    local t=$1
    [ -d "$t" ] && return
    echo "generating big-files tree..." >&2
    mkdir -p "$t/data"
    dd if=/dev/urandom of="$t/data/big1" bs=1048576 count=1800 2>/dev/null
    dd if=/dev/urandom of="$t/data/big2" bs=1048576 count=1400 2>/dev/null
    for i in $(seq 1 8); do
        dd if=/dev/urandom of="$t/data/med$i" bs=1048576 count=100 2>/dev/null
    done
}

run_case() {          # $1 name, $2 tree, $3 image-blocks, $4 inodes, $5 runs, $6 warmup
    local name=$1 tree=$2 blocks=$3 inodes=$4 runs=${5:-5} warmup=${6:-1}
    local img_a="$OUT/$name-mke2fs.img" img_b="$OUT/$name-mkext4.img"
    echo "== $name ($runs runs + $warmup warmup)" >&2
    # Heartbeat so CI logs show liveness during long runs (a slow run is
    # not a hang: mke2fs is quadratic on flat directories). Exits on its
    # own if the script dies mid-benchmark.
    local t0=$SECONDS
    ( while sleep 30; do kill -0 $$ 2>/dev/null || exit
          echo "   [$name] still benchmarking, $((SECONDS - t0))s elapsed" >&2; done ) &
    local hb=$!
    hyperfine --warmup "$warmup" --runs "$runs" --export-json "$OUT/$name.json" \
        --command-name mke2fs     "rm -f $img_a && $E2SBIN/mke2fs $MKFS_OPTS -N $inodes -d $tree $img_a $blocks" \
        --command-name mkext4 "rm -f $img_b && MKEXT4_INODES=$inodes $MKFS_RS $tree $img_b $((blocks * 4096))"
    kill "$hb" 2>/dev/null || true
    wait "$hb" 2>/dev/null || true
    # Sanity: our image must be fsck-clean.
    "$E2SBIN/e2fsck" -fn "$img_b" >/dev/null
    python3 - "$OUT/$name.json" <<'EOF'
import json, sys
r = json.load(open(sys.argv[1]))["results"]
by = {x["command"]: x["mean"] for x in r}
ratio = by["mke2fs"] / by["mkext4"]
print(f'{sys.argv[1]}: mke2fs {by["mke2fs"]:.3f}s  mkext4 {by["mkext4"]:.3f}s  speedup {ratio:.2f}x')
if __import__("os").environ.get("ASSERT_FASTER") == "1" and ratio < 1.0:
    sys.exit(f"mkext4 lost to mke2fs ({ratio:.2f}x)")
EOF
}

if [ "${BENCH_SMALL:-1}" = 1 ]; then
    make_small_tree "$OUT/tree-small"
    run_case small "$OUT/tree-small" 262144 160000    # 1 GiB image
fi
if [ "${BENCH_FLAT:-1}" = 1 ]; then
    FLAT_ENTRIES=${FLAT_ENTRIES:-20000}
    FLAT_TREE="$OUT/tree-flat-$FLAT_ENTRIES"   # keyed by size: no stale cache
    make_flat_tree "$FLAT_TREE" "$FLAT_ENTRIES"
    if [ "$FLAT_ENTRIES" -ge 60000 ]; then
        # 2 runs, no warmup: a single mke2fs pass over a flat 120k-entry
        # directory takes ~8 minutes (quadratic dirent insertion), so the
        # usual 1+5 schedule would cost an hour for no extra signal.
        run_case flat "$FLAT_TREE" 262144 $((FLAT_ENTRIES + 40000)) 2 0  # 1 GiB image
    else
        run_case flat "$FLAT_TREE" 262144 $((FLAT_ENTRIES + 40000))     # 1 GiB image
    fi
fi
if [ "${BENCH_BIG:-1}" = 1 ]; then
    make_big_tree "$OUT/tree-big"
    run_case big "$OUT/tree-big" 1310720 65536        # 5 GiB image
fi
