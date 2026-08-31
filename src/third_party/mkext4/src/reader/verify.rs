//! Structural verification: an fsck-lite that recomputes every checksum
//! type and validates the structures this crate implements differently
//! from e2fsprogs (htree hash placement, extent ordering, xattr order).
//!
//! This complements — never replaces — `e2fsck -fn` in the test gates:
//! e2fsck owns full accounting (pass 5); this pass owns "did we compute
//! every checksum and hash exactly right", which e2fsck can only answer
//! for structures it chooses to inspect.

use super::{Fs, ReadAt};
use crate::csum;
use crate::dirhash::{half_md4, Signedness};
use crate::spec::{self, bg_flags, iflags, ro_compat, DirentIter, DxNodeView, DxRootView, Inode};

/// One verification finding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Issue {
    /// Human-readable description, with enough context to locate the
    /// structure (group / inode / block).
    pub what: String,
}

macro_rules! issue {
    ($issues:expr, $($arg:tt)*) => {
        $issues.push(Issue { what: format!($($arg)*) })
    };
}

pub(super) fn verify<R: ReadAt>(fs: &Fs<R>) -> crate::Result<Vec<Issue>> {
    let mut issues = Vec::new();
    let sb = fs.superblock();
    let has_csum = sb.feature_ro_compat & ro_compat::METADATA_CSUM != 0;

    verify_groups(fs, has_csum, &mut issues)?;

    // Reserved inodes must be marked used.
    for ino in 1..=10u32.min(sb.inodes_count) {
        if !fs.inode_in_use(ino)? {
            issue!(issues, "reserved inode {ino} not marked in use");
        }
    }

    // Walk every in-use inode.
    let ipg = sb.inodes_per_group;
    for g in 0..sb.group_count() {
        let desc = fs.group_desc(g)?;
        if desc.flags & bg_flags::INODE_UNINIT != 0 {
            continue;
        }
        let bitmap = fs.read_block(desc.inode_bitmap)?;
        for idx in 0..ipg as usize {
            if bitmap[idx / 8] & (1 << (idx % 8)) == 0 {
                continue;
            }
            let ino = g as u32 * ipg + idx as u32 + 1;
            verify_inode(fs, ino, has_csum, &mut issues)?;
        }
    }

    verify_journal(fs, &mut issues)?;
    Ok(issues)
}

fn verify_groups<R: ReadAt>(
    fs: &Fs<R>,
    has_csum: bool,
    issues: &mut Vec<Issue>,
) -> crate::Result<()> {
    let sb = fs.superblock();
    let seed = fs.fs_seed();
    for g in 0..sb.group_count() {
        let raw = fs.group_desc_raw(g)?;
        let desc = fs.group_desc(g)?;
        if !has_csum {
            continue;
        }
        let want = csum::group_desc(seed, g as u32, raw);
        if want != desc.checksum {
            issue!(
                issues,
                "group {g}: desc checksum {want:#06x} != stored {:#06x}",
                desc.checksum
            );
        }

        // Block bitmap.
        let bb = fs.read_block(desc.block_bitmap)?;
        if desc.flags & bg_flags::BLOCK_UNINIT != 0 {
            if desc.block_bitmap_csum != 0 {
                issue!(issues, "group {g}: BLOCK_UNINIT but bitmap csum nonzero");
            }
        } else {
            let want = u32::from(csum::block_bitmap(seed, &bb));
            if want != desc.block_bitmap_csum & 0xFFFF {
                issue!(issues, "group {g}: block bitmap csum mismatch");
            }
            // Truncated last group: padding bits must be 1.
            let first = u64::from(sb.first_data_block);
            let group_start = first + g * u64::from(sb.blocks_per_group);
            let blocks_in_group =
                (sb.blocks_count - group_start).min(u64::from(sb.blocks_per_group)) as usize;
            for b in blocks_in_group..bb.len() * 8 {
                if bb[b / 8] & (1 << (b % 8)) == 0 {
                    issue!(issues, "group {g}: block bitmap padding bit {b} not set");
                    break;
                }
            }
        }

        // Inode bitmap.
        let ib = fs.read_block(desc.inode_bitmap)?;
        if desc.flags & bg_flags::INODE_UNINIT != 0 {
            if desc.inode_bitmap_csum != 0 {
                issue!(issues, "group {g}: INODE_UNINIT but bitmap csum nonzero");
            }
        } else {
            let want = u32::from(csum::inode_bitmap(seed, &ib, sb.inodes_per_group));
            if want != desc.inode_bitmap_csum & 0xFFFF {
                issue!(issues, "group {g}: inode bitmap csum mismatch");
            }
            for b in sb.inodes_per_group as usize..ib.len() * 8 {
                if ib[b / 8] & (1 << (b % 8)) == 0 {
                    issue!(issues, "group {g}: inode bitmap padding bit {b} not set");
                    break;
                }
            }
        }
    }
    Ok(())
}

