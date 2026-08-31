use std::collections::BTreeMap;
use std::io::Read;
use std::path::Path;

use a3s_box_core::error::{BoxError, Result};
use mkext4::{Meta, SpecialKind};
use tar::Archive;

use super::tree::{normalize_archive_path, EntryMetadata, GuestPath, LogicalRootfs};

const DEFAULT_MAX_LAYER_BYTES: u64 = 16 * 1024 * 1024 * 1024;
const MAX_LAYER_ENTRIES: usize = 1_000_000;
const PAX_XATTR_PREFIX: &[u8] = b"SCHILY.xattr.";

pub(super) fn apply_layer(
    layer_path: &Path,
    digest: &str,
    compressed_size: u64,
    rootfs: &mut LogicalRootfs,
) -> Result<()> {
    rootfs.begin_layer()?;
    let decoder = crate::oci::layer_reader::open_verified(
        layer_path,
        digest,
        compressed_size,
        rootfs.spool.path(),
    )?;
    let limit = crate::oci::limited_reader::cap_from_env(
        "A3S_BOX_MAX_LAYER_BYTES",
        DEFAULT_MAX_LAYER_BYTES,
    );
    let decoder = crate::oci::limited_reader::LimitedReader::new(decoder, limit);
    let mut archive = Archive::new(decoder);
    let entries = archive.entries().map_err(|error| {
        oci_error(format!(
            "Failed to read OCI layer {}: {error}",
            layer_path.display()
        ))
    })?;

    for (index, entry) in entries.enumerate() {
        if index >= MAX_LAYER_ENTRIES {
            return Err(oci_error(format!(
                "OCI layer {} exceeds the {}-entry limit",
                layer_path.display(),
                MAX_LAYER_ENTRIES
            )));
        }
        let mut entry = entry.map_err(|error| {
            oci_error(format!(
                "Failed to read entry from {}: {error}",
                layer_path.display()
            ))
        })?;
        let raw_path = entry.path_bytes().into_owned();
        let path = normalize_archive_path(&raw_path)?;
        reject_reserved_path(&path)?;
        let Some(name) = path.last() else {
            if entry.header().entry_type().is_dir() {
                rootfs.directory(&path, metadata(&mut entry, &raw_path)?)?;
                continue;
            }
            return Err(oci_error("OCI root entry is not a directory"));
        };

        if name == b".wh..wh..opq" {
            rootfs.opaque(&path[..path.len() - 1].to_vec())?;
            continue;
        }
        if let Some(victim) = name.strip_prefix(b".wh.") {
            if victim.is_empty() {
                return Err(oci_error("OCI whiteout has an empty victim name"));
            }
            let mut victim_path = path[..path.len() - 1].to_vec();
            victim_path.push(victim.to_vec());
            reject_reserved_path(&victim_path)?;
            rootfs.whiteout(&victim_path)?;
            continue;
        }

        let entry_type = entry.header().entry_type();
        if entry_type.is_hard_link() {
            reject_overlay_xattrs(&mut entry, &raw_path)?;
            let target = entry
                .link_name_bytes()
                .ok_or_else(|| oci_error("OCI hardlink has no target"))?;
            let target = normalize_archive_path(&target)?;
            reject_reserved_path(&target)?;
            rootfs.hardlink(&path, &target)?;
            continue;
        }

        let metadata = metadata(&mut entry, &raw_path)?;
        if entry_type.is_dir() {
            rootfs.directory(&path, metadata)?;
        } else if entry_type.is_symlink() {
            let target = entry
                .link_name_bytes()
                .ok_or_else(|| oci_error("OCI symlink has no target"))?
                .into_owned();
            rootfs.symlink(&path, metadata, target)?;
        } else if entry_type.is_file() || entry_type.is_contiguous() || entry_type.is_gnu_sparse() {
            let size = entry.size();
            rootfs.regular(&path, metadata, &mut entry, size)?;
        } else if entry_type.is_character_special() {
            let (major, minor) = device_numbers(entry.header(), &raw_path)?;
            rootfs.special(&path, metadata, SpecialKind::Char { major, minor })?;
        } else if entry_type.is_block_special() {
            let (major, minor) = device_numbers(entry.header(), &raw_path)?;
            rootfs.special(&path, metadata, SpecialKind::Block { major, minor })?;
        } else if entry_type.is_fifo() {
            rootfs.special(&path, metadata, SpecialKind::Fifo)?;
        } else {
            return Err(oci_error(format!(
                "Unsupported OCI entry type {:?} at {}",
                entry_type,
                display_bytes(&raw_path)
            )));
        }
    }
    Ok(())
}

