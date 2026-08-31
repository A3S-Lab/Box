//! The builder API: declare a namespace, seal it into a frozen layout,
//! then stream file contents.
//!
//! Memory model: one fixed-size record per inode slot, all
//! names in a single byte arena, children as per-directory vectors of
//! (name-ref, slot) — no per-entry heap strings.

mod htree;
mod seal;
mod writer;
mod xattr_build;

pub use seal::Layout;
pub use writer::{ImageWriter, Summary};

use crate::spec::{self, LINK_MAX};
use crate::{Error, Result};

/// Feature selection. Only one profile exists today; the type keeps the
/// door open without a breaking change.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Features(());

impl Features {
    /// has_journal, ext_attr, dir_index; filetype, extents, flex_bg;
    /// sparse_super, large_file, huge_file, dir_nlink, extra_isize,
    /// metadata_csum.
    pub const LINUX_ROOTFS: Features = Features(());
}

/// Inode count policy.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InodeCount {
    /// mke2fs's default ratio: one inode per 16384 bytes.
    Auto,
    /// Exactly this many (rounded up to fill itable blocks).
    Exact(u32),
}

/// Seconds + nanoseconds since the epoch (34-bit seconds reaching year 2446, 30-bit nanoseconds).
pub type Timespec = (i64, u32);

/// Per-inode metadata. `mode` carries permission bits only (0o7777);
/// the file type comes from the declaring call.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Meta {
    /// Permission bits (setuid/setgid/sticky included).
    pub mode: u16,
    /// Owner uid.
    pub uid: u32,
    /// Owner gid.
    pub gid: u32,
    /// Modification time. Also the default for atime/ctime/crtime.
    pub mtime: Timespec,
    /// Access time (defaults to mtime).
    pub atime: Option<Timespec>,
    /// Inode-change time (defaults to mtime).
    pub ctime: Option<Timespec>,
    /// Creation time (defaults to mtime).
    pub crtime: Option<Timespec>,
}

impl Meta {
    /// `mode` + uid/gid, everything timestamped at `mtime`.
    pub fn new(mode: u16, uid: u32, gid: u32, mtime: Timespec) -> Meta {
        Meta {
            mode,
            uid,
            gid,
            mtime,
            atime: None,
            ctime: None,
            crtime: None,
        }
    }
}

/// Builder options — together with the declaration sequence and file
/// contents, the complete determinism inputs: the same options and calls
/// always produce byte-identical images.
///
/// Construct with [`Options::new`] and adjust fields as needed:
///
/// ```
/// use mkext4::{InodeCount, Options};
///
/// let mut opts = Options::new(1 << 30, [0x42; 16], 1_704_067_200);
/// opts.label = Some("rootfs".into());
/// opts.inodes = InodeCount::Exact(200_000);
/// ```
///
/// The struct is `#[non_exhaustive]`, so new options can be added
/// without breaking construction.
#[derive(Debug, Clone)]
#[non_exhaustive]
pub struct Options {
    /// Total image size in bytes (multiple of 4096). Required.
    pub size_bytes: u64,
    /// Filesystem UUID (never sampled).
    pub fs_uuid: [u8; 16],
    /// htree hash seed (`s_hash_seed`). Defaults to a seed derived from
    /// the UUID (what `mke2fs -E hash_seed=UUID` does).
    pub hash_seed: [u32; 4],
    /// Superblock timestamps (`s_mkfs_time`, `s_wtime`, `s_lastcheck`)
    /// and the timestamp of fs-owned inodes (root default, lost+found,
    /// reserved inodes).
    pub epoch: i64,
    /// Inode count policy.
    pub inodes: InodeCount,
    /// Volume label (≤ 16 bytes).
    pub label: Option<String>,
    /// Reserved blocks percentage (mke2fs default 5).
    pub reserved_percent: u8,
    /// Journal size override in blocks (default: sized by a tier table
    /// matching mke2fs, from 1024 blocks on tiny images to 262144).
    pub journal_blocks: Option<u32>,
    /// Feature profile.
    pub features: Features,
}

