//! `seal()`: freeze the namespace into a complete physical layout.
//!
//! Everything is decided here — inode numbers, every block address,
//! every metadata byte (rendered or renderable), and the offsets of all
//! future file data. The emission pass in `writer.rs` only walks the
//! result.

use super::{FsBuilder, InodeCount, InodeHandle, Meta, NodeKind};
use crate::layout::alloc::{Allocator, Run};
use crate::layout::Geometry;
use crate::spec::{
    self, bg_flags, dirent, file_type, iflags, Extent, ExtentHeader, ExtentIdx, GroupDesc, Inode,
    JournalSuperblock, Superblock, BLOCKS_PER_GROUP, BLOCK_SIZE, JOURNAL_INO, LINK_MAX, ROOT_INO,
};
use crate::{csum, Error, Result};

const LOST_FOUND_INO: u32 = 11;
const LOST_FOUND_BLOCKS: u64 = 4;
/// First inode number handed to namespace slots.
const FIRST_DYN_INO: u32 = 12;
/// Max blocks per initialized extent.
const MAX_EXTENT: u64 = 32768;

/// A pre-rendered metadata region or an on-demand itable region.
#[derive(Debug)]
pub(crate) enum SegSrc {
    /// Rendered bytes (multiple of 4096).
    Bytes(Vec<u8>),
    /// Inode table of a group: rendered block-by-block during emission.
    Itable { group: u32 },
}

#[derive(Debug)]
pub(crate) struct Segment {
    /// First block.
    pub(crate) block: u64,
    /// Length in blocks.
    pub(crate) len: u64,
    pub(crate) src: SegSrc,
}

/// The extent map of one inode: the flattened extents plus any rendered
/// tree blocks.
#[derive(Debug)]
pub(crate) struct ExtentPlan {
    /// (logical block, physical block, length).
    pub(crate) extents: Vec<(u32, u64, u32)>,
    /// The 60-byte i_block content (root node).
    pub(crate) root: [u8; 60],
    /// i_blocks contribution of tree blocks (512-byte units).
    pub(crate) tree_blocks: u64,
}

/// Phase-2 output: the frozen layout: every block address, every metadata byte, and the final offset of all future file data.
pub struct Layout {
    pub(crate) opts: super::Options,
    pub(crate) geo: Geometry,
    /// slot -> inode number (0 = dropped from layout).
    pub(crate) slot_ino: Vec<u32>,
    /// Data runs + declared size per slot (files only).
    pub(crate) file_runs: Vec<(Vec<Run>, u64)>,
    /// All metadata segments, sorted by block.
    pub(crate) segments: Vec<Segment>,
    /// All file-data runs, sorted by block (gap-skip list for emission).
    pub(crate) data_runs: Vec<Run>,
    /// Rendered 256-byte inodes, indexed by `ino - 1`. Inode numbering
    /// is dense (1..=max_ino, no gaps), so a flat array carries zero
    /// per-entry overhead and lets the inode table be emitted as
    /// contiguous copies. ~256 B per inode retained until the writer
    /// finishes.
    pub(crate) inodes: Vec<[u8; 256]>,
    /// Highest used inode number.
    pub(crate) max_ino: u32,
}

impl std::fmt::Debug for Layout {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Layout")
            .field("image_len", &self.image_len())
            .field("inodes", &self.max_ino)
            .field("segments", &self.segments.len())
            .finish_non_exhaustive()
    }
}

impl Layout {
    /// Total image length in bytes.
    pub fn image_len(&self) -> u64 {
        self.opts.size_bytes
    }

    /// The final (offset, length-in-bytes) data ranges of a declared
    /// file, in logical order.
    pub fn extents(&self, f: InodeHandle) -> Vec<(u64, u64)> {
        let Some((runs, size)) = self.file_runs.get(f.0 as usize) else {
            return Vec::new();
        };
        let mut remaining = *size;
        let mut out = Vec::with_capacity(runs.len());
        for r in runs {
            let bytes = (r.len * BLOCK_SIZE as u64).min(remaining);
            out.push((r.start * BLOCK_SIZE as u64, bytes));
            remaining -= bytes;
        }
        out
    }

    /// Phase 3: create the writer. All metadata and free-space zeros are
    /// emitted to `sink` before this returns.
    pub fn writer<S: crate::sink::RegionSink>(
        &self,
        sink: S,
    ) -> Result<super::writer::ImageWriter<'_, S>> {
        super::writer::ImageWriter::new(self, sink)
    }
}

/// The extent plan of a file with no mapped blocks: a valid header with
/// zero entries (the verified empty-file shape).
fn empty_extent_plan() -> ExtentPlan {
    let mut root = [0u8; 60];
    ExtentHeader {
        entries: 0,
        max: 4,
        depth: 0,
        generation: 0,
    }
    .encode(&mut root);
    ExtentPlan {
        extents: Vec::new(),
        root,
        tree_blocks: 0,
    }
}

/// Attach dense logical block numbers to a run list (non-sparse files).
fn with_logicals(runs: &[Run]) -> Vec<(u32, Run)> {
    let mut out = Vec::with_capacity(runs.len());
    let mut logical = 0u32;
    for r in runs {
        out.push((logical, *r));
        logical += r.len as u32;
    }
    out
}

