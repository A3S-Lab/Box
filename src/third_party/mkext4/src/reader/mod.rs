//! Verification-grade ext4 reader.
//!
//! Purpose-built to be the differential oracle: it reads images produced
//! by `mke2fs` (validating this crate's understanding of the format
//! against e2fsprogs) and later round-trips images produced by this
//! crate's writer. It favors explicitness and checkability over speed,
//! but is still allocation-conscious: file content streams through a
//! caller-provided buffer.
//!
//! Sources implement [`ReadAt`] (pread-style positioned reads): stateless,
//! no seeking, safe to share.

mod verify;

pub use verify::Issue;

use crate::spec::{
    self, incompat, DirentIter, DxRootView, Extent, ExtentHeader, ExtentIdx, GroupDesc, Inode,
    Superblock,
};
use crate::{corrupt, Error, Result};

/// Positioned reads. Implementations must fill `buf` completely.
pub trait ReadAt {
    /// Read exactly `buf.len()` bytes starting at absolute `offset`.
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()>;
}

#[cfg(unix)]
impl ReadAt for std::fs::File {
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        std::os::unix::fs::FileExt::read_exact_at(self, buf, offset)
    }
}

impl ReadAt for [u8] {
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        let off = usize::try_from(offset).map_err(|_| std::io::ErrorKind::UnexpectedEof)?;
        let end = off
            .checked_add(buf.len())
            .ok_or(std::io::ErrorKind::UnexpectedEof)?;
        if end > self.len() {
            return Err(std::io::ErrorKind::UnexpectedEof.into());
        }
        buf.copy_from_slice(&self[off..end]);
        Ok(())
    }
}

impl<T: ReadAt + ?Sized> ReadAt for &T {
    fn read_exact_at(&self, offset: u64, buf: &mut [u8]) -> std::io::Result<()> {
        (**self).read_exact_at(offset, buf)
    }
}

/// One logical→physical mapping of a file, flattened from its extent tree.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FileExtent {
    /// First logical block.
    pub logical: u32,
    /// First physical block.
    pub physical: u64,
    /// Blocks covered.
    pub len: u32,
    /// Allocated but unwritten (reads as zeros).
    pub unwritten: bool,
}

/// A directory entry with an owned name.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    /// Inode number.
    pub inode: u32,
    /// `file_type` byte (see [`crate::spec::file_type`]).
    pub file_type: u8,
    /// Name bytes.
    pub name: Vec<u8>,
}

/// An open filesystem.
pub struct Fs<R: ReadAt> {
    src: R,
    sb: Superblock,
    /// Raw primary GDT, one `desc_size` record per group.
    gdt: Vec<u8>,
    block_size: u64,
    fs_seed: u32,
}

impl<R: ReadAt> std::fmt::Debug for Fs<R> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Fs")
            .field("uuid", &self.sb.uuid)
            .field("blocks", &self.sb.blocks_count)
            .field("inodes", &self.sb.inodes_count)
            .finish_non_exhaustive()
    }
}

impl<R: ReadAt> Fs<R> {
    /// Open and validate a filesystem: superblock magic, checksum (when
    /// metadata_csum), and the incompat-feature allowlist.
    pub fn open(src: R) -> Result<Fs<R>> {
        let mut sb_bytes = [0u8; Superblock::LEN];
        src.read_exact_at(1024, &mut sb_bytes)?;
        let sb = Superblock::decode(&sb_bytes)?;

        let unknown = sb.feature_incompat & !incompat::READER;
        if unknown != 0 {
            return Err(Error::Unsupported(format!(
                "incompat features {unknown:#x}"
            )));
        }
        if sb.block_size() != spec::BLOCK_SIZE as u64 {
            return Err(Error::Unsupported(format!(
                "block size {}",
                sb.block_size()
            )));
        }
        // Geometry sanity: everything the reader later divides by or
        // indexes with is validated here, exactly once — hostile images
        // reach all of these fields.
        if sb.blocks_per_group != spec::BLOCKS_PER_GROUP {
            return Err(corrupt(
                "superblock",
                format!("blocks_per_group {}", sb.blocks_per_group),
            ));
        }
        if sb.inodes_per_group == 0 || sb.inodes_per_group > spec::BLOCKS_PER_GROUP {
            return Err(corrupt(
                "superblock",
                format!("inodes_per_group {}", sb.inodes_per_group),
            ));
        }
        if !matches!(sb.inode_size, 128 | 256 | 512 | 1024 | 2048 | 4096) {
            return Err(corrupt(
                "superblock",
                format!("inode_size {}", sb.inode_size),
            ));
        }
        if sb.blocks_count == 0 {
            return Err(corrupt("superblock", "zero block count"));
        }
        if u64::from(sb.inodes_count) > u64::from(sb.inodes_per_group) * sb.group_count() {
            return Err(corrupt(
                "superblock",
                format!("inodes_count {} exceeds groups * ipg", sb.inodes_count),
            ));
        }
        // Without meta_bg the whole GDT must fit inside the first group;
        // this also bounds the allocation below.
        if sb.group_count() * sb.desc_size() as u64
            > u64::from(spec::BLOCKS_PER_GROUP) * sb.block_size()
        {
            return Err(corrupt("superblock", "group descriptor table too large"));
        }
        let fs_seed = if sb.feature_incompat & incompat::CSUM_SEED != 0 {
            sb.checksum_seed
        } else {
            crate::csum::fs_seed(&sb.uuid)
        };
        if sb.feature_ro_compat & spec::ro_compat::METADATA_CSUM != 0 {
            let want = crate::csum::superblock(&sb_bytes);
            if want != sb.checksum {
                return Err(corrupt(
                    "superblock",
                    format!("checksum {:#010x} != stored {:#010x}", want, sb.checksum),
                ));
            }
        }

        let block_size = sb.block_size();
        let groups = sb.group_count() as usize;
        let desc_size = sb.desc_size();
        let mut gdt = vec![0u8; groups * desc_size];
        // The GDT starts at the block after the superblock's block.
        let gdt_start = (u64::from(sb.first_data_block) + 1) * block_size;
        src.read_exact_at(gdt_start, &mut gdt)?;

        Ok(Fs {
            src,
            sb,
            gdt,
            block_size,
            fs_seed,
        })
    }

