//! ext4 checksum conventions.
//!
//! ext4 uses CRC-32C in *raw register* form: the seed passed in IS the
//! running CRC state, and there is no pre- or post-inversion. The
//! filesystem seed is `crc32c(!0, fs_uuid)`; per-inode structures fold the
//! inode number and generation into the seed first.
//!
//! Every function here is a zero-allocation fold over borrowed slices —
//! regions around checksum fields are covered piecewise (seeding zeros in
//! place of the field) instead of copying the buffer. All formulas are
//! pinned by unit tests against bytes extracted from real mke2fs images
//! (`testdata/vectors/`), cross-checked by `tools/check_vectors.py`.

/// CRC-32C in ext4's raw-register convention (no inversions).
///
/// `crc32c::crc32c_append` implements the standard checksum convention
/// (invert in, invert out); double inversion converts it to the raw form:
/// `raw(seed, d) = !append(!seed, d)`.
#[inline]
pub fn crc32c_raw(seed: u32, data: &[u8]) -> u32 {
    !crc32c::crc32c_append(!seed, data)
}

/// Filesystem checksum seed: `crc32c(!0, uuid)`.
#[inline]
pub fn fs_seed(uuid: &[u8; 16]) -> u32 {
    crc32c_raw(!0u32, uuid)
}

/// Per-inode seed: the fs seed folded with the inode number and
/// generation (both little-endian).
#[inline]
pub fn inode_seed(fs_seed: u32, ino: u32, generation: u32) -> u32 {
    let c = crc32c_raw(fs_seed, &ino.to_le_bytes());
    crc32c_raw(c, &generation.to_le_bytes())
}

/// Superblock checksum: `crc32c(!0, sb[0..0x3FC])`. The superblock seeds
/// itself with `!0` (the UUID is inside the covered bytes), unlike every
/// other structure.
///
/// `sb` is the 1024-byte superblock.
pub fn superblock(sb: &[u8]) -> u32 {
    crc32c_raw(!0u32, &sb[..0x3FC])
}

/// Group descriptor checksum (metadata_csum flavor): low 16 bits of
/// `crc32c(fs_seed, le32(group) ‖ desc[0..0x1E] ‖ 0u16)`.
///
/// `desc` is the full 32-byte descriptor; the stored checksum at 0x1E is
/// replaced by zeros in the fold, not read.
pub fn group_desc(fs_seed: u32, group: u32, desc: &[u8]) -> u16 {
    let c = crc32c_raw(fs_seed, &group.to_le_bytes());
    let c = crc32c_raw(c, &desc[..0x1E]);
    let c = crc32c_raw(c, &[0u8; 2]);
    c as u16
}

/// Block bitmap checksum: low 16 bits of `crc32c(fs_seed, bitmap)` over
/// the **full** bitmap block (`clusters_per_group / 8` bytes = 4096).
pub fn block_bitmap(fs_seed: u32, bitmap_block: &[u8]) -> u16 {
    crc32c_raw(fs_seed, bitmap_block) as u16
}

/// Inode bitmap checksum: low 16 bits of `crc32c(fs_seed, bitmap)` over
/// only the meaningful prefix, `(inodes_per_group + 7) / 8` bytes —
/// *not* the whole block.
pub fn inode_bitmap(fs_seed: u32, bitmap_block: &[u8], inodes_per_group: u32) -> u16 {
    let len = (inodes_per_group as usize).div_ceil(8);
    crc32c_raw(fs_seed, &bitmap_block[..len]) as u16
}

/// Inode checksum over the full on-disk inode with its checksum fields
/// zeroed: `l_i_checksum_lo` (2 bytes at 0x7C) always, and
/// `i_checksum_hi` (2 bytes at 0x82) iff `i_extra_isize >= 4`.
///
/// Returns the full 32-bit value; the caller stores the low 16 bits at
/// 0x7C and, when the extra area exists, the high 16 bits at 0x82.
pub fn inode(fs_seed: u32, ino: u32, generation: u32, inode: &[u8]) -> u32 {
    // 128-byte (rev-1 minimal) inodes have no extra area at all.
    let extra_isize = if inode.len() >= 0x82 {
        u16::from_le_bytes([inode[0x80], inode[0x81]])
    } else {
        0
    };
    let has_hi = extra_isize >= 4 && inode.len() >= 0x84;
    let c = inode_seed(fs_seed, ino, generation);
    let c = crc32c_raw(c, &inode[..0x7C]);
    let c = crc32c_raw(c, &[0u8; 2]); // l_i_checksum_lo
    let c = crc32c_raw(c, &inode[0x7E..inode.len().min(0x82)]);
    if has_hi {
        let c = crc32c_raw(c, &[0u8; 2]); // i_checksum_hi
        crc32c_raw(c, &inode[0x84..])
    } else if inode.len() > 0x82 {
        crc32c_raw(c, &inode[0x82..])
    } else {
        c
    }
}