/// Build the extent structures for (logical, physical-run) pairs.
/// Allocates tree blocks from `alloc` when more than 4 extents are
/// needed and pushes their rendered bytes as segments.
fn plan_extents(
    pairs: &[(u32, Run)],
    ino: u32,
    fs_seed: u32,
    alloc: &mut Allocator,
    segments: &mut Vec<Segment>,
) -> Result<ExtentPlan> {
    let mut extents: Vec<(u32, u64, u32)> = Vec::with_capacity(pairs.len());
    for &(logical, r) in pairs {
        debug_assert!(r.len <= MAX_EXTENT);
        extents.push((logical, r.start, r.len as u32));
    }
    let mut plan = ExtentPlan {
        extents,
        root: [0; 60],
        tree_blocks: 0,
    };

    let write_leaf_entries = |buf: &mut [u8], entries: &[(u32, u64, u32)]| {
        for (i, &(lo, phys, len)) in entries.iter().enumerate() {
            Extent {
                logical: lo,
                raw_len: len as u16,
                start: phys,
            }
            .encode(&mut buf[ExtentHeader::LEN + i * 12..]);
        }
    };

    let n = plan.extents.len();
    if n <= 4 {
        ExtentHeader {
            entries: n as u16,
            max: 4,
            depth: 0,
            generation: 0,
        }
        .encode(&mut plan.root);
        write_leaf_entries(&mut plan.root, &plan.extents);
        return Ok(plan);
    }

    // Depth 1 (or 2): leaves at 100% fill, then index nodes.
    let per_leaf = spec::extent::ExtentIdx::node_capacity(BLOCK_SIZE);
    let n_leaves = n.div_ceil(per_leaf);
    let seed = csum::inode_seed(fs_seed, ino, 0);
    let leaf_blocks = alloc
        .take(n_leaves as u64, 1)
        .ok_or_else(|| Error::Invalid("image full (extent tree)".into()))?;
    let mut level1: Vec<(u32, u64)> = Vec::with_capacity(n_leaves); // (first logical, block)
    for (li, chunk) in plan.extents.chunks(per_leaf).enumerate() {
        let block = leaf_blocks[li].start;
        let mut buf = vec![0u8; BLOCK_SIZE];
        ExtentHeader {
            entries: chunk.len() as u16,
            max: per_leaf as u16,
            depth: 0,
            generation: 0,
        }
        .encode(&mut buf);
        write_leaf_entries(&mut buf, chunk);
        let c = csum::extent_block(seed, &buf);
        crate::le::put_u32(&mut buf, BLOCK_SIZE - 4, c);
        level1.push((chunk[0].0, block));
        segments.push(Segment {
            block,
            len: 1,
            src: SegSrc::Bytes(buf),
        });
        plan.tree_blocks += 8;
    }

    let (depth, top): (u16, Vec<(u32, u64)>) = if level1.len() <= 4 {
        (1, level1)
    } else {
        // Depth 2: index nodes over the leaves.
        let n_idx = level1.len().div_ceil(per_leaf);
        if n_idx > 4 {
            return Err(Error::Unsupported(
                "extent tree deeper than 2 levels".into(),
            ));
        }
        let idx_blocks = alloc
            .take(n_idx as u64, 1)
            .ok_or_else(|| Error::Invalid("image full (extent tree)".into()))?;
        let mut level2 = Vec::with_capacity(n_idx);
        for (ii, chunk) in level1.chunks(per_leaf).enumerate() {
            let block = idx_blocks[ii].start;
            let mut buf = vec![0u8; BLOCK_SIZE];
            ExtentHeader {
                entries: chunk.len() as u16,
                max: per_leaf as u16,
                depth: 1,
                generation: 0,
            }
            .encode(&mut buf);
            for (i, &(lo, child)) in chunk.iter().enumerate() {
                ExtentIdx {
                    logical: lo,
                    leaf: child,
                }
                .encode(&mut buf[ExtentHeader::LEN + i * 12..]);
            }
            let c = csum::extent_block(seed, &buf);
            crate::le::put_u32(&mut buf, BLOCK_SIZE - 4, c);
            level2.push((chunk[0].0, block));
            segments.push(Segment {
                block,
                len: 1,
                src: SegSrc::Bytes(buf),
            });
            plan.tree_blocks += 8;
        }
        (2, level2)
    };

    ExtentHeader {
        entries: top.len() as u16,
        max: 4,
        depth,
        generation: 0,
    }
    .encode(&mut plan.root);
    for (i, &(lo, child)) in top.iter().enumerate() {
        ExtentIdx {
            logical: lo,
            leaf: child,
        }
        .encode(&mut plan.root[ExtentHeader::LEN + i * 12..]);
    }
    Ok(plan)
}

/// Pack dirents into blocks: records in declaration order, the last
/// record of each block stretched to the checksum tail, whose shape is
/// written here but whose checksum the caller fills in.
fn pack_dirents(entries: &[(Vec<u8>, u32, u8)]) -> Vec<Vec<u8>> {
    // entries: (name, ino, file_type); "." and ".." are synthesized by
    // the caller as the first two entries of the slice.
    let usable = BLOCK_SIZE - 12; // checksum tail
    let mut blocks: Vec<Vec<(usize, usize)>> = vec![Vec::new()]; // (entry idx, rec_len)
    let mut used = 0usize;
    for (i, (name, _, _)) in entries.iter().enumerate() {
        let rl = dirent::rec_len_for(name.len());
        if used + rl > usable {
            blocks.push(Vec::new());
            used = 0;
        }
        blocks.last_mut().unwrap().push((i, rl));
        used += rl;
    }
    let mut out = Vec::with_capacity(blocks.len());
    for blk in &blocks {
        let mut buf = vec![0u8; BLOCK_SIZE];
        let mut off = 0usize;
        for (j, &(i, rl)) in blk.iter().enumerate() {
            let (name, ino, ft) = &entries[i];
            let last = j == blk.len() - 1;
            let rec_len = if last { usable - off } else { rl };
            crate::le::put_u32(&mut buf, off, *ino);
            crate::le::put_u16(&mut buf, off + 4, rec_len as u16);
            buf[off + 6] = name.len() as u8;
            buf[off + 7] = *ft;
            buf[off + 8..off + 8 + name.len()].copy_from_slice(name);
            off += rec_len;
        }
        if blk.is_empty() {
            // Empty block: one unused dirent spanning to the tail.
            crate::le::put_u16(&mut buf, 4, usable as u16);
        }
        // Tail shape; checksum filled in later.
        crate::le::put_u16(&mut buf, BLOCK_SIZE - 12 + 4, 12);
        buf[BLOCK_SIZE - 12 + 7] = 0xDE;
        out.push(buf);
    }
    out
}

