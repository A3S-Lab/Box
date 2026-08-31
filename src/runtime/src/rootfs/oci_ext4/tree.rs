use std::collections::{BTreeMap, HashMap, VecDeque};
use std::io::Read;
use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use mkext4::{Meta, SpecialKind};

use super::super::ext4::sparse::SparseLayout;

pub(super) type GuestPath = Vec<Vec<u8>>;
pub(super) type NodeId = u64;

#[derive(Clone, Copy)]
pub(super) struct NamespaceEntry {
    pub(super) node: NodeId,
    generation: u64,
}

const MAX_SYMLINK_HOPS: usize = 40;
const MAX_LOGICAL_ENTRIES: usize = 1_000_000;
const MAX_GUEST_PATH_BYTES: usize = 4095;

#[derive(Clone)]
pub(super) struct EntryMetadata {
    pub(super) meta: Meta,
    pub(super) xattrs: BTreeMap<String, Vec<u8>>,
}

pub(super) enum NodeKind {
    Directory,
    Regular {
        source: PathBuf,
        size: u64,
        sparse: Option<SparseLayout>,
    },
    Symlink {
        target: Vec<u8>,
    },
    Special(SpecialKind),
}

pub(super) struct Node {
    pub(super) kind: NodeKind,
    pub(super) meta: Meta,
    pub(super) xattrs: BTreeMap<String, Vec<u8>>,
    links: usize,
}

#[derive(Debug)]
enum ResolveComponent {
    Parent,
    Normal(Vec<u8>),
}

/// A Linux namespace represented only by raw guest paths and opaque content
/// files. No guest directory name is ever materialized on the host.
pub(super) struct LogicalRootfs {
    pub(super) spool: tempfile::TempDir,
    pub(super) spool_sequence: u64,
    next_node: NodeId,
    current_generation: u64,
    pub(super) entries: BTreeMap<GuestPath, NamespaceEntry>,
    pub(super) nodes: HashMap<NodeId, Node>,
}

impl LogicalRootfs {
    pub(super) fn new(spool_parent: &Path) -> Result<Self> {
        std::fs::create_dir_all(spool_parent).map_err(BoxError::IoError)?;
        let spool = tempfile::Builder::new()
            .prefix(super::CONTENT_STAGING_PREFIX)
            .tempdir_in(spool_parent)
            .map_err(|error| {
                BoxError::BuildError(format!(
                    "Failed to create OCI ext4 content spool in {}: {error}",
                    spool_parent.display()
                ))
            })?;
        let root = Node {
            kind: NodeKind::Directory,
            meta: canonical_meta(0o755),
            xattrs: BTreeMap::new(),
            links: 1,
        };
        Ok(Self {
            spool,
            spool_sequence: 0,
            next_node: 1,
            current_generation: 0,
            entries: BTreeMap::from([(
                Vec::new(),
                NamespaceEntry {
                    node: 0,
                    generation: 0,
                },
            )]),
            nodes: HashMap::from([(0, root)]),
        })
    }

    pub(super) fn begin_layer(&mut self) -> Result<()> {
        self.current_generation = self
            .current_generation
            .checked_add(1)
            .ok_or_else(|| build_error("OCI layer generation overflow"))?;
        Ok(())
    }

    pub(super) fn directory(
        &mut self,
        archive_path: &GuestPath,
        metadata: EntryMetadata,
    ) -> Result<()> {
        if archive_path.is_empty() {
            let root = self.nodes.get_mut(&0).expect("logical root inode");
            root.meta = metadata.meta;
            root.xattrs.extend(metadata.xattrs);
            return Ok(());
        }
        let destination = self.resolve_destination(archive_path, true)?;
        if let Some(entry) = self.entries.get(&destination).copied() {
            if matches!(self.nodes[&entry.node].kind, NodeKind::Directory) {
                let node = self.nodes.get_mut(&entry.node).expect("directory inode");
                node.meta = metadata.meta;
                node.xattrs.extend(metadata.xattrs);
                self.entries
                    .get_mut(&destination)
                    .expect("directory namespace entry")
                    .generation = self.current_generation;
                return Ok(());
            }
        }
        self.replace_with_new(
            destination,
            NodeKind::Directory,
            metadata.meta,
            metadata.xattrs,
        )?;
        Ok(())
    }

