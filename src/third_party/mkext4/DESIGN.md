# mkext4 — design

A deterministic, streaming, pure-Rust ext4 image writer (plus a
verification-grade reader). Input: an abstract namespace declared through a
builder API plus content streams. Output: a byte-exact ext4 image emitted
through a dumb positional sink. No C dependencies, no mounts, no clocks, no
RNG. Apache-2.0.

Every on-disk claim in this document marked **[verified]** has been checked
against real `mke2fs 1.47.4` images by `tools/check_vectors.py` (an
independent reimplementation of every checksum/hash algorithm, run against
the image matrix built by `tools/mkrefs.sh`; all checks pass on all images).
Golden byte fixtures live in `testdata/vectors/`.

---

## 1. Goals, non-goals, fixed constraints

**Goals**
- Byte-deterministic images: identical builder-call sequence + identical
  options ⇒ identical bytes (§2).
- Streaming: all metadata bytes are emitted the moment the namespace is
  sealed, before any file content arrives; data blocks then stream in
  declaration order at ascending offsets (§4).
- Faster than `mke2fs -F -t ext4 -d` (single pass, no staged directory tree).
- Every image passes `e2fsck -fn` clean and mounts correctly on Linux.

**Fixed constraints**
- Block size 4096 only; other sizes are a constructor error.
- Feature set (constant; word values [verified]):
  - `s_feature_compat = 0x2C` — has_journal (0x4), ext_attr (0x8),
    dir_index (0x20)
  - `s_feature_incompat = 0x242` — filetype (0x2), extents (0x40),
    flex_bg (0x200)
  - `s_feature_ro_compat = 0x46B` — sparse_super (0x1), large_file (0x2),
    huge_file (0x8), **dir_nlink (0x20)**, extra_isize (0x40),
    metadata_csum (0x400). dir_nlink is included so that a flat directory
    of > ~65k subdirectories (pnpm stores, content-addressed caches) is
    representable instead of a builder error — it costs one ro_compat bit
    and a store-nlink-1-on-overflow rule (§9).
- Explicitly **off**: 64bit, resize_inode, orphan_file,
  metadata_csum_seed, meta_bg, inline_data, bigalloc, largedir, ea_inode,
  quota, mmp, encrypt, casefold.
- Image size is an explicit, required option (4096-aligned byte count).
  Practical ceiling from the off-features: < 2^32 blocks (16 TiB); design
  target ≤ 64 GiB. No online growth support (no resize_inode; reserved GDT
  blocks = 0).

**Non-goals**
- Reading/writing feature combinations the writer does not emit is a
  non-goal for the writer; the **reader** additionally handles what stock
  `mke2fs` emits (it is the differential oracle and must read mke2fs
  images), but not the full historical ext2/3 matrix.
- No journal replay in the reader; the writer's journal is always empty.

## 2. Determinism contract

The output image is a pure function of:

1. **Options**: image size in bytes; filesystem UUID; htree hash seed
   (4×u32, stored as `s_hash_seed`); epoch (superblock timestamp fields);
   volume label; inode count policy (`Exact(n)` or `Auto`); reserved-blocks
   percent; journal size override (blocks) if given.
2. **The declaration sequence, including order**: every `mkdir` / `file` /
   `hardlink` / `symlink` / `mknod` / `set_meta` / `set_xattr` / `remove`
   call with its arguments
   (names as byte strings, per-file metadata: mode, uid, gid, mtime — and
   optional atime/ctime/crtime, each with nanosecond precision — xattrs,
   declared sizes and hole maps).
3. **File content bytes** — which affect *only* the corresponding data
   blocks: ext4 has no data checksums, so no metadata byte depends on
   content (§4).

Consequences and rules:
- The crate never reads a clock, RNG, process ID, hostname, or environment.
- No iteration over hash-ordered containers reaches the output path
  (implementation rule: `BTreeMap`/`Vec` + stable sorts only).
- The htree hash uses **signed-char** `str2hashbuf` as a fixed constant
  (`s_flags = 0x0001`, `signed_directory_hash`), independent of build
  platform (ADR-6).
- Declaration order is semantic input: it determines inode numbering and
  data-block placement. Re-ordering declarations produces a different
  (equally valid) image.
- xattrs are canonicalized by sorting (§13), so xattr declaration order does
  *not* affect output — one less footgun; everything else is order-exact.
- **Cross-version**: byte-stability is guaranteed for the same crate
  version only. Layout policy (allocation order, htree packing, extent
  splits, xattr placement) may change between versions; what never changes
  is semantic equivalence (same namespace, fsck-clean). Golden-hash tests
  are updated deliberately when policy changes.

The contract is *sensitivity*, not just stability: changing any single
input (one mtime, one byte of one name, declaration order) must be able to
change the output. A test asserts a one-field change produces a different
image hash.

## 3. Three-phase API

```rust
let mut b = FsBuilder::new(Options {
    size_bytes: 30 << 30,                  // required, multiple of 4096
    block_size: BlockSize::B4096,          // only accepted value
    fs_uuid: [u8; 16],
    hash_seed: [u32; 4],
    epoch: 1_704_067_200,                  // superblock times (s_mkfs_time etc.)
    inodes: InodeCount::Auto,              // or Exact(n)
    label: Option<&str>,                   // ≤16 bytes
    reserved_percent: 5,                   // s_r_blocks_count = 5% (mke2fs default)
    journal_blocks: None,                  // None = size-tiered default (§15)
    features: Features::LINUX_ROOTFS,      // the fixed set of §1 (validated)
})?;

let usr = b.mkdir(ROOT, "usr", meta)?;             // handles, not paths
let f   = b.file(usr, "cat", meta, 8_192)?;        // size required now
let s   = b.file_sparse(usr, "img", meta, &[Data(4096), Hole(1 << 30), Data(4096)])?;
b.hardlink(usr, "dog", f)?;                        // shared inode, nlink 2
b.symlink(usr, "sh", "bash", meta)?;
b.mknod(usr, "null", meta, NodeKind::Char { major: 1, minor: 3 })?;
b.set_xattr(f, "security.selinux", value)?;
b.set_meta(ROOT, root_meta)?;                      // root is declarable too
b.remove(usr, "stale")?;                           // recursive tombstone

let layout = b.seal()?;                            // freeze EVERYTHING (§4)
let mut w = layout.writer(&mut sink)?;             // emits all metadata + zeros here
w.fill(f, &mut reader)?;                           // exactly 8_192 bytes
w.fill(s, &mut reader)?;                           // only the Data segments: 8_192 bytes
let summary = w.finish()?;                         // error if any file unfilled
```

- **Handles** (`InodeHandle`) are indices into the builder's table; hardlinks
  are first-class (`hardlink` bumps nlink, adds a dirent).
