//! Deterministic bulk htree construction .
//!
//! Entries are hashed, sorted by `(hash, minor, declaration index)`, and
//! packed into 100%-full leaves; dx levels are built bottom-up with the
//! collision-continuation bit set when a hash run straddles leaves.
//! Logical block layout: 0 = dx_root, 1..=L = leaves, L+1.. = dx_nodes.

use crate::dirhash::{half_md4, HashSeed, Signedness};
use crate::spec::{dirent, htree, BLOCK_SIZE};
use crate::{csum, Error, Result};

/// One directory entry to be placed: (name, inode, file_type).
pub(crate) type Entry = (Vec<u8>, u32, u8);

/// The threshold at which e2fsck's rehash builds an index instead of a
/// linear directory: total entry record bytes (excluding "." and "..")
/// at or above `blocksize - 24`.
pub(crate) fn needs_htree(entries: &[Entry]) -> bool {
    let bytes: usize = entries
        .iter()
        .map(|(name, _, _)| dirent::rec_len_for(name.len()))
        .sum();
    bytes >= BLOCK_SIZE - 24
}

/// Rendered htree directory: `blocks[k]` is logical block k (dx_root,
/// then leaves, then dx_nodes), each fully checksummed.
pub(crate) struct HtreeDir {
    pub(crate) blocks: Vec<Vec<u8>>,
}