    pub(super) fn regular<R: Read>(
        &mut self,
        archive_path: &GuestPath,
        metadata: EntryMetadata,
        reader: &mut R,
        expected_size: u64,
    ) -> Result<()> {
        let destination = self.resolve_destination(archive_path, true)?;
        let (source, sparse) = self.spool(reader, expected_size)?;
        self.replace_with_new(
            destination,
            NodeKind::Regular {
                source,
                size: expected_size,
                sparse,
            },
            metadata.meta,
            metadata.xattrs,
        )?;
        Ok(())
    }

    pub(super) fn symlink(
        &mut self,
        archive_path: &GuestPath,
        metadata: EntryMetadata,
        target: Vec<u8>,
    ) -> Result<()> {
        validate_symlink_target(&target)?;
        let destination = self.resolve_destination(archive_path, true)?;
        self.replace_with_new(
            destination,
            NodeKind::Symlink { target },
            metadata.meta,
            metadata.xattrs,
        )?;
        Ok(())
    }

    pub(super) fn special(
        &mut self,
        archive_path: &GuestPath,
        metadata: EntryMetadata,
        kind: SpecialKind,
    ) -> Result<()> {
        let destination = self.resolve_destination(archive_path, true)?;
        self.replace_with_new(
            destination,
            NodeKind::Special(kind),
            metadata.meta,
            metadata.xattrs,
        )?;
        Ok(())
    }

    pub(super) fn hardlink(&mut self, archive_path: &GuestPath, target: &GuestPath) -> Result<()> {
        let target = self
            .resolve_existing_entry(target, false)?
            .ok_or_else(|| build_error("OCI hardlink target does not exist"))?;
        let target_id = self.entries[&target].node;
        if matches!(self.nodes[&target_id].kind, NodeKind::Directory) {
            return Err(build_error("OCI hardlink target is a directory"));
        }
        let destination = self.resolve_destination(archive_path, true)?;
        if destination == target {
            return Ok(());
        }
        if !self.entries.contains_key(&destination) && self.entries.len() >= MAX_LOGICAL_ENTRIES {
            return Err(build_error("OCI rootfs exceeds the logical entry limit"));
        }
        self.nodes
            .get_mut(&target_id)
            .expect("hardlink target inode")
            .links += 1;
        self.remove_tree(&destination, true)?;
        self.entries.insert(
            destination,
            NamespaceEntry {
                node: target_id,
                generation: self.current_generation,
            },
        );
        Ok(())
    }

    pub(super) fn whiteout(&mut self, victim: &GuestPath) -> Result<()> {
        if let Some(path) = self.resolve_existing_entry(victim, false)? {
            self.remove_older_tree(&path, true)?;
        }
        Ok(())
    }

    pub(super) fn opaque(&mut self, directory: &GuestPath) -> Result<()> {
        if let Some(path) = self.resolve_existing_directory(directory)? {
            self.remove_older_tree(&path, false)?;
        }
        Ok(())
    }

    fn resolve_destination(&mut self, path: &GuestPath, create: bool) -> Result<GuestPath> {
        let (name, parent) = path
            .split_last()
            .ok_or_else(|| build_error("OCI entry has no filename"))?;
        let mut resolved = self
            .resolve_directory(&parent.to_vec(), create)?
            .ok_or_else(|| build_error("OCI entry parent does not exist"))?;
        self.touch_path_and_ancestors(&resolved);
        resolved.push(name.clone());
        Ok(resolved)
    }

    fn resolve_existing_entry(
        &mut self,
        path: &GuestPath,
        follow_final: bool,
    ) -> Result<Option<GuestPath>> {
        let resolved = if follow_final {
            self.resolve_path(path, true, false)?
        } else if path.is_empty() {
            Some(Vec::new())
        } else {
            let (name, parent) = path
                .split_last()
                .ok_or_else(|| build_error("OCI entry has no filename"))?;
            let Some(mut parent) = self.resolve_directory(&parent.to_vec(), false)? else {
                return Ok(None);
            };
            parent.push(name.clone());
            self.entries.contains_key(&parent).then_some(parent)
        };
        Ok(resolved.filter(|path| self.entries.contains_key(path)))
    }

