//! Block group descriptors (32 bytes without the 64bit feature; 64 bytes
//! with it — the reader handles both, the writer emits 32).

use crate::le::{put_u16, put_u32, u16 as le16, u32 as le32};
use crate::{corrupt, Result};

/// One group descriptor with `_lo`/`_hi` pairs combined. For 32-byte
/// descriptors the high halves are zero.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
#[allow(missing_docs)] // the on-disk field names are the documentation
pub struct GroupDesc {
    pub block_bitmap: u64,
    pub inode_bitmap: u64,
    pub inode_table: u64,
    pub free_blocks_count: u32,
    pub free_inodes_count: u32,
    pub used_dirs_count: u32,
    pub flags: u16,
    pub exclude_bitmap: u64,
    pub block_bitmap_csum: u32,
    pub inode_bitmap_csum: u32,
    pub itable_unused: u32,
    pub checksum: u16,
}

impl GroupDesc {
    /// Decode one descriptor; `b.len()` must be the descriptor size
    /// (32 or ≥ 64).
    pub fn decode(b: &[u8]) -> Result<GroupDesc> {
        if b.len() != 32 && b.len() < 64 {
            return Err(corrupt("group descriptor", "size must be 32 or >= 64"));
        }
        let mut d = GroupDesc {
            block_bitmap: u64::from(le32(b, 0x00)),
            inode_bitmap: u64::from(le32(b, 0x04)),
            inode_table: u64::from(le32(b, 0x08)),
            free_blocks_count: u32::from(le16(b, 0x0C)),
            free_inodes_count: u32::from(le16(b, 0x0E)),
            used_dirs_count: u32::from(le16(b, 0x10)),
            flags: le16(b, 0x12),
            exclude_bitmap: u64::from(le32(b, 0x14)),
            block_bitmap_csum: u32::from(le16(b, 0x18)),
            inode_bitmap_csum: u32::from(le16(b, 0x1A)),
            itable_unused: u32::from(le16(b, 0x1C)),
            checksum: le16(b, 0x1E),
        };
        if b.len() >= 64 {
            d.block_bitmap |= u64::from(le32(b, 0x20)) << 32;
            d.inode_bitmap |= u64::from(le32(b, 0x24)) << 32;
            d.inode_table |= u64::from(le32(b, 0x28)) << 32;
            d.free_blocks_count |= u32::from(le16(b, 0x2C)) << 16;
            d.free_inodes_count |= u32::from(le16(b, 0x2E)) << 16;
            d.used_dirs_count |= u32::from(le16(b, 0x30)) << 16;
            d.itable_unused |= u32::from(le16(b, 0x32)) << 16;
            d.exclude_bitmap |= u64::from(le32(b, 0x34)) << 32;
            d.block_bitmap_csum |= u32::from(le16(b, 0x38)) << 16;
            d.inode_bitmap_csum |= u32::from(le16(b, 0x3A)) << 16;
        }
        Ok(d)
    }

    /// Encode as a 32-byte descriptor (the only size the writer emits).
    /// High halves must be zero — the writer never produces them.
    pub fn encode32(&self, out: &mut [u8]) {
        let b = &mut out[..32];
        b.fill(0);
        put_u32(b, 0x00, self.block_bitmap as u32);
        put_u32(b, 0x04, self.inode_bitmap as u32);
        put_u32(b, 0x08, self.inode_table as u32);
        put_u16(b, 0x0C, self.free_blocks_count as u16);
        put_u16(b, 0x0E, self.free_inodes_count as u16);
        put_u16(b, 0x10, self.used_dirs_count as u16);
        put_u16(b, 0x12, self.flags);
        put_u32(b, 0x14, self.exclude_bitmap as u32);
        put_u16(b, 0x18, self.block_bitmap_csum as u16);
        put_u16(b, 0x1A, self.inode_bitmap_csum as u16);
        put_u16(b, 0x1C, self.itable_unused as u16);
        put_u16(b, 0x1E, self.checksum);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::bg_flags;

    #[test]
    fn roundtrip_fixture_gdt() {
        let gdt = std::fs::read(format!(
            "{}/testdata/vectors/gdt.bin",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        for (g, raw) in gdt.chunks_exact(32).enumerate() {
            let d = GroupDesc::decode(raw).unwrap();
            let mut out = [0u8; 32];
            d.encode32(&mut out);
            assert_eq!(out, raw, "group {g}");
        }
    }

    #[test]
    fn fixture_group0_shape() {
        let gdt = std::fs::read(format!(
            "{}/testdata/vectors/gdt.bin",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let g0 = GroupDesc::decode(&gdt[..32]).unwrap();
        // Flex-packed metadata: bitmaps then itables at the front of g0.
        assert_eq!(g0.block_bitmap, 2);
        assert_eq!(g0.inode_bitmap, 6);
        assert_eq!(g0.inode_table, 10);
        assert_eq!(g0.flags, bg_flags::INODE_ZEROED);
        let g1 = GroupDesc::decode(&gdt[32..64]).unwrap();
        assert_ne!(g1.flags & bg_flags::INODE_UNINIT, 0);
        assert_eq!(g1.inode_bitmap_csum, 0, "uninit bitmap stores csum 0");
    }
}
