//! The ext4 superblock (1024 bytes at image offset 1024).
//!
//! Regions this crate never sets —
//! MMP, snapshot, error tracking, quota inums, encryption, mount options
//! strings, sparse_super2 backups, and the reserved tail — decode-ignore
//! and encode-zero; everything mke2fs 1.47.x writes nonzero for our
//! feature set is modeled (proven by the byte-exact round-trip test).

use crate::le::{put_u16, put_u32, put_u64, u16 as le16, u32 as le32, u64 as le64};
use crate::spec::consts::SB_MAGIC;
use crate::{corrupt, Result};

/// Decoded superblock. Field names mirror the on-disk names without the
/// `s_` prefix; `_lo`/`_hi` pairs are combined into full-width fields.
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(missing_docs)] // the on-disk field names are the documentation
pub struct Superblock {
    pub inodes_count: u32,
    pub blocks_count: u64,
    pub r_blocks_count: u64,
    pub free_blocks_count: u64,
    pub free_inodes_count: u32,
    pub first_data_block: u32,
    pub log_block_size: u32,
    pub log_cluster_size: u32,
    pub blocks_per_group: u32,
    pub clusters_per_group: u32,
    pub inodes_per_group: u32,
    pub mtime: u32,
    pub wtime: u32,
    pub mnt_count: u16,
    pub max_mnt_count: u16,
    pub state: u16,
    pub errors: u16,
    pub minor_rev_level: u16,
    pub lastcheck: u32,
    pub checkinterval: u32,
    pub creator_os: u32,
    pub rev_level: u32,
    pub def_resuid: u16,
    pub def_resgid: u16,
    pub first_ino: u32,
    pub inode_size: u16,
    pub block_group_nr: u16,
    pub feature_compat: u32,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    pub uuid: [u8; 16],
    pub volume_name: [u8; 16],
    pub last_mounted: [u8; 64],
    pub reserved_gdt_blocks: u16,
    pub journal_uuid: [u8; 16],
    pub journal_inum: u32,
    pub journal_dev: u32,
    pub last_orphan: u32,
    pub hash_seed: [u32; 4],
    pub def_hash_version: u8,
    pub jnl_backup_type: u8,
    pub desc_size: u16,
    pub default_mount_opts: u32,
    pub first_meta_bg: u32,
    pub mkfs_time: u32,
    pub jnl_blocks: [u32; 17],
    pub min_extra_isize: u16,
    pub want_extra_isize: u16,
    pub flags: u32,
    pub log_groups_per_flex: u8,
    pub checksum_type: u8,
    pub kbytes_written: u64,
    pub overhead_clusters: u32,
    pub lpf_ino: u32,
    pub checksum_seed: u32,
    pub checksum: u32,
}

impl Superblock {
    /// Byte length of the on-disk superblock.
    pub const LEN: usize = 1024;