fn metadata<R: Read>(entry: &mut tar::Entry<'_, R>, path: &[u8]) -> Result<EntryMetadata> {
    let header = entry.header();
    let mode = u16::try_from(
        header.mode().map_err(|error| {
            oci_error(format!("Invalid mode at {}: {error}", display_bytes(path)))
        })? & 0o7777,
    )
    .map_err(|_| oci_error("OCI mode exceeds the ext4 metadata range"))?;
    let uid =
        u32::try_from(header.uid().map_err(|error| {
            oci_error(format!("Invalid uid at {}: {error}", display_bytes(path)))
        })?)
        .map_err(|_| oci_error(format!("OCI uid exceeds u32 at {}", display_bytes(path))))?;
    let gid =
        u32::try_from(header.gid().map_err(|error| {
            oci_error(format!("Invalid gid at {}: {error}", display_bytes(path)))
        })?)
        .map_err(|_| oci_error(format!("OCI gid exceeds u32 at {}", display_bytes(path))))?;
    let mtime = i64::try_from(header.mtime().map_err(|error| {
        oci_error(format!("Invalid mtime at {}: {error}", display_bytes(path)))
    })?)
    .map_err(|_| oci_error(format!("OCI mtime exceeds i64 at {}", display_bytes(path))))?;
    let xattrs = collect_xattrs(entry, path)?;
    Ok(EntryMetadata {
        meta: Meta::new(mode, uid, gid, (mtime, 0)),
        xattrs,
    })
}

fn collect_xattrs<R: Read>(
    entry: &mut tar::Entry<'_, R>,
    path: &[u8],
) -> Result<BTreeMap<String, Vec<u8>>> {
    let mut xattrs = BTreeMap::new();
    let Some(extensions) = entry.pax_extensions().map_err(|error| {
        oci_error(format!(
            "Failed to inspect PAX metadata at {}: {error}",
            display_bytes(path)
        ))
    })?
    else {
        return Ok(xattrs);
    };
    for extension in extensions {
        let extension = extension.map_err(|error| {
            oci_error(format!(
                "Invalid PAX metadata at {}: {error}",
                display_bytes(path)
            ))
        })?;
        let Some(name) = extension.key_bytes().strip_prefix(PAX_XATTR_PREFIX) else {
            continue;
        };
        if name.starts_with(b"trusted.overlay.") || name.starts_with(b"user.overlay.") {
            return Err(oci_error(format!(
                "OCI entry {} contains reserved overlayfs metadata",
                display_bytes(path)
            )));
        }
        let name = std::str::from_utf8(name).map_err(|_| {
            oci_error(format!(
                "OCI entry {} has a non-UTF-8 xattr name",
                display_bytes(path)
            ))
        })?;
        xattrs.insert(name.to_string(), extension.value_bytes().to_vec());
    }
    Ok(xattrs)
}

fn reject_overlay_xattrs<R: Read>(entry: &mut tar::Entry<'_, R>, path: &[u8]) -> Result<()> {
    collect_xattrs(entry, path).map(|_| ())
}

fn device_numbers(header: &tar::Header, path: &[u8]) -> Result<(u32, u32)> {
    let major = header
        .device_major()
        .map_err(|error| {
            oci_error(format!(
                "Invalid device major at {}: {error}",
                display_bytes(path)
            ))
        })?
        .ok_or_else(|| oci_error("OCI device entry has no major number"))?;
    let minor = header
        .device_minor()
        .map_err(|error| {
            oci_error(format!(
                "Invalid device minor at {}: {error}",
                display_bytes(path)
            ))
        })?
        .ok_or_else(|| oci_error("OCI device entry has no minor number"))?;
    Ok((major, minor))
}

fn reject_reserved_path(path: &GuestPath) -> Result<()> {
    let Some(first) = path.first() else {
        return Ok(());
    };
    let reserved = [
        b".a3s_image_metadata_v1.json".as_slice(),
        b".a3s_image_metadata_v1.json.tmp".as_slice(),
        b".a3s_rootfs_metadata_v1.json".as_slice(),
        b".a3s_rootfs_metadata_v1.json.tmp".as_slice(),
        b".a3s_rootfs_metadata_v1.previous.json".as_slice(),
    ];
    if reserved.contains(&first.as_slice()) {
        return Err(oci_error(format!(
            "OCI layer contains reserved internal path {}",
            display_bytes(first)
        )));
    }
    Ok(())
}

fn display_bytes(value: &[u8]) -> String {
    String::from_utf8_lossy(value).into_owned()
}

fn oci_error(message: impl Into<String>) -> BoxError {
    BoxError::OciImageError(message.into())
}