    pub(super) fn resolve_existing_directory(
        &mut self,
        path: &GuestPath,
    ) -> Result<Option<GuestPath>> {
        let Some(path) = self.resolve_path(path, true, false)? else {
            return Ok(None);
        };
        let id = self.entries[&path].node;
        if matches!(self.nodes[&id].kind, NodeKind::Directory) {
            Ok(Some(path))
        } else {
            Err(build_error("OCI path is not a directory"))
        }
    }

    pub(super) fn resolve_directory(
        &mut self,
        path: &GuestPath,
        create: bool,
    ) -> Result<Option<GuestPath>> {
        let resolved = self.resolve_path(path, true, create)?;
        if let Some(path) = resolved.as_ref() {
            if create && !self.entries.contains_key(path) {
                self.replace_with_new(
                    path.clone(),
                    NodeKind::Directory,
                    canonical_meta(0o755),
                    BTreeMap::new(),
                )?;
            }
            let id = self.entries[path].node;
            if !matches!(self.nodes[&id].kind, NodeKind::Directory) {
                return Err(build_error("OCI path is not a directory"));
            }
        }
        Ok(resolved)
    }

    pub(super) fn resolve_path(
        &mut self,
        path: &GuestPath,
        follow_final: bool,
        create_missing: bool,
    ) -> Result<Option<GuestPath>> {
        let mut pending = path
            .iter()
            .cloned()
            .map(ResolveComponent::Normal)
            .collect::<VecDeque<_>>();
        let mut resolved = GuestPath::new();
        let mut hops = 0;
        while let Some(component) = pending.pop_front() {
            let ResolveComponent::Normal(component) = component else {
                if resolved.pop().is_none() {
                    return Err(build_error("Rootfs symlink target escapes the guest root"));
                }
                continue;
            };
            let mut candidate = resolved.clone();
            candidate.push(component.clone());
            let is_final = pending.is_empty();
            let Some(entry) = self.entries.get(&candidate).copied() else {
                if !create_missing {
                    return Ok(None);
                }
                if !is_final || !follow_final {
                    self.replace_with_new(
                        candidate.clone(),
                        NodeKind::Directory,
                        canonical_meta(0o755),
                        BTreeMap::new(),
                    )?;
                }
                resolved.push(component);
                continue;
            };
            match &self.nodes[&entry.node].kind {
                NodeKind::Symlink { target } if !is_final || follow_final => {
                    hops += 1;
                    if hops > MAX_SYMLINK_HOPS {
                        return Err(build_error("Too many rootfs symlink hops"));
                    }
                    let (absolute, target) = parse_symlink_target(target)?;
                    if absolute {
                        resolved.clear();
                    }
                    for component in target.into_iter().rev() {
                        pending.push_front(component);
                    }
                }
                NodeKind::Directory => resolved.push(component),
                NodeKind::Regular { .. } | NodeKind::Special(_) if is_final && follow_final => {
                    resolved.push(component)
                }
                _ => return Err(build_error("Rootfs path component is not a directory")),
            }
        }
        Ok(Some(resolved))
    }

    pub(super) fn replace_with_new(
        &mut self,
        path: GuestPath,
        kind: NodeKind,
        meta: Meta,
        xattrs: BTreeMap<String, Vec<u8>>,
    ) -> Result<()> {
        self.remove_tree(&path, true)?;
        if self.entries.len() >= MAX_LOGICAL_ENTRIES {
            return Err(build_error("OCI rootfs exceeds the logical entry limit"));
        }
        let id = self.next_node;
        self.next_node = self
            .next_node
            .checked_add(1)
            .ok_or_else(|| build_error("OCI inode identity overflow"))?;
        self.nodes.insert(
            id,
            Node {
                kind,
                meta,
                xattrs,
                links: 1,
            },
        );
        self.entries.insert(
            path,
            NamespaceEntry {
                node: id,
                generation: self.current_generation,
            },
        );
        Ok(())
    }

