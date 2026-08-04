//! Conversion from Box image metadata to the portable OCI guest contract.

use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::Path;

use a3s_box_core::rootfs_metadata::{
    RootfsEntryKind, RootfsMetadataManifest, IMAGE_ROOTFS_METADATA_PATH,
    IMAGE_ROOTFS_METADATA_TEMP_PATH,
};
use a3s_box_core::{BoxError, Result};
use a3s_oci_sdk::{
    PortableRootfsEntryKind, PortableRootfsMetadataEntry, PortableRootfsMetadataManifest,
    PORTABLE_ROOTFS_METADATA_FILE, PORTABLE_ROOTFS_METADATA_MAX_BYTES,
    PORTABLE_ROOTFS_METADATA_MAX_ENTRIES,
};
use base64::Engine as _;
use oci_spec::runtime::Spec;

const MAX_SOURCE_METADATA_BYTES: u64 = 64 * 1024 * 1024;
const MAX_ENCODED_PATH_BYTES: usize = 16 * 1024;
const MAX_DECODED_PATH_BYTES: usize = 4_096;
const PORTABLE_ROOTFS_METADATA_TEMP_FILE: &str = ".a3s-oci-rootfs-metadata.v1.json.tmp";

const RUNTIME_INTERNAL_ROOT_ENTRIES: &[&[u8]] = &[
    b".a3s-box-env",
    b".a3s-box-exec.json",
    b".a3s_exit_code",
    b".a3s_host_live_logs_drained",
    b".a3s_host_result_collected",
    b".a3s_image_metadata_v1.json",
    b".a3s_image_metadata_v1.json.tmp",
    b".a3s_rootfs_metadata_v1.json",
    b".a3s_rootfs_metadata_v1.json.tmp",
    b".a3s_rootfs_metadata_v1.previous.json",
    b".a3s-oci-rootfs-metadata.v1.json",
    b".a3s-oci-rootfs-metadata.v1.json.tmp",
    b"guest-init.stderr.log",
    b"guest-init.stdout.log",
    b"init-rust.log",
    b"init.krun.log",
    b"init.trace.log",
];

/// Publish the one-shot OCI metadata manifest inside a portable rootfs.
///
/// The Box image manifest is consumed only after the replacement contract is
/// durably encoded. Callers must invoke this on the copied handoff rootfs, not
/// on Box's retained image/cache generation.
pub(crate) fn publish_portable_rootfs_metadata(rootfs: &Path) -> Result<()> {
    validate_plain_directory(rootfs, "portable rootfs")?;

    let source = rootfs.join(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/'));
    let source_temporary = rootfs.join(IMAGE_ROOTFS_METADATA_TEMP_PATH.trim_start_matches('/'));
    ensure_absent(&source_temporary, "Box image metadata temporary")?;

    let mut file = open_plain_file(&source, "Box image metadata")?;
    let length = file.metadata().map_err(BoxError::IoError)?.len();
    if length > MAX_SOURCE_METADATA_BYTES {
        return Err(metadata_error(format!(
            "Box image metadata exceeds the {MAX_SOURCE_METADATA_BYTES}-byte input limit: {}",
            source.display()
        )));
    }
    let mut bytes = Vec::with_capacity(length as usize);
    Read::by_ref(&mut file)
        .take(MAX_SOURCE_METADATA_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(BoxError::IoError)?;
    if bytes.len() as u64 > MAX_SOURCE_METADATA_BYTES {
        return Err(metadata_error(
            "Box image metadata grew beyond its input limit while reading".to_string(),
        ));
    }

    let source_manifest: RootfsMetadataManifest =
        serde_json::from_slice(&bytes).map_err(|error| {
            metadata_error(format!(
                "invalid Box image metadata {}: {error}",
                source.display()
            ))
        })?;
    source_manifest.validate().map_err(metadata_error)?;
    let encoded = encode_portable_manifest(source_manifest)?;

    let destination = rootfs.join(PORTABLE_ROOTFS_METADATA_FILE);
    let temporary = rootfs.join(PORTABLE_ROOTFS_METADATA_TEMP_FILE);
    ensure_absent(&destination, "portable rootfs metadata")?;
    ensure_absent(&temporary, "portable rootfs metadata temporary")?;

    let publish = (|| -> Result<()> {
        let mut output = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&temporary)
            .map_err(BoxError::IoError)?;
        output.write_all(&encoded).map_err(BoxError::IoError)?;
        output.sync_all().map_err(BoxError::IoError)?;
        drop(output);
        std::fs::rename(&temporary, &destination).map_err(BoxError::IoError)?;
        remove_plain_file(&source, "consumed Box image metadata")?;
        sync_directory(rootfs).map_err(BoxError::IoError)
    })();

    if let Err(error) = publish {
        let _ = remove_path_no_follow(&temporary);
        let _ = remove_path_no_follow(&destination);
        return Err(error);
    }
    Ok(())
}

/// Copy a prepared Box rootfs and atomically publish one operation-scoped OCI bundle.
pub(crate) fn publish_portable_bundle(
    source_rootfs: &Path,
    spec: &Spec,
    bundle_directory: &Path,
) -> Result<()> {
    let operation_directory = bundle_directory.parent().ok_or_else(|| {
        metadata_error(format!(
            "portable OCI bundle has no operation parent: {}",
            bundle_directory.display()
        ))
    })?;
    std::fs::create_dir_all(operation_directory).map_err(BoxError::IoError)?;
    validate_plain_directory(operation_directory, "portable OCI operation directory")?;
    ensure_absent(bundle_directory, "portable OCI bundle")?;

    let pending = operation_directory.join("bundle.pending");
    ensure_absent(&pending, "portable OCI bundle temporary")?;
    std::fs::create_dir(&pending).map_err(BoxError::IoError)?;

    let publish = (|| -> Result<()> {
        let rootfs = pending.join("rootfs");
        crate::cache::layer_cache::copy_dir_recursive(source_rootfs, &rootfs)?;
        make_owner_writable(&rootfs)?;
        publish_portable_rootfs_metadata(&rootfs)?;

        let config = pending.join("config.json");
        let encoded = serde_json::to_vec_pretty(spec)
            .map_err(|error| metadata_error(format!("failed to encode OCI bundle: {error}")))?;
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&config)
            .map_err(BoxError::IoError)?;
        file.write_all(&encoded).map_err(BoxError::IoError)?;
        file.sync_all().map_err(BoxError::IoError)?;
        drop(file);
        sync_directory(&pending).map_err(BoxError::IoError)?;

        std::fs::rename(&pending, bundle_directory).map_err(BoxError::IoError)?;
        sync_directory(operation_directory).map_err(BoxError::IoError)
    })();

    if let Err(error) = publish {
        let _ = remove_plain_directory_tree(&pending, operation_directory);
        return Err(error);
    }
    Ok(())
}