struct InodeRender {
    meta: Meta,
    ftype: u8,
    size: u64,
    blocks512: u64,
    nlink: u32,
    flags: u32,
    block: [u8; 60],
    /// xattr block address (0 = none). Adds 8 to i_blocks.
    file_acl: u64,
    /// Rendered in-inode xattr area (empty = none).
    ibody: Vec<u8>,
}

impl InodeRender {
    fn plain(meta: Meta, ftype: u8, nlink: u32) -> InodeRender {
        InodeRender {
            meta,
            ftype,
            size: 0,
            blocks512: 0,
            nlink,
            flags: 0,
            block: [0; 60],
            file_acl: 0,
            ibody: Vec::new(),
        }
    }
}

fn render_inode(fs_seed: u32, ino: u32, r: &InodeRender) -> [u8; 256] {
    let type_bits = match r.ftype {
        file_type::REG => 0o10,
        file_type::DIR => 0o04,
        file_type::SYMLINK => 0o12,
        file_type::CHR => 0o02,
        file_type::BLK => 0o06,
        file_type::FIFO => 0o01,
        file_type::SOCK => 0o14,
        _ => 0,
    } << 12;
    let (mtime, mtime_extra) = Inode::encode_timestamp(r.meta.mtime.0, r.meta.mtime.1);
    let at = r.meta.atime.unwrap_or(r.meta.mtime);
    let ct = r.meta.ctime.unwrap_or(r.meta.mtime);
    let crt = r.meta.crtime.unwrap_or(r.meta.mtime);
    let (atime, atime_extra) = Inode::encode_timestamp(at.0, at.1);
    let (ctime, ctime_extra) = Inode::encode_timestamp(ct.0, ct.1);
    let (crtime, crtime_extra) = Inode::encode_timestamp(crt.0, crt.1);
    let nlink = if r.ftype == file_type::DIR && r.nlink > LINK_MAX {
        1 // dir_nlink overflow convention
    } else {
        r.nlink
    };
    let mut inode = Inode {
        mode: type_bits | (r.meta.mode & 0o7777),
        uid: r.meta.uid,
        gid: r.meta.gid,
        size: r.size,
        atime,
        ctime,
        mtime,
        dtime: 0,
        links_count: nlink as u16,
        blocks: r.blocks512,
        flags: r.flags,
        version: 0,
        block: r.block,
        generation: 0,
        file_acl: r.file_acl,
        extra_isize: 32,
        checksum: 0,
        ctime_extra,
        mtime_extra,
        atime_extra,
        crtime,
        crtime_extra,
        projid: 0,
        ibody: r.ibody.clone(),
    };
    let mut buf = [0u8; 256];
    inode.encode(&mut buf);
    let c = csum::inode(fs_seed, ino, 0, &buf);
    inode.checksum = c;
    inode.encode(&mut buf);
    buf
}

/// Zero-body inode with a valid checksum (reserved inodes 3-7, 9, 10).
fn render_reserved_inode(fs_seed: u32, ino: u32, times: Option<i64>) -> [u8; 256] {
    let mut buf = [0u8; 256];
    if let Some(t) = times {
        // Inode 1 (bad blocks): atime/ctime/mtime = epoch, nothing else.
        let (secs, _) = Inode::encode_timestamp(t, 0);
        crate::le::put_u32(&mut buf, 0x08, secs);
        crate::le::put_u32(&mut buf, 0x0C, secs);
        crate::le::put_u32(&mut buf, 0x10, secs);
    }
    let c = csum::inode(fs_seed, ino, 0, &buf);
    crate::le::put_u16(&mut buf, 0x7C, c as u16);
    buf
}