impl Options {
    /// Options with the required determinism inputs and defaults for the
    /// rest: UUID-derived hash seed, automatic inode count, no label,
    /// 5% reserved blocks, tier-sized journal.
    pub fn new(size_bytes: u64, fs_uuid: [u8; 16], epoch: i64) -> Options {
        Options {
            size_bytes,
            fs_uuid,
            hash_seed: crate::dirhash::seed_from_uuid(&fs_uuid),
            epoch,
            inodes: InodeCount::Auto,
            label: None,
            reserved_percent: 5,
            journal_blocks: None,
            features: Features::LINUX_ROOTFS,
        }
    }
}

/// Declared xattrs per builder slot.
pub(crate) type XattrDecls = std::collections::BTreeMap<u32, Vec<xattr_build::Attr>>;

/// Handle to a declared inode. Hardlinks are additional names for the
/// same handle.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub struct InodeHandle(pub(crate) u32);

/// The root directory handle.
pub const ROOT: InodeHandle = InodeHandle(0);

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct NameRef {
    off: u32,
    len: u8,
}

#[derive(Debug)]
pub(crate) enum NodeKind {
    Dir {
        /// (name, child) in declaration order; tombstoned entries keep
        /// their slot but are skipped everywhere. This vector alone
        /// defines dirent order — the index below never reaches output.
        children: Vec<(NameRef, u32, bool)>,
        /// Live name -> position in `children`, so duplicate checks and
        /// removals are O(1) instead of a scan (flat 100k+-entry
        /// directories are a primary target). Tombstoned names are
        /// deleted from the index so they can be redeclared.
        ///
        /// Determinism note: a HashMap is safe here because it is used
        /// for point lookups only and is NEVER iterated — the crate's
        /// no-hash-ordered-iteration rule is about iteration order
        /// reaching the output path.
        index: std::collections::HashMap<Box<[u8]>, usize>,
    },
    File {
        size: u64,
    },
    Sparse {
        /// Data segments as (byte offset, byte length), sorted, disjoint.
        data_segs: Vec<(u64, u64)>,
        /// Total logical size (data + holes).
        size: u64,
    },
    Symlink {
        target: Vec<u8>,
    },
    Special {
        /// Dirent file_type (CHR / BLK / FIFO / SOCK).
        ftype: u8,
        major: u32,
        minor: u32,
    },
}

/// One segment of a sparse file declaration, in logical order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SparseSeg {
    /// Bytes supplied by `fill`.
    Data(u64),
    /// Absent blocks; read back as zeros; never filled.
    Hole(u64),
}

/// Kind of special inode for [`FsBuilder::mknod`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpecialKind {
    /// Character device.
    Char {
        /// Major number (≤ 4095).
        major: u32,
        /// Minor number (≤ 1048575).
        minor: u32,
    },
    /// Block device.
    Block {
        /// Major number (≤ 4095).
        major: u32,
        /// Minor number (≤ 1048575).
        minor: u32,
    },
    /// Named pipe.
    Fifo,
    /// Unix socket.
    Socket,
}

#[derive(Debug)]
pub(crate) struct Node {
    pub(crate) kind: NodeKind,
    pub(crate) meta: Meta,
    /// Dirent names pointing at this node (hardlink count for files;
    /// for dirs always 1 — extra links come from "." and children's "..").
    pub(crate) nlink: u32,
}

/// Phase 1: namespace declaration.
#[derive(Debug)]
pub struct FsBuilder {
    pub(crate) opts: Options,
    pub(crate) nodes: Vec<Node>,
    pub(crate) names: Vec<u8>,
    /// xattrs per slot: (name_index, name-after-prefix, value), few
    /// nodes have any, so a side map beats a per-node field.
    pub(crate) xattrs: XattrDecls,
    /// Total declared file bytes of all currently-linked files (upper
    /// bound; recomputed exactly at seal).
    declared_bytes: u64,
}

