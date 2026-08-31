//! Linear directory blocks: `ext4_dir_entry_2` records plus the
//! metadata_csum tail (a fake 12-byte dirent holding the block checksum).

use crate::le::{u16 as le16, u32 as le32};
use crate::{corrupt, Result};

/// One directory entry, borrowing its name from the block buffer.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DirEntryRef<'a> {
    /// Inode number (never 0 for entries yielded by [`DirentIter`]).
    pub inode: u32,
    /// `file_type` byte (see [`crate::spec::file_type`]).
    pub file_type: u8,
    /// Name bytes (1..=255, no NUL / `/`).
    pub name: &'a [u8],
    /// Byte offset of this record within the block.
    pub offset: usize,
    /// On-disk record length (includes padding to the next entry).
    pub rec_len: u16,
}

/// Iterator over the *used* entries of one directory block. Unused
/// records (inode 0), including the checksum tail, are skipped; record
/// chains that don't tile the block terminate the iteration with an
/// error.
pub struct DirentIter<'a> {
    block: &'a [u8],
    offset: usize,
    failed: bool,
}

impl<'a> DirentIter<'a> {
    /// Iterate `block` (any dirent-formatted block: linear dir block,
    /// htree leaf, or lost+found padding block).
    pub fn new(block: &'a [u8]) -> Self {
        DirentIter {
            block,
            offset: 0,
            failed: false,
        }
    }
}

impl<'a> Iterator for DirentIter<'a> {
    type Item = Result<DirEntryRef<'a>>;

    fn next(&mut self) -> Option<Self::Item> {
        while !self.failed && self.offset < self.block.len() {
            let b = self.block;
            let off = self.offset;
            if b.len() - off < 8 {
                self.failed = true;
                return Some(Err(corrupt("dirent", "trailing bytes shorter than header")));
            }
            let inode = le32(b, off);
            let rec_len = le16(b, off + 4);
            let name_len = b[off + 6] as usize;
            let file_type = b[off + 7];
            if rec_len < 8 || rec_len % 4 != 0 || off + rec_len as usize > b.len() {
                self.failed = true;
                return Some(Err(corrupt(
                    "dirent",
                    format!("bad rec_len {rec_len} at offset {off}"),
                )));
            }
            self.offset = off + rec_len as usize;
            if inode == 0 {
                continue; // unused record or checksum tail
            }
            if 8 + name_len > rec_len as usize {
                self.failed = true;
                return Some(Err(corrupt(
                    "dirent",
                    format!("name_len {name_len} overflows rec_len {rec_len}"),
                )));
            }
            return Some(Ok(DirEntryRef {
                inode,
                file_type,
                name: &b[off + 8..off + 8 + name_len],
                offset: off,
                rec_len,
            }));
        }
        None
    }
}

/// If `block` ends in a metadata_csum dirent tail
/// (`{inode 0, rec_len 12, name_len 0, file_type 0xDE}`), return the
/// stored checksum.
pub fn tail_checksum(block: &[u8]) -> Option<u32> {
    let t = block.len().checked_sub(12)?;
    (le32(block, t) == 0 && le16(block, t + 4) == 12 && block[t + 6] == 0 && block[t + 7] == 0xDE)
        .then(|| le32(block, t + 8))
}

/// On-disk record length for a name: `align4(8 + name_len)`.
pub fn rec_len_for(name_len: usize) -> usize {
    (8 + name_len + 3) & !3
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::spec::file_type;

    fn vector(name: &str) -> Vec<u8> {
        std::fs::read(format!(
            "{}/testdata/vectors/{name}",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap()
    }

    #[test]
    fn root_block_entries() {
        let blk = vector("dirblock_root.bin");
        let entries: Vec<_> = DirentIter::new(&blk).map(|e| e.unwrap()).collect();
        assert_eq!(entries[0].name, b".");
        assert_eq!(entries[0].inode, 2);
        assert_eq!(entries[0].file_type, file_type::DIR);
        assert_eq!(entries[1].name, b"..");
        assert_eq!(entries[1].inode, 2);
        let names: Vec<_> = entries.iter().map(|e| e.name).collect();
        assert!(names.contains(&&b"lost+found"[..]));
        assert!(names.contains(&&b"small.txt"[..]));
        assert!(names.contains(&&b"sym_59"[..]));
        // Fixture facts: hardlink shares the target's inode.
        let by_name = |n: &[u8]| entries.iter().find(|e| e.name == n).unwrap();
        assert_eq!(
            by_name(b"small.txt").inode,
            by_name(b"hardlink_to_small").inode
        );
        assert_eq!(by_name(b"sym_59").file_type, file_type::SYMLINK);
        assert_eq!(by_name(b"fifo").file_type, file_type::FIFO);
        assert_eq!(by_name(b"sock").file_type, file_type::SOCK);
        assert_eq!(by_name(b"dev_c_old").file_type, file_type::CHR);
        assert_eq!(by_name(b"dev_b_old").file_type, file_type::BLK);
    }

    #[test]
    fn tail_detected() {
        let blk = vector("dirblock_root.bin");
        assert!(tail_checksum(&blk).is_some());
        // The "empty" lost+found padding block also carries a tail.
        let empty = vector("dirblock_empty.bin");
        assert!(tail_checksum(&empty).is_some());
        assert_eq!(DirentIter::new(&empty).count(), 0, "no used entries");
    }

    #[test]
    fn rec_len_alignment() {
        assert_eq!(rec_len_for(1), 12);
        assert_eq!(rec_len_for(4), 12);
        assert_eq!(rec_len_for(5), 16);
        assert_eq!(rec_len_for(255), 264);
    }

    #[test]
    fn corrupt_rec_len_errors() {
        let mut blk = vector("dirblock_root.bin");
        blk[4] = 3; // rec_len of "." → 3: unaligned and < 8
        blk[5] = 0;
        let r: crate::Result<Vec<_>> = DirentIter::new(&blk).collect();
        assert!(r.is_err());
    }
}