fn verify_inode<R: ReadAt>(
    fs: &Fs<R>,
    ino: u32,
    has_csum: bool,
    issues: &mut Vec<Issue>,
) -> crate::Result<()> {
    // fs.inode() verifies the inode checksum itself.
    let inode = match fs.inode(ino) {
        Ok(i) => i,
        Err(e) => {
            issue!(issues, "inode {ino}: {e}");
            return Ok(());
        }
    };
    // Reserved inodes other than root/journal hold no walkable content.
    if ino < spec::FIRST_INO && ino != spec::ROOT_INO && ino != spec::JOURNAL_INO {
        return Ok(());
    }
    if inode.links_count == 0 && inode.dtime != 0 {
        // A deleted-but-still-bitmapped inode; e2fsck owns that verdict.
        return Ok(());
    }

    let extents = match fs.extents(ino, &inode) {
        Ok(e) => e,
        Err(e) => {
            issue!(issues, "inode {ino}: extent walk: {e}");
            return Ok(());
        }
    };
    // Extents must be sorted, non-overlapping, in-range.
    for w in extents.windows(2) {
        if u64::from(w[0].logical) + u64::from(w[0].len) > u64::from(w[1].logical) {
            issue!(issues, "inode {ino}: extents overlap or unsorted");
            break;
        }
    }
    let sb = fs.superblock();
    for e in &extents {
        if e.physical + u64::from(e.len) > sb.blocks_count {
            issue!(issues, "inode {ino}: extent past end of fs");
        }
    }

    match inode.file_type() {
        spec::inode::FileType::Dir => verify_dir(fs, ino, &inode, has_csum, issues)?,
        spec::inode::FileType::Symlink
            if inode.fast_symlink_target().is_none()
                && (inode.size == 0 || inode.size >= fs.block_size) =>
        {
            issue!(issues, "inode {ino}: symlink target size {}", inode.size);
        }
        _ => {}
    }

    if inode.file_acl != 0 {
        verify_xattr_block(fs, ino, &inode, has_csum, issues)?;
    }
    Ok(())
}

