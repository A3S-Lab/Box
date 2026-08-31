//! htree (dir_index) structures: dx_root in directory block 0, optional
//! dx_node interior blocks, dirent-formatted leaves.
//!
//! Layout: dx_root = "." + ".." dirents, then
//! `dx_root_info` at 0x18, then the entry array at 0x20 whose slot 0
//! aliases `{limit u16, count u16, block u32}` (its hash is implicitly
//! 0). dx_node = fake dirent `{0, rec_len 4096}`, then the same
//! countlimit + entries at 0x8. Both end with an 8-byte
//! `dx_tail {reserved, checksum}` occupying the slot excluded from
//! `limit`.

use crate::le::{u16 as le16, u32 as le32};
use crate::{corrupt, Result};

/// `dx_root_info` (8 bytes at offset 0x18 of the root block).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DxInfo {
    /// Hash algorithm (1 = half_md4 — the only value this crate writes).
    pub hash_version: u8,
    /// Length of this info structure (always 8).
    pub info_length: u8,
    /// Tree depth below the root: 0 = leaves, 1 = one dx_node level.
    pub indirect_levels: u8,
    /// Unused flags (0).
    pub unused_flags: u8,
}

/// One interior entry: names hashing at/above `hash` (low bit = collision
/// continuation) live at/under directory-logical block `block`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DxEntry {
    /// Major hash bound (entry 0's is implicitly 0).
    pub hash: u32,
    /// Directory-logical block number of the child (leaf or dx_node).
    pub block: u32,
}

/// Parsed view of a dx_root block.
#[derive(Debug)]
pub struct DxRootView {
    /// The info structure.
    pub info: DxInfo,
    /// Entry capacity (`limit`), including the aliased slot 0.
    pub limit: u16,
    /// Valid entries (`count`), including slot 0.
    pub count: u16,
    /// The entries; `entries[0].hash == 0` by construction.
    pub entries: Vec<DxEntry>,
    /// Stored `dt_checksum`.
    pub checksum: u32,
}

/// Parsed view of a dx_node block.
#[derive(Debug)]
pub struct DxNodeView {
    /// Entry capacity including slot 0.
    pub limit: u16,
    /// Valid entries including slot 0.
    pub count: u16,
    /// The entries; `entries[0].hash == 0` by construction.
    pub entries: Vec<DxEntry>,
    /// Stored `dt_checksum`.
    pub checksum: u32,
}

/// Countlimit offset within a dx_root block.
pub const ROOT_COUNT_OFFSET: usize = 0x20;
/// Countlimit offset within a dx_node block.
pub const NODE_COUNT_OFFSET: usize = 0x8;

/// dx_root entry capacity for a block size, with the checksum tail slot
/// excluded: `(bs - 32) / 8 - 1`.
pub fn root_limit(block_size: usize) -> u16 {
    ((block_size - 0x20) / 8 - 1) as u16
}

/// dx_node entry capacity for a block size, tail slot excluded:
/// `(bs - 8) / 8 - 1`.
pub fn node_limit(block_size: usize) -> u16 {
    ((block_size - 0x8) / 8 - 1) as u16
}

fn parse_entries(
    b: &[u8],
    count_offset: usize,
    what: &'static str,
) -> Result<(u16, u16, Vec<DxEntry>, u32)> {
    let limit = le16(b, count_offset);
    let count = le16(b, count_offset + 2);
    if count == 0 || count > limit {
        return Err(corrupt(what, format!("count {count} vs limit {limit}")));
    }
    let end = count_offset + usize::from(limit) * 8 + 8;
    if end > b.len() {
        return Err(corrupt(what, format!("limit {limit} overflows block")));
    }
    let mut entries = Vec::with_capacity(usize::from(count));
    entries.push(DxEntry {
        hash: 0,
        block: le32(b, count_offset + 4),
    });
    for i in 1..usize::from(count) {
        entries.push(DxEntry {
            hash: le32(b, count_offset + 8 * i),
            block: le32(b, count_offset + 8 * i + 4),
        });
    }
    let tail = count_offset + usize::from(limit) * 8;
    Ok((limit, count, entries, le32(b, tail + 4)))
}

