//! On-disk inodes (256 bytes in images this crate writes; the reader also
//! accepts 128-byte rev-1 inodes without the extra area).

use crate::le::{put_u16, put_u32, u16 as le16, u32 as le32};
use crate::spec::consts::{EXTENT_MAGIC, INODE_SIZE};
use crate::{corrupt, Result};

/// Decoded inode. `_lo`/`_hi` splits (uid, gid, size, blocks, file_acl,
/// version, checksum) are combined; `i_block` stays raw — interpret it via
/// the typed accessors ([`Inode::dev_numbers`],
/// [`Inode::fast_symlink_target`], or the extent parsers in
/// [`crate::spec::extent`]).
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(missing_docs)] // the on-disk field names are the documentation
pub struct Inode {
    pub mode: u16,
    pub uid: u32,
    pub gid: u32,
    pub size: u64,
    pub atime: u32,
    pub ctime: u32,
    pub mtime: u32,
    pub dtime: u32,
    pub links_count: u16,
    /// 512-byte units, counting data blocks plus the inode's own
    /// metadata blocks (extent tree, xattr block, directory content).
    pub blocks: u64,
    pub flags: u32,
    pub version: u64,
    pub block: [u8; 60],
    pub generation: u32,
    pub file_acl: u64,
    pub extra_isize: u16,
    pub checksum: u32,
    pub ctime_extra: u32,
    pub mtime_extra: u32,
    pub atime_extra: u32,
    pub crtime: u32,
    pub crtime_extra: u32,
    pub projid: u32,
    /// Raw bytes of the in-inode xattr area (from `0x80 + extra_isize` to
    /// the end of the inode slot). Empty when the area is all zero.
    /// Parse with [`crate::spec::xattr::ibody_entries`].
    pub ibody: Vec<u8>,
}

impl Default for Inode {
    fn default() -> Self {
        Inode {
            mode: 0,
            uid: 0,
            gid: 0,
            size: 0,
            atime: 0,
            ctime: 0,
            mtime: 0,
            dtime: 0,
            links_count: 0,
            blocks: 0,
            flags: 0,
            version: 0,
            block: [0; 60],
            generation: 0,
            file_acl: 0,
            extra_isize: 0,
            checksum: 0,
            ctime_extra: 0,
            mtime_extra: 0,
            atime_extra: 0,
            crtime: 0,
            crtime_extra: 0,
            projid: 0,
            ibody: Vec::new(),
        }
    }
}

impl Inode {
    /// Decode from an inode-table slot (`b.len()` = the fs inode size).
    pub fn decode(b: &[u8]) -> Result<Inode> {
        if b.len() < 128 {
            return Err(corrupt("inode", "short buffer"));
        }
        let mut ino = Inode {
            mode: le16(b, 0x00),
            uid: u32::from(le16(b, 0x02)) | u32::from(le16(b, 0x78)) << 16,
            gid: u32::from(le16(b, 0x18)) | u32::from(le16(b, 0x7A)) << 16,
            size: u64::from(le32(b, 0x04)) | u64::from(le32(b, 0x6C)) << 32,
            atime: le32(b, 0x08),
            ctime: le32(b, 0x0C),
            mtime: le32(b, 0x10),
            dtime: le32(b, 0x14),
            links_count: le16(b, 0x1A),
            blocks: u64::from(le32(b, 0x1C)) | u64::from(le16(b, 0x74)) << 32,
            flags: le32(b, 0x20),
            version: u64::from(le32(b, 0x24)),
            block: b[0x28..0x64].try_into().unwrap(),
            generation: le32(b, 0x64),
            file_acl: u64::from(le32(b, 0x68)) | u64::from(le16(b, 0x76)) << 32,
            extra_isize: 0,
            checksum: u32::from(le16(b, 0x7C)),
            ctime_extra: 0,
            mtime_extra: 0,
            atime_extra: 0,
            crtime: 0,
            crtime_extra: 0,
            projid: 0,
            ibody: Vec::new(),
        };
        if b.len() > 0x80 {
            ino.extra_isize = le16(b, 0x80);
            let extra_end = 0x80 + usize::from(ino.extra_isize);
            let has = |field_end: usize| b.len() >= field_end && extra_end >= field_end;
            if has(0x84) {
                ino.checksum |= u32::from(le16(b, 0x82)) << 16;
            }
            if has(0x88) {
                ino.ctime_extra = le32(b, 0x84);
            }
            if has(0x8C) {
                ino.mtime_extra = le32(b, 0x88);
            }
            if has(0x90) {
                ino.atime_extra = le32(b, 0x8C);
            }
            if has(0x94) {
                ino.crtime = le32(b, 0x90);
            }
            if has(0x98) {
                ino.crtime_extra = le32(b, 0x94);
            }
            if has(0x9C) {
                ino.version |= u64::from(le32(b, 0x98)) << 32;
            }
            if has(0xA0) {
                ino.projid = le32(b, 0x9C);
            }
            let area = &b[extra_end.min(b.len())..];
            if area.iter().any(|&x| x != 0) {
                ino.ibody = area.to_vec();
            }
        }
        Ok(ino)
    }