impl FsBuilder {
    /// Start declaring a namespace.
    pub fn new(opts: Options) -> Result<FsBuilder> {
        if opts.size_bytes == 0 || opts.size_bytes % 4096 != 0 {
            return Err(Error::Invalid(
                "size_bytes must be a positive multiple of 4096".into(),
            ));
        }
        if let Some(l) = &opts.label {
            if l.len() > 16 {
                return Err(Error::Invalid("label longer than 16 bytes".into()));
            }
        }
        if opts.reserved_percent > 50 {
            return Err(Error::Invalid("reserved_percent > 50".into()));
        }
        // The superblock's own timestamps are plain 32-bit fields.
        if opts.epoch < 0 || opts.epoch > i64::from(u32::MAX) {
            return Err(Error::Invalid(format!(
                "epoch {} outside the superblock's u32 range",
                opts.epoch
            )));
        }
        let root_meta = Meta::new(0o755, 0, 0, (opts.epoch, 0));
        Ok(FsBuilder {
            opts,
            nodes: vec![Node {
                kind: NodeKind::Dir {
                    children: Vec::new(),
                    index: std::collections::HashMap::new(),
                },
                meta: root_meta,
                nlink: 1,
            }],
            names: Vec::new(),
            xattrs: std::collections::BTreeMap::new(),
            declared_bytes: 0,
        })
    }

    fn intern(&mut self, name: &[u8]) -> Result<NameRef> {
        if name.is_empty() || name.len() > 255 {
            return Err(Error::Invalid(format!(
                "name length {} (must be 1..=255)",
                name.len()
            )));
        }
        if name.contains(&b'/') || name.contains(&0) || name == b"." || name == b".." {
            return Err(Error::Invalid(format!("invalid name {name:?}")));
        }
        let off = u32::try_from(self.names.len())
            .map_err(|_| Error::Invalid("name arena overflow".into()))?;
        self.names.extend_from_slice(name);
        Ok(NameRef {
            off,
            len: name.len() as u8,
        })
    }

    pub(crate) fn name(&self, r: NameRef) -> &[u8] {
        &self.names[r.off as usize..r.off as usize + usize::from(r.len)]
    }

    #[allow(clippy::type_complexity)]
    fn dir_mut(
        &mut self,
        dir: InodeHandle,
    ) -> Result<(
        &mut Vec<(NameRef, u32, bool)>,
        &mut std::collections::HashMap<Box<[u8]>, usize>,
    )> {
        match self.nodes.get_mut(dir.0 as usize).map(|n| &mut n.kind) {
            Some(NodeKind::Dir { children, index }) => Ok((children, index)),
            Some(_) => Err(Error::Invalid("parent is not a directory".into())),
            None => Err(Error::Invalid("invalid handle".into())),
        }
    }

    fn check_duplicate(&self, dir: InodeHandle, name: &[u8]) -> Result<()> {
        let Some(NodeKind::Dir { index, .. }) = self.nodes.get(dir.0 as usize).map(|n| &n.kind)
        else {
            return Err(Error::Invalid("parent is not a directory".into()));
        };
        if index.contains_key(name) {
            return Err(Error::Invalid(format!(
                "duplicate name {name:?} (remove it first)"
            )));
        }
        Ok(())
    }

    fn add_entry(&mut self, dir: InodeHandle, name: &[u8], child: u32) -> Result<()> {
        self.check_duplicate(dir, name)?;
        let nref = self.intern(name)?;
        let (children, index) = self.dir_mut(dir)?;
        index.insert(name.into(), children.len());
        children.push((nref, child, false));
        Ok(())
    }

    /// Declare a directory.
    pub fn mkdir(
        &mut self,
        parent: InodeHandle,
        name: impl AsRef<[u8]>,
        meta: Meta,
    ) -> Result<InodeHandle> {
        let name = name.as_ref();
        let slot = self.nodes.len() as u32;
        self.add_entry(parent, name, slot)?;
        self.nodes.push(Node {
            kind: NodeKind::Dir {
                children: Vec::new(),
                index: std::collections::HashMap::new(),
            },
            meta,
            nlink: 1,
        });
        Ok(InodeHandle(slot))
    }