pub(super) fn seal(b: FsBuilder) -> Result<Layout> {
    // --- inode numbering: reachable slots in declaration order ---------
    let mut reachable = vec![false; b.nodes.len()];
    mark_reachable(&b, 0, &mut reachable)?;
    let mut slot_ino = vec![0u32; b.nodes.len()];
    slot_ino[0] = ROOT_INO;
    let mut next = FIRST_DYN_INO;
    for (slot, node) in b.nodes.iter().enumerate().skip(1) {
        if reachable[slot] && node.nlink > 0 {
            slot_ino[slot] = next;
            next += 1;
        }
    }
    let max_ino = next - 1;
    let used_inodes = max_ino; // 1..=max_ino all used (11 = lost+found)

    // --- geometry -------------------------------------------------------
    let total_inodes = match b.opts.inodes {
        InodeCount::Auto => None,
        InodeCount::Exact(n) => Some(n),
    };
    let geo = Geometry::new(b.opts.size_bytes, total_inodes, b.opts.journal_blocks)?;
    if geo.inodes_count() < used_inodes {
        return Err(Error::Invalid(format!(
            "{used_inodes} inodes needed but geometry provides {}",
            geo.inodes_count()
        )));
    }
    let fs_seed = csum::fs_seed(&b.opts.fs_uuid);

    // --- fixed metadata placement ---------------------------------------
    // Reserved runs: primary sb+GDT, backups, then flex metadata.
    let gdt = u64::from(geo.gdt_blocks);
    let mut reserved = vec![Run {
        start: 0,
        len: 1 + gdt,
    }];
    for g in geo.backup_groups() {
        reserved.push(Run {
            start: geo.group_start(g),
            len: 1 + gdt,
        });
    }
    reserved.sort_by_key(|r| r.start);

    // Flex metadata: per span, a local skipping cursor from the leader.
    let mut group_meta: Vec<(u64, u64, u64)> = Vec::with_capacity(geo.groups as usize);
    let mut flex_runs: Vec<Run> = Vec::new();
    let itb = u64::from(geo.itable_blocks());
    for span_leader in (0..geo.groups).step_by(geo.groups_per_flex as usize) {
        let members: Vec<u32> =
            (span_leader..(span_leader + geo.groups_per_flex).min(geo.groups)).collect();
        let mut cur = Allocator::new(geo.group_start(span_leader), geo.blocks, reserved.clone());
        let mut bb = Vec::new();
        let mut ib = Vec::new();
        for _ in &members {
            bb.push(cur.take_one().ok_or_else(err_full)?);
        }
        for _ in &members {
            ib.push(cur.take_one().ok_or_else(err_full)?);
        }
        let mut it = Vec::new();
        for _ in &members {
            // The itable must be contiguous; take() splits only at
            // reserved runs, so require a single run.
            let runs = cur.take(itb, u64::MAX).ok_or_else(err_full)?;
            if runs.len() != 1 {
                // Jump past the interruption and retry once.
                let runs = cur.take(itb, u64::MAX).ok_or_else(err_full)?;
                if runs.len() != 1 {
                    return Err(Error::Invalid(
                        "cannot place a contiguous inode table".into(),
                    ));
                }
                it.push(runs[0].start);
            } else {
                it.push(runs[0].start);
            }
        }
        for (i, _) in members.iter().enumerate() {
            group_meta.push((bb[i], ib[i], it[i]));
        }
        let end = cur.cursor();
        flex_runs.push(Run {
            start: geo.group_start(span_leader).max(bb[0]),
            len: end - bb[0],
        });
    }
    // Merge flex runs into reserved for the global allocator.
    let mut all_reserved = reserved.clone();
    all_reserved.extend(flex_runs.iter().copied());
    all_reserved.sort_by_key(|r| r.start);

    let mut alloc = Allocator::new(0, geo.blocks, all_reserved);
    let mut segments: Vec<Segment> = Vec::new();

    // --- journal ----------------------------------------------------------
    let journal_runs = alloc
        .take(u64::from(geo.journal_blocks), MAX_EXTENT)
        .ok_or_else(err_full)?;
    let journal_plan = plan_extents(
        &with_logicals(&journal_runs),
        JOURNAL_INO,
        fs_seed,
        &mut alloc,
        &mut segments,
    )?;

    // --- namespace metadata: per inode, in inode order ----------------------
    // For each inode: xattr block, then directory / slow-symlink content.
    struct DirPlan {
        slot: u32,
        runs: Vec<Run>,
        plan: ExtentPlan,
        nblocks: u64,
        is_htree: bool,
    }
    let mut dir_blocks: Vec<DirPlan> = Vec::new();
    let mut dir_rendered: Vec<(u64, Vec<u8>)> = Vec::new(); // (block, bytes)
                                                            // Per slot: (i_file_acl block or 0, rendered ibody area).
    let mut xattr_out: Vec<(u64, Vec<u8>)> = vec![(0, Vec::new()); b.nodes.len()];
    // Per slot: slow-symlink extent plan.
    let mut symlink_plans: Vec<Option<ExtentPlan>> = (0..b.nodes.len()).map(|_| None).collect();

    // lost+found first (inode 11): block 0 dots, 3 empty blocks.
    let lf_runs = alloc
        .take(LOST_FOUND_BLOCKS, MAX_EXTENT)
        .ok_or_else(err_full)?;
    let lf_plan = plan_extents(
        &with_logicals(&lf_runs),
        LOST_FOUND_INO,
        fs_seed,
        &mut alloc,
        &mut segments,
    )?;
    {
        let entries = vec![
            (b".".to_vec(), LOST_FOUND_INO, file_type::DIR),
            (b"..".to_vec(), ROOT_INO, file_type::DIR),
        ];
        let mut blocks = pack_dirents(&entries);
        while blocks.len() < LOST_FOUND_BLOCKS as usize {
            blocks.extend(pack_dirents(&[])); // empty block
        }
        let seed = csum::inode_seed(fs_seed, LOST_FOUND_INO, 0);
        let mut phys = expand_runs(&lf_runs);
        for (buf, p) in blocks.iter_mut().zip(phys.drain(..)) {
            let c = csum::dirent_block(seed, buf);
            crate::le::put_u32(buf, BLOCK_SIZE - 4, c);
            dir_rendered.push((p, std::mem::take(buf)));
        }
    }

    let parents = parent_map(&b, &slot_ino);
    for slot in 0..b.nodes.len() as u32 {
        let ino = slot_ino[slot as usize];
        if ino == 0 {
            continue;
        }

        // xattr blocks precede the inode's content blocks on disk.
        if let Some(attrs) = b.xattrs.get(&slot) {
            let plan = super::xattr_build::plan(attrs.clone())?;
            let mut acl = 0u64;
            if !plan.block_attrs.is_empty() {
                let block = alloc.take_one().ok_or_else(err_full)?;
                let bytes = super::xattr_build::render_block(&plan.block_attrs, fs_seed, block);
                segments.push(Segment {
                    block,
                    len: 1,
                    src: SegSrc::Bytes(bytes),
                });
                acl = block;
            }
            xattr_out[slot as usize] = (acl, plan.ibody);
        }

        match &b.nodes[slot as usize].kind {
            NodeKind::Dir { children, .. } => {
                let parent_ino = if slot == 0 {
                    ROOT_INO
                } else {
                    parents[slot as usize]
                };
                let mut entries: Vec<(Vec<u8>, u32, u8)> = Vec::with_capacity(children.len() + 1);
                if slot == 0 {
                    entries.push((b"lost+found".to_vec(), LOST_FOUND_INO, file_type::DIR));
                }
                for &(nref, child, dead) in children {
                    if dead || slot_ino[child as usize] == 0 {
                        continue;
                    }
                    let ft = match b.nodes[child as usize].kind {
                        NodeKind::Dir { .. } => file_type::DIR,
                        NodeKind::File { .. } | NodeKind::Sparse { .. } => file_type::REG,
                        NodeKind::Symlink { .. } => file_type::SYMLINK,
                        NodeKind::Special { ftype, .. } => ftype,
                    };
                    entries.push((b.name(nref).to_vec(), slot_ino[child as usize], ft));
                }

                let is_htree = super::htree::needs_htree(&entries);
                let blocks: Vec<Vec<u8>> = if is_htree {
                    super::htree::build(ino, parent_ino, &entries, &b.opts.hash_seed, fs_seed)?
                        .blocks
                } else {
                    // Linear: "." and ".." lead, then declaration order.
                    let mut with_dots: Vec<(Vec<u8>, u32, u8)> =
                        Vec::with_capacity(entries.len() + 2);
                    with_dots.push((b".".to_vec(), ino, file_type::DIR));
                    with_dots.push((b"..".to_vec(), parent_ino, file_type::DIR));
                    with_dots.extend(entries);
                    let mut blocks = pack_dirents(&with_dots);
                    let seed = csum::inode_seed(fs_seed, ino, 0);
                    for buf in &mut blocks {
                        let c = csum::dirent_block(seed, buf);
                        crate::le::put_u32(buf, BLOCK_SIZE - 4, c);
                    }
                    blocks
                };

                let nblocks = blocks.len() as u64;
                let runs = alloc.take(nblocks, MAX_EXTENT).ok_or_else(err_full)?;
                let plan = plan_extents(
                    &with_logicals(&runs),
                    ino,
                    fs_seed,
                    &mut alloc,
                    &mut segments,
                )?;
                let mut phys = expand_runs(&runs);
                for (buf, p) in blocks.into_iter().zip(phys.drain(..)) {
                    dir_rendered.push((p, buf));
                }
                dir_blocks.push(DirPlan {
                    slot,
                    runs,
                    plan,
                    nblocks,
                    is_htree,
                });
            }
            NodeKind::Symlink { target } if target.len() > 59 => {
                let block = alloc.take_one().ok_or_else(err_full)?;
                let mut bytes = vec![0u8; BLOCK_SIZE];
                bytes[..target.len()].copy_from_slice(target);
                segments.push(Segment {
                    block,
                    len: 1,
                    src: SegSrc::Bytes(bytes),
                });
                let run = [(
                    0u32,
                    Run {
                        start: block,
                        len: 1,
                    },
                )];
                symlink_plans[slot as usize] =
                    Some(plan_extents(&run, ino, fs_seed, &mut alloc, &mut segments)?);
            }
            _ => {}
        }
    }

    // --- file data ----------------------------------------------------------
    let mut file_runs: Vec<(Vec<Run>, u64)> = vec![(Vec::new(), 0); b.nodes.len()];
    let mut file_plans: Vec<Option<(ExtentPlan, u64)>> = (0..b.nodes.len()).map(|_| None).collect();
    let mut data_runs: Vec<Run> = Vec::new();
    for (slot, node) in b.nodes.iter().enumerate() {
        let ino = slot_ino[slot];
        if ino == 0 {
            continue;
        }
        // (logical offset, byte length) data segments; dense for regular
        // files, gapped for sparse ones.
        let segs: Vec<(u64, u64)> = match &node.kind {
            NodeKind::File { size } if *size > 0 => vec![(0, *size)],
            NodeKind::Sparse { data_segs, .. } => data_segs.clone(),
            _ => continue,
        };
        let mut pairs: Vec<(u32, Run)> = Vec::new();
        let mut runs: Vec<Run> = Vec::new();
        let mut data_bytes = 0u64;
        for (offset, len) in &segs {
            let nblocks = len.div_ceil(BLOCK_SIZE as u64);
            let seg_runs = alloc.take(nblocks, MAX_EXTENT).ok_or_else(err_full)?;
            let mut logical = (offset / BLOCK_SIZE as u64) as u32;
            for r in &seg_runs {
                pairs.push((logical, *r));
                logical += r.len as u32;
            }
            runs.extend(seg_runs);
            data_bytes += len;
        }
        let alloc_blocks: u64 = runs.iter().map(|r| r.len).sum();
        let plan = plan_extents(&pairs, ino, fs_seed, &mut alloc, &mut segments)?;
        data_runs.extend(runs.iter().copied());
        file_runs[slot] = (runs, data_bytes);
        file_plans[slot] = Some((plan, alloc_blocks));
    }

    // --- block bitmap ----------------------------------------------------
    // One big bitvec; everything not explicitly used is free.
    let mut bitmap = vec![0u8; (geo.blocks as usize).div_ceil(8)];
    let mut used_blocks = 0u64;
    let mut mark = |start: u64, len: u64| {
        for blk in start..start + len {
            let (byte, bit) = ((blk / 8) as usize, blk % 8);
            debug_assert_eq!(bitmap[byte] & (1 << bit), 0, "double-alloc at {blk}");
            bitmap[byte] |= 1 << bit;
        }
        used_blocks += len;
    };
    mark(0, 1 + gdt);
    for g in geo.backup_groups() {
        mark(geo.group_start(g), 1 + gdt);
    }
    for &(bb, ib, it) in &group_meta {
        mark(bb, 1);
        mark(ib, 1);
        mark(it, itb);
    }
    for r in &journal_runs {
        mark(r.start, r.len);
    }
    for r in &lf_runs {
        mark(r.start, r.len);
    }
    for d in &dir_blocks {
        for r in &d.runs {
            mark(r.start, r.len);
        }
    }
    for r in &data_runs {
        mark(r.start, r.len);
    }
    for seg in &segments {
        // Extent tree blocks pushed by plan_extents.
        mark(seg.block, seg.len);
    }

    // --- inodes -----------------------------------------------------------
    // Dense by construction: slots for 1..=max_ino, filled below.
    let mut inodes = vec![[0u8; 256]; max_ino as usize];
    let set_ino = |inodes: &mut Vec<[u8; 256]>, ino: u32, raw: [u8; 256]| {
        inodes[ino as usize - 1] = raw;
    };
    set_ino(
        &mut inodes,
        1,
        render_reserved_inode(fs_seed, 1, Some(b.opts.epoch)),
    );
    for ino in [3u32, 4, 5, 6, 7, 9, 10] {
        set_ino(&mut inodes, ino, render_reserved_inode(fs_seed, ino, None));
    }
    let fs_meta = Meta::new(0, 0, 0, (b.opts.epoch, 0));
    set_ino(
        &mut inodes,
        JOURNAL_INO,
        render_inode(
            fs_seed,
            JOURNAL_INO,
            &InodeRender {
                meta: Meta {
                    mode: 0o600,
                    ..fs_meta
                },
                ftype: file_type::REG,
                size: u64::from(geo.journal_blocks) * BLOCK_SIZE as u64,
                blocks512: u64::from(geo.journal_blocks) * 8 + journal_plan.tree_blocks,
                nlink: 1,
                flags: iflags::EXTENTS,
                block: journal_plan.root,
                file_acl: 0,
                ibody: Vec::new(),
            },
        ),
    );
    set_ino(
        &mut inodes,
        LOST_FOUND_INO,
        render_inode(
            fs_seed,
            LOST_FOUND_INO,
            &InodeRender {
                meta: Meta {
                    mode: 0o700,
                    ..fs_meta
                },
                ftype: file_type::DIR,
                size: LOST_FOUND_BLOCKS * BLOCK_SIZE as u64,
                blocks512: LOST_FOUND_BLOCKS * 8 + lf_plan.tree_blocks,
                nlink: 2,
                flags: iflags::EXTENTS,
                block: lf_plan.root,
                file_acl: 0,
                ibody: Vec::new(),
            },
        ),
    );

    // Subdir counts for nlink.
    let mut subdirs = vec![0u32; b.nodes.len()];
    for (slot, node) in b.nodes.iter().enumerate() {
        if slot_ino[slot] == 0 {
            continue;
        }
        if let NodeKind::Dir { children, .. } = &node.kind {
            subdirs[slot] = children
                .iter()
                .filter(|&&(_, c, dead)| {
                    !dead
                        && slot_ino[c as usize] != 0
                        && matches!(b.nodes[c as usize].kind, NodeKind::Dir { .. })
                })
                .count() as u32;
        }
    }

    // Directories: htree dirs get INDEX_FL.
    for d in &dir_blocks {
        let slot = d.slot as usize;
        let ino = slot_ino[slot];
        let extra_root_link = if slot == 0 { 1 } else { 0 }; // lost+found's ".."
        let (file_acl, ibody) = std::mem::take(&mut xattr_out[slot]);
        let flags = iflags::EXTENTS | if d.is_htree { iflags::INDEX } else { 0 };
        set_ino(
            &mut inodes,
            ino,
            render_inode(
                fs_seed,
                ino,
                &InodeRender {
                    meta: b.nodes[slot].meta,
                    ftype: file_type::DIR,
                    size: d.nblocks * BLOCK_SIZE as u64,
                    blocks512: d.nblocks * 8
                        + d.plan.tree_blocks
                        + if file_acl != 0 { 8 } else { 0 },
                    nlink: 2 + subdirs[slot] + extra_root_link,
                    flags,
                    block: d.plan.root,
                    file_acl,
                    ibody,
                },
            ),
        );
    }
    // Files, sparse files, symlinks, specials.
    for (slot, node) in b.nodes.iter().enumerate() {
        let ino = slot_ino[slot];
        if ino == 0 {
            continue;
        }
        let (file_acl, ibody) = std::mem::take(&mut xattr_out[slot]);
        let acl_512 = if file_acl != 0 { 8 } else { 0 };
        let render = match &node.kind {
            NodeKind::Dir { .. } => continue, // handled above
            NodeKind::File { size } | NodeKind::Sparse { size, .. } => {
                let empty = (empty_extent_plan(), 0u64);
                let (plan, nblocks) = file_plans[slot].as_ref().unwrap_or(&empty);
                InodeRender {
                    size: *size,
                    blocks512: nblocks * 8 + plan.tree_blocks + acl_512,
                    flags: iflags::EXTENTS,
                    block: plan.root,
                    file_acl,
                    ibody,
                    ..InodeRender::plain(node.meta, file_type::REG, node.nlink)
                }
            }
            NodeKind::Symlink { target } if target.len() <= 59 => {
                let mut block = [0u8; 60];
                block[..target.len()].copy_from_slice(target);
                InodeRender {
                    size: target.len() as u64,
                    block,
                    file_acl,
                    ibody,
                    blocks512: acl_512,
                    ..InodeRender::plain(node.meta, file_type::SYMLINK, node.nlink)
                }
            }
            NodeKind::Symlink { target } => {
                let plan = symlink_plans[slot].as_ref().unwrap();
                InodeRender {
                    size: target.len() as u64,
                    blocks512: 8 + plan.tree_blocks + acl_512,
                    flags: iflags::EXTENTS,
                    block: plan.root,
                    file_acl,
                    ibody,
                    ..InodeRender::plain(node.meta, file_type::SYMLINK, node.nlink)
                }
            }
            NodeKind::Special {
                ftype,
                major,
                minor,
            } => {
                let mut block = [0u8; 60];
                if *ftype == file_type::CHR || *ftype == file_type::BLK {
                    if *major < 256 && *minor < 256 {
                        crate::le::put_u32(&mut block, 0, (major << 8) | minor);
                    } else {
                        crate::le::put_u32(
                            &mut block,
                            4,
                            (minor & 0xFF) | (major << 8) | ((minor & !0xFFu32) << 12),
                        );
                    }
                }
                InodeRender {
                    block,
                    file_acl,
                    ibody,
                    blocks512: acl_512,
                    ..InodeRender::plain(node.meta, *ftype, node.nlink)
                }
            }
        };
        set_ino(&mut inodes, ino, render_inode(fs_seed, ino, &render));
    }

    // --- group descriptors -------------------------------------------------
    let ipg = geo.inodes_per_group;
    // Directory count per group (bg_used_dirs_count), one pass.
    let mut dir_count_per_group = vec![0u32; geo.groups as usize];
    for (idx, raw) in inodes.iter().enumerate() {
        if crate::le::u16(raw, 0) >> 12 == 0o04 {
            dir_count_per_group[(idx as u32 / ipg) as usize] += 1;
        }
    }
    let mut descs: Vec<GroupDesc> = Vec::with_capacity(geo.groups as usize);
    let mut total_free_blocks = 0u64;
    for g in 0..geo.groups {
        let (bb, ib, it) = group_meta[g as usize];
        let start = geo.group_start(g) as usize;
        let in_group = geo.blocks_in_group(g) as usize;
        // Slice this group's bitmap bits into a padded block.
        let mut bb_block = vec![0u8; BLOCK_SIZE];
        let mut free = 0u32;
        for i in 0..in_group {
            let blk = start + i;
            let used = bitmap[blk / 8] & (1 << (blk % 8)) != 0;
            if used {
                bb_block[i / 8] |= 1 << (i % 8);
            } else {
                free += 1;
            }
        }
        for i in in_group..BLOCK_SIZE * 8 {
            bb_block[i / 8] |= 1 << (i % 8);
        }
        total_free_blocks += u64::from(free);

        let used_inodes_in_group = (u64::from(used_inodes)
            .saturating_sub(u64::from(g) * u64::from(ipg)))
        .min(u64::from(ipg)) as u32;
        let uninit = used_inodes_in_group == 0;
        let mut ib_block = vec![0u8; BLOCK_SIZE];
        if !uninit {
            for i in 0..used_inodes_in_group as usize {
                ib_block[i / 8] |= 1 << (i % 8);
            }
            for i in ipg as usize..BLOCK_SIZE * 8 {
                ib_block[i / 8] |= 1 << (i % 8);
            }
        }
        let dirs_in_group = dir_count_per_group[g as usize];

        let mut d = GroupDesc {
            block_bitmap: bb,
            inode_bitmap: ib,
            inode_table: it,
            free_blocks_count: free,
            free_inodes_count: ipg - used_inodes_in_group,
            used_dirs_count: dirs_in_group,
            flags: bg_flags::INODE_ZEROED | if uninit { bg_flags::INODE_UNINIT } else { 0 },
            exclude_bitmap: 0,
            block_bitmap_csum: u32::from(csum::block_bitmap(fs_seed, &bb_block)),
            inode_bitmap_csum: if uninit {
                0
            } else {
                u32::from(csum::inode_bitmap(fs_seed, &ib_block, ipg))
            },
            itable_unused: ipg - used_inodes_in_group,
            checksum: 0,
        };
        segments.push(Segment {
            block: bb,
            len: 1,
            src: SegSrc::Bytes(bb_block),
        });
        segments.push(Segment {
            block: ib,
            len: 1,
            src: SegSrc::Bytes(ib_block),
        });
        segments.push(Segment {
            block: it,
            len: itb,
            src: SegSrc::Itable { group: g },
        });
        let mut raw = [0u8; 32];
        d.encode32(&mut raw);
        d.checksum = csum::group_desc(fs_seed, g, &raw);
        descs.push(d);
    }

    let mut gdt_bytes = vec![0u8; (gdt as usize) * BLOCK_SIZE];
    for (g, d) in descs.iter().enumerate() {
        d.encode32(&mut gdt_bytes[g * 32..]);
    }

    // --- superblock ---------------------------------------------------------
    let journal_inode_raw = inodes[JOURNAL_INO as usize - 1];
    let mut jnl_blocks = [0u32; 17];
    for (i, w) in jnl_blocks.iter_mut().enumerate().take(15) {
        *w = crate::le::u32(&journal_inode_raw, 0x28 + 4 * i);
    }
    jnl_blocks[15] = 0; // i_size_high
    jnl_blocks[16] = (u64::from(geo.journal_blocks) * BLOCK_SIZE as u64) as u32;

    let mut volume_name = [0u8; 16];
    if let Some(l) = &b.opts.label {
        volume_name[..l.len()].copy_from_slice(l.as_bytes());
    }
    let epoch32 = b.opts.epoch as u32;
    let mut sb = Superblock {
        inodes_count: geo.inodes_count(),
        blocks_count: geo.blocks,
        r_blocks_count: geo.blocks * u64::from(b.opts.reserved_percent) / 100,
        free_blocks_count: total_free_blocks,
        free_inodes_count: geo.inodes_count() - used_inodes,
        first_data_block: 0,
        log_block_size: 2,
        log_cluster_size: 2,
        blocks_per_group: BLOCKS_PER_GROUP,
        clusters_per_group: BLOCKS_PER_GROUP,
        inodes_per_group: ipg,
        mtime: 0,
        wtime: epoch32,
        mnt_count: 0,
        max_mnt_count: 0xFFFF,
        state: 1,
        errors: 1,
        minor_rev_level: 0,
        lastcheck: epoch32,
        checkinterval: 0,
        creator_os: 0,
        rev_level: 1,
        def_resuid: 0,
        def_resgid: 0,
        first_ino: 11,
        inode_size: 256,
        block_group_nr: 0,
        feature_compat: spec::compat::WRITER,
        feature_incompat: spec::incompat::WRITER,
        feature_ro_compat: spec::ro_compat::WRITER,
        uuid: b.opts.fs_uuid,
        volume_name,
        last_mounted: [0; 64],
        reserved_gdt_blocks: 0,
        journal_uuid: [0; 16],
        journal_inum: JOURNAL_INO,
        journal_dev: 0,
        last_orphan: 0,
        hash_seed: b.opts.hash_seed,
        def_hash_version: 1,
        jnl_backup_type: 1,
        desc_size: 0,
        default_mount_opts: 0x0C,
        first_meta_bg: 0,
        mkfs_time: epoch32,
        jnl_blocks,
        min_extra_isize: 32,
        want_extra_isize: 32,
        flags: 0x1,
        log_groups_per_flex: 4,
        checksum_type: 1,
        kbytes_written: used_blocks * 4,
        overhead_clusters: geo.overhead() as u32,
        lpf_ino: 0,
        checksum_seed: 0,
        checksum: 0,
    };

    // Block 0: 1024 pad + sb + pad.
    let mut block0 = vec![0u8; BLOCK_SIZE];
    sb.encode(&mut block0[1024..2048]);
    let c = csum::superblock(&block0[1024..2048]);
    sb.checksum = c;
    sb.encode(&mut block0[1024..2048]);
    segments.push(Segment {
        block: 0,
        len: 1,
        src: SegSrc::Bytes(block0),
    });
    segments.push(Segment {
        block: 1,
        len: gdt,
        src: SegSrc::Bytes(gdt_bytes.clone()),
    });
    // Backups: identical except s_block_group_nr + checksum.
    for g in geo.backup_groups() {
        let mut bsb = sb.clone();
        bsb.block_group_nr = g as u16;
        let mut block = vec![0u8; BLOCK_SIZE];
        bsb.encode(&mut block);
        let c = csum::superblock(&block[..1024]);
        bsb.checksum = c;
        bsb.encode(&mut block);
        segments.push(Segment {
            block: geo.group_start(g),
            len: 1,
            src: SegSrc::Bytes(block),
        });
        segments.push(Segment {
            block: geo.group_start(g) + 1,
            len: gdt,
            src: SegSrc::Bytes(gdt_bytes.clone()),
        });
    }

    // Journal superblock (block 0 of the journal); the rest of the
    // journal is zeros via the gap filler.
    let jsb = JournalSuperblock {
        blocktype: 4,
        blocksize: BLOCK_SIZE as u32,
        maxlen: geo.journal_blocks,
        first: 1,
        sequence: 1,
        start: 0,
        errno: 0,
        feature_compat: 0,
        feature_incompat: 0,
        feature_ro_compat: 0,
        uuid: b.opts.fs_uuid,
        nr_users: 1,
        dynsuper: 0,
        max_transaction: 0,
        max_trans_data: 0,
        checksum_type: 0,
        num_fc_blocks: 0,
        head: 0,
        checksum: 0,
    };
    let mut jsb_block = vec![0u8; BLOCK_SIZE];
    jsb.encode(&mut jsb_block);
    segments.push(Segment {
        block: journal_runs[0].start,
        len: 1,
        src: SegSrc::Bytes(jsb_block),
    });

    // Directory content blocks.
    for (block, bytes) in dir_rendered {
        segments.push(Segment {
            block,
            len: 1,
            src: SegSrc::Bytes(bytes),
        });
    }

    segments.sort_by_key(|s| s.block);
    data_runs.sort_by_key(|r| r.start);

    Ok(Layout {
        opts: b.opts,
        geo,
        slot_ino,
        file_runs,
        segments,
        data_runs,
        inodes,
        max_ino,
    })
}