- **Root metadata**: `ROOT` is a real handle. `set_meta(ROOT, meta)` and
  `set_xattr(ROOT, …)` cover OCI layers whose `./` entry carries explicit
  root mode/owner/mtime. If never declared, root defaults to mode 0o755,
  uid 0, gid 0, all timestamps = `epoch`, no xattrs. `set_meta(h, meta)`
  works on *any* live handle before seal (last call wins) — it is part of
  the declaration sequence and thus of the determinism input.
- **`remove(parent, name)` is a recursive tombstone**: removing a
  directory removes its entire subtree (the opaque-whiteout case). Any
  inode whose link count drops to zero leaves the layout entirely — no
  inode number, no blocks, no dirent. Hardlinked files survive subtree
  removal if a link outside the subtree remains. **Data-order rule**:
  data-block order is declaration order *filtered to surviving files* —
  removing a subtree deletes its slots from the sequence (no holes, no
  renumbering of others), and re-added files occupy their new declaration
  position. Whiteout/subtree-removal patterns are part of the proptest
  namespace generator (§19.7).
- **Sizes are final**: `fill` must supply exactly the declared byte count
  (short/long reads are errors). Sparse files declare an explicit
  data/hole segment map; only data segments are filled.
- **Fill completeness**: zero-length files, all-hole sparse files, and
  every non-regular inode are complete at seal — `finish()` does not
  require a `fill` for them (a `fill` supplying exactly 0 bytes is an
  allowed no-op). If a `fill` fails partway (source error, short read,
  sink error), the **writer is poisoned**: the failed file's remaining
  bytes are unemitted, and every subsequent call including `finish()`
  returns an error identifying the poisoned state. There is no partial
  recovery — callers that can retry rebuild the writer from the (still
  valid, reusable) `layout`. The already-emitted prefix is well-formed
  per the sink contract, so a consumer can safely discard by offset.
- **Push-style fill** (deferred convenience, phase 4+): `fill_writer(f) ->
  impl io::Write` alongside pull-style `fill(f, &mut impl Read)`, for
  channel-fed pipelines that push; same exactness rules, completion on
  `Write` drop/flush-to-declared-size.
- **Memory model** (the 1M-inode budget is designed, not discovered):
  per-inode fixed-size record (target ≤ 64 B: type/mode/uid/gid packed,
  times, size, nlink, parent, xattr ref); all names in a single append-only
  byte arena referenced by (u32 offset, u8 len); children stored as
  contiguous index runs (children of one dir are declared contiguously or
  gathered once at seal — no per-entry heap allocation, no PathBuf/String
  anywhere); xattrs deduplicated through an interning arena; extent lists
  computed streaming during the seal sweep, never materialized per-block.
  Budget: ~100–150 B/inode + name bytes ⇒ ≈ 200 MiB for 1M inodes; hard
  design ceiling ≤ 1 GiB including htree build scratch (hash-sort of the
  largest single directory).
- `seal()` freezes: inode numbers, every block address, extent trees,
  dirent/htree bytes, bitmaps, group descriptors, superblock — the complete
  metadata image and the offset of every future data byte
  (`layout.extents(f)` exposes them).
- Validation (builder-time or seal-time errors): name length 1..=255 bytes,
  no `/` or NUL, not `.`/`..`; duplicate names only via explicit overwrite
  semantics of `remove`+re-add; symlink target 1..=4095 bytes; regular-file
  hardlink count ≤ 65,000 (directories have no subdir bound — dir_nlink,
  §9); hardlinks to directories rejected; size overflow vs image size; inode
  exhaustion vs `InodeCount`; xattr total size vs in-inode + one block
  (§13); device numbers within Linux `dev_t` range (major ≤ 4095 for the
  new encoding, minor ≤ 2^20−1).
- Everything is synchronous; the API is usable from a blocking thread. No
  tokio, no async traits.

## 4. Sink contract & emission order — metadata before data

```rust
pub trait RegionSink {
    fn data(&mut self, offset: u64, bytes: &[u8]) -> io::Result<()>;
    fn zeros(&mut self, offset: u64, len: u64) -> io::Result<()>;
}
```

**Invariant**: across a writer's lifetime, every byte of `[0, image_len)`
is covered exactly once, by `data` or `zeros`, and is final when emitted.
No rewrites, ever.

### Why every metadata byte is seal-computable

ext4's `metadata_csum` covers metadata only; **no structure anywhere in the
format checksums file content**. With sizes declared up front, every block
therefore falls into one of two classes:

| block type | contents depend on | class |
|---|---|---|
| superblock + backups | options, totals (sizes, counts) | seal-computable |
| group descriptors + backups | layout totals, bitmap/itable addresses | seal-computable |
| block/inode bitmaps | final allocation map | seal-computable |
| inode tables | declared metadata, sizes, extent roots, csums | seal-computable |
| extent tree interior/leaf blocks | block addresses (fixed at seal) | seal-computable |
| directory blocks (linear + htree) | names, inode numbers, hash seed | seal-computable |
| xattr blocks | declared xattrs | seal-computable |
| slow-symlink blocks | declared target | seal-computable |
| journal (empty jbd2) | uuid + journal length | seal-computable |
| free space / itable slack / journal body | nothing (zeros) | seal-computable |
| **file data blocks** | **content stream** | fill-time |

Nothing else exists in the image ⇒ all metadata and all zeros can be
emitted at `layout.writer()` time, before the first `fill`.

### Emission schedule

1. **Metadata pass** (inside `layout.writer()`): one ascending sweep over
   `[0, image_len)` that emits every byte *not owned by declared file
   data* — metadata via `data()`, everything free/slack via `zeros()`
   (coalesced into maximal runs). This includes the zeros beyond the last
   data block, journal body, itable tails, and all free groups, so a
   consumer can retire untouched regions immediately without reading.
2. **Data pass**: `fill()` calls emit declared file blocks. Because data
   blocks are allocated in declaration order by a monotone cursor (§5),
   filling in declaration order emits at strictly ascending offsets.
   Out-of-order `fill` is legal and correct — it only loses the ascending
   property. A file's final partial block is emitted as `data(content)` +
   `zeros(pad)` covering the block tail.

The overall byte stream is therefore *not* globally offset-monotonic (the
metadata pass sweeps the whole image first); each of the two passes is
individually ascending. This is exactly what the downstream chunk-hashing
consumer wants: chunk completion is offset-indexed, metadata+zeros retire
most chunks instantly, and data completes the rest in ascending order.

A `CheckingSink` test double asserts: exactly-once coverage, finality,
metadata-pass-before-data, ascending data offsets under declaration-order
fill.

## 5. Geometry & physical layout