fn encode_portable_manifest(source: RootfsMetadataManifest) -> Result<Vec<u8>> {
    if source.entries.len() > PORTABLE_ROOTFS_METADATA_MAX_ENTRIES {
        return Err(metadata_error(format!(
            "Box image metadata has {} entries, exceeding the portable {}-entry limit",
            source.entries.len(),
            PORTABLE_ROOTFS_METADATA_MAX_ENTRIES
        )));
    }

    let mut unique = HashSet::with_capacity(source.entries.len());
    let mut entries = Vec::with_capacity(source.entries.len());
    for entry in source.entries {
        if entry.uid > u32::MAX as u64 || entry.gid > u32::MAX as u64 {
            return Err(metadata_error(
                "Box image metadata uid/gid exceeds the portable Linux ID range".to_string(),
            ));
        }
        let normalized = decode_normalized_path(&entry.path_base64)?;
        if normalized
            .first()
            .is_some_and(|first| RUNTIME_INTERNAL_ROOT_ENTRIES.contains(&first.as_slice()))
            || !unique.insert(normalized)
        {
            return Err(metadata_error(
                "Box image metadata contains a duplicate or runtime-internal path".to_string(),
            ));
        }

        let kind = match entry.kind {
            RootfsEntryKind::Directory => PortableRootfsEntryKind::Directory,
            RootfsEntryKind::Regular => PortableRootfsEntryKind::Regular,
            RootfsEntryKind::Symlink => PortableRootfsEntryKind::Symlink,
        };
        validate_link_target(kind, entry.link_target_base64.as_deref())?;
        entries.push(PortableRootfsMetadataEntry {
            path_base64: entry.path_base64,
            kind,
            mode: entry.mode,
            uid: entry.uid,
            gid: entry.gid,
            mtime: entry.mtime,
            size: entry.size,
            link_target_base64: entry.link_target_base64,
        });
    }

    let manifest = PortableRootfsMetadataManifest::new(entries);
    manifest.validate().map_err(metadata_error)?;
    let encoded = serde_json::to_vec(&manifest)
        .map_err(|error| metadata_error(format!("failed to encode portable metadata: {error}")))?;
    if encoded.len() as u64 > PORTABLE_ROOTFS_METADATA_MAX_BYTES {
        return Err(metadata_error(format!(
            "portable rootfs metadata has {} bytes, exceeding the {}-byte limit",
            encoded.len(),
            PORTABLE_ROOTFS_METADATA_MAX_BYTES
        )));
    }
    Ok(encoded)
}

