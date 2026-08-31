//! Extended attributes: the in-inode area (after `i_extra_isize`) and the
//! single external xattr block (`i_file_acl`).
//!
//! Both regions share the entry format `{name_len u8, name_index u8,
//! value_offs u16, value_inum u32, value_size u32, hash u32, name}` with
//! 4-byte entry alignment and a 4-zero-byte list terminator. Value
//! offsets are relative to the *first entry position* for the in-inode
//! area (after the 4-byte magic — kernel `IFIRST`; verified byte-exactly)
//! and to the block start for xattr blocks.

use crate::le::{u16 as le16, u32 as le32};
use crate::spec::consts::XATTR_MAGIC;
use crate::{corrupt, Result};

/// One decoded attribute (owned; xattr data is small).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct XattrEntry {
    /// Namespace index (1 = `user.`, 6 = `security.`, …).
    pub name_index: u8,
    /// Name bytes after the namespace prefix.
    pub name: Vec<u8>,
    /// Attribute value.
    pub value: Vec<u8>,
    /// Stored `e_hash` (0 for in-inode entries).
    pub hash: u32,
}

impl XattrEntry {
    /// The full attribute name (`prefix + name`), or `None` for an
    /// unknown namespace index.
    pub fn full_name(&self) -> Option<Vec<u8>> {
        let prefix: &[u8] = match self.name_index {
            0 => b"",
            1 => b"user.",
            2 => b"system.posix_acl_access",
            3 => b"system.posix_acl_default",
            4 => b"trusted.",
            6 => b"security.",
            7 => b"system.",
            _ => return None,
        };
        let mut v = Vec::with_capacity(prefix.len() + self.name.len());
        v.extend_from_slice(prefix);
        v.extend_from_slice(&self.name);
        Some(v)
    }
}

/// Walk an entry list. `region` starts at the first entry; `value_base`
/// is the buffer values are offset against.
fn parse_entries(region: &[u8], value_base: &[u8]) -> Result<Vec<XattrEntry>> {
    let mut out = Vec::new();
    let mut off = 0usize;
    loop {
        if off + 4 > region.len() {
            return Err(corrupt("xattr", "unterminated entry list"));
        }
        if le32(region, off) == 0 {
            return Ok(out);
        }
        if off + 16 > region.len() {
            return Err(corrupt("xattr", "entry header overflows region"));
        }
        let name_len = region[off] as usize;
        let name_index = region[off + 1];
        let value_offs = le16(region, off + 2) as usize;
        let value_inum = le32(region, off + 4);
        let value_size = le32(region, off + 8) as usize;
        let hash = le32(region, off + 12);
        if value_inum != 0 {
            return Err(crate::Error::Unsupported("ea_inode xattr values".into()));
        }
        if off + 16 + name_len > region.len() {
            return Err(corrupt("xattr", "name overflows region"));
        }
        if value_offs + value_size > value_base.len() {
            return Err(corrupt("xattr", "value overflows region"));
        }
        out.push(XattrEntry {
            name_index,
            name: region[off + 16..off + 16 + name_len].to_vec(),
            value: value_base[value_offs..value_offs + value_size].to_vec(),
            hash,
        });
        off += 16 + name_len + (4 - name_len % 4) % 4;
    }
}

/// Parse the in-inode xattr area of a raw inode-table slot. `extra_isize`
/// is the inode's `i_extra_isize`; returns an empty list when there is no
/// area or no magic.
pub fn ibody_entries(raw_inode: &[u8], extra_isize: u16) -> Result<Vec<XattrEntry>> {
    let start = 0x80 + usize::from(extra_isize);
    if start + 4 > raw_inode.len() || le32(raw_inode, start) != XATTR_MAGIC {
        return Ok(Vec::new());
    }
    // Values are offset from IFIRST — the first entry position, after the
    // 4-byte magic.
    let area = &raw_inode[start + 4..];
    parse_entries(area, area)
}

/// Parsed view of an external xattr block.
#[derive(Debug)]
pub struct XattrBlockView {
    /// `h_refcount` (always 1 in images this crate writes).
    pub refcount: u32,
    /// Stored `h_hash`.
    pub hash: u32,
    /// Stored `h_checksum`.
    pub checksum: u32,
    /// The attributes, in on-disk order.
    pub entries: Vec<XattrEntry>,
}

impl XattrBlockView {
    /// Parse a full xattr block.
    pub fn parse(block: &[u8]) -> Result<XattrBlockView> {
        if block.len() < 0x20 {
            return Err(corrupt("xattr block", "short buffer"));
        }
        if le32(block, 0) != XATTR_MAGIC {
            return Err(corrupt(
                "xattr block",
                format!("bad magic {:#010x}", le32(block, 0)),
            ));
        }
        if le32(block, 8) != 1 {
            return Err(corrupt("xattr block", "h_blocks != 1"));
        }
        Ok(XattrBlockView {
            refcount: le32(block, 4),
            hash: le32(block, 0x0C),
            checksum: le32(block, 0x10),
            // Entries start after the 32-byte header; values are offset
            // from the block start.
            entries: parse_entries(&block[0x20..], block)?,
        })
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
    fn ibody_fixture() {
        let raw = vector("inode_xattr_mixed.bin");
        let extra = u16::from_le_bytes(raw[0x80..0x82].try_into().unwrap());
        let entries = ibody_entries(&raw, extra).unwrap();
        // debugfs wrote selinux first (insertion order), then user.alpha.
        assert_eq!(entries[0].full_name().unwrap(), b"security.selinux");
        assert_eq!(entries[0].value, b"system_u:object_r:etc_t:s0");
        assert_eq!(entries[0].hash, 0, "ibody entries store hash 0");
        assert!(entries
            .iter()
            .any(|e| e.full_name().unwrap() == b"user.alpha" && e.value == b"aaaa"));
    }

    #[test]
    fn ibody_absent_is_empty() {
        let raw = vector("inode_2_root.bin");
        let extra = u16::from_le_bytes(raw[0x80..0x82].try_into().unwrap());
        assert!(ibody_entries(&raw, extra).unwrap().is_empty());
    }

    #[test]
    fn block_fixture() {
        let blk = vector("xattr_block.bin");
        let v = XattrBlockView::parse(&blk).unwrap();
        assert_eq!(v.refcount, 1);
        assert_eq!(v.entries.len(), 1);
        let e = &v.entries[0];
        assert_eq!(e.full_name().unwrap(), b"user.big");
        assert_eq!(e.value.len(), 256);
        // e_hash matches the documented legacy formula.
        assert_eq!(e.hash, crate::csum::xattr_entry_hash(&e.name, &e.value));
    }
}