- `blocks_per_group = 32768` (8 × 4096). `group_count =
  ceil(blocks / 32768)`; the last group may be short — **any** length ≥ 1
  block is valid ([verified]: mke2fs 1.47.4 builds a 32769-block fs — a
  1-block last group — and it passes `e2fsck -fn`; with flex_bg the group's
  bitmaps/itable live in the flex leader, so a tiny tail group holds only
  its backup superblock, if any).
- **Inodes per group**: from `InodeCount::Auto`, total inodes =
  `blocks × 4096 / 16384` (mke2fs's default bytes-per-inode ratio), or the
  explicit `Exact(n)`. Then `ipg = ceil(total / group_count)` rounded **up
  to a multiple of 16** so the 256-byte-inode table fills whole blocks
  ([verified]: 34816 requested → ipg 6976 = 436 exact itable blocks; also
  ipg 7632 = 477 blocks), capped at 32768, min 16. `itable_blocks_per_group
  = ipg / 16`. `s_inodes_count = ipg × group_count`.
- **flex_bg factor 16** (`s_log_groups_per_flex = 4`). The flex *leader*
  (first group of each flex span) packs, in order: the 16 groups' block
  bitmaps, then their 16 inode bitmaps, then their 16 inode tables
  [verified layout on all images]. A partial trailing flex span packs the
  same way with fewer members.
- **sparse_super backups**: a backup superblock (1 block: 1024-byte
  superblock at offset 0 of the block, zero-padded) + full GDT copy at the
  first blocks of groups 1, 3^n, 5^n, 7^n (1, 3, 5, 7, 9, 25, 27, 49, 81,
  125, 243, 343, …). Backup superblocks are byte-identical to the primary
  except `s_block_group_nr` (0x5A) and the recomputed `s_checksum`; backup
  GDTs are byte-identical to the primary GDT (ADR-1a; [verified] that
  mke2fs backup GDTs are identical at mkfs time, and that its backup
  *superblocks* differ only in stale free counts / `s_state=0` /
  `s_kbytes_written` — we write fresh values instead; recorded deviation
  §18. Note `e2fsck -b` reports differences even on pristine mke2fs images,
  so backup-fsck is a recovery path, not a cleanliness gate).
- **Physical order** (ascending block addresses; ADR-1):

  ```
  block 0:            [1024 zero pad][superblock][zero pad to 4096]
  blocks 1..1+G:      primary GDT   (G = ceil(group_count·32/4096))
  flex-0 metadata:    16 block bitmaps, 16 inode bitmaps, 16 inode tables
  journal:            J blocks (§15; split at reserved runs)
  namespace metadata: for each inode in inode-number order:
                        xattr block, then dir/htree/symlink content blocks
  file data:          declaration order of surviving files, single
                        ascending cursor; a file needing an extent tree
                        (> 4 extents) gets its tree blocks immediately
                        after its own data runs — the tree's size depends
                        on how the data allocation split, so placing it
                        behind the data keeps the layout single-pass
                        (blocks are still seal-computable and emitted in
                        the metadata pass; only their position is
                        interleaved)
  ```

  The cursor for namespace metadata and file data is one monotone
  allocator that **skips** reserved runs it encounters: backup sb+GDT at
  the start of backup groups, and each flex leader's packed metadata. Data
  blocks otherwise pack densely — between two flex-metadata runs there are
  ~2 GiB of contiguous allocatable space, so multi-GiB files fragment into
  at most a handful of extents plus the mandatory ≤32768-block splits.
- Neither the kernel nor e2fsck imposes locality constraints on dir/
  extent/xattr blocks beyond "in range, marked used, not overlapping other
  metadata"; with flex_bg, e2fsck widens legal bitmap/itable placement to
  the whole fs. Packing all namespace metadata early is unusual-looking
  but structurally conventional (it is what flex_bg exists to allow) and
  keeps the metadata pass one dense prefix + small per-flex islands.
- **Inode numbering**: inode 2 = root, 11 = lost+found, then namespace
  inodes in **declaration order** starting at 12 (hardlinks share their
  target's number; `remove`d-and-unreferenced files are skipped). One
  number space; no per-group balancing (deviation §18 — irrelevant to
  correctness, and `bg_used_dirs_count`/free counts are computed from the
  actual map).

## 6. Superblock

All offsets are within the 1024-byte superblock. Values [verified] against
`ref512` unless marked *policy* (our deterministic choice where mke2fs
writes something non-deterministic or environment-dependent).

| offset | field | value |
|---|---|---|
| 0x00 | s_inodes_count | ipg × group_count |
| 0x04 | s_blocks_count_lo | size_bytes / 4096 |
| 0x08 | s_r_blocks_count_lo | reserved_percent of blocks (default 5%) |
| 0x0C/0x10 | s_free_blocks/inodes_count | computed from final map |
| 0x14 | s_first_data_block | 0 |
| 0x18/0x1C | s_log_block_size / s_log_cluster_size | 2 / 2 |
| 0x20/0x24 | s_blocks/clusters_per_group | 32768 / 32768 |
| 0x28 | s_inodes_per_group | ipg |
| 0x2C | s_mtime | 0 (never mounted) |
| 0x30 | s_wtime | epoch *(policy: mke2fs uses wall clock)* |
| 0x34/0x36 | s_mnt_count / s_max_mnt_count | 0 / 0xFFFF (−1) |
| 0x38/0x3A/0x3C/0x3E | s_magic / s_state / s_errors / s_minor_rev | 0xEF53 / 1 (clean) / 1 (continue) / 0 |
| 0x40/0x44 | s_lastcheck / s_checkinterval | epoch / 0 |
| 0x48/0x4C | s_creator_os / s_rev_level | 0 (Linux) / 1 |
| 0x50/0x52 | s_def_resuid/gid | 0 / 0 |
| 0x54/0x58 | s_first_ino / s_inode_size | 11 / 256 |
| 0x5A | s_block_group_nr | 0 primary; group # in backups |
| 0x5C/0x60/0x64 | feature compat/incompat/ro | 0x2C / 0x242 / 0x46B |
| 0x68/0x78 | s_uuid / s_volume_name | options |
| 0x88 | s_last_mounted | zeros |
| 0xC8/0xCC/0xCD | s_algorithm_usage_bitmap / s_prealloc_* | 0 / 0 / 0 |
| 0xCE | s_reserved_gdt_blocks | 0 (no resize_inode) |
| 0xD0 | s_journal_uuid | zeros (internal journal) |
| 0xE0/0xE4/0xE8 | s_journal_inum / s_journal_dev / s_last_orphan | 8 / 0 / 0 |
| 0xEC | s_hash_seed[4] | options (explicit) |
| 0xFC/0xFD | s_def_hash_version / s_jnl_backup_type | 1 (half_md4) / 1 |
| 0xFE | s_desc_size | 0 (32-byte descs implied without 64bit) |
| 0x100 | s_default_mount_opts | 0x0C (user_xattr, acl) |
| 0x104 | s_first_meta_bg | 0 |
| 0x108 | s_mkfs_time | epoch |
| 0x10C | s_jnl_blocks[17] | journal i_block[0..15], i_size_high, i_size (§15) |
| 0x15C/0x15E | s_min/want_extra_isize | 32 / 32 |
| 0x160 | s_flags | 0x1 (signed_directory_hash, fixed; ADR-6) |
| 0x174/0x175 | s_log_groups_per_flex / s_checksum_type | 4 / 1 (crc32c) |
| 0x178 | s_kbytes_written (u64) | ceil((metadata bytes + Σ file sizes)/1024) *(policy; §18)* |
| 0x248 | s_overhead_clusters | computed exactly (§17) |
| 0x268/0x26C/0x270 | s_lpf_ino / s_prj_quota_inum / s_checksum_seed | 0 / 0 / 0 |
| 0x3FC | s_checksum | crc32c(~0, sb[0..0x3FC]) [verified] |

Everything not listed is zero. MMP, snapshot, error-tracking, quota, and
encryption fields are all zero [verified].

## 7. Group descriptors & UNINIT policy

32-byte descriptors ([verified] offsets): 0x00 block_bitmap, 0x04
inode_bitmap, 0x08 inode_table (all u32 block addresses), 0x0C
free_blocks (u16), 0x0E free_inodes (u16), 0x10 used_dirs (u16), 0x12
bg_flags (u16), 0x14 exclude_bitmap (0), 0x18 block_bitmap_csum_lo (u16),
0x1A inode_bitmap_csum_lo (u16), 0x1C itable_unused (u16), 0x1E
bg_checksum (u16).

- `bg_checksum = crc32c(fs_seed, le32(group_nr) ‖ desc[0x00..0x1E] ‖
  0u16) & 0xFFFF` [verified all groups, all images].
- Flags policy (ADR-3):
  - `EXT4_BG_INODE_ZEROED` (0x4) on **every** group (we emit fully zeroed
    itables — matches non-lazy mke2fs [verified]).
  - `EXT4_BG_INODE_UNINIT` (0x1) on groups with zero used inodes; their
    inode-bitmap *block* is emitted as zeros and `inode_bitmap_csum_lo = 0`
    ([verified]: mke2fs stores exactly this).
  - `EXT4_BG_BLOCK_UNINIT` (0x2): **never set** (deviation §18; mke2fs sets
    it with a zero bitmap block + zero csum on fully-free groups
    [verified]). We emit a real bitmap + real checksum for every group —
    uniform, unambiguous, still cheap because a free-group bitmap is a
    constant block.
- `bg_itable_unused = ipg − (index of highest used inode in the group + 1)`
  — counts the guaranteed-unused *tail* of the itable ([verified]: group 0
  with 332 used inodes stores 7860).
- `bg_used_dirs_count` counts directory inodes *homed* in the group;
  e2fsck pass 5 recomputes it.

## 8. Bitmaps

- **Block bitmap**: one block per group, 1 bit per block, LSB-first within
  bytes; bit set = in use. Non-final groups use all 32768 bits (the bitmap
  exactly fills its block — no padding exists). In a truncated final group,
  all bits past `s_blocks_count` through the end of the 4096-byte block are
  **set to 1** [verified on refodd].
- **Inode bitmap**: bits 0..ipg−1 meaningful; all remaining bits of the
  block set to 1 [verified] — except INODE_UNINIT groups, whose bitmap
  block is all zeros (§7).
- **Checksums** (stored per group in the descriptor, low 16 bits of crc32c
  since descs are 32-byte):
  - block bitmap: `crc32c(fs_seed, bitmap[0..4096])` — full block
    ([verified]; `clusters_per_group/8 = 4096`).
  - inode bitmap: `crc32c(fs_seed, bitmap[0..(ipg+7)/8])` — **only the
    meaningful prefix** ([verified]; e.g. 1024 of 4096 bytes at ipg 8192 —
    checksumming the whole block is the classic mismatch).
- The bitmap contents must equal the allocator's final map exactly; e2fsck
  pass 5 recomputes both bitmaps and every free count from first
  principles.

## 9. Inode table

256-byte inodes, `i_extra_isize = 32` (extra area = 0x80..0xA0).
[verified] field offsets: 0x00 mode, 0x02 uid_lo, 0x04 size_lo, 0x08/0x0C/
0x10/0x14 atime/ctime/mtime/dtime, 0x18 gid_lo, 0x1A links, 0x1C
blocks_lo, 0x20 flags, 0x24 version_lo, 0x28 i_block[15] (60 bytes), 0x64
generation, 0x68 file_acl_lo, 0x6C size_high, 0x74 osd2 (blocks_high,
file_acl_high, uid_high, gid_high, **checksum_lo @0x7C**), 0x80
extra_isize, 0x82 **checksum_hi**, 0x84/0x88/0x8C ctime/mtime/atime_extra,
0x90/0x94 crtime/crtime_extra, 0x98 version_hi, 0x9C projid.

- **Checksum** [verified]: seed' = crc32c(crc32c(fs_seed, le32(ino)),
  le32(i_generation)); csum = crc32c(seed', inode[0..256] with bytes
  0x7C..0x7E zeroed, and 0x82..0x84 zeroed **iff** `i_extra_isize ≥ 4`).
  Low 16 bits → 0x7C; high 16 → 0x82 when the extra area exists.
- `i_generation = 0` everywhere (determinism).
- **Timestamps**: seconds in the classic fields; `*_extra = (nsec << 2) |
  epoch_bits` where epoch_bits extend the sign-extended 32-bit seconds
  (post-2038). Defaults: atime = ctime = crtime = mtime unless declared.
- **i_blocks** (512-byte units, `huge_file` on but `HUGE_FILE_FL` never
  set — files ≤ 64 GiB keep i_blocks < 2^32): counts data blocks + extent
  tree blocks + the xattr block + directory/htree blocks + slow-symlink
  block. Holes count nothing. Journal inode counts its whole area
  [verified: 4096-block journal → i_blocks 32768].
- **i_flags**: `EXT4_EXTENTS_FL` (0x80000) on every regular file (including
  empty ones [verified]), directory, slow symlink, and the journal inode;
  never on fast symlinks, devices, FIFOs, sockets. `EXT4_INDEX_FL` (0x1000)
  on htree directories.
- Per-type matrix:

| type | mode high bits | i_block | i_size | i_blocks |
|---|---|---|---|---|
| regular | 0o10 | extent root | bytes | blocks×8 |
| dir | 0o04 | extent root | blocks×4096 | blocks×8 |
| fast symlink (target ≤ 59 B) | 0o12 | target bytes, zero-padded | target len | 0 |
| slow symlink (60..=4095 B) | 0o12 | extent root | target len | 8 (+tree) |
| char/block dev | 0o02 / 0o06 | encoding §14 | 0 | 0 |
| fifo / socket | 0o01 / 0o14 | zeros | 0 | 0 |

  (Fast/slow boundary [verified]: 59-byte target inline, 60-byte target
  allocates a block.)
- **Reserved inodes 1–10** [verified]: bitmap bits always set; `s_first_ino
  = 11`. Inode 1 (bad blocks): all zero except atime/ctime/mtime = epoch
  and a valid checksum. Inodes 3–7, 9, 10: all-zero body + valid
  `checksum_lo` (their extra_isize is 0, so no checksum_hi). Inode 2 =
  root, 8 = journal (§15), 11 = lost+found: mode 0o40700 root:root, 4
  blocks (16 KiB) — block 0 a normal empty-dir block with "."/"..",
  blocks 1–3 "empty" dirent blocks (§11) [verified].
- Directory `i_links_count = 2 + subdirectory count`, **unless** that
  exceeds 65,000 (EXT4_LINK_MAX): then store `i_links_count = 1`, the
  dir_nlink convention for "uncounted" (kernel and e2fsck accept nlink 1
  on directories when the ro_compat bit is set). Regular files: hardlink
  count, hard error above 65,000 (dir_nlink does not cover files). Root
  counts itself.

## 10. Extent trees

- Node layout: 12-byte header `{eh_magic 0xF30A, eh_entries, eh_max,
  eh_depth, eh_generation=0}` + 12-byte entries. Leaf entry:
  `{ee_block, ee_len, ee_start_hi, ee_start_lo}`; index entry:
  `{ei_block, ei_leaf_lo, ei_leaf_hi, ei_unused}`. `*_hi = 0` always
  (no 64bit).
- Root in `i_block`: header + up to 4 entries (`eh_max = 4` [verified]).
  Interior/leaf blocks: `eh_max = (4096 − 12 − 4)/12 = 340` [verified],
  4-byte `ext4_extent_tail` checksum at byte 4092.
- Block checksum [verified]: `crc32c(inode_seed, block[0..4092])` stored at
  4092 (coverage = header + all 340 slots, used or not), where inode_seed
  is the same ino+generation seed as §9.
- `ee_len` encoding [verified the sharp edge]: 1..=32768 = initialized
  extent of that length (32768 = 0x8000 itself is valid!); values > 32768
  encode unwritten extents of `len − 32768` (max 32767). **We never emit
  unwritten extents** — declared holes are simply absent extents (ADR-9):
  no blocks, no i_blocks, reads as zeros.
- Deterministic bulk build (ADR-4): collect the file's extents (contiguous
  allocation runs, split at 32768 blocks and around skipped metadata
  runs). If ≤ 4 → inline root, depth 0. Otherwise build bottom-up at 100%
  fill: leaves take 340 extents each, index nodes 340 children, root ≤ 4;
  depth = minimal. First `ee_block`/`ei_block` = first *mapped* logical
  block of its subtree (holes may precede). fsck checks ordering/overlap,
  not fill factor. Capacity: depth 1 ⇒ 1360 extents (≥ 170 GiB contiguous),
  depth 2 ⇒ 462 400 — sparse-heavy multi-GiB files fit in depth ≤ 2.