    /// The decoded superblock.
    pub fn superblock(&self) -> &Superblock {
        &self.sb
    }

    /// The filesystem checksum seed.
    pub fn fs_seed(&self) -> u32 {
        self.fs_seed
    }

    /// Read one filesystem block.
    pub fn read_block(&self, block: u64) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; self.block_size as usize];
        self.src.read_exact_at(block * self.block_size, &mut buf)?;
        Ok(buf)
    }

    /// Decode group descriptor `g`.
    pub fn group_desc(&self, g: u64) -> Result<GroupDesc> {
        let ds = self.sb.desc_size();
        let off = g as usize * ds;
        if off + ds > self.gdt.len() {
            return Err(corrupt("group", format!("group {g} out of range")));
        }
        GroupDesc::decode(&self.gdt[off..off + ds])
    }

    /// Raw bytes of group descriptor `g` (for checksum verification).
    pub fn group_desc_raw(&self, g: u64) -> Result<&[u8]> {
        let ds = self.sb.desc_size();
        let off = g as usize * ds;
        self.gdt
            .get(off..off + ds)
            .ok_or_else(|| corrupt("group", format!("group {g} out of range")))
    }

    /// Read the raw inode-table slot for inode `ino` (1-based).
    pub fn inode_raw(&self, ino: u32) -> Result<Vec<u8>> {
        if ino == 0 || ino > self.sb.inodes_count {
            return Err(corrupt("inode", format!("inode {ino} out of range")));
        }
        let ipg = u64::from(self.sb.inodes_per_group);
        let (g, idx) = ((u64::from(ino) - 1) / ipg, (u64::from(ino) - 1) % ipg);
        let desc = self.group_desc(g)?;
        let isize = u64::from(self.sb.inode_size);
        let off = desc.inode_table * self.block_size + idx * isize;
        let mut buf = vec![0u8; isize as usize];
        self.src.read_exact_at(off, &mut buf)?;
        Ok(buf)
    }

    /// Decode inode `ino`, verifying its checksum when metadata_csum is
    /// enabled.
    pub fn inode(&self, ino: u32) -> Result<Inode> {
        let raw = self.inode_raw(ino)?;
        let inode = Inode::decode(&raw)?;
        if self.sb.feature_ro_compat & spec::ro_compat::METADATA_CSUM != 0 {
            let full = crate::csum::inode(self.fs_seed, ino, inode.generation, &raw);
            let (got, stored) = if inode.extra_isize >= 4 {
                (full, inode.checksum)
            } else {
                (full & 0xFFFF, inode.checksum & 0xFFFF)
            };
            if got != stored {
                return Err(corrupt(
                    "inode",
                    format!("inode {ino} checksum {got:#x} != stored {stored:#x}"),
                ));
            }
        }
        Ok(inode)
    }

    /// Flatten an inode's extent tree into sorted [`FileExtent`]s,
    /// verifying interior-block checksums along the way.
    pub fn extents(&self, ino: u32, inode: &Inode) -> Result<Vec<FileExtent>> {
        let Some(root) = inode.extent_root() else {
            return Ok(Vec::new());
        };
        let mut out = Vec::new();
        self.walk_extent_node(ino, inode, root, None, &mut out)?;
        Ok(out)
    }

    fn walk_extent_node(
        &self,
        ino: u32,
        inode: &Inode,
        node: &[u8],
        expect_depth: Option<u16>,
        out: &mut Vec<FileExtent>,
    ) -> Result<()> {
        let h = ExtentHeader::decode(node)?;
        // ext4's real maximum depth is 5; anything deeper is hostile (and
        // would otherwise let a forged tree explode the walk).
        if h.depth > 5 {
            return Err(corrupt("extent tree", format!("depth {}", h.depth)));
        }
        if let Some(d) = expect_depth {
            if h.depth != d {
                return Err(corrupt("extent tree", "child depth mismatch"));
            }
        }
        let cap = (node.len() - ExtentHeader::LEN) / ExtentHeader::ENTRY_LEN;
        if usize::from(h.entries) > usize::from(h.max) || usize::from(h.max) > cap {
            return Err(corrupt(
                "extent tree",
                format!("entries {} max {} cap {cap}", h.entries, h.max),
            ));
        }
        for i in 0..usize::from(h.entries) {
            let e = spec::extent::entry(node, i);
            if h.depth == 0 {
                let ext = Extent::decode(e);
                if ext.is_empty() {
                    return Err(corrupt("extent tree", "zero-length extent"));
                }
                out.push(FileExtent {
                    logical: ext.logical,
                    physical: ext.start,
                    len: ext.len(),
                    unwritten: ext.is_unwritten(),
                });
            } else {
                let idx = ExtentIdx::decode(e);
                let child = self.read_block(idx.leaf)?;
                if self.sb.feature_ro_compat & spec::ro_compat::METADATA_CSUM != 0 {
                    let seed = crate::csum::inode_seed(self.fs_seed, ino, inode.generation);
                    let want = crate::csum::extent_block(seed, &child);
                    let stored = crate::le::u32(&child, child.len() - 4);
                    if want != stored {
                        return Err(corrupt(
                            "extent tree",
                            format!("block {} checksum mismatch", idx.leaf),
                        ));
                    }
                }
                self.walk_extent_node(ino, inode, &child, Some(h.depth - 1), out)?;
            }
        }
        Ok(())
    }

    /// Read file content starting at byte `offset` into `buf`; holes and
    /// unwritten extents read as zeros. Returns bytes read (short only at
    /// end of file).
    pub fn read_file_at(
        &self,
        inode: &Inode,
        extents: &[FileExtent],
        offset: u64,
        buf: &mut [u8],
    ) -> Result<usize> {
        if offset >= inode.size {
            return Ok(0);
        }
        let want = usize::try_from((inode.size - offset).min(buf.len() as u64)).unwrap();
        let buf = &mut buf[..want];
        buf.fill(0);
        let bs = self.block_size;
        for e in extents {
            if e.unwritten {
                continue;
            }
            let ext_start = u64::from(e.logical) * bs;
            let ext_end = ext_start + u64::from(e.len) * bs;
            let start = ext_start.max(offset);
            let end = ext_end.min(offset + want as u64);
            if start >= end {
                continue;
            }
            let phys = e.physical * bs + (start - ext_start);
            // The last extent may map past i_size; clamp to the request.
            let dst = &mut buf[(start - offset) as usize..(end - offset) as usize];
            self.src.read_exact_at(phys, dst)?;
        }
        Ok(want)
    }

    /// Read a whole file into memory. Convenience for tests/small files;
    /// refuses sizes larger than the filesystem itself (hostile images
    /// could otherwise demand absurd allocations) — stream through
    /// [`Fs::read_file_at`] for big files.
    pub fn read_file(&self, ino: u32) -> Result<Vec<u8>> {
        let inode = self.inode(ino)?;
        if inode.size > self.sb.blocks_count * self.block_size {
            return Err(corrupt(
                "inode",
                format!("inode {ino} size {} exceeds filesystem", inode.size),
            ));
        }
        let extents = self.extents(ino, &inode)?;
        let mut buf = vec![
            0u8;
            usize::try_from(inode.size).map_err(|_| Error::Unsupported(
                "file too large for read_file".into()
            ))?
        ];
        self.read_file_at(&inode, &extents, 0, &mut buf)?;
        Ok(buf)
    }

    /// A symlink's target: inline for fast symlinks, content block for
    /// slow ones (which the format bounds to one block).
    pub fn symlink_target(&self, ino: u32) -> Result<Vec<u8>> {
        let inode = self.inode(ino)?;
        if let Some(t) = inode.fast_symlink_target() {
            return Ok(t.to_vec());
        }
        if inode.size >= self.block_size {
            return Err(corrupt(
                "symlink",
                format!("inode {ino} target size {}", inode.size),
            ));
        }
        self.read_file(ino)
    }

    /// All directory entries of `ino`, including "." and "..", in block
    /// order. Handles linear and hash-indexed directories (dx_root
    /// contributes its dot entries; dx_node blocks are recognized by
    /// their block-spanning fake dirent and skipped).
    pub fn read_dir(&self, ino: u32) -> Result<Vec<DirEntry>> {
        let inode = self.inode(ino)?;
        if inode.file_type() != spec::inode::FileType::Dir {
            return Err(corrupt("directory", format!("inode {ino} is not a dir")));
        }
        let extents = self.extents(ino, &inode)?;
        let is_htree = inode.flags & spec::iflags::INDEX != 0;
        let mut out = Vec::new();
        let total_blocks = inode.size / self.block_size;
        for logical in 0..total_blocks {
            let Some(block) = self.file_block(&extents, logical) else {
                return Err(corrupt("directory", "hole in directory"));
            };
            let data = self.read_block(block)?;
            if is_htree {
                if logical == 0 {
                    DxRootView::parse(&data)?; // structure check only
                    out.push(DirEntry {
                        inode: crate::le::u32(&data, 0),
                        file_type: data[7],
                        name: b".".to_vec(),
                    });
                    out.push(DirEntry {
                        inode: crate::le::u32(&data, 0x0C),
                        file_type: data[0x13],
                        name: b"..".to_vec(),
                    });
                    continue;
                }
                if crate::le::u32(&data, 0) == 0
                    && usize::from(crate::le::u16(&data, 4)) == data.len()
                {
                    continue; // dx_node
                }
            }
            for e in DirentIter::new(&data) {
                let e = e?;
                out.push(DirEntry {
                    inode: e.inode,
                    file_type: e.file_type,
                    name: e.name.to_vec(),
                });
            }
        }
        Ok(out)
    }

    /// Find the physical block backing `logical`, if mapped and written.
    pub fn file_block(&self, extents: &[FileExtent], logical: u64) -> Option<u64> {
        for e in extents {
            let lo = u64::from(e.logical);
            if (lo..lo + u64::from(e.len)).contains(&logical) && !e.unwritten {
                return Some(e.physical + (logical - lo));
            }
        }
        None
    }

    /// Look up one name in a directory. Deliberately a linear scan
    /// (this reader is a verification oracle, not a fast path), so it
    /// works identically on linear and hash-indexed directories.
    pub fn lookup(&self, dir_ino: u32, name: &[u8]) -> Result<Option<u32>> {
        Ok(self
            .read_dir(dir_ino)?
            .into_iter()
            .find(|e| e.name == name)
            .map(|e| e.inode))
    }

    /// Resolve an absolute path (no symlink following) to an inode.
    pub fn resolve(&self, path: &str) -> Result<u32> {
        let mut ino = spec::ROOT_INO;
        for part in path.split('/').filter(|p| !p.is_empty()) {
            ino = self.lookup(ino, part.as_bytes())?.ok_or_else(|| {
                Error::Io(std::io::Error::new(
                    std::io::ErrorKind::NotFound,
                    format!("{path}: component {part} not found"),
                ))
            })?;
        }
        Ok(ino)
    }

    /// All xattrs of an inode: in-inode entries first, then block entries
    /// (on-disk order preserved).
    pub fn xattrs(&self, ino: u32) -> Result<Vec<spec::XattrEntry>> {
        let raw = self.inode_raw(ino)?;
        let inode = Inode::decode(&raw)?;
        let mut out = spec::ibody_entries(&raw, inode.extra_isize)?;
        if inode.file_acl != 0 {
            let block = self.read_block(inode.file_acl)?;
            out.extend(spec::XattrBlockView::parse(&block)?.entries);
        }
        Ok(out)
    }

    /// Whether inode `ino` is marked used in its group's inode bitmap.
    pub fn inode_in_use(&self, ino: u32) -> Result<bool> {
        let ipg = u64::from(self.sb.inodes_per_group);
        let (g, idx) = (
            (u64::from(ino) - 1) / ipg,
            ((u64::from(ino) - 1) % ipg) as usize,
        );
        let desc = self.group_desc(g)?;
        if desc.flags & spec::bg_flags::INODE_UNINIT != 0 {
            return Ok(false);
        }
        let bitmap = self.read_block(desc.inode_bitmap)?;
        Ok(bitmap[idx / 8] & (1 << (idx % 8)) != 0)
    }

    /// Run the full structural verification suite (checksums, htree
    /// placement, extent ordering, bitmap padding). Returns all issues
    /// found; empty means clean.
    pub fn verify(&self) -> Result<Vec<Issue>> {
        verify::verify(self)
    }
}