    /// Decode from the 1024 superblock bytes. Validates the magic only;
    /// feature gating is the reader's job.
    pub fn decode(b: &[u8]) -> Result<Superblock> {
        if b.len() < Self::LEN {
            return Err(corrupt("superblock", "short buffer"));
        }
        if le16(b, 0x38) != SB_MAGIC {
            return Err(corrupt(
                "superblock",
                format!("bad magic {:#06x}", le16(b, 0x38)),
            ));
        }
        let mut sb = Superblock {
            inodes_count: le32(b, 0x00),
            blocks_count: u64::from(le32(b, 0x04)),
            r_blocks_count: u64::from(le32(b, 0x08)),
            free_blocks_count: u64::from(le32(b, 0x0C)),
            free_inodes_count: le32(b, 0x10),
            first_data_block: le32(b, 0x14),
            log_block_size: le32(b, 0x18),
            log_cluster_size: le32(b, 0x1C),
            blocks_per_group: le32(b, 0x20),
            clusters_per_group: le32(b, 0x24),
            inodes_per_group: le32(b, 0x28),
            mtime: le32(b, 0x2C),
            wtime: le32(b, 0x30),
            mnt_count: le16(b, 0x34),
            max_mnt_count: le16(b, 0x36),
            state: le16(b, 0x3A),
            errors: le16(b, 0x3C),
            minor_rev_level: le16(b, 0x3E),
            lastcheck: le32(b, 0x40),
            checkinterval: le32(b, 0x44),
            creator_os: le32(b, 0x48),
            rev_level: le32(b, 0x4C),
            def_resuid: le16(b, 0x50),
            def_resgid: le16(b, 0x52),
            first_ino: le32(b, 0x54),
            inode_size: le16(b, 0x58),
            block_group_nr: le16(b, 0x5A),
            feature_compat: le32(b, 0x5C),
            feature_incompat: le32(b, 0x60),
            feature_ro_compat: le32(b, 0x64),
            uuid: b[0x68..0x78].try_into().unwrap(),
            volume_name: b[0x78..0x88].try_into().unwrap(),
            last_mounted: b[0x88..0xC8].try_into().unwrap(),
            reserved_gdt_blocks: le16(b, 0xCE),
            journal_uuid: b[0xD0..0xE0].try_into().unwrap(),
            journal_inum: le32(b, 0xE0),
            journal_dev: le32(b, 0xE4),
            last_orphan: le32(b, 0xE8),
            hash_seed: [le32(b, 0xEC), le32(b, 0xF0), le32(b, 0xF4), le32(b, 0xF8)],
            def_hash_version: b[0xFC],
            jnl_backup_type: b[0xFD],
            desc_size: le16(b, 0xFE),
            default_mount_opts: le32(b, 0x100),
            first_meta_bg: le32(b, 0x104),
            mkfs_time: le32(b, 0x108),
            jnl_blocks: {
                let mut j = [0u32; 17];
                for (i, w) in j.iter_mut().enumerate() {
                    *w = le32(b, 0x10C + 4 * i);
                }
                j
            },
            min_extra_isize: le16(b, 0x15C),
            want_extra_isize: le16(b, 0x15E),
            flags: le32(b, 0x160),
            log_groups_per_flex: b[0x174],
            checksum_type: b[0x175],
            kbytes_written: le64(b, 0x178),
            overhead_clusters: le32(b, 0x248),
            lpf_ino: le32(b, 0x268),
            checksum_seed: le32(b, 0x270),
            checksum: le32(b, 0x3FC),
        };
        // 64bit images carry the high halves of the block counters.
        if sb.feature_incompat & crate::spec::incompat::BIT64 != 0 {
            sb.blocks_count |= u64::from(le32(b, 0x150)) << 32;
            sb.r_blocks_count |= u64::from(le32(b, 0x154)) << 32;
            sb.free_blocks_count |= u64::from(le32(b, 0x158)) << 32;
        }
        Ok(sb)
    }