## 11. Directories — linear format

- `ext4_dir_entry_2`: `{inode u32, rec_len u16, name_len u8, file_type u8,
  name}`; entries 4-byte aligned (`rec_len = align4(8 + name_len)`), the
  last real entry's rec_len stretches to the checksum tail. file_type:
  1 reg, 2 dir, 3 chr, 4 blk, 5 fifo, 6 sock, 7 symlink.
- Every dir block ends with a 12-byte **tail** `{inode 0, rec_len 12,
  name_len 0, file_type 0xDE, checksum u32}`; checksum =
  `crc32c(inode_seed, block[0..4084])` [verified].
- Block 0 starts with "." then ".." (rec_len 12 each). Entries are packed
  in **declaration order**, greedy first-fit into successive blocks (an
  entry never splits). An "empty" block (lost+found padding) is a single
  unused dirent `{inode 0, rec_len 4084, file_type 0}` + tail [verified].
- **Linear/htree threshold — the oracle's exact rule, adopted verbatim**
  (ADR-5): a directory stays linear iff the sum of its entry records
  (`Σ align4(8 + name_len)` over real entries, excluding "."/"..") is
  **< blocksize − 24** (4072). That is e2fsck 1.47.4 `rehash.c`'s
  compression test, [verified] empirically at the boundary: 254 16-byte
  entries (4064 B) → 2-block *linear*; 255 (4080 B) → htree. So linear
  directories occupy 1 block, or exactly 2 in the narrow window where
  entries exceed block 0's dot-reduced capacity but sit under the
  threshold. Anything at/over the threshold is built as htree (§12).
  (Background [verified]: `mke2fs -d` itself leaves even 120k-entry
  directories linear — htree-at-mkfs is produced by `e2fsck -fD`, whose
  output is our layout oracle. dir_index is a hard requirement here, so
  linear-only is not a fallback.)