    /// Declare a regular file. `size` is final: `fill` must supply
    /// exactly this many bytes.
    pub fn file(
        &mut self,
        parent: InodeHandle,
        name: impl AsRef<[u8]>,
        meta: Meta,
        size: u64,
    ) -> Result<InodeHandle> {
        let name = name.as_ref();
        let slot = self.nodes.len() as u32;
        self.add_entry(parent, name, slot)?;
        self.nodes.push(Node {
            kind: NodeKind::File { size },
            meta,
            nlink: 1,
        });
        self.declared_bytes += size;
        Ok(InodeHandle(slot))
    }

    /// Declare a sparse file from a segment map. Every segment except the
    /// last must be a multiple of 4096 bytes (holes are whole absent
    /// blocks). `fill` later supplies only the `Data` bytes, in order.
    pub fn file_sparse(
        &mut self,
        parent: InodeHandle,
        name: impl AsRef<[u8]>,
        meta: Meta,
        segments: &[SparseSeg],
    ) -> Result<InodeHandle> {
        let name = name.as_ref();
        let mut data_segs = Vec::new();
        let mut offset = 0u64;
        for (i, seg) in segments.iter().enumerate() {
            let len = match *seg {
                SparseSeg::Data(l) => l,
                SparseSeg::Hole(l) => l,
            };
            if len == 0 {
                return Err(Error::Invalid("zero-length sparse segment".into()));
            }
            if i + 1 != segments.len() && len % 4096 != 0 {
                return Err(Error::Invalid(
                    "sparse segments must be block-aligned (except the last)".into(),
                ));
            }
            if let SparseSeg::Data(l) = *seg {
                // Merge adjacent data segments for a canonical map.
                match data_segs.last_mut() {
                    Some((o, dl)) if *o + *dl == offset => *dl += l,
                    _ => data_segs.push((offset, l)),
                }
            }
            offset += len;
        }
        let size = offset;
        let slot = self.nodes.len() as u32;
        self.add_entry(parent, name, slot)?;
        self.nodes.push(Node {
            kind: NodeKind::Sparse { data_segs, size },
            meta,
            nlink: 1,
        });
        self.declared_bytes += size;
        Ok(InodeHandle(slot))
    }

    /// Declare a symlink. Targets of ≤ 59 bytes are stored inline (fast
    /// symlink); longer ones occupy one block. Max 4095 bytes.
    pub fn symlink(
        &mut self,
        parent: InodeHandle,
        name: impl AsRef<[u8]>,
        target: impl AsRef<[u8]>,
        meta: Meta,
    ) -> Result<InodeHandle> {
        let name = name.as_ref();
        let t = target.as_ref();
        if t.is_empty() || t.len() > 4095 || t.contains(&0) {
            return Err(Error::Invalid(format!(
                "symlink target length {} (must be 1..=4095, no NUL)",
                t.len()
            )));
        }
        let slot = self.nodes.len() as u32;
        self.add_entry(parent, name, slot)?;
        self.nodes.push(Node {
            kind: NodeKind::Symlink { target: t.to_vec() },
            meta,
            nlink: 1,
        });
        Ok(InodeHandle(slot))
    }

    /// Declare a device node, FIFO, or socket.
    pub fn mknod(
        &mut self,
        parent: InodeHandle,
        name: impl AsRef<[u8]>,
        meta: Meta,
        kind: SpecialKind,
    ) -> Result<InodeHandle> {
        let name = name.as_ref();
        let (ftype, major, minor) = match kind {
            SpecialKind::Char { major, minor } => (spec::file_type::CHR, major, minor),
            SpecialKind::Block { major, minor } => (spec::file_type::BLK, major, minor),
            SpecialKind::Fifo => (spec::file_type::FIFO, 0, 0),
            SpecialKind::Socket => (spec::file_type::SOCK, 0, 0),
        };
        if major > 0xFFF || minor > 0xF_FFFF {
            return Err(Error::Invalid(format!(
                "device numbers ({major}, {minor}) out of dev_t range"
            )));
        }
        let slot = self.nodes.len() as u32;
        self.add_entry(parent, name, slot)?;
        self.nodes.push(Node {
            kind: NodeKind::Special {
                ftype,
                major,
                minor,
            },
            meta,
            nlink: 1,
        });
        Ok(InodeHandle(slot))
    }

