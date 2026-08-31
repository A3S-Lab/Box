use std::collections::{BTreeMap, HashMap};

use a3s_box_core::error::{BoxError, Result};
use mkext4::{FsBuilder, InodeHandle, SparseSeg, ROOT};

use super::super::ext4::sparse::{FileFill, SourceSegment};
use super::super::ext4::{new_ext4_fs_builder, Ext4ArtifactOptions};
use super::tree::{build_error, GuestPath, LogicalRootfs, NodeKind};

impl LogicalRootfs {
    pub(super) fn declare(
        &self,
        options: Ext4ArtifactOptions,
    ) -> Result<(FsBuilder, Vec<FileFill>)> {
        let mut builder = new_ext4_fs_builder(options)?;
        let root = &self.nodes[&0];
        builder.set_meta(ROOT, root.meta).map_err(mkext4_error)?;
        apply_xattrs(&mut builder, ROOT, &root.xattrs)?;

        let mut paths = self
            .entries
            .iter()
            .filter(|(path, _)| !path.is_empty())
            .collect::<Vec<_>>();
        paths.sort_by(|(left, _), (right, _)| {
            left.len().cmp(&right.len()).then_with(|| left.cmp(right))
        });
        let mut path_handles = BTreeMap::from([(Vec::new(), ROOT)]);
        let mut inode_handles = HashMap::new();
        let mut fills = Vec::new();
        for (path, entry) in paths {
            let parent_path = path[..path.len() - 1].to_vec();
            let parent = *path_handles.get(&parent_path).ok_or_else(|| {
                build_error(format!(
                    "Logical OCI tree has a missing parent for {}",
                    display_guest_path(path)
                ))
            })?;
            let name = path.last().expect("non-root path");
            if let Some(handle) = inode_handles.get(&entry.node).copied() {
                builder
                    .hardlink(parent, name, handle)
                    .map_err(mkext4_error)?;
                path_handles.insert(path.clone(), handle);
                continue;
            }
            let node = &self.nodes[&entry.node];
            let handle = match &node.kind {
                NodeKind::Directory => builder
                    .mkdir(parent, name, node.meta)
                    .map_err(mkext4_error)?,
                NodeKind::Regular {
                    source,
                    size,
                    sparse,
                } => {
                    if let Some(sparse) = sparse {
                        let segments = sparse
                            .segments
                            .iter()
                            .map(|segment| match *segment {
                                SourceSegment::Data { len } => SparseSeg::Data(len),
                                SourceSegment::Hole { len } => SparseSeg::Hole(len),
                            })
                            .collect::<Vec<_>>();
                        let handle = builder
                            .file_sparse(parent, name, node.meta, &segments)
                            .map_err(mkext4_error)?;
                        if !sparse.data_ranges.is_empty() {
                            fills.push(FileFill::Sparse {
                                handle,
                                path: source.clone(),
                                ranges: sparse.data_ranges.clone(),
                            });
                        }
                        handle
                    } else {
                        let handle = builder
                            .file(parent, name, node.meta, *size)
                            .map_err(mkext4_error)?;
                        if *size > 0 {
                            fills.push(FileFill::Dense {
                                handle,
                                path: source.clone(),
                            });
                        }
                        handle
                    }
                }
                NodeKind::Symlink { target } => builder
                    .symlink(parent, name, target, node.meta)
                    .map_err(mkext4_error)?,
                NodeKind::Special(kind) => builder
                    .mknod(parent, name, node.meta, *kind)
                    .map_err(mkext4_error)?,
            };
            apply_xattrs(&mut builder, handle, &node.xattrs)?;
            inode_handles.insert(entry.node, handle);
            path_handles.insert(path.clone(), handle);
        }
        Ok((builder, fills))
    }
}

fn display_guest_path(path: &GuestPath) -> String {
    let mut rendered = String::from("/");
    rendered.push_str(
        &path
            .iter()
            .map(|component| String::from_utf8_lossy(component))
            .collect::<Vec<_>>()
            .join("/"),
    );
    rendered
}

fn apply_xattrs(
    builder: &mut FsBuilder,
    handle: InodeHandle,
    xattrs: &BTreeMap<String, Vec<u8>>,
) -> Result<()> {
    for (name, value) in xattrs {
        if !super::super::ext4::is_linux_xattr_name(name) {
            return Err(build_error(format!(
                "Unsupported Linux xattr name {name:?}"
            )));
        }
        builder
            .set_xattr(handle, name, value)
            .map_err(mkext4_error)?;
    }
    Ok(())
}

fn mkext4_error(error: mkext4::Error) -> BoxError {
    build_error(format!("ext4 artifact builder failed: {error}"))
}