## 12. Directories — htree

Verified against `e2fsck -fD`-built trees (1-level, 300 entries; 2-level,
120k entries) — structure, limits, checksums, and every leaf-name hash.

- **dx_root** (block 0): "." (rec_len 12), ".." (rec_len = 4084, i.e. to
  the tail), then at 0x18 `dx_root_info {reserved_zero u32, hash_version=1
  (half_md4), info_length=8, indirect_levels, unused_flags=0}`; at 0x20 the
  entry array. Slot 0 is the `dx_countlimit` alias: `{limit u16, count
  u16, block u32}` — its "hash" is implicitly 0; real entries `{hash u32,
  block u32}` follow. `limit = 507` [verified] (= (4096−32)/8 − 1 tail
  slot). Logical block pointers are *within the directory file*.
- **dx_node** (interior, only when indirect_levels = 1): fake dirent
  `{inode 0, rec_len 4096}` (8 bytes), then countlimit + entries;
  `limit = 510` [verified] (= (4096−8)/8 − 1).
- **dx_tail** `{dt_reserved u32, dt_checksum u32}` occupies the last 8
  bytes (the slot excluded from limit). Checksum [verified]:
  `crc32c(inode_seed, block[0..count_offset + count·8]) ⊕ then
  dt_reserved (4 B) ⊕ then 4 zero bytes` — count_offset 0x20 for root, 0x8
  for nodes; only `count` entries are covered, not `limit`.
- **Leaves** are ordinary dirent blocks (§11 incl. tail). `indirect_levels
  ≤ 1` without largedir (2-level tree ≈ 507·510 leaves ≫ any target).