    /// Set an extended attribute on any live handle (including [`ROOT`]).
    /// Attributes are canonically sorted on disk, so declaration order
    /// does not affect the output; setting the same name again replaces
    /// the value.
    pub fn set_xattr(&mut self, handle: InodeHandle, name: &str, value: &[u8]) -> Result<()> {
        if self.nodes.get(handle.0 as usize).is_none() {
            return Err(Error::Invalid("invalid handle".into()));
        }
        let (index, suffix): (u8, &str) = if let Some(s) = name.strip_prefix("user.") {
            (1, s)
        } else if name == "system.posix_acl_access" {
            (2, "")
        } else if name == "system.posix_acl_default" {
            (3, "")
        } else if let Some(s) = name.strip_prefix("trusted.") {
            (4, s)
        } else if let Some(s) = name.strip_prefix("security.") {
            (6, s)
        } else if let Some(s) = name.strip_prefix("system.") {
            (7, s)
        } else {
            return Err(Error::Invalid(format!(
                "xattr name {name:?} has no recognized namespace"
            )));
        };
        if suffix.len() > 255 {
            return Err(Error::Invalid("xattr name too long".into()));
        }
        let list = self.xattrs.entry(handle.0).or_default();
        if let Some(e) = list
            .iter_mut()
            .find(|(i, n, _)| *i == index && n == suffix.as_bytes())
        {
            e.2 = value.to_vec();
        } else {
            list.push((index, suffix.as_bytes().to_vec(), value.to_vec()));
        }
        Ok(())
    }

    /// Add another name for an existing file (shared inode).
    pub fn hardlink(
        &mut self,
        parent: InodeHandle,
        name: impl AsRef<[u8]>,
        target: InodeHandle,
    ) -> Result<()> {
        let name = name.as_ref();
        match self.nodes.get(target.0 as usize).map(|n| &n.kind) {
            Some(NodeKind::Dir { .. }) => {
                return Err(Error::Invalid("hardlink to a directory".into()))
            }
            Some(_) => {}
            None => return Err(Error::Invalid("invalid handle".into())),
        }
        if self.nodes[target.0 as usize].nlink >= LINK_MAX {
            return Err(Error::Invalid(format!("more than {LINK_MAX} links")));
        }
        self.add_entry(parent, name, target.0)?;
        self.nodes[target.0 as usize].nlink += 1;
        Ok(())
    }

    /// Replace an inode's metadata (any live handle, including [`ROOT`]);
    /// the last call before seal wins.
    pub fn set_meta(&mut self, handle: InodeHandle, meta: Meta) -> Result<()> {
        self.nodes
            .get_mut(handle.0 as usize)
            .ok_or_else(|| Error::Invalid("invalid handle".into()))?
            .meta = meta;
        Ok(())
    }

    /// Remove `name` from `dir`. Removing a directory tombstones its
    /// entire subtree; files drop out entirely when their
    /// last link goes.
    pub fn remove(&mut self, dir: InodeHandle, name: impl AsRef<[u8]>) -> Result<()> {
        let name = name.as_ref();
        let (children, index) = self.dir_mut(dir)?;
        let Some(idx) = index.remove(name) else {
            return Err(Error::Invalid(format!("{name:?} not found")));
        };
        children[idx].2 = true;
        let child = children[idx].1;
        match &self.nodes[child as usize].kind {
            NodeKind::Dir { .. } => self.remove_subtree(child),
            _ => self.unlink(child),
        }
        Ok(())
    }

    /// Drop one link of a non-directory node.
    fn unlink(&mut self, slot: u32) {
        let bytes = match &self.nodes[slot as usize].kind {
            NodeKind::File { size } => *size,
            NodeKind::Sparse { size, .. } => *size,
            _ => 0,
        };
        self.nodes[slot as usize].nlink -= 1;
        if self.nodes[slot as usize].nlink == 0 {
            self.declared_bytes -= bytes;
        }
    }

