//! Filesystem geometry: block groups, flex_bg packing, sparse_super
//! backups, inode-table sizing, journal size tiers, and the overhead
//! formula. Pure functions of the options — no allocation decisions here.
//!
//! Every rule is pinned against mke2fs 1.47.4 behavior.

use crate::spec::{BLOCKS_PER_GROUP, BLOCK_SIZE, INODE_SIZE};
use crate::{Error, Result};

/// Inodes per itable block (4096 / 256).
pub const INODES_PER_BLOCK: u32 = (BLOCK_SIZE / INODE_SIZE) as u32;

/// Static geometry derived from the image size and inode policy.
#[derive(Debug, Clone)]
pub struct Geometry {
    /// Total blocks (`s_blocks_count`).
    pub blocks: u64,
    /// Block group count.
    pub groups: u32,
    /// Inodes per group (multiple of 16, itable fills whole blocks).
    pub inodes_per_group: u32,
    /// Blocks of GDT after the superblock (primary and each backup).
    pub gdt_blocks: u32,
    /// Journal length in blocks.
    pub journal_blocks: u32,
    /// flex_bg factor (16; `s_log_groups_per_flex = 4`).
    pub groups_per_flex: u32,
}

impl Geometry {
    /// Compute geometry. `total_inodes` of `None` means auto (mke2fs's
    /// default bytes-per-inode ratio of 16384).
    pub fn new(
        size_bytes: u64,
        total_inodes: Option<u32>,
        journal_blocks: Option<u32>,
    ) -> Result<Geometry> {
        if size_bytes == 0 || size_bytes % BLOCK_SIZE as u64 != 0 {
            return Err(Error::Unsupported(format!(
                "image size {size_bytes} is not a positive multiple of {BLOCK_SIZE}"
            )));
        }
        let blocks = size_bytes / BLOCK_SIZE as u64;
        if blocks >= u64::from(u32::MAX) {
            return Err(Error::Unsupported(
                "image needs >= 2^32 blocks (64bit feature not emitted)".into(),
            ));
        }
        if blocks < 2048 {
            return Err(Error::Unsupported(
                "image too small for a journal (< 2048 blocks)".into(),
            ));
        }
        let groups = blocks.div_ceil(u64::from(BLOCKS_PER_GROUP)) as u32;

        // Inodes: ratio default, then per-group, rounded up to fill whole
        // itable blocks, capped at one bitmap block's worth.
        let wanted = match total_inodes {
            Some(n) => u64::from(n),
            None => blocks * BLOCK_SIZE as u64 / 16384,
        };
        let ipg = wanted
            .div_ceil(u64::from(groups))
            .next_multiple_of(u64::from(INODES_PER_BLOCK))
            .clamp(u64::from(INODES_PER_BLOCK), 32768) as u32;

        let gdt_blocks = (groups * 32).div_ceil(BLOCK_SIZE as u32);
        let journal_blocks = match journal_blocks {
            Some(j) => j,
            None => default_journal_blocks(blocks),
        };
        if u64::from(journal_blocks) * 4 > blocks {
            return Err(Error::Unsupported(
                "journal larger than a quarter of the fs".into(),
            ));
        }

        Ok(Geometry {
            blocks,
            groups,
            inodes_per_group: ipg,
            gdt_blocks,
            journal_blocks,
            groups_per_flex: 16,
        })
    }

    /// Total inode count (`s_inodes_count`).
    pub fn inodes_count(&self) -> u32 {
        self.inodes_per_group * self.groups
    }

    /// Itable blocks per group.
    pub fn itable_blocks(&self) -> u32 {
        self.inodes_per_group / INODES_PER_BLOCK
    }

    /// First block of group `g`.
    pub fn group_start(&self, g: u32) -> u64 {
        u64::from(g) * u64::from(BLOCKS_PER_GROUP)
    }

    /// Number of blocks in group `g` (the last may be short).
    pub fn blocks_in_group(&self, g: u32) -> u32 {
        (self.blocks - self.group_start(g)).min(u64::from(BLOCKS_PER_GROUP)) as u32
    }