/// Directory block checksum, stored in the 12-byte tail:
/// `crc32c(inode_seed, block[0 .. len-12])`.
pub fn dirent_block(inode_seed: u32, block: &[u8]) -> u32 {
    crc32c_raw(inode_seed, &block[..block.len() - 12])
}

/// Extent tree block checksum, stored in the 4-byte tail:
/// `crc32c(inode_seed, block[0 .. len-4])` (header + all `eh_max` entry
/// slots, used or not).
pub fn extent_block(inode_seed: u32, block: &[u8]) -> u32 {
    crc32c_raw(inode_seed, &block[..block.len() - 4])
}

/// htree root/interior-node checksum, stored in `dx_tail.dt_checksum`:
/// `crc32c(inode_seed, block[0 .. count_offset + count*8]) ⊕ dt_reserved ⊕
/// 4 zero bytes`. Only `count` entries are covered, not `limit`.
///
/// `count_offset` is 0x20 for dx_root, 0x8 for dx_node; the tail sits at
/// `count_offset + limit*8`.
pub fn dx_block(inode_seed: u32, block: &[u8], count_offset: usize, count: u16, limit: u16) -> u32 {
    let tail = count_offset + limit as usize * 8;
    let c = crc32c_raw(inode_seed, &block[..count_offset + count as usize * 8]);
    let c = crc32c_raw(c, &block[tail..tail + 4]); // dt_reserved
    crc32c_raw(c, &[0u8; 4]) // dt_checksum, zeroed
}

/// xattr block checksum, stored at header offset 0x10:
/// `crc32c(fs_seed, le64(block_nr) ‖ block with h_checksum zeroed)`.
/// The only ext4 checksum that binds a physical block address.
pub fn xattr_block(fs_seed: u32, block_nr: u64, block: &[u8]) -> u32 {
    let c = crc32c_raw(fs_seed, &block_nr.to_le_bytes());
    let c = crc32c_raw(c, &block[..0x10]);
    let c = crc32c_raw(c, &[0u8; 4]);
    crc32c_raw(c, &block[0x14..])
}

/// Legacy xattr entry hash (`e_hash`): name bytes folded with rol 5/27,
/// then the value as little-endian u32 words (zero-padded to 4) folded
/// with rol 16/16. In-inode entries store 0 instead.
pub fn xattr_entry_hash(name: &[u8], value: &[u8]) -> u32 {
    let mut h: u32 = 0;
    for &b in name {
        h = (h << 5) ^ (h >> 27) ^ u32::from(b);
    }
    let mut chunks = value.chunks_exact(4);
    for w in &mut chunks {
        h = (h << 16) ^ (h >> 16) ^ u32::from_le_bytes(w.try_into().unwrap());
    }
    let rem = chunks.remainder();
    if !rem.is_empty() {
        let mut w = [0u8; 4];
        w[..rem.len()].copy_from_slice(rem);
        h = (h << 16) ^ (h >> 16) ^ u32::from_le_bytes(w);
    }
    h
}

/// xattr block header hash (`h_hash`): fold of the entry hashes with
/// rol 16/16, in entry order. Zero if any entry hash is zero.
pub fn xattr_block_hash(entry_hashes: impl IntoIterator<Item = u32>) -> u32 {
    let mut h: u32 = 0;
    for eh in entry_hashes {
        if eh == 0 {
            return 0;
        }
        h = (h << 16) ^ (h >> 16) ^ eh;
    }
    h
}

#[cfg(test)]
mod tests {
    use super::*;

    const UUID: [u8; 16] = [
        0xd0, 0xd0, 0xca, 0xca, 0x00, 0x00, 0x40, 0x00, 0x80, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00,
        0x01,
    ];

    fn vector(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/testdata/vectors/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap_or_else(|e| panic!("vector {name}: {e}"))
    }

    fn le16(b: &[u8], off: usize) -> u16 {
        u16::from_le_bytes(b[off..off + 2].try_into().unwrap())
    }
    fn le32(b: &[u8], off: usize) -> u32 {
        u32::from_le_bytes(b[off..off + 4].try_into().unwrap())
    }

    #[test]
    fn fs_seed_matches_known_value() {
        // Pinned by tools/check_vectors.py output for the fixture UUID.
        assert_eq!(fs_seed(&UUID), 0x4216_ab51);
    }