    /// Encode into a 256-byte inode-table slot.
    pub fn encode(&self, out: &mut [u8]) {
        let b = &mut out[..INODE_SIZE];
        b.fill(0);
        put_u16(b, 0x00, self.mode);
        put_u16(b, 0x02, self.uid as u16);
        put_u32(b, 0x04, self.size as u32);
        put_u32(b, 0x08, self.atime);
        put_u32(b, 0x0C, self.ctime);
        put_u32(b, 0x10, self.mtime);
        put_u32(b, 0x14, self.dtime);
        put_u16(b, 0x18, self.gid as u16);
        put_u16(b, 0x1A, self.links_count);
        put_u32(b, 0x1C, self.blocks as u32);
        put_u32(b, 0x20, self.flags);
        put_u32(b, 0x24, self.version as u32);
        b[0x28..0x64].copy_from_slice(&self.block);
        put_u32(b, 0x64, self.generation);
        put_u32(b, 0x68, self.file_acl as u32);
        put_u32(b, 0x6C, (self.size >> 32) as u32);
        put_u16(b, 0x74, (self.blocks >> 32) as u16);
        put_u16(b, 0x76, (self.file_acl >> 32) as u16);
        put_u16(b, 0x78, (self.uid >> 16) as u16);
        put_u16(b, 0x7A, (self.gid >> 16) as u16);
        put_u16(b, 0x7C, self.checksum as u16);
        put_u16(b, 0x80, self.extra_isize);
        if self.extra_isize >= 4 {
            put_u16(b, 0x82, (self.checksum >> 16) as u16);
        }
        if self.extra_isize >= 8 {
            put_u32(b, 0x84, self.ctime_extra);
        }
        if self.extra_isize >= 12 {
            put_u32(b, 0x88, self.mtime_extra);
        }
        if self.extra_isize >= 16 {
            put_u32(b, 0x8C, self.atime_extra);
        }
        if self.extra_isize >= 20 {
            put_u32(b, 0x90, self.crtime);
        }
        if self.extra_isize >= 24 {
            put_u32(b, 0x94, self.crtime_extra);
        }
        if self.extra_isize >= 28 {
            put_u32(b, 0x98, (self.version >> 32) as u32);
        }
        if self.extra_isize >= 32 {
            put_u32(b, 0x9C, self.projid);
        }
        if !self.ibody.is_empty() {
            let start = 0x80 + usize::from(self.extra_isize);
            b[start..start + self.ibody.len()].copy_from_slice(&self.ibody);
        }
    }

    /// POSIX file type from the mode's high bits.
    pub fn file_type(&self) -> FileType {
        match self.mode >> 12 {
            0o01 => FileType::Fifo,
            0o02 => FileType::CharDev,
            0o04 => FileType::Dir,
            0o06 => FileType::BlockDev,
            0o10 => FileType::Regular,
            0o12 => FileType::Symlink,
            0o14 => FileType::Socket,
            _ => FileType::Unknown,
        }
    }

    /// True when this inode maps its contents through an extent tree.
    pub fn uses_extents(&self) -> bool {
        self.flags & crate::spec::iflags::EXTENTS != 0
    }

    /// A symlink is "fast" (target inline in `i_block`) when it has no
    /// mapped blocks and no extent flag.
    pub fn fast_symlink_target(&self) -> Option<&[u8]> {
        if self.file_type() == FileType::Symlink && !self.uses_extents() && self.size <= 60 {
            Some(&self.block[..self.size as usize])
        } else {
            None
        }
    }

    /// Decode device numbers from `i_block` (char/block devices only):
    /// old encoding in word 0 (`major < 256 && minor < 256`), new encoding
    /// in word 1 otherwise.
    pub fn dev_numbers(&self) -> Option<(u32, u32)> {
        match self.file_type() {
            FileType::CharDev | FileType::BlockDev => {
                let w0 = le32(&self.block, 0);
                let w1 = le32(&self.block, 4);
                Some(if w0 != 0 {
                    (w0 >> 8 & 0xFF, w0 & 0xFF)
                } else {
                    (w1 >> 8 & 0xFFF, (w1 & 0xFF) | (w1 >> 12 & !0xFFu32))
                })
            }
            _ => None,
        }
    }