    /// sparse_super: does group `g` hold a backup superblock + GDT?
    /// (Group 0 holds the primary.)
    pub fn has_backup(&self, g: u32) -> bool {
        fn is_power_of(mut n: u32, base: u32) -> bool {
            while n % base == 0 {
                n /= base;
            }
            n == 1
        }
        g == 1 || (g > 1 && (is_power_of(g, 3) || is_power_of(g, 5) || is_power_of(g, 7)))
    }

    /// All backup groups, ascending.
    pub fn backup_groups(&self) -> Vec<u32> {
        (1..self.groups).filter(|&g| self.has_backup(g)).collect()
    }

    /// s_overhead_clusters: all blocks that can never hold file or
    /// directory data (verified against mke2fs).
    pub fn overhead(&self) -> u64 {
        let backups = self.backup_groups().len() as u64;
        1 + u64::from(self.gdt_blocks)
            + backups * (1 + u64::from(self.gdt_blocks))
            + 2 * u64::from(self.groups)
            + u64::from(self.groups) * u64::from(self.itable_blocks())
            + u64::from(self.journal_blocks)
    }
}

/// mke2fs's journal size tiers (blocks → journal blocks). Verified at 5
/// points; remaining boundaries from `ext2fs_default_journal_size`.
pub fn default_journal_blocks(fs_blocks: u64) -> u32 {
    match fs_blocks {
        0..=32767 => 1024,
        32768..=262143 => 4096,
        262144..=524287 => 8192,
        524288..=4194303 => 16384,
        4194304..=8388607 => 32768,
        8388608..=16777215 => 65536,
        16777216..=33554431 => 131072,
        _ => 262144,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn journal_tiers_match_mke2fs() {
        // Empirically verified against mke2fs 1.47.4.
        assert_eq!(default_journal_blocks(4096), 1024);
        assert_eq!(default_journal_blocks(131072), 4096);
        assert_eq!(default_journal_blocks(305000), 8192);
        assert_eq!(default_journal_blocks(2097152), 16384);
        assert_eq!(default_journal_blocks(16777216), 131072);
    }

    #[test]
    fn ipg_matches_mke2fs() {
        // 305000 blocks -> 10 groups, ipg 7632 (refodd).
        let g = Geometry::new(305000 * 4096, None, None).unwrap();
        assert_eq!(g.groups, 10);
        assert_eq!(g.inodes_per_group, 7632);
        assert_eq!(g.itable_blocks(), 477);
        // 139264 blocks -> 5 groups, ipg 6976 (bisect experiment).
        let g = Geometry::new(139264 * 4096, None, None).unwrap();
        assert_eq!(g.groups, 5);
        assert_eq!(g.inodes_per_group, 6976);
        // 131072 blocks -> 4 groups, ipg 8192 (ref512).
        let g = Geometry::new(131072 * 4096, None, None).unwrap();
        assert_eq!(g.inodes_per_group, 8192);
    }

    #[test]
    fn backup_groups_sparse_super() {
        let g = Geometry::new(16777216 * 4096, None, None).unwrap(); // 512 groups
        assert_eq!(
            g.backup_groups(),
            vec![1, 3, 5, 7, 9, 25, 27, 49, 81, 125, 243, 343]
        );
    }

    #[test]
    fn overhead_matches_mke2fs() {
        // ref512: 131072 blocks -> overhead 6158.
        let g = Geometry::new(131072 * 4096, None, None).unwrap();
        assert_eq!(g.overhead(), 6158);
        // ref64g: 16777216 blocks -> overhead 394305.
        let g = Geometry::new(16777216 * 4096, None, None).unwrap();
        assert_eq!(g.overhead(), 394305);
    }

    #[test]
    fn one_block_last_group_is_valid() {
        let g = Geometry::new(32769 * 4096, None, None).unwrap();
        assert_eq!(g.groups, 2);
        assert_eq!(g.blocks_in_group(1), 1);
    }

    #[test]
    fn rejects_bad_sizes() {
        assert!(Geometry::new(4095, None, None).is_err()); // unaligned
        assert!(Geometry::new(1024 * 4096, None, None).is_err()); // too small
    }
}