pub(crate) fn build(
    self_ino: u32,
    parent_ino: u32,
    entries: &[Entry],
    seed: &HashSeed,
    fs_seed: u32,
) -> Result<HtreeDir> {
    let inode_seed = csum::inode_seed(fs_seed, self_ino, 0);

    // Hash + sort. Declaration index is the tiebreaker for identical
    // (hash, minor) pairs, keeping the output deterministic.
    let mut hashed: Vec<(u32, u32, usize)> = entries
        .iter()
        .enumerate()
        .map(|(i, (name, _, _))| {
            let (h, minor) = half_md4(seed, Signedness::Signed, name);
            (h, minor, i)
        })
        .collect();
    hashed.sort_unstable();

    // Pack leaves at 100% fill.
    let usable = BLOCK_SIZE - 12;
    let mut leaves: Vec<Vec<usize>> = vec![Vec::new()];
    let mut used = 0usize;
    for &(_, _, i) in &hashed {
        let rl = dirent::rec_len_for(entries[i].0.len());
        if used + rl > usable {
            leaves.push(Vec::new());
            used = 0;
        }
        leaves.last_mut().unwrap().push(i);
        used += rl;
    }

    // dx entries for the leaf level: (hash-with-collision-bit, logical).
    let mut leaf_dx: Vec<(u32, u32)> = Vec::with_capacity(leaves.len());
    let mut prev_last_hash: Option<u32> = None;
    let mut base = 0usize; // index into `hashed` of the leaf's first entry
    for (k, leaf) in leaves.iter().enumerate() {
        let first_hash = hashed[base].0;
        let hash = match prev_last_hash {
            Some(p) if k > 0 && p == first_hash => first_hash | 1,
            _ => first_hash,
        };
        leaf_dx.push((hash, (k + 1) as u32));
        prev_last_hash = Some(hashed[base + leaf.len() - 1].0);
        base += leaf.len();
    }

    // Level structure.
    let root_limit = usize::from(htree::root_limit(BLOCK_SIZE));
    let node_limit = usize::from(htree::node_limit(BLOCK_SIZE));
    let (indirect_levels, node_groups): (u8, Vec<&[(u32, u32)]>) = if leaf_dx.len() <= root_limit {
        (0, vec![&leaf_dx[..]])
    } else {
        let groups: Vec<&[(u32, u32)]> = leaf_dx.chunks(node_limit).collect();
        if groups.len() > root_limit {
            return Err(Error::Unsupported(
                "directory needs more than a 2-level htree (largedir)".into(),
            ));
        }
        (1, groups)
    };

    let mut blocks: Vec<Vec<u8>> = Vec::with_capacity(1 + leaves.len() + node_groups.len());
    blocks.push(Vec::new()); // dx_root placeholder

    // Leaves: dirent blocks in sorted order, entries within a leaf in
    // (hash, minor, decl) order.
    let mut cursor = 0usize;
    for leaf in &leaves {
        let mut buf = vec![0u8; BLOCK_SIZE];
        let mut off = 0usize;
        for (j, &i) in leaf.iter().enumerate() {
            let (name, ino, ft) = &entries[i];
            let last = j == leaf.len() - 1;
            let rl = dirent::rec_len_for(name.len());
            let rec_len = if last { usable - off } else { rl };
            crate::le::put_u32(&mut buf, off, *ino);
            crate::le::put_u16(&mut buf, off + 4, rec_len as u16);
            buf[off + 6] = name.len() as u8;
            buf[off + 7] = *ft;
            buf[off + 8..off + 8 + name.len()].copy_from_slice(name);
            off += rec_len;
        }
        crate::le::put_u16(&mut buf, BLOCK_SIZE - 12 + 4, 12);
        buf[BLOCK_SIZE - 12 + 7] = 0xDE;
        let c = csum::dirent_block(inode_seed, &buf);
        crate::le::put_u32(&mut buf, BLOCK_SIZE - 4, c);
        blocks.push(buf);
        cursor += leaf.len();
    }
    debug_assert_eq!(cursor, hashed.len());

    // Interior nodes (level 1), when present.
    let first_node_logical = (1 + leaves.len()) as u32;
    let mut root_dx: Vec<(u32, u32)> = Vec::with_capacity(node_groups.len());
    if indirect_levels == 1 {
        for (gi, group) in node_groups.iter().enumerate() {
            let mut buf = vec![0u8; BLOCK_SIZE];
            // Fake dirent spanning the block.
            crate::le::put_u16(&mut buf, 4, BLOCK_SIZE as u16);
            write_countlimit_entries(&mut buf, htree::NODE_COUNT_OFFSET, node_limit, group);
            let c = csum::dx_block(
                inode_seed,
                &buf,
                htree::NODE_COUNT_OFFSET,
                group.len() as u16,
                node_limit as u16,
            );
            let tail = htree::NODE_COUNT_OFFSET + node_limit * 8;
            crate::le::put_u32(&mut buf, tail + 4, c);
            blocks.push(buf);
            // The root's entry for this node keeps the node's first
            // hash, including its collision bit.
            root_dx.push((group[0].0, first_node_logical + gi as u32));
        }
    } else {
        root_dx = leaf_dx.clone();
    }

    // dx_root.
    let mut root = vec![0u8; BLOCK_SIZE];
    crate::le::put_u32(&mut root, 0, self_ino);
    crate::le::put_u16(&mut root, 4, 12);
    root[6] = 1;
    root[7] = crate::spec::file_type::DIR;
    root[8] = b'.';
    crate::le::put_u32(&mut root, 0x0C, parent_ino);
    crate::le::put_u16(&mut root, 0x10, (BLOCK_SIZE - 12) as u16);
    root[0x12] = 2;
    root[0x13] = crate::spec::file_type::DIR;
    root[0x14] = b'.';
    root[0x15] = b'.';
    // dx_root_info at 0x18: reserved_zero, hash_version 1 (half_md4),
    // info_length 8, indirect_levels, unused_flags.
    root[0x1C] = 1;
    root[0x1D] = 8;
    root[0x1E] = indirect_levels;
    write_countlimit_entries(&mut root, htree::ROOT_COUNT_OFFSET, root_limit, &root_dx);
    let c = csum::dx_block(
        inode_seed,
        &root,
        htree::ROOT_COUNT_OFFSET,
        root_dx.len() as u16,
        root_limit as u16,
    );
    let tail = htree::ROOT_COUNT_OFFSET + root_limit * 8;
    crate::le::put_u32(&mut root, tail + 4, c);
    blocks[0] = root;

    Ok(HtreeDir { blocks })
}

/// Write `{limit, count, block0}` + the remaining entries. `dx[0]`'s
/// hash is implicit (slot 0 aliases the countlimit), so it must be the
/// level's lower bound; its collision bit, if any, lives in the PARENT's
/// entry for this block.
fn write_countlimit_entries(buf: &mut [u8], count_offset: usize, limit: usize, dx: &[(u32, u32)]) {
    crate::le::put_u16(buf, count_offset, limit as u16);
    crate::le::put_u16(buf, count_offset + 2, dx.len() as u16);
    crate::le::put_u32(buf, count_offset + 4, dx[0].1);
    for (i, &(hash, block)) in dx.iter().enumerate().skip(1) {
        crate::le::put_u32(buf, count_offset + 8 * i, hash);
        crate::le::put_u32(buf, count_offset + 8 * i + 4, block);
    }
}
