//! The jbd2 journal superblock (journal block 0). All fields big-endian —
//! the only big-endian structure in an ext4 image.
//!
//! The writer emits mke2fs's empty-journal shape: a feature-less v2
//! superblock (no checksum features even with metadata_csum — the kernel
//! upgrades the journal on first mount), sequence 1, start 0, and zeros
//! for the rest of the journal.

use crate::le::be;
use crate::spec::consts::JBD2_MAGIC;
use crate::{corrupt, Result};

/// Decoded jbd2 superblock (v2).
#[derive(Debug, Clone, PartialEq, Eq)]
#[allow(missing_docs)] // the on-disk field names are the documentation
pub struct JournalSuperblock {
    /// `h_blocktype`: 3 = v1 superblock, 4 = v2 (the only kind we write).
    pub blocktype: u32,
    pub blocksize: u32,
    pub maxlen: u32,
    pub first: u32,
    pub sequence: u32,
    pub start: u32,
    pub errno: u32,
    pub feature_compat: u32,
    pub feature_incompat: u32,
    pub feature_ro_compat: u32,
    pub uuid: [u8; 16],
    pub nr_users: u32,
    pub dynsuper: u32,
    pub max_transaction: u32,
    pub max_trans_data: u32,
    pub checksum_type: u8,
    pub num_fc_blocks: u32,
    pub head: u32,
    pub checksum: u32,
}

impl JournalSuperblock {
    /// Byte length of the on-disk structure (fields; the rest of the
    /// 1024-byte area is user records / padding).
    pub const LEN: usize = 1024;

    /// Decode from the first bytes of the journal.
    pub fn decode(b: &[u8]) -> Result<JournalSuperblock> {
        if b.len() < Self::LEN {
            return Err(corrupt("journal superblock", "short buffer"));
        }
        if be::u32(b, 0) != JBD2_MAGIC {
            return Err(corrupt(
                "journal superblock",
                format!("bad magic {:#010x}", be::u32(b, 0)),
            ));
        }
        let blocktype = be::u32(b, 4);
        if blocktype != 3 && blocktype != 4 {
            return Err(corrupt(
                "journal superblock",
                format!("blocktype {blocktype}"),
            ));
        }
        Ok(JournalSuperblock {
            blocktype,
            blocksize: be::u32(b, 12),
            maxlen: be::u32(b, 16),
            first: be::u32(b, 20),
            sequence: be::u32(b, 24),
            start: be::u32(b, 28),
            errno: be::u32(b, 32),
            feature_compat: be::u32(b, 36),
            feature_incompat: be::u32(b, 40),
            feature_ro_compat: be::u32(b, 44),
            uuid: b[48..64].try_into().unwrap(),
            nr_users: be::u32(b, 64),
            dynsuper: be::u32(b, 68),
            max_transaction: be::u32(b, 72),
            max_trans_data: be::u32(b, 76),
            checksum_type: b[80],
            num_fc_blocks: be::u32(b, 84),
            head: be::u32(b, 88),
            checksum: be::u32(b, 0xFC),
        })
    }

    /// Encode into `out` (≥ 1024 bytes); unmodeled regions zeroed.
    pub fn encode(&self, out: &mut [u8]) {
        let b = &mut out[..Self::LEN];
        b.fill(0);
        be::put_u32(b, 0, JBD2_MAGIC);
        be::put_u32(b, 4, self.blocktype);
        // h_sequence (offset 8) is always 0 in a superblock.
        be::put_u32(b, 12, self.blocksize);
        be::put_u32(b, 16, self.maxlen);
        be::put_u32(b, 20, self.first);
        be::put_u32(b, 24, self.sequence);
        be::put_u32(b, 28, self.start);
        be::put_u32(b, 32, self.errno);
        be::put_u32(b, 36, self.feature_compat);
        be::put_u32(b, 40, self.feature_incompat);
        be::put_u32(b, 44, self.feature_ro_compat);
        b[48..64].copy_from_slice(&self.uuid);
        be::put_u32(b, 64, self.nr_users);
        be::put_u32(b, 68, self.dynsuper);
        be::put_u32(b, 72, self.max_transaction);
        be::put_u32(b, 76, self.max_trans_data);
        b[80] = self.checksum_type;
        be::put_u32(b, 84, self.num_fc_blocks);
        be::put_u32(b, 88, self.head);
        be::put_u32(b, 0xFC, self.checksum);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_fixture_jsb() {
        let raw = std::fs::read(format!(
            "{}/testdata/vectors/jsb.bin",
            env!("CARGO_MANIFEST_DIR")
        ))
        .unwrap();
        let jsb = JournalSuperblock::decode(&raw).unwrap();
        assert_eq!(jsb.blocktype, 4, "v2 superblock");
        assert_eq!(jsb.blocksize, 4096);
        assert_eq!(jsb.first, 1);
        assert_eq!(jsb.sequence, 1);
        assert_eq!(jsb.start, 0);
        assert_eq!(jsb.nr_users, 1);
        assert_eq!(
            (
                jsb.feature_compat,
                jsb.feature_incompat,
                jsb.feature_ro_compat
            ),
            (0, 0, 0),
            "empty journal is feature-less"
        );
        assert_eq!(jsb.checksum_type, 0);
        let mut out = vec![0u8; JournalSuperblock::LEN];
        jsb.encode(&mut out);
        assert_eq!(out, raw, "jsb does not round-trip byte-exactly");
    }
}