    fn remove_tree(&mut self, path: &GuestPath, include_root: bool) -> Result<()> {
        let victims = self
            .entries
            .keys()
            .filter(|candidate| {
                candidate.starts_with(path) && (include_root || candidate.len() > path.len())
            })
            .cloned()
            .collect::<Vec<_>>();
        for victim in victims {
            let Some(entry) = self.entries.remove(&victim) else {
                continue;
            };
            self.release_node(entry.node)?;
        }
        Ok(())
    }

    fn remove_older_tree(&mut self, path: &GuestPath, include_root: bool) -> Result<()> {
        let victims = self
            .entries
            .iter()
            .filter(|(candidate, entry)| {
                candidate.starts_with(path)
                    && (include_root || candidate.len() > path.len())
                    && entry.generation < self.current_generation
            })
            .map(|(path, _)| path.clone())
            .collect::<Vec<_>>();
        for victim in victims {
            let Some(entry) = self.entries.remove(&victim) else {
                continue;
            };
            self.release_node(entry.node)?;
        }
        Ok(())
    }

    fn release_node(&mut self, node_id: NodeId) -> Result<()> {
        let node = self
            .nodes
            .get_mut(&node_id)
            .ok_or_else(|| build_error("Logical OCI namespace references a missing inode"))?;
        node.links = node
            .links
            .checked_sub(1)
            .ok_or_else(|| build_error("Logical OCI inode link count underflow"))?;
        if node.links != 0 {
            return Ok(());
        }
        let node = self
            .nodes
            .remove(&node_id)
            .ok_or_else(|| build_error("Logical OCI inode disappeared during release"))?;
        if let NodeKind::Regular { source, .. } = node.kind {
            std::fs::remove_file(&source).map_err(|error| {
                build_error(format!(
                    "Failed to release superseded OCI content {}: {error}",
                    source.display()
                ))
            })?;
        }
        Ok(())
    }

    fn touch_path_and_ancestors(&mut self, path: &GuestPath) {
        for depth in 0..=path.len() {
            if let Some(entry) = self.entries.get_mut(&path[..depth]) {
                entry.generation = self.current_generation;
            }
        }
    }
}

pub(super) fn normalize_archive_path(raw: &[u8]) -> Result<GuestPath> {
    if raw.starts_with(b"/") || raw.contains(&0) || raw.len() > MAX_GUEST_PATH_BYTES {
        return Err(build_error(
            "OCI layer path must be a relative NUL-free Linux path of at most 4095 bytes",
        ));
    }
    let mut path = GuestPath::new();
    for component in raw.split(|byte| *byte == b'/') {
        match component {
            b"" | b"." => {}
            b".." => return Err(build_error("OCI layer path contains '..'")),
            value if value.len() > 255 => {
                return Err(build_error("OCI filename exceeds 255 bytes"))
            }
            value => path.push(value.to_vec()),
        }
    }
    Ok(path)
}

pub(super) fn guest_path(path: &str) -> Result<GuestPath> {
    normalize_archive_path(path.as_bytes())
}

fn parse_symlink_target(raw: &[u8]) -> Result<(bool, Vec<ResolveComponent>)> {
    validate_symlink_target(raw)?;
    let absolute = raw.starts_with(b"/");
    let mut components = Vec::new();
    for component in raw.split(|byte| *byte == b'/') {
        match component {
            b"" | b"." => {}
            b".." => components.push(ResolveComponent::Parent),
            value => components.push(ResolveComponent::Normal(value.to_vec())),
        }
    }
    Ok((absolute, components))
}

fn validate_symlink_target(target: &[u8]) -> Result<()> {
    if target.is_empty() || target.len() > 4095 || target.contains(&0) {
        return Err(build_error(
            "OCI symlink target must be 1..=4095 NUL-free bytes",
        ));
    }
    Ok(())
}

pub(super) fn canonical_meta(mode: u16) -> Meta {
    Meta::new(mode, 0, 0, (0, 0))
}

pub(super) fn build_error(message: impl Into<String>) -> BoxError {
    BoxError::BuildError(message.into())
}
