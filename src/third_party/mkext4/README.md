# mkext4

[![crates.io](https://img.shields.io/crates/v/mkext4)](https://crates.io/crates/mkext4)
[![docs.rs](https://img.shields.io/docsrs/mkext4)](https://docs.rs/mkext4)
[![CI](https://github.com/cortexapps/mkext4/actions/workflows/ci.yml/badge.svg)](https://github.com/cortexapps/mkext4/actions/workflows/ci.yml)

Deterministic, streaming, pure-Rust ext4 image builder — plus a
verification-grade reader. No C dependencies, no kernel mounts, no clocks,
no RNG, no `unsafe`.

```rust,ignore
use mkext4::{FsBuilder, Meta, Options, ROOT};

let mut b = FsBuilder::new(options)?;          // size, uuid, hash seed, epoch…
let usr = b.mkdir(ROOT, "usr", meta)?;
let cat = b.file(usr, "cat", meta, 8_192)?;    // sizes are declared up front
b.hardlink(usr, "dog", cat)?;
b.symlink(usr, "sh", "bash", meta)?;
b.set_xattr(cat, "security.selinux", ctx)?;

let layout = b.seal()?;                        // every metadata byte frozen here
let mut w = layout.writer(&mut sink)?;         // metadata emitted immediately
w.fill(cat, &mut content_reader)?;             // data streams behind it
w.finish()?;
```

Supports everything a Linux rootfs needs: regular files (including
multi-GiB extent trees and sparse holes), directories with eager htree
indexing at node_modules scale, hardlinks, fast/slow symlinks,
char/block/FIFO/socket nodes, in-inode and block xattrs, full ownership /
mode / nanosecond-timestamp metadata, and a well-formed empty journal.
See [`examples/mkfs.rs`](examples/mkfs.rs) for the `mke2fs -d` equivalent
built on this API.

## The byte-stability contract

Know this before depending on the determinism:

- **Within one crate version, bytes are stable**: the same options and
  the same declaration sequence produce a byte-identical image, on any
  machine, every time.
- **Across versions, semantics are stable — bytes are not**: every
  version produces a correct, `e2fsck`-clean, kernel-mountable image of
  the same namespace, but layout policy (allocation order, htree
  packing, split points) may improve between releases.
- If you rely on byte-identical output — content-addressed dedup, image
  caching, chunk manifests — **pin an exact version** (`mkext4 = "=x.y.z"`)
  and treat every upgrade as a deliberate cache-invalidation event.

## Why

Turning a declared namespace (think: unpacked container layers) into a
bootable ext4 root filesystem usually means staging a directory tree on
disk and running `mke2fs -d`. This crate replaces that with a single pass:

1. **Deterministic by construction.** Identical builder calls + identical
   options ⇒ byte-identical images. Filesystem UUID, htree hash seed, and
   every timestamp are explicit inputs; declaration order is part of the
   input. The crate never consults a clock or RNG.
2. **Streaming output.** ext4 checksums metadata only — no structure
   depends on file content — so once the namespace is sealed, *every*
   metadata byte is emitted immediately, before any file content arrives.
   Data blocks then stream in declaration order at ascending offsets.
   Consumers see every byte of the image exactly once, final when emitted.
3. **Fast.** One pass, no staged tree, no rewrites. Benchmarked against
   `mke2fs -F -t ext4 -d` (a CI gate, not an aspiration).

The sink — the entire output contract — is deliberately dumb, so it can be
a file, a memory buffer, or a chunk-hashing uploader:

```rust,ignore
pub trait RegionSink {
    fn data(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()>;
    fn zeros(&mut self, offset: u64, len: u64) -> io::Result<()>;
}
```

Every byte of the image is covered exactly once and is final when emitted;
`zeros` regions (free space, journal body, sparse holes) never carry a
buffer, so consumers can retire them without I/O. `FileSink` (positioned
writes, zeros elided into file sparseness) and an in-memory `VecSink` are
included.

The full design — on-disk layout decisions, the determinism contract, and
the metadata-before-data argument — is recorded in [DESIGN.md](DESIGN.md).

## Performance

`tools/bench.sh` (hyperfine) builds the same tree with `mke2fs -F -t ext4
-d` and with this crate (`examples/mkfs.rs`), identical feature sets and
inode counts, both non-lazy, output gated on `e2fsck -fn`:

| benchmark | platform | mke2fs 1.47.4 | mkext4 | speedup |
|---|---|---|---|---|
| ~120k small files (node_modules-like) | macOS, M-series/APFS | 6.83 s | 3.25 s | **2.1×** |
| | Linux, GitHub runner/ext4 | 5.88 s | 1.73 s | **3.4×** |
| 120k files in ONE directory (pnpm-store-like) | macOS, M-series/APFS | 401 s | 5.88 s | **68×** |
| | Linux, GitHub runner/ext4 | 506 s | 1.67 s | **303×** |
| ~4.2 GiB tree with multi-GiB files | macOS, M-series/APFS | 17.5 s | 3.14 s | **5.6×** |
| | Linux, GitHub runner/ext4 | 28.9 s | 5.39 s | **5.4×** |

The flat-directory row is not a typo: `mke2fs -d` inserts dirents by
linear scan, so a single huge directory is quadratic — over eight
*minutes* for a shape a pnpm store hits routinely. mkext4 builds the
htree in one pass and stays at filesystem speed. (The recurring CI gate
runs a 20k-entry flat dir to keep runs short — still 55× on Linux;
the 120k headline above comes from a dispatched full-size run.)

Profiling shows mkext4 spends under 3% of wall time in its own code —
the rest is read/stat/pwrite syscalls against the source tree and
output file, i.e. the tool runs at filesystem speed.

The CI bench lane fails if mkext4 ever loses. The one-pass design is
the difference: no staged tree re-walk, metadata computed once, and
`zeros` regions never touch the sink.

## How it's verified

The writer is never validated only by its own reader:

- **`e2fsck -fn` passes clean on every image every test produces** —
  including 8 freshly generated property-test namespaces per CI run.
- **Kernel oracle:** Linux CI loop-mounts generated images and diffs the
  full tree through the kernel — content bytes, modes, owners, nanosecond
  mtimes, hardlink identity, symlink targets, device numbers, xattrs via
  `getfattr`, htree lookups, sparse-hole non-allocation.
- **Differential reader:** the bundled reader is validated against
  `mke2fs`-produced images first, then used to round-trip the writer's
  output, re-deriving every checksum and re-hashing every htree entry.
- **Byte vectors:** every checksum/hash algorithm is pinned by bytes
  extracted from real mke2fs images (`testdata/vectors/`), cross-checked
  by an independent Python implementation (`tools/check_vectors.py`).
- **Determinism gates:** identical inputs must produce byte-identical
  images, and a single changed mtime must change the output.

The oracle toolchain is pinned (e2fsprogs 1.47.4, built from source in
CI), so "verified" always means against a known reference.

Feature set emitted by the writer: `has_journal ext_attr dir_index
filetype extent flex_bg sparse_super large_file huge_file dir_nlink
extra_isize metadata_csum` — 4096-byte blocks, 256-byte inodes. Images
mount read-write on any modern Linux kernel.

## Installing

```sh
cargo add mkext4
```

Early release (0.0.x): the API may still move before 0.1.
Byte-stability is guaranteed per crate version: the same inputs on the
same version produce identical images, while layout policy may improve
between versions (treat a version bump as a cache-invalidation event if
you rely on image hashes).

## Repository layout

| path | contents |
|---|---|
| `DESIGN.md` | layout decisions, determinism contract, ADRs |
| `src/build/` | builder → seal → streaming writer |
| `src/spec/` | on-disk structures (byte-exact encode/decode) |
| `src/reader/` | verification-grade reader |
| `src/csum.rs`, `src/dirhash.rs` | crc32c conventions, half_md4 directory hash |
| `examples/mkfs.rs` | `mke2fs -d` equivalent CLI |
| `tools/` | reference-image harness, format checker, benchmarks |
| `testdata/vectors/` | golden bytes extracted from mke2fs images |

## License

Apache-2.0.