fn decode_normalized_path(encoded: &str) -> Result<Vec<Vec<u8>>> {
    if encoded.len() > MAX_ENCODED_PATH_BYTES {
        return Err(metadata_error(
            "Box image metadata path is too large".to_string(),
        ));
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| metadata_error(format!("invalid Box image metadata path: {error}")))?;
    if raw.is_empty() || raw.len() > MAX_DECODED_PATH_BYTES || raw.contains(&0) {
        return Err(metadata_error(
            "Box image metadata path is empty, too large, or contains NUL".to_string(),
        ));
    }
    if raw.starts_with(b"/") {
        return Err(metadata_error(
            "Box image metadata path must be relative".to_string(),
        ));
    }

    let mut normalized = Vec::new();
    for component in raw.split(|byte| *byte == b'/') {
        if component.is_empty() || component == b"." {
            continue;
        }
        if component == b".." {
            return Err(metadata_error(
                "Box image metadata path contains a parent component".to_string(),
            ));
        }
        normalized.push(component.to_vec());
    }
    Ok(normalized)
}

fn validate_link_target(kind: PortableRootfsEntryKind, encoded: Option<&str>) -> Result<()> {
    if kind != PortableRootfsEntryKind::Symlink {
        if encoded.is_some() {
            return Err(metadata_error(
                "non-symlink Box image metadata contains a link target".to_string(),
            ));
        }
        return Ok(());
    }
    let encoded = encoded.ok_or_else(|| {
        metadata_error("Box image symlink metadata is missing its target".to_string())
    })?;
    if encoded.len() > MAX_ENCODED_PATH_BYTES {
        return Err(metadata_error(
            "Box image symlink target is too large".to_string(),
        ));
    }
    let raw = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| metadata_error(format!("invalid Box image symlink target: {error}")))?;
    if raw.len() > MAX_DECODED_PATH_BYTES || raw.contains(&0) {
        return Err(metadata_error(
            "Box image symlink target is too large or contains NUL".to_string(),
        ));
    }
    Ok(())
}

fn validate_plain_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(BoxError::IoError)?;
    if !metadata.is_dir() || metadata_is_reparse_point(&metadata) {
        return Err(metadata_error(format!(
            "{label} is not a plain directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn open_plain_file(path: &Path, label: &str) -> Result<File> {
    #[cfg(unix)]
    let file = {
        use std::os::unix::fs::OpenOptionsExt as _;
        OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW)
            .open(path)
    };
    #[cfg(windows)]
    let file = a3s_box_core::windows_file::open_regular_file(path, None).map(|opened| opened.0);
    #[cfg(not(any(unix, windows)))]
    let file = File::open(path);

    let file = file.map_err(|error| {
        metadata_error(format!(
            "failed to open {label} {} without following links: {error}",
            path.display()
        ))
    })?;
    let metadata = file.metadata().map_err(BoxError::IoError)?;
    if !metadata.is_file() || metadata_is_reparse_point(&metadata) {
        return Err(metadata_error(format!(
            "{label} is not a plain file: {}",
            path.display()
        )));
    }
    Ok(file)
}

fn remove_plain_file(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(BoxError::IoError)?;
    if !metadata.is_file() || metadata_is_reparse_point(&metadata) {
        return Err(metadata_error(format!(
            "{label} is not a plain file: {}",
            path.display()
        )));
    }
    std::fs::remove_file(path).map_err(BoxError::IoError)
}

fn ensure_absent(path: &Path, label: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(metadata_error(format!(
            "refusing to overwrite {label}: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(BoxError::IoError(error)),
    }
}

#[cfg(unix)]
fn make_owner_writable(path: &Path) -> Result<()> {
    use std::os::unix::fs::{MetadataExt as _, PermissionsExt as _};

    let metadata = std::fs::symlink_metadata(path).map_err(BoxError::IoError)?;
    std::fs::set_permissions(
        path,
        std::fs::Permissions::from_mode((metadata.mode() & 0o7777) | 0o200),
    )
    .map_err(BoxError::IoError)
}

#[cfg(not(unix))]
fn make_owner_writable(_path: &Path) -> Result<()> {
    Ok(())
}

fn remove_plain_directory_tree(path: &Path, expected_parent: &Path) -> std::io::Result<()> {
    if path.parent() != Some(expected_parent) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "refusing to remove unscoped OCI bundle temporary: {}",
                path.display()
            ),
        ));
    }
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() || metadata_is_reparse_point(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "OCI bundle temporary is not a plain directory: {}",
                path.display()
            ),
        ));
    }
    std::fs::remove_dir_all(path)
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> std::io::Result<()> {
    File::open(path)?.sync_all()
}