- **Hash** [verified byte-for-byte against debugfs and 120k real entries]:
  half_md4 (3-round, target order a,d,c,b), signed-char `str2hashbuf`,
  seed = `s_hash_seed` (the option), consumed 32 name bytes per transform;
  `hash = buf[1] & ~1`, `minor = buf[2]`. Vectors in
  `testdata/vectors/dx_hash.json`.
- **Deterministic bulk build** (ADR-5): hash all entries; sort by
  `(hash, minor, declaration index)`; pack leaves to 100% fill in that
  order (entries inside a leaf stay in that sorted order — deterministic;
  [verified] e2fsck -D also emits hash-sorted leaves, though it packs to
  80% — `htree_slack_percentage` 20 default; our 100% fill is a recorded
  deviation, §18); dx entry i carries
  the first hash of leaf i; if a leaf's first hash equals the previous
  leaf's last hash, set the collision-continuation bit (`hash | 1`) —
  same-hash runs must never be split *between* leaves when avoidable (split
  only if a single hash value exceeds one leaf's capacity, which the
  collision bit handles). "." and ".." exist only in dx_root. Below the
  §11 threshold → linear instead.
- `i_size` covers all htree blocks; `EXT4_INDEX_FL` set; every block of the
  directory is covered by its extent tree as usual.

## 13. Extended attributes

- **In-inode area** (0xA0..0x100, 96 bytes): header = magic `0xEA020000`
  (u32) [verified], then entries; values packed downward from the end of
  the inode. Entry: `{e_name_len u8, e_name_index u8, e_value_offs u16,
  e_value_inum u32 = 0, e_value_size u32, e_hash u32, name bytes}`, entries
  4-byte aligned, list terminated by 4 zero bytes. **`e_hash = 0` for
  in-inode entries** [verified]; `e_value_offs` is relative to the **first
  entry position, i.e. after the 4-byte magic** (kernel `IFIRST`)
  [verified byte-exactly — offsets relative to the magic word itself are
  off by 4].
- **xattr block** (at most one, `i_file_acl`, i_blocks += 8 [verified]):
  32-byte header `{h_magic 0xEA020000, h_refcount = 1, h_blocks = 1,
  h_hash, h_checksum, reserved}`; entries from 0x20, values packed downward
  from block end; `e_value_offs` relative to block start.
- **Checksum** [verified]: `crc32c(fs_seed, le64(block_nr) ‖
  block[0..4096] with h_checksum zeroed)` — the only checksum in the
  format that binds a physical block address.
- **Hashes**: `e_hash` = fold of name (rol 5/27 per byte) then value (rol
  16/16 per LE u32 word, value zero-padded to 4) [verified against a real
  entry]; `h_hash` = fold (rol 16/16) of entry hashes. debugfs writes
  `h_hash = 0` and fsck accepts, but the kernel computes both — we emit
  real hashes (kernel formula).
- **Policy** (ADR-7): no block sharing ever (`h_refcount = 1` always —
  dedup would couple unrelated inodes and complicate streaming for ~zero
  benefit on rootfs images). Canonical order: entries sorted by
  `(e_name_index, name)` in *both* regions (the block format requires
  sorted; in-inode order is free [verified: debugfs writes insertion
  order] — we sort everywhere so xattr declaration order is
  output-irrelevant). Placement: attributes are assigned to the in-inode
  area in sorted order while they fit (entry + value); the first that
  doesn't fit moves *itself and all remaining* to the block (single
  deterministic split point). Oversize total → builder error.
- Name-index compression: 1 `user.`, 2 `system.posix_acl_access`,
  3 `system.posix_acl_default`, 4 `trusted.`, 6 `security.`, 7 `system.`
  (others rejected).

## 14. Special files & symlinks

- **Devices** [verified both encodings]: if major < 256 && minor < 256 →
  old encoding, `i_block[0] = (major << 8) | minor`, `i_block[1] = 0`;
  otherwise new encoding, `i_block[0] = 0`, `i_block[1] = (minor & 0xFF) |
  (major << 8) | ((minor & ~0xFF) << 12)`. Limits: major ≤ 4095, minor ≤
  1048575.
- **FIFO / socket**: i_block all zero, size 0.
- **Symlinks**: target ≤ 59 bytes → fast (bytes in i_block, zero-padded,
  no EXTENTS_FL, i_blocks 0); 60..=4095 → slow (one extent-mapped block,
  target + zero padding) [verified at the 59/60 boundary].

## 15. Journal (empty jbd2)

- Inode 8: mode 0o100600 root:root, links 1, EXTENTS_FL, allocated
  right after flex-0 metadata (deviation from mke2fs's mid-device
  placement, §18). The journal is *extent-mapped like any file* and its
  allocation splits around reserved runs (backup superblocks/GDTs, flex
  metadata) — jbd2 requires logical, not physical, contiguity; a large
  journal near the front necessarily straddles backup groups.
  `i_size = blocks × 4096`, `i_blocks = blocks × 8` (+ extent tree
  blocks if the split forces > 4 extents) [verified base shape].
- Default size (blocks) by fs size [verified at 5 points, remaining tier
  boundaries from `ext2fs_default_journal_size`]:

  | fs blocks | journal blocks |
  |---|---|
  | < 2048 | error (no journal possible — reject) |
  | < 32768 | 1024 |
  | < 262144 | 4096 |
  | < 524288 | 8192 |
  | < 4194304 | 16384 |
  | < 8388608 | 32768 |
  | < 16777216 | 65536 |
  | < 33554432 | 131072 |
  | ≥ 33554432 | 262144 |

- **Journal superblock** (journal block 0, first 1024 bytes; **all fields
  big-endian**) [verified]: `h_magic 0xC03B3998, h_blocktype 4 (SB v2),
  h_sequence 0; s_blocksize 4096, s_maxlen = blocks, s_first 1,
  s_sequence 1, s_start 0, s_errno 0; feature compat/incompat/ro_compat
  ALL ZERO; s_uuid = fs uuid; s_nr_users 1`; everything else zero,
  including `s_checksum_type` — **mke2fs writes an un-checksummed v2
  journal even with metadata_csum**; the kernel upgrades it on first
  mount. We emit exactly this. Rest of block 0 and all remaining journal
  blocks: zeros (via `zeros()`).
- Superblock backup [verified]: `s_jnl_backup_type = 1`;
  `s_jnl_blocks[0..14] = journal i_block[0..14]` (the extent root),
  `[15] = i_size_high = 0`, `[16] = i_size`. `s_journal_uuid = 0`.

## 16. Checksums — unified table

`crc32c` = Castagnoli, reflected, polynomial 0x82F63B78, **no final xor**;
the seed argument is the running register. `fs_seed = crc32c(~0, s_uuid)`
(metadata_csum_seed is off, so the field at 0x270 stays 0 and the seed
always derives from the UUID). `inode_seed(ino) = crc32c(crc32c(fs_seed,
le32(ino)), le32(i_generation))` — generation is always 0 for us. All
formulas [verified] on every image in the matrix:

| structure | stored at | computation |
|---|---|---|
| superblock | 0x3FC u32 | crc32c(**~0**, sb[0..0x3FC]) — seeds itself, not fs_seed |
| group desc | 0x1E u16 | crc32c(fs_seed, le32(group) ‖ desc[0..0x1E] ‖ 0u16) & 0xFFFF |
| block bitmap | desc 0x18 u16 | crc32c(fs_seed, bitmap[0..4096]) & 0xFFFF |
| inode bitmap | desc 0x1A u16 | crc32c(fs_seed, bitmap[0..(ipg+7)/8]) & 0xFFFF |
| inode | 0x7C u16 + 0x82 u16 | crc32c(inode_seed, inode with csum fields zeroed) |
| extent block | byte 4092 u32 | crc32c(inode_seed, block[0..4092]) |
| dirent block | tail u32 | crc32c(inode_seed, block[0..4084]) |
| dx root/node | dt_checksum | crc32c(inode_seed, block[0..cnt_off+count·8]) ⊕ dt_reserved ⊕ 4 zero bytes |
| xattr block | 0x10 u32 | crc32c(fs_seed, le64(blocknr) ‖ block with 0x10..0x14 zeroed) |
| uninit bitmaps | — | stored 0 (INODE_UNINIT; BLOCK_UNINIT n/a for us) |
| journal | — | none (empty journal is feature-less) |

crc16 is never needed (GDT_CSUM and METADATA_CSUM are mutually exclusive;
we use only the latter). Rust unit tests reproduce every row from the
committed vector blobs.

## 17. Accounting & overhead

`s_overhead_clusters` = every block that is not (and can never be) file or
directory data [verified exactly on 512 MiB and 64 GiB images]:

```
overhead = 1 (sb) + G (primary GDT)
         + n_backups × (1 + G)               # sparse_super groups
         + 2 × group_count                   # both bitmaps
         + group_count × ipg/16              # inode tables
         + journal_blocks
```

Free blocks/inodes and all per-group counts are computed from the final
allocation map; e2fsck pass 5 must find zero discrepancies. lost+found's 4
blocks and root's dir block count as *used data*, not overhead [verified].
The closed form lets callers predict usable capacity for a given image
size before declaring anything.

## 18. Deviations from mke2fs (deliberate, all fsck-clean)

1. **htree built eagerly at mkfs time** for any directory at/over the
   §11 threshold — stock `mke2fs -d` leaves all directories linear
   (verified, contrary to folklore); our layout oracle is `e2fsck -fD`
   output (whose threshold we adopt exactly), and dir_index is a product
   requirement.
