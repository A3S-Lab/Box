//! xattr rendering: canonical sort, in-inode area first
//! with a single split point, external block with entry/block hashes and
//! the location-bound checksum.

use crate::spec::{BLOCK_SIZE, INODE_SIZE, XATTR_MAGIC};
use crate::{csum, Error, Result};

/// (name_index, name, value)
pub(crate) type Attr = (u8, Vec<u8>, Vec<u8>);

/// In-inode xattr area size for our geometry (256-byte inodes,
/// extra_isize 32): bytes 0xA0..0x100.
const IBODY_AREA: usize = INODE_SIZE - 0x80 - 32;

fn entry_bytes(a: &Attr) -> usize {
    16 + a.1.len().next_multiple_of(4)
}

fn value_bytes(a: &Attr) -> usize {
    a.2.len().next_multiple_of(4)
}

/// Split sorted attrs at the single deterministic point: everything that
/// fits in the in-inode area stays there; the first that doesn't (and
/// all after it) go to the block.
pub(crate) struct XattrPlan {
    /// Rendered in-inode area (empty when no attrs fit), sized exactly
    /// [`IBODY_AREA`].
    pub(crate) ibody: Vec<u8>,
    /// Attrs destined for the external block (rendered later, once the
    /// block address is known).
    pub(crate) block_attrs: Vec<Attr>,
}

pub(crate) fn plan(mut attrs: Vec<Attr>) -> Result<XattrPlan> {
    // Canonical order: the on-disk block format requires
    // (name_index, name_len, name); we use it everywhere.
    attrs.sort_by(|a, b| (a.0, a.1.len(), &a.1).cmp(&(b.0, b.1.len(), &b.1)));
    for a in &attrs {
        if a.2.len() > BLOCK_SIZE - 32 - 4 - entry_bytes(a) {
            return Err(Error::Invalid(format!(
                "xattr value of {} bytes cannot fit an xattr block",
                a.2.len()
            )));
        }
    }

    // Greedy fill of the in-inode area: 4-byte magic + entries + 4-byte
    // terminator growing up, values growing down.
    let usable = IBODY_AREA - 4; // after the magic
    let mut split = 0usize;
    let (mut ent, mut val) = (0usize, 0usize);
    for a in &attrs {
        let (e, v) = (entry_bytes(a), value_bytes(a));
        if ent + e + 4 + val + v > usable {
            break;
        }
        ent += e;
        val += v;
        split += 1;
    }
    let block_attrs = attrs.split_off(split);

    let mut total_block = 32 + 4;
    for a in &block_attrs {
        total_block += entry_bytes(a) + value_bytes(a);
    }
    if total_block > BLOCK_SIZE {
        return Err(Error::Invalid(
            "xattrs exceed the in-inode area plus one block".into(),
        ));
    }

    let ibody = if attrs.is_empty() {
        Vec::new()
    } else {
        let mut area = vec![0u8; IBODY_AREA];
        crate::le::put_u32(&mut area, 0, XATTR_MAGIC);
        // Offsets are relative to IFIRST (after the magic).
        render_entries(&mut area[4..], &attrs, usable, 0);
        area
    };
    Ok(XattrPlan { ibody, block_attrs })
}

/// Render the external block: header, entries, values, hashes, checksum.
pub(crate) fn render_block(attrs: &[Attr], fs_seed: u32, block_nr: u64) -> Vec<u8> {
    let mut buf = vec![0u8; BLOCK_SIZE];
    crate::le::put_u32(&mut buf, 0, XATTR_MAGIC);
    crate::le::put_u32(&mut buf, 4, 1); // h_refcount
    crate::le::put_u32(&mut buf, 8, 1); // h_blocks
    let entry_hashes = render_entries(&mut buf[0x20..], attrs, BLOCK_SIZE - 0x20, 0x20);
    let h_hash = csum::xattr_block_hash(entry_hashes);
    crate::le::put_u32(&mut buf, 0x0C, h_hash);
    let c = csum::xattr_block(fs_seed, block_nr, &buf);
    crate::le::put_u32(&mut buf, 0x10, c);
    buf
}

