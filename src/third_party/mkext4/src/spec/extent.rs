//! Extent trees: 12-byte header + 12-byte entries, root inline in
//! `i_block` (max 4 entries), interior/leaf blocks with a 4-byte checksum
//! tail.

use crate::le::{put_u16, put_u32, u16 as le16, u32 as le32};
use crate::spec::consts::EXTENT_MAGIC;
use crate::{corrupt, Result};

/// Node header (`ext4_extent_header`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtentHeader {
    /// Valid entries following the header.
    pub entries: u16,
    /// Entry capacity of this node.
    pub max: u16,
    /// 0 = leaf (entries are [`Extent`]); >0 = index node ([`ExtentIdx`]).
    pub depth: u16,
    /// Unused by this crate; kernel scratch.
    pub generation: u32,
}

impl ExtentHeader {
    /// Header length in bytes.
    pub const LEN: usize = 12;
    /// Entry length in bytes (both kinds).
    pub const ENTRY_LEN: usize = 12;

    /// Decode and validate the magic.
    pub fn decode(b: &[u8]) -> Result<ExtentHeader> {
        if b.len() < Self::LEN {
            return Err(corrupt("extent header", "short buffer"));
        }
        if le16(b, 0) != EXTENT_MAGIC {
            return Err(corrupt(
                "extent header",
                format!("bad magic {:#06x}", le16(b, 0)),
            ));
        }
        Ok(ExtentHeader {
            entries: le16(b, 2),
            max: le16(b, 4),
            depth: le16(b, 6),
            generation: le32(b, 8),
        })
    }

    /// Encode the 12 header bytes.
    pub fn encode(&self, out: &mut [u8]) {
        put_u16(out, 0, EXTENT_MAGIC);
        put_u16(out, 2, self.entries);
        put_u16(out, 4, self.max);
        put_u16(out, 6, self.depth);
        put_u32(out, 8, self.generation);
    }
}

/// Leaf entry (`ext4_extent`): a run of mapped blocks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Extent {
    /// First logical block covered.
    pub logical: u32,
    /// Raw length field: 1..=32768 initialized; >32768 = unwritten,
    /// actual length − 32768.
    pub raw_len: u16,
    /// First physical block (48-bit).
    pub start: u64,
}

impl Extent {
    /// Decode from a 12-byte entry.
    pub fn decode(b: &[u8]) -> Extent {
        Extent {
            logical: le32(b, 0),
            raw_len: le16(b, 4),
            start: u64::from(le16(b, 6)) << 32 | u64::from(le32(b, 8)),
        }
    }

    /// Encode into a 12-byte entry.
    pub fn encode(&self, out: &mut [u8]) {
        put_u32(out, 0, self.logical);
        put_u16(out, 4, self.raw_len);
        put_u16(out, 6, (self.start >> 32) as u16);
        put_u32(out, 8, self.start as u32);
    }

    /// Block count covered. `raw_len == 32768` (0x8000) is a *valid
    /// initialized* max-length extent; only values above it are unwritten.
    pub fn len(&self) -> u32 {
        if self.raw_len > 32768 {
            u32::from(self.raw_len) - 32768
        } else {
            u32::from(self.raw_len)
        }
    }

    /// Whether the run is allocated-but-unwritten (reads as zeros).
    pub fn is_unwritten(&self) -> bool {
        self.raw_len > 32768
    }

    /// True when `len()` would be zero (invalid on disk).
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }
}

/// Index entry (`ext4_extent_idx`): points at a child node block.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ExtentIdx {
    /// First logical block covered by the child subtree.
    pub logical: u32,
    /// Physical block of the child node (48-bit).
    pub leaf: u64,
}

impl ExtentIdx {
    /// Decode from a 12-byte entry.
    pub fn decode(b: &[u8]) -> ExtentIdx {
        ExtentIdx {
            logical: le32(b, 0),
            leaf: u64::from(le32(b, 4)) | u64::from(le16(b, 8)) << 32,
        }
    }

    /// Encode into a 12-byte entry.
    pub fn encode(&self, out: &mut [u8]) {
        put_u32(out, 0, self.logical);
        put_u32(out, 4, self.leaf as u32);
        put_u16(out, 8, (self.leaf >> 32) as u16);
        put_u16(out, 10, 0);
    }

    /// Entry capacity of an interior/leaf *block* node with a checksum
    /// tail: `(block_size - 12 - 4) / 12`.
    pub fn node_capacity(block_size: usize) -> usize {
        (block_size - ExtentHeader::LEN - 4) / ExtentHeader::ENTRY_LEN
    }
}

/// Slice the `i`-th entry region of a node.
pub fn entry(b: &[u8], i: usize) -> &[u8] {
    let off = ExtentHeader::LEN + i * ExtentHeader::ENTRY_LEN;
    &b[off..off + ExtentHeader::ENTRY_LEN]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let h = ExtentHeader {
            entries: 3,
            max: 340,
            depth: 1,
            generation: 0,
        };
        let mut b = [0u8; 12];
        h.encode(&mut b);
        assert_eq!(ExtentHeader::decode(&b).unwrap(), h);
    }

    #[test]
    fn raw_len_32768_is_initialized_max() {
        let e = Extent {
            logical: 0,
            raw_len: 32768,
            start: 100,
        };
        assert_eq!(e.len(), 32768);
        assert!(!e.is_unwritten());
        let u = Extent {
            logical: 0,
            raw_len: 32769,
            start: 100,
        };
        assert_eq!(u.len(), 1);
        assert!(u.is_unwritten());
    }

    #[test]
    fn node_capacity_at_4096() {
        assert_eq!(ExtentIdx::node_capacity(4096), 340);
    }

    #[test]
    fn journal_inode_root_parses() {
        let raw = std::fs::read(format!(
            "{}/testdata/vectors/inode_8_journal.bin",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let root = &raw[0x28..0x64];
        let h = ExtentHeader::decode(root).unwrap();
        assert_eq!(h.depth, 0);
        assert_eq!(h.max, 4);
        assert!(h.entries >= 1);
        let e = Extent::decode(entry(root, 0));
        assert_eq!(e.logical, 0);
        assert!(!e.is_unwritten());
    }
}