    /// Iterative (worklist) subtree removal: recursion here would put
    /// namespace depth on the call stack, and untrusted inputs (e.g. a
    /// hostile tar with a 50k-component path) control that depth.
    fn remove_subtree(&mut self, dir_slot: u32) {
        let mut stack = vec![dir_slot];
        while let Some(slot) = stack.pop() {
            self.nodes[slot as usize].nlink = 0;
            let NodeKind::Dir { children, index } = &mut self.nodes[slot as usize].kind else {
                unreachable!()
            };
            index.clear();
            let live: Vec<u32> = children
                .iter_mut()
                .filter(|e| !e.2)
                .map(|e| {
                    e.2 = true;
                    e.1
                })
                .collect();
            for child in live {
                match &self.nodes[child as usize].kind {
                    NodeKind::Dir { .. } => stack.push(child),
                    _ => self.unlink(child),
                }
            }
        }
    }

    /// Phase 2: freeze the complete layout. Consumes the builder; every
    /// metadata byte and every future data offset is decided here.
    pub fn seal(self) -> Result<Layout> {
        seal::seal(self)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn builder() -> FsBuilder {
        FsBuilder::new(Options::new(16 << 20, [1; 16], 1_704_067_200)).unwrap()
    }

    fn meta() -> Meta {
        Meta::new(0o644, 0, 0, (1_704_067_200, 0))
    }

    #[test]
    fn name_index_tracks_declarations() {
        let mut b = builder();
        b.file(ROOT, "a", meta(), 0).unwrap();
        // Duplicate rejected via the index.
        assert!(matches!(
            b.file(ROOT, "a", meta(), 0),
            Err(Error::Invalid(_))
        ));
        // Remove deletes the index entry: the name is redeclarable and a
        // second remove errors.
        b.remove(ROOT, "a").unwrap();
        assert!(matches!(b.remove(ROOT, "a"), Err(Error::Invalid(_))));
        b.file(ROOT, "a", meta(), 0).unwrap();
        // Tombstoned entry stays in `children` (declaration order), the
        // live redeclaration is a new position.
        let NodeKind::Dir { children, index } = &b.nodes[0].kind else {
            unreachable!()
        };
        assert_eq!(children.len(), 2);
        assert!(children[0].2, "first declaration is tombstoned");
        assert!(!children[1].2);
        assert_eq!(index.len(), 1);
        let key: Box<[u8]> = b"a"[..].into();
        assert_eq!(index[&key], 1);
    }

    #[test]
    fn subtree_removal_clears_index_and_unlinks() {
        let mut b = builder();
        let d = b.mkdir(ROOT, "d", meta()).unwrap();
        let f = b.file(d, "f", meta(), 10).unwrap();
        b.hardlink(ROOT, "keep", f).unwrap(); // survives subtree removal
        let sub = b.mkdir(d, "sub", meta()).unwrap();
        b.file(sub, "g", meta(), 0).unwrap();
        b.remove(ROOT, "d").unwrap();
        assert_eq!(b.nodes[d.0 as usize].nlink, 0);
        assert_eq!(b.nodes[sub.0 as usize].nlink, 0);
        assert_eq!(b.nodes[f.0 as usize].nlink, 1, "outside hardlink survives");
        // "d" is redeclarable at the root.
        b.mkdir(ROOT, "d", meta()).unwrap();
    }

    #[test]
    fn deep_namespace_removal_does_not_recurse() {
        // 100k-deep chain: stack-safe removal is required.
        let mut b = builder();
        let mut dir = ROOT;
        for i in 0..100_000 {
            dir = b.mkdir(dir, format!("d{i}"), meta()).unwrap();
        }
        b.remove(ROOT, "d0").unwrap();
        assert!(matches!(b.remove(ROOT, "d0"), Err(Error::Invalid(_))));
    }
}