/// Write entries at the start of `region` and values downward from its
/// end. Stored `e_value_offs` = `offs_base` + offset-within-region: pass
/// 0 for the in-inode area (offsets are IFIRST-relative there) and 0x20
/// for a block (offsets are block-relative, region starts after the
/// header). `offs_base != 0` also selects block semantics: real e_hash
/// values (in-inode entries store 0). Returns the per-entry hashes.
fn render_entries(region: &mut [u8], attrs: &[Attr], usable: usize, offs_base: usize) -> Vec<u32> {
    let is_block = offs_base != 0;
    let mut ent = 0usize;
    let mut val_cursor = usable;
    let mut hashes = Vec::with_capacity(attrs.len());
    for (idx, name, value) in attrs {
        val_cursor -= value.len().next_multiple_of(4);
        region[val_cursor..val_cursor + value.len()].copy_from_slice(value);
        let e_hash = if is_block {
            csum::xattr_entry_hash(name, value)
        } else {
            0
        };
        region[ent] = name.len() as u8;
        region[ent + 1] = *idx;
        crate::le::put_u16(region, ent + 2, (offs_base + val_cursor) as u16);
        // e_value_inum stays 0.
        crate::le::put_u32(region, ent + 8, value.len() as u32);
        crate::le::put_u32(region, ent + 12, e_hash);
        region[ent + 16..ent + 16 + name.len()].copy_from_slice(name);
        ent += 16 + name.len().next_multiple_of(4);
        hashes.push(e_hash);
    }
    hashes
}

#[cfg(test)]
mod tests {
    use super::*;

    fn a(idx: u8, name: &str, value: &[u8]) -> Attr {
        (idx, name.as_bytes().to_vec(), value.to_vec())
    }

    #[test]
    fn small_attrs_stay_in_inode() {
        let p = plan(vec![a(1, "small", b"v"), a(6, "selinux", b"ctx")]).unwrap();
        assert!(p.block_attrs.is_empty());
        assert_eq!(p.ibody.len(), IBODY_AREA);
        let entries =
            crate::spec::xattr::ibody_entries(&[&[0u8; 0xA0][..], &p.ibody].concat(), 32).unwrap();
        assert_eq!(entries.len(), 2);
        // Canonical order: user.small (idx 1) before security.selinux (6).
        assert_eq!(entries[0].name, b"small");
        assert_eq!(entries[0].value, b"v");
        assert_eq!(entries[1].value, b"ctx");
        assert!(entries.iter().all(|e| e.hash == 0));
    }

    #[test]
    fn big_value_spills_to_block_with_hashes() {
        let big = vec![0x5A; 200];
        let p = plan(vec![a(1, "small", b"v"), a(1, "z_big", &big)]).unwrap();
        assert_eq!(p.block_attrs.len(), 1);
        let blk = render_block(&p.block_attrs, 0x1234_5678, 999);
        let view = crate::spec::XattrBlockView::parse(&blk).unwrap();
        assert_eq!(view.entries.len(), 1);
        assert_eq!(view.entries[0].value, big);
        assert_eq!(view.entries[0].hash, csum::xattr_entry_hash(b"z_big", &big));
        assert_eq!(view.checksum, csum::xattr_block(0x1234_5678, 999, &blk));
        assert_ne!(view.hash, 0);
    }

    #[test]
    fn sort_is_index_then_len_then_name() {
        let p = plan(vec![
            a(6, "bb", b""),
            a(1, "long-name", b""),
            a(1, "aa", b""),
            a(1, "ab", b""),
        ])
        .unwrap();
        assert!(p.block_attrs.is_empty(), "92 bytes fit exactly");
        let entries =
            crate::spec::xattr::ibody_entries(&[&[0u8; 0xA0][..], &p.ibody].concat(), 32).unwrap();
        let names: Vec<&[u8]> = entries.iter().map(|e| &e.name[..]).collect();
        assert_eq!(names, [b"aa" as &[u8], b"ab", b"long-name", b"bb"]);
    }

    #[test]
    fn oversize_total_rejected() {
        assert!(plan(vec![a(1, "x", &[0u8; 5000])]).is_err());
    }
}