impl DxRootView {
    /// Parse directory block 0 of a hash-indexed directory.
    pub fn parse(block: &[u8]) -> Result<DxRootView> {
        if block.len() < 0x30 {
            return Err(corrupt("dx_root", "short buffer"));
        }
        // dot / dotdot sanity: real dirents at 0x00 and 0x0C with names
        // "." and ".." (".."'s rec_len stretches to the tail).
        if le32(block, 0) == 0 || &block[8..9] != b"." {
            return Err(corrupt("dx_root", "missing '.' entry"));
        }
        if le32(block, 0x0C) == 0 || &block[0x14..0x16] != b".." {
            return Err(corrupt("dx_root", "missing '..' entry"));
        }
        if le32(block, 0x18) != 0 {
            return Err(corrupt("dx_root", "reserved_zero not zero"));
        }
        let info = DxInfo {
            hash_version: block[0x1C],
            info_length: block[0x1D],
            indirect_levels: block[0x1E],
            unused_flags: block[0x1F],
        };
        if info.info_length != 8 {
            return Err(corrupt(
                "dx_root",
                format!("info_length {}", info.info_length),
            ));
        }
        if info.indirect_levels > 1 {
            return Err(crate::Error::Unsupported(format!(
                "htree with indirect_levels {} (largedir)",
                info.indirect_levels
            )));
        }
        let (limit, count, entries, checksum) = parse_entries(block, ROOT_COUNT_OFFSET, "dx_root")?;
        Ok(DxRootView {
            info,
            limit,
            count,
            entries,
            checksum,
        })
    }
}

impl DxNodeView {
    /// Parse a dx_node (interior) block.
    pub fn parse(block: &[u8]) -> Result<DxNodeView> {
        if block.len() < 0x18 {
            return Err(corrupt("dx_node", "short buffer"));
        }
        if le32(block, 0) != 0 || usize::from(le16(block, 4)) != block.len() {
            return Err(corrupt("dx_node", "fake dirent header mismatch"));
        }
        let (limit, count, entries, checksum) = parse_entries(block, NODE_COUNT_OFFSET, "dx_node")?;
        Ok(DxNodeView {
            limit,
            count,
            entries,
            checksum,
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
    fn limits_at_4096() {
        assert_eq!(root_limit(4096), 507);
        assert_eq!(node_limit(4096), 510);
    }

    #[test]
    fn parse_fixture_root() {
        let blk = vector("dx_root_bigdir.bin");
        let root = DxRootView::parse(&blk).unwrap();
        assert_eq!(root.info.hash_version, 1, "half_md4");
        assert_eq!(root.info.indirect_levels, 0);
        assert_eq!(root.limit, 507);
        assert_eq!(usize::from(root.count), root.entries.len());
        assert_eq!(root.entries[0].hash, 0);
        // Entries are ascending by hash.
        for w in root.entries.windows(2) {
            assert!(w[0].hash < w[1].hash);
        }
    }

    #[test]
    fn leaf_names_hash_into_ranges() {
        use crate::dirhash::{half_md4, Signedness};
        let root = DxRootView::parse(&vector("dx_root_bigdir.bin")).unwrap();
        let leaf = vector("dx_leaf_bigdir.bin");
        // Fixture seed (dx_hash.json).
        let seed = [0xefbe_adde, 0xad4e_adde, 0xadde_ad8e, 0x0000_efbe];
        // The leaf blob is the directory's logical block 1 = the child of
        // root entry 0; its hash range is [0, entries[1].hash).
        let hi = root.entries.get(1).map(|e| e.hash).unwrap_or(u32::MAX);
        let mut checked = 0;
        for e in crate::spec::dirent::DirentIter::new(&leaf) {
            let e = e.unwrap();
            let (h, _) = half_md4(&seed, Signedness::Signed, e.name);
            assert!(h < hi, "{:?} hashes outside its leaf", e.name);
            checked += 1;
        }
        assert!(checked > 50, "leaf should hold many entries");
    }
}