fn err_full() -> Error {
    Error::Invalid("image size too small for the declared namespace".into())
}

fn expand_runs(runs: &[Run]) -> Vec<u64> {
    let mut out = Vec::new();
    for r in runs {
        out.extend(r.start..r.start + r.len);
    }
    out
}

/// Iterative (worklist) reachability: recursion here would put
/// namespace depth on the call stack, which untrusted inputs control.
fn mark_reachable(b: &FsBuilder, root: u32, seen: &mut [bool]) -> Result<()> {
    let mut stack = vec![root];
    while let Some(slot) = stack.pop() {
        if std::mem::replace(&mut seen[slot as usize], true) {
            continue;
        }
        if let NodeKind::Dir { children, .. } = &b.nodes[slot as usize].kind {
            for &(_, child, dead) in children {
                if !dead {
                    stack.push(child);
                }
            }
        }
    }
    Ok(())
}

/// slot -> parent inode number, one pass over every live child list
/// (replaces a per-directory scan that was quadratic in namespace size).
fn parent_map(b: &FsBuilder, slot_ino: &[u32]) -> Vec<u32> {
    let mut parents = vec![0u32; b.nodes.len()];
    for (p, node) in b.nodes.iter().enumerate() {
        if slot_ino[p] == 0 {
            continue;
        }
        if let NodeKind::Dir { children, .. } = &node.kind {
            for &(_, c, dead) in children {
                if !dead {
                    parents[c as usize] = slot_ino[p];
                }
            }
        }
    }
    parents
}