1a. **htree leaves packed to 100% fill** — `e2fsck -fD` leaves 20% slack
   (`htree_slack_percentage`) for future kernel inserts; our images are
   effectively immutable rootfs content, so slack is pure waste (a
   first-insert leaf split at runtime is the kernel's normal path).
   Structural tests assert our invariants + fsck/kernel oracles, not
   byte-parity with `-fD` layouts.
2. **Journal placement**: immediately after flex-0 metadata, not
   mid-device (mke2fs's placement optimizes spinning-disk seeks; ours
   optimizes a dense metadata prefix for streaming).
3. **No BLOCK_UNINIT** ever; real bitmaps + checksums everywhere. (mke2fs
   marks fully-free groups uninit with zeroed csum.)
4. **Backups are fresh**: backup superblocks equal the primary except
   `s_block_group_nr`/`s_checksum`; mke2fs leaves stale free counts,
   `s_state = 0`, and stale `s_kbytes_written` in backups.
5. **Allocation order**: inode numbers and data placement follow
   declaration order; mke2fs follows its own traversal and grouping
   heuristics. (Any consistent assignment is valid.)
6. **s_kbytes_written**: deterministic (metadata + declared data, in KiB);
   mke2fs records its actual I/O volume.
7. **xattr h_hash/e_hash computed** per kernel formula in the block case;
   debugfs-authored references store 0. (Matching the kernel, not
   debugfs.)
8. **s_wtime = epoch** rather than wall clock.

Structural identities we deliberately share with mke2fs: flex packing
shape, ipg rounding, journal size tiers, overhead accounting, uninit-csum
conventions, reserved-inode layout, lost+found sizing (4 blocks), empty
featureless journal, the dir_nlink feature (a stock mke2fs default), and
e2fsck's linear/htree threshold.

## 19. Verification plan & acceptance gates

Phase-1 assets (already running):
- `tools/mkrefs.sh` — reference-image matrix (16 MiB, 305000-block odd
  geometry, 512 MiB, 8 GiB, 64 GiB; htree variants via `e2fsck -fD`),
  every image gated on `e2fsck -fn`.
- `tools/check_vectors.py` — independent reimplementation of §16 + §12
  hashes; all checks pass on all images; dumps `testdata/vectors/`.

Writer gates (phases 3+):
1. `e2fsck -fn` exit 0 on every image any test produces (helper makes this
   one call).
2. Kernel oracle in Linux CI: loop-mount, full-tree diff (bytes, modes,
   owners, timestamps, xattrs, symlink targets, hardlink identity, device
   numbers), 100k-entry directory lookups through the mount (htree proof).
3. Differential reader oracle: our reader walks mke2fs images (this
   matrix) and must agree with debugfs/dumpe2fs; the writer is then
   validated against reader + fsck + kernel — never only against our own
   reader.
4. Structural spot-checks: `dumpe2fs`/`debugfs` field diffs against §6/§7
   tables; `debugfs htree`/`ex` on generated trees; 10-GiB sparse extent
   trees.
5. Determinism goldens: same input twice ⇒ identical bytes (hash-pinned);
   plus sensitivity tests (one mtime change ⇒ different hash).
6. `CheckingSink`: exactly-once coverage of [0, image_len), finality,
   metadata-before-data, ascending data offsets.
7. proptest namespaces (nested dirs, name edge cases, hardlink webs, size
   distributions incl. 0-byte and extent-boundary files, hole maps, and
   whiteout patterns: recursive `remove` of populated subtrees, remove +
   re-add under the same name, hardlinks surviving subtree removal) ⇒
   build ⇒ fsck + reader round-trip.
8. Unit tests per module against `testdata/vectors/` blobs.
9. CI from the first Rust commit: Linux lane (full, incl. loop mounts),
   macOS lane (everything except mounts; e2fsprogs via brew).
10. **Performance gate** (phase 5, then permanent): two committed
   hyperfine benchmarks — (a) ~120k-file small-file tree
   (node_modules-like), (b) ~4 GiB tree containing multi-GiB files — must
   beat `mke2fs -F -t ext4 -d <tree>` wall-clock on the Linux CI runner
   class, with numbers reported in the README. A regression that loses to
   mke2fs fails CI; the goal in §1 is enforced, not aspirational.

**Oracle pinning**: all [verified] claims and the differential matrix are
against **e2fsprogs 1.47.4**; CI pins that exact version (brew pin on
macOS, built-from-tag on Linux — distro drift must not silently change the
oracle). The fsck gate additionally runs an **older vintage, e2fsck
1.46.x**, on every generated image — the public crate's output must be
clean under more than one fsck generation. The mount lane records its
kernel (ubuntu-24.04 runners, linux 6.8.x at time of writing) in the
workflow and README.

Notes: `e2fsck -b <backup>` is *not* a gate (it reports differences even
on pristine mke2fs images); backup correctness is covered by byte-equality
tests against §5's rule plus fsck of images whose primary sb is manually
clobbered (recovery drill, Linux CI).

## 20. Appendix A — ADRs

- **ADR-1 Physical layout / emission order.** Dense metadata prefix
  (sb, GDT, flex-0 metadata, journal, all namespace metadata), then data in
  declaration order; two ascending emission passes (metadata+zeros, then
  data). Rejected: mke2fs-faithful scattered placement (no benefit without
  spinning disks; hurts chunk retirement), fully-front-packed bitmaps/
  itables of *all* flex groups (legal under flex_bg but untested territory
  in kernel heuristics; backups are immovable anyway, so the prefix can
  never be perfect).
- **ADR-1a Backups fresh, not stale.** Byte-identical-to-primary (modulo
  group_nr/csum) beats replicating mke2fs's write-ordering artifacts.
- **ADR-2 flex_bg 16, mke2fs packing shape.** Compatibility-shaped layout,
  nothing exotic; factor is an option only if a need appears.
- **ADR-3 UNINIT flags.** INODE_UNINIT + itable_unused: yes (real runtime
  value, matches mke2fs, csum convention verified). BLOCK_UNINIT: never
  (uniformity; real bitmaps are one constant block per free group).
- **ADR-4 Extent trees: bulk bottom-up, 100% fill.** No incremental-split
  replay; fsck doesn't check fill factor; deterministic and simplest.
- **ADR-5 htree threshold & build.** Threshold = e2fsck `rehash.c`'s rule,
  adopted verbatim (entry bytes < blocksize−24 ⇒ linear, even when that
  means a 2-block linear dir) so there is no divergence window at the
  boundary; bulk build with hash-sorted leaves at 100% fill (oracle uses
  80% — deviation §18.1a); collision bit on equal-hash leaf boundaries.
  Linear fallback is not needed (dir_index is a requirement).
- **ADR-6 Hash signedness fixed: signed (s_flags 0x1).** Matches x86-64
  and Apple-silicon e2fsprogs output; platform char signedness must never
  leak (arm64-Linux mke2fs would write unsigned — we are constant).
- **ADR-7 xattrs: no sharing; canonical sort; single split point.**
  Determinism and simplicity over the ~0 gain of mbcache-style dedup.
- **ADR-8 s_overhead_clusters: computed exactly** (formula §17, verified
  against mke2fs) rather than 0/lazy — statfs correctness with no mount-
  time computation.
- **ADR-9 Holes are absent extents,** never unwritten: no block cost, no
  i_blocks inflation, natural zeros() semantics. Unwritten extents remain
  reader-supported (mke2fs images may contain them).
- **ADR-10 Journal: mke2fs size tiers, early placement (split at
  reserved runs), feature-less empty jsb** (kernel upgrades on first
  mount — verified that's mke2fs behavior too). Physical contiguity was
  considered and dropped: backups every 32768 blocks make a front-placed
  large journal non-contiguous by construction, and jbd2 only needs
  logical contiguity through the extent map.

## Appendix B — resolved research log

| question | resolution | evidence |
|---|---|---|
| Does mke2fs -d build htree? | **No** — even 120k-entry dirs are linear; `e2fsck -fD` is the htree authority | debugfs `stat`/`htree` on ref512/ref8g |
| e2fsck -fD linear/htree threshold | linear iff entry bytes < blocksize−24 (254×16 B entries → 2-block linear; 255 → htree), leaves packed with 20% slack (`htree_slack_percentage`) | boundary bisect at n=250..260 + rehash.c v1.47.4 |
| dx_root/node limits | 507 / 510 with metadata_csum | htree dumps + formula match |
| dx checksum coverage | count-covered entries + dt_reserved + zeroed csum word | brute-forced variants; exact match |
| half_md4 details | signed str2hashbuf, seed = s_hash_seed words LE, hash = buf[1]&~1, minor = buf[2] | 4 debugfs vectors + 300 + 120k leaf names in range |
| inode-bitmap csum length | (ipg+7)/8 bytes, not the block | mismatch reproduced then fixed |
| BLOCK_UNINIT semantics | zero bitmap block + zero csum; set by mke2fs on fully-free groups | refodd/ref64g descs |
| INODE_UNINIT bitmap | entire block zero, csum 0 | ref512 groups 1–3 |
| Reserved inodes 3–7,9,10 | zero body + valid csum_lo (extra_isize 0 ⇒ no csum_hi) | itable bytes + recompute |
| Inode 1 | links 0, times = mkfs time, valid csum | inode_1 vector |
| Empty file | EXTENTS_FL + header {magic, 0 entries, max 4, depth 0} | inode_empty vector |
| Fast/slow symlink boundary | 59 inline / 60 block | sym_59 / sym_60 |
| ee_len = 32768 | valid initialized max-length extent (only >32768 is unwritten) | ref64g journal extents |
| Empty journal features | all zero, no csum_v3, checksum_type 0, s_uuid = fs uuid, BE fields | jsb.bin |
| s_jnl_blocks layout | i_block[0..14] ‖ i_size_high ‖ i_size | sb vs journal inode bytes |
| Backup sb/GDT | GDT identical; sb differs in group_nr, csum, stale counts/state/kbytes | byte diff |
| ipg rounding | up to multiple of 16 (whole itable blocks) | 6963.2→6976, 7625→7632 |
| Minimum last group | none — 1-block last group is valid and fsck-clean | 32769-block build |
| overhead formula | fixed metadata + journal, excluding dir data | exact match at 512 MiB & 64 GiB |
| xattr entry hash | rol5/27 name, rol16/16 value words | real entry match |
| xattr h_hash | debugfs writes 0 (fsck accepts); kernel computes fold — we compute | probe + kernel formula |
| dev encodings | old iff major<256 && minor<256; new packs minor around major | both verified |
| e2fsck -b | reports errors even on pristine mke2fs images — recovery path, not gate | refodd -b run |