    /// Encode into `out` (≥ 1024 bytes). Unmodeled regions are zeroed.
    /// The stored `checksum` field is written as-is; use
    /// [`crate::csum::superblock`] to compute it.
    pub fn encode(&self, out: &mut [u8]) {
        let b = &mut out[..Self::LEN];
        b.fill(0);
        put_u32(b, 0x00, self.inodes_count);
        put_u32(b, 0x04, self.blocks_count as u32);
        put_u32(b, 0x08, self.r_blocks_count as u32);
        put_u32(b, 0x0C, self.free_blocks_count as u32);
        put_u32(b, 0x10, self.free_inodes_count);
        put_u32(b, 0x14, self.first_data_block);
        put_u32(b, 0x18, self.log_block_size);
        put_u32(b, 0x1C, self.log_cluster_size);
        put_u32(b, 0x20, self.blocks_per_group);
        put_u32(b, 0x24, self.clusters_per_group);
        put_u32(b, 0x28, self.inodes_per_group);
        put_u32(b, 0x2C, self.mtime);
        put_u32(b, 0x30, self.wtime);
        put_u16(b, 0x34, self.mnt_count);
        put_u16(b, 0x36, self.max_mnt_count);
        put_u16(b, 0x38, SB_MAGIC);
        put_u16(b, 0x3A, self.state);
        put_u16(b, 0x3C, self.errors);
        put_u16(b, 0x3E, self.minor_rev_level);
        put_u32(b, 0x40, self.lastcheck);
        put_u32(b, 0x44, self.checkinterval);
        put_u32(b, 0x48, self.creator_os);
        put_u32(b, 0x4C, self.rev_level);
        put_u16(b, 0x50, self.def_resuid);
        put_u16(b, 0x52, self.def_resgid);
        put_u32(b, 0x54, self.first_ino);
        put_u16(b, 0x58, self.inode_size);
        put_u16(b, 0x5A, self.block_group_nr);
        put_u32(b, 0x5C, self.feature_compat);
        put_u32(b, 0x60, self.feature_incompat);
        put_u32(b, 0x64, self.feature_ro_compat);
        b[0x68..0x78].copy_from_slice(&self.uuid);
        b[0x78..0x88].copy_from_slice(&self.volume_name);
        b[0x88..0xC8].copy_from_slice(&self.last_mounted);
        put_u16(b, 0xCE, self.reserved_gdt_blocks);
        b[0xD0..0xE0].copy_from_slice(&self.journal_uuid);
        put_u32(b, 0xE0, self.journal_inum);
        put_u32(b, 0xE4, self.journal_dev);
        put_u32(b, 0xE8, self.last_orphan);
        for (i, w) in self.hash_seed.iter().enumerate() {
            put_u32(b, 0xEC + 4 * i, *w);
        }
        b[0xFC] = self.def_hash_version;
        b[0xFD] = self.jnl_backup_type;
        put_u16(b, 0xFE, self.desc_size);
        put_u32(b, 0x100, self.default_mount_opts);
        put_u32(b, 0x104, self.first_meta_bg);
        put_u32(b, 0x108, self.mkfs_time);
        for (i, w) in self.jnl_blocks.iter().enumerate() {
            put_u32(b, 0x10C + 4 * i, *w);
        }
        if self.feature_incompat & crate::spec::incompat::BIT64 != 0 {
            put_u32(b, 0x150, (self.blocks_count >> 32) as u32);
            put_u32(b, 0x154, (self.r_blocks_count >> 32) as u32);
            put_u32(b, 0x158, (self.free_blocks_count >> 32) as u32);
        }
        put_u16(b, 0x15C, self.min_extra_isize);
        put_u16(b, 0x15E, self.want_extra_isize);
        put_u32(b, 0x160, self.flags);
        b[0x174] = self.log_groups_per_flex;
        b[0x175] = self.checksum_type;
        put_u64(b, 0x178, self.kbytes_written);
        put_u32(b, 0x248, self.overhead_clusters);
        put_u32(b, 0x268, self.lpf_ino);
        put_u32(b, 0x270, self.checksum_seed);
        put_u32(b, 0x3FC, self.checksum);
    }

    /// Block size in bytes (`1024 << log_block_size`).
    pub fn block_size(&self) -> u64 {
        1024u64 << self.log_block_size
    }

    /// Number of block groups.
    pub fn group_count(&self) -> u64 {
        let bpg = u64::from(self.blocks_per_group);
        (self.blocks_count - u64::from(self.first_data_block)).div_ceil(bpg)
    }

    /// Group descriptor size (0 in pre-64bit superblocks means 32).
    pub fn desc_size(&self) -> usize {
        if self.desc_size == 0 {
            32
        } else {
            self.desc_size as usize
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn vector(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/testdata/vectors/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn roundtrip_primary_and_backup() {
        for name in ["sb_primary.bin", "sb_backup_g1.bin"] {
            let raw = vector(name);
            let sb = Superblock::decode(&raw).unwrap();
            let mut out = vec![0u8; Superblock::LEN];
            sb.encode(&mut out);
            assert_eq!(out, raw, "{name} does not round-trip byte-exactly");
        }
    }

    #[test]
    fn fixture_fields() {
        let sb = Superblock::decode(&vector("sb_primary.bin")).unwrap();
        assert_eq!(sb.block_size(), 4096);
        assert_eq!(sb.blocks_count, 131072);
        assert_eq!(sb.inodes_per_group, 8192);
        assert_eq!(sb.group_count(), 4);
        assert_eq!(sb.desc_size(), 32);
        assert_eq!(sb.feature_compat, crate::spec::compat::WRITER);
        assert_eq!(sb.feature_incompat, crate::spec::incompat::WRITER);
        assert_eq!(sb.feature_ro_compat, crate::spec::ro_compat::WRITER);
        assert_eq!(sb.first_ino, 11);
        assert_eq!(sb.inode_size, 256);
        assert_eq!(sb.checksum_type, 1);
        assert_eq!(sb.flags & 0x3, 0x1, "signed_directory_hash");
        assert_eq!(sb.def_hash_version, 1, "half_md4");
        assert_eq!(sb.log_groups_per_flex, 4);
    }

    #[test]
    fn rejects_bad_magic() {
        let mut raw = vector("sb_primary.bin");
        raw[0x38] = 0;
        assert!(Superblock::decode(&raw).is_err());
    }
}