fn verify_dir<R: ReadAt>(
    fs: &Fs<R>,
    ino: u32,
    inode: &Inode,
    has_csum: bool,
    issues: &mut Vec<Issue>,
) -> crate::Result<()> {
    let extents = fs.extents(ino, inode)?;
    let seed = csum::inode_seed(fs.fs_seed(), ino, inode.generation);
    let total_blocks = inode.size / fs.block_size;
    let is_htree = inode.flags & iflags::INDEX != 0;

    if !is_htree {
        for logical in 0..total_blocks {
            let Some(block) = fs.file_block(&extents, logical) else {
                issue!(issues, "inode {ino}: directory hole at block {logical}");
                continue;
            };
            let data = fs.read_block(block)?;
            verify_dirent_block(&data, ino, logical, seed, has_csum, issues);
        }
        return Ok(());
    }

    // htree: parse the root, walk index levels, then check every leaf's
    // names hash into the dx range that points at it.
    let root_block = fs.read_block(fs.file_block(&extents, 0).unwrap_or(0))?;
    let root = match DxRootView::parse(&root_block) {
        Ok(r) => r,
        Err(e) => {
            issue!(issues, "inode {ino}: dx_root: {e}");
            return Ok(());
        }
    };
    if has_csum {
        let want = csum::dx_block(
            seed,
            &root_block,
            spec::htree::ROOT_COUNT_OFFSET,
            root.count,
            root.limit,
        );
        if want != root.checksum {
            issue!(issues, "inode {ino}: dx_root checksum mismatch");
        }
    }
    if root.info.hash_version != 1 {
        issue!(
            issues,
            "inode {ino}: unsupported hash_version {}",
            root.info.hash_version
        );
        return Ok(());
    }
    let sb = fs.superblock();
    let signedness = if sb.flags & 0x2 != 0 {
        Signedness::Unsigned
    } else {
        Signedness::Signed
    };

    // Collect (leaf_block, lo_hash, hi_bound) — the collision bit is
    // already folded into the exclusive upper bound.
    let mut leaves: Vec<(u32, u32, u64)> = Vec::new();
    let ranges = |entries: &[spec::DxEntry]| -> Vec<(u32, u32, u64)> {
        let mut v = Vec::with_capacity(entries.len());
        for (i, e) in entries.iter().enumerate() {
            // Upper bound: next entry's hash. A set collision bit means
            // names hashing exactly to (hash & !1) may continue here.
            let hi = entries
                .get(i + 1)
                .map(|n| {
                    let base = u64::from(n.hash & !1);
                    if n.hash & 1 != 0 {
                        base + 1 // inclusive of base
                    } else {
                        base
                    }
                })
                .unwrap_or(1 << 32);
            v.push((e.block, e.hash & !1, hi));
        }
        v
    };

    if root.info.indirect_levels == 0 {
        leaves = ranges(&root.entries);
    } else {
        for (node_block, node_lo, node_hi) in ranges(&root.entries) {
            let Some(phys) = fs.file_block(&extents, u64::from(node_block)) else {
                issue!(issues, "inode {ino}: dx_node block {node_block} unmapped");
                continue;
            };
            let data = fs.read_block(phys)?;
            let node = match DxNodeView::parse(&data) {
                Ok(n) => n,
                Err(e) => {
                    issue!(issues, "inode {ino}: dx_node {node_block}: {e}");
                    continue;
                }
            };
            if has_csum {
                let want = csum::dx_block(
                    seed,
                    &data,
                    spec::htree::NODE_COUNT_OFFSET,
                    node.count,
                    node.limit,
                );
                if want != node.checksum {
                    issue!(
                        issues,
                        "inode {ino}: dx_node {node_block} checksum mismatch"
                    );
                }
            }
            let mut node_leaves = ranges(&node.entries);
            // Entry 0's hash is implicit: this node's lower bound comes
            // from the PARENT's entry, not the stored (aliased) slot 0.
            // Likewise the last leaf's upper bound is the parent's.
            if let Some(first) = node_leaves.first_mut() {
                first.1 = node_lo;
            }
            match node_leaves.last_mut() {
                Some(last) if last.2 > node_hi => last.2 = node_hi,
                _ => {}
            }
            // Every leaf range must nest within the parent range.
            for &(_, lo, hi) in &node_leaves {
                if u64::from(lo) < u64::from(node_lo) || hi > node_hi {
                    issue!(
                        issues,
                        "inode {ino}: dx_node {node_block} range escapes parent"
                    );
                    break;
                }
            }
            leaves.extend(node_leaves);
        }
    }

    let hash_seed = sb.hash_seed;
    let mut leaf_seen = std::collections::BTreeSet::new();
    for (leaf_block, lo, hi) in leaves {
        if !leaf_seen.insert(leaf_block) {
            issue!(
                issues,
                "inode {ino}: leaf block {leaf_block} referenced twice"
            );
            continue;
        }
        let Some(phys) = fs.file_block(&extents, u64::from(leaf_block)) else {
            issue!(issues, "inode {ino}: leaf block {leaf_block} unmapped");
            continue;
        };
        let data = fs.read_block(phys)?;
        verify_dirent_block(&data, ino, u64::from(leaf_block), seed, has_csum, issues);
        for e in DirentIter::new(&data) {
            let Ok(e) = e else {
                issue!(issues, "inode {ino}: leaf {leaf_block}: bad dirent chain");
                break;
            };
            if e.name == b"." || e.name == b".." {
                continue;
            }
            let (h, _) = half_md4(&hash_seed, signedness, e.name);
            if u64::from(h) < u64::from(lo) || u64::from(h) >= hi {
                issue!(
                    issues,
                    "inode {ino}: name {:?} hash {h:#010x} outside leaf {leaf_block} range [{lo:#x}, {hi:#x})",
                    String::from_utf8_lossy(e.name)
                );
            }
        }
    }
    Ok(())
}