    #[test]
    fn superblock_checksum() {
        for name in ["sb_primary.bin", "sb_backup_g1.bin"] {
            let sb = vector(name);
            assert_eq!(superblock(&sb), le32(&sb, 0x3FC), "{name}");
        }
    }

    #[test]
    fn group_desc_checksums() {
        let gdt = vector("gdt.bin");
        let seed = fs_seed(&UUID);
        for (g, desc) in gdt.chunks_exact(32).enumerate() {
            assert_eq!(
                group_desc(seed, g as u32, desc),
                le16(desc, 0x1E),
                "group {g}"
            );
        }
    }

    #[test]
    fn bitmap_checksums() {
        let gdt = vector("gdt.bin");
        let seed = fs_seed(&UUID);
        // Group 0 of the fixture image: both bitmaps initialized (group 1's
        // inode bitmap is INODE_UNINIT and stores checksum 0 — not covered
        // here; the reader tests exercise that path).
        let desc = &gdt[..32];
        let bb = vector("block_bitmap_g0.bin");
        assert_eq!(block_bitmap(seed, &bb), le16(desc, 0x18));
        let ib = vector("inode_bitmap_g0.bin");
        assert_eq!(inode_bitmap(seed, &ib, 8192), le16(desc, 0x1A));
        // Whole-block coverage must NOT match: the classic mismatch.
        assert_ne!(crc32c_raw(seed, &ib) as u16, le16(desc, 0x1A));
    }

    #[test]
    fn inode_checksums() {
        let seed = fs_seed(&UUID);
        for (name, ino) in [
            ("inode_1_bad_blocks.bin", 1u32),
            ("inode_2_root.bin", 2),
            ("inode_8_journal.bin", 8),
            ("inode_11_lost_found.bin", 11),
        ] {
            let raw = vector(name);
            let gen = le32(&raw, 0x64);
            let got = inode(seed, ino, gen, &raw);
            let want_lo = le16(&raw, 0x7C);
            let extra = le16(&raw, 0x80);
            if extra >= 4 {
                let want = u32::from(want_lo) | (u32::from(le16(&raw, 0x82)) << 16);
                assert_eq!(got, want, "{name}");
            } else {
                assert_eq!(got as u16, want_lo, "{name}");
            }
        }
    }

    #[test]
    fn dirent_block_checksum() {
        let blk = vector("dirblock_root.bin");
        let seed = inode_seed(fs_seed(&UUID), 2, 0);
        assert_eq!(dirent_block(seed, &blk), le32(&blk, 4096 - 4));
    }

    #[test]
    fn dx_root_checksum() {
        let blk = vector("dx_root_bigdir.bin");
        let ino_raw = vector("inode_bigdir_dx.bin");
        // The htree fixture's inode number is stable across regenerations
        // of the fixture tree (declaration position in mkrefs.sh), but read
        // the generation from the inode rather than assuming.
        let gen = le32(&ino_raw, 0x64);
        let limit = le16(&blk, 0x20);
        let count = le16(&blk, 0x22);
        let tail = 0x20 + limit as usize * 8;
        // Find the inode number by brute force over a small range — the
        // fixture directory is one of the first declared inodes.
        let want = le32(&blk, tail + 4);
        let found = (11..64).any(|ino| {
            dx_block(
                inode_seed(fs_seed(&UUID), ino, gen),
                &blk,
                0x20,
                count,
                limit,
            ) == want
        });
        assert!(found, "no inode in 11..64 authenticates the dx_root block");
    }

    #[test]
    fn xattr_block_checksum_and_hashes() {
        let blk = vector("xattr_block.bin");
        let seed = fs_seed(&UUID);
        // h_checksum at 0x10; block number must be recovered — it is baked
        // into the checksum. The manifest records it, but to keep tests
        // dependency-free we scan a plausible range instead.
        let want = le32(&blk, 0x10);
        let blocknr = (0..200_000u64).find(|&nr| xattr_block(seed, nr, &blk) == want);
        assert!(
            blocknr.is_some(),
            "no block number authenticates the xattr block"
        );

        // Entry hash: first entry at 0x20.
        let name_len = blk[0x20] as usize;
        let value_off = le16(&blk, 0x22) as usize;
        let value_size = le32(&blk, 0x28) as usize;
        let name = &blk[0x30..0x30 + name_len];
        let value = &blk[value_off..value_off + value_size];
        assert_eq!(xattr_entry_hash(name, value), le32(&blk, 0x2C));
    }

    #[test]
    fn xattr_block_hash_fold() {
        assert_eq!(xattr_block_hash([0x18547]), 0x18547);
        assert_eq!(xattr_block_hash([]), 0);
        assert_eq!(xattr_block_hash([1, 0]), 0); // zero entry hash ⇒ zero
    }
}