#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> std::io::Result<()> {
    Ok(())
}

#[cfg(windows)]
fn remove_path_no_follow(path: &Path) -> std::io::Result<()> {
    a3s_box_core::windows_file::remove_path_no_follow(path)
}

#[cfg(not(windows))]
fn remove_path_no_follow(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn metadata_error(message: String) -> BoxError {
    BoxError::OciImageError(message)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn encoded(raw: &[u8]) -> String {
        base64::engine::general_purpose::STANDARD.encode(raw)
    }

    fn entry(
        path: &[u8],
        kind: RootfsEntryKind,
    ) -> a3s_box_core::rootfs_metadata::RootfsMetadataEntry {
        a3s_box_core::rootfs_metadata::RootfsMetadataEntry {
            path_base64: encoded(path),
            kind,
            mode: if kind == RootfsEntryKind::Directory {
                0o755
            } else {
                0o644
            },
            uid: 12,
            gid: 34,
            mtime: 56,
            size: 78,
            link_target_base64: (kind == RootfsEntryKind::Symlink).then(|| encoded(b"tool")),
        }
    }

    fn write_source(root: &Path, entries: Vec<a3s_box_core::rootfs_metadata::RootfsMetadataEntry>) {
        let manifest = RootfsMetadataManifest::new(entries);
        std::fs::write(
            root.join(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/')),
            serde_json::to_vec(&manifest).unwrap(),
        )
        .unwrap();
    }

    #[test]
    fn publishes_exact_portable_contract_and_consumes_box_manifest() {
        let temporary = tempfile::tempdir().unwrap();
        write_source(
            temporary.path(),
            vec![
                entry(b".", RootfsEntryKind::Directory),
                entry(b"./bin/tool", RootfsEntryKind::Regular),
                entry(b"./bin/link", RootfsEntryKind::Symlink),
            ],
        );

        publish_portable_rootfs_metadata(temporary.path()).unwrap();

        assert!(!temporary
            .path()
            .join(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/'))
            .exists());
        let manifest: PortableRootfsMetadataManifest = serde_json::from_slice(
            &std::fs::read(temporary.path().join(PORTABLE_ROOTFS_METADATA_FILE)).unwrap(),
        )
        .unwrap();
        manifest.validate().unwrap();
        assert_eq!(manifest.entries.len(), 3);
        assert_eq!(manifest.entries[1].path_base64, encoded(b"./bin/tool"));
        assert_eq!(manifest.entries[1].uid, 12);
        assert_eq!(manifest.entries[1].gid, 34);
        assert_eq!(manifest.entries[2].kind, PortableRootfsEntryKind::Symlink);
        assert_eq!(
            manifest.entries[2].link_target_base64,
            Some(encoded(b"tool"))
        );
    }

    #[test]
    fn rejects_normalized_duplicates_without_publishing() {
        let temporary = tempfile::tempdir().unwrap();
        write_source(
            temporary.path(),
            vec![
                entry(b"./bin/tool", RootfsEntryKind::Regular),
                entry(b"bin//./tool", RootfsEntryKind::Regular),
            ],
        );

        let error = publish_portable_rootfs_metadata(temporary.path()).unwrap_err();

        assert!(error.to_string().contains("duplicate"));
        assert!(!temporary
            .path()
            .join(PORTABLE_ROOTFS_METADATA_FILE)
            .exists());
        assert!(temporary
            .path()
            .join(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/'))
            .exists());
    }

    #[test]
    fn rejects_runtime_internal_and_unsafe_paths_without_publishing() {
        for path in [
            b"./.a3s-box-env".as_slice(),
            b"./.a3s-box-env/child".as_slice(),
            b"../escape".as_slice(),
            b"/absolute".as_slice(),
        ] {
            let temporary = tempfile::tempdir().unwrap();
            write_source(
                temporary.path(),
                vec![entry(path, RootfsEntryKind::Regular)],
            );

            assert!(publish_portable_rootfs_metadata(temporary.path()).is_err());
            assert!(!temporary
                .path()
                .join(PORTABLE_ROOTFS_METADATA_FILE)
                .exists());
        }
    }

    #[test]
    fn rejects_invalid_link_contract_without_publishing() {
        let temporary = tempfile::tempdir().unwrap();
        let mut invalid = entry(b"./bin/tool", RootfsEntryKind::Regular);
        invalid.link_target_base64 = Some(encoded(b"target"));
        write_source(temporary.path(), vec![invalid]);

        assert!(publish_portable_rootfs_metadata(temporary.path()).is_err());
        assert!(!temporary
            .path()
            .join(PORTABLE_ROOTFS_METADATA_FILE)
            .exists());
    }
}