fn verify_dirent_block(
    data: &[u8],
    ino: u32,
    logical: u64,
    seed: u32,
    has_csum: bool,
    issues: &mut Vec<Issue>,
) {
    // Structure: the record chain must tile the block (DirentIter errors
    // otherwise).
    for e in DirentIter::new(data) {
        if let Err(e) = e {
            issue!(issues, "inode {ino}: dir block {logical}: {e}");
            return;
        }
    }
    if has_csum {
        match spec::dirent::tail_checksum(data) {
            Some(stored) => {
                let want = csum::dirent_block(seed, data);
                if want != stored {
                    issue!(issues, "inode {ino}: dir block {logical} checksum mismatch");
                }
            }
            None => issue!(issues, "inode {ino}: dir block {logical} missing csum tail"),
        }
    }
}

fn verify_xattr_block<R: ReadAt>(
    fs: &Fs<R>,
    ino: u32,
    inode: &Inode,
    has_csum: bool,
    issues: &mut Vec<Issue>,
) -> crate::Result<()> {
    let data = fs.read_block(inode.file_acl)?;
    let view = match spec::XattrBlockView::parse(&data) {
        Ok(v) => v,
        Err(e) => {
            issue!(issues, "inode {ino}: xattr block: {e}");
            return Ok(());
        }
    };
    if has_csum {
        let want = csum::xattr_block(fs.fs_seed(), inode.file_acl, &data);
        if want != view.checksum {
            issue!(issues, "inode {ino}: xattr block checksum mismatch");
        }
    }
    // Entries must be sorted by (name_index, name_len, name).
    for w in view.entries.windows(2) {
        let a = (w[0].name_index, w[0].name.len(), &w[0].name);
        let b = (w[1].name_index, w[1].name.len(), &w[1].name);
        if a > b {
            issue!(issues, "inode {ino}: xattr block entries unsorted");
            break;
        }
    }
    // Entry hashes, when set, must match the legacy formula.
    for e in &view.entries {
        if e.hash != 0 && e.hash != csum::xattr_entry_hash(&e.name, &e.value) {
            issue!(
                issues,
                "inode {ino}: xattr {:?} e_hash mismatch",
                e.full_name()
            );
        }
    }
    if view.hash != 0 {
        let want = csum::xattr_block_hash(view.entries.iter().map(|e| e.hash));
        if want != view.hash {
            issue!(issues, "inode {ino}: xattr h_hash mismatch");
        }
    }
    Ok(())
}

fn verify_journal<R: ReadAt>(fs: &Fs<R>, issues: &mut Vec<Issue>) -> crate::Result<()> {
    let sb = fs.superblock();
    if sb.feature_compat & spec::compat::HAS_JOURNAL == 0 || sb.journal_inum == 0 {
        return Ok(());
    }
    let ino = sb.journal_inum;
    let inode = fs.inode(ino)?;
    let extents = fs.extents(ino, &inode)?;
    let Some(first) = fs.file_block(&extents, 0) else {
        issue!(issues, "journal inode has no block 0");
        return Ok(());
    };
    let block = fs.read_block(first)?;
    match spec::JournalSuperblock::decode(&block) {
        Ok(jsb) => {
            if u64::from(jsb.blocksize) != fs.block_size {
                issue!(
                    issues,
                    "journal blocksize {} != fs block size",
                    jsb.blocksize
                );
            }
            if u64::from(jsb.maxlen) * fs.block_size != inode.size {
                issue!(
                    issues,
                    "journal maxlen {} inconsistent with i_size",
                    jsb.maxlen
                );
            }
        }
        Err(e) => issue!(issues, "journal superblock: {e}"),
    }
    Ok(())
}