    /// The extent-tree root region of `i_block`, if the header magic is
    /// present.
    pub fn extent_root(&self) -> Option<&[u8]> {
        if self.uses_extents() && le16(&self.block, 0) == EXTENT_MAGIC {
            Some(&self.block[..])
        } else {
            None
        }
    }

    /// Combine a classic seconds field with its `_extra` word into
    /// (seconds since the epoch, nanoseconds). The extra word's low two
    /// bits extend the seconds range past 2038; the rest is nanoseconds.
    pub fn timestamp(secs: u32, extra: u32) -> (i64, u32) {
        let epoch_bits = i64::from(extra & 0x3);
        (i64::from(secs as i32) + (epoch_bits << 32), extra >> 2)
    }

    /// Inverse of [`Inode::timestamp`]. Valid for seconds in
    /// `[-2^31, 3·2^32 + 2^31)`; the decoder sign-extends the low word,
    /// so the epoch bits count offsets from that signed base.
    pub fn encode_timestamp(secs: i64, nsec: u32) -> (u32, u32) {
        let epoch_bits = (((secs + (1 << 31)) >> 32) & 0x3) as u32;
        (secs as u32, (nsec << 2) | epoch_bits)
    }
}

/// POSIX file type of an inode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(missing_docs)]
pub enum FileType {
    Regular,
    Dir,
    Symlink,
    CharDev,
    BlockDev,
    Fifo,
    Socket,
    Unknown,
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
    fn roundtrip_all_inode_vectors() {
        for name in [
            "inode_1_bad_blocks.bin",
            "inode_2_root.bin",
            "inode_8_journal.bin",
            "inode_11_lost_found.bin",
            "inode_small_txt.bin",
            "inode_empty.bin",
            "inode_sym_59.bin",
            "inode_sym_60.bin",
            "inode_dev_c_old.bin",
            "inode_dev_c_new.bin",
            "inode_sparse_small.bin",
            "inode_xattr_ibody.bin",
            "inode_xattr_mixed.bin",
            "inode_xattr_block.bin",
            "inode_bigdir_dx.bin",
        ] {
            let raw = vector(name);
            let ino = Inode::decode(&raw).unwrap();
            let mut out = vec![0u8; INODE_SIZE];
            ino.encode(&mut out);
            assert_eq!(out, raw, "{name} does not round-trip byte-exactly");
        }
    }

    #[test]
    fn typed_accessors() {
        let root = Inode::decode(&vector("inode_2_root.bin")).unwrap();
        assert_eq!(root.file_type(), FileType::Dir);
        assert!(root.uses_extents());
        assert!(root.extent_root().is_some());

        let sym = Inode::decode(&vector("inode_sym_59.bin")).unwrap();
        assert_eq!(sym.fast_symlink_target(), Some(&[b'a'; 59][..]));
        let slow = Inode::decode(&vector("inode_sym_60.bin")).unwrap();
        assert_eq!(slow.fast_symlink_target(), None);
        assert!(slow.uses_extents());

        let dev_old = Inode::decode(&vector("inode_dev_c_old.bin")).unwrap();
        assert_eq!(dev_old.dev_numbers(), Some((5, 1)));
        let dev_new = Inode::decode(&vector("inode_dev_c_new.bin")).unwrap();
        assert_eq!(dev_new.dev_numbers(), Some((254, 300)));

        let empty = Inode::decode(&vector("inode_empty.bin")).unwrap();
        assert_eq!(empty.size, 0);
        assert!(empty.extent_root().is_some(), "empty file keeps a header");
    }

    #[test]
    fn timestamp_encoding() {
        // Pre-2038, no nanoseconds.
        assert_eq!(Inode::timestamp(1_600_000_000, 0), (1_600_000_000, 0));
        // Post-2038: epoch bit 1 extends the range.
        let secs = i64::from(u32::MAX) + 100;
        let (s, e) = Inode::encode_timestamp(secs, 7);
        assert_eq!(Inode::timestamp(s, e), (secs, 7));
        // Negative (pre-1970) stays sign-extended with epoch bits 0.
        let (s, e) = Inode::encode_timestamp(-1, 0);
        assert_eq!(Inode::timestamp(s, e), (-1, 0));
    }
}
