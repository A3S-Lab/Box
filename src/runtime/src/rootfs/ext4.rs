//! Deterministic guest-native ext4 artifact construction.
//!
//! This adapter deliberately owns the policy around `mkext4`: capacity
//! limits, OCI metadata replay, host-only xattr filtering, validation, and
//! atomic publication are A3S contracts rather than properties delegated to
//! a third-party tree-walking example.

use std::collections::{BTreeMap, HashMap};
use std::fs::File;
use std::io::Read;
#[cfg(any(target_os = "macos", all(unix, test)))]
use std::io::{Seek, SeekFrom};
use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::{FileTypeExt, MetadataExt};
use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::rootfs_metadata::{
    RootfsEntryKind, RootfsMetadataEntry, RootfsMetadataManifest, IMAGE_ROOTFS_METADATA_PATH,
};
use base64::Engine as _;
use mkext4::sink::FileSink;
use mkext4::{FsBuilder, InodeHandle, Meta, Options, SparseSeg, SpecialKind, ROOT};
use serde::{Deserialize, Serialize};

#[path = "ext4_sparse.rs"]
mod sparse;

use sparse::{sparse_layout, FileFill, SourceSegment};

/// Versioned A3S wrapper contract for a published ext4 artifact directory.
pub const EXT4_ARTIFACT_SCHEMA: &str = "a3s.box.rootfs-ext4.v1";
/// Exact writer identity included in cache keys and artifact manifests.
pub const EXT4_BUILDER_ID: &str = "mkext4/0.0.3+a3s-adapter-v2";
/// Builder identities that remain safe to resume as already-published disks.
#[cfg(any(target_os = "macos", all(unix, test)))]
pub(super) const LEGACY_EXT4_BUILDER_IDS: &[&str] = &["mkext4/0.0.3+a3s-adapter-v1"];

pub(super) const DISK_FILE_NAME: &str = "rootfs.ext4";
pub(super) const MANIFEST_FILE_NAME: &str = "artifact.json";
pub(super) const STAGING_DIRECTORY_PREFIX: &str = ".a3s-rootfs-ext4-";
const MIN_CAPACITY_BYTES: u64 = 16 * 1024 * 1024;
const MAX_CAPACITY_BYTES: u64 = 64 * 1024 * 1024 * 1024;
const MAX_IMAGE_METADATA_BYTES: u64 = 64 * 1024 * 1024;

/// Validation result for a mutable guest-owned generation selected for boot.
#[cfg(any(target_os = "macos", all(unix, test)))]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ext4ResumeValidation {
    /// The filesystem was cleanly unmounted and passed full structural checks.
    Clean,
    /// The ext4 journal must be replayed by the guest kernel before use.
    JournalRecoveryRequired,
}

/// Determinism inputs and capacity policy for one ext4 artifact.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Ext4ArtifactOptions {
    pub capacity_bytes: u64,
    pub fs_uuid: [u8; 16],
    pub epoch: i64,
}

impl Ext4ArtifactOptions {
    pub fn from_disk_mib(disk_mib: u32, fs_uuid: [u8; 16]) -> Result<Self> {
        let capacity_bytes = u64::from(disk_mib)
            .checked_mul(1024 * 1024)
            .ok_or_else(|| BoxError::BuildError("ext4 capacity overflow".to_string()))?;
        let options = Self {
            capacity_bytes,
            fs_uuid,
            epoch: 0,
        };
        options.validate()?;
        Ok(options)
    }

    pub(super) fn validate(&self) -> Result<()> {
        if !(MIN_CAPACITY_BYTES..=MAX_CAPACITY_BYTES).contains(&self.capacity_bytes) {
            return Err(BoxError::BuildError(format!(
                "ext4 capacity {} is outside the supported range {}..={} bytes",
                self.capacity_bytes, MIN_CAPACITY_BYTES, MAX_CAPACITY_BYTES
            )));
        }
        if self.capacity_bytes & 4095 != 0 {
            return Err(BoxError::BuildError(
                "ext4 capacity must be 4096-byte aligned".to_string(),
            ));
        }
        if !(0..=i64::from(u32::MAX)).contains(&self.epoch) {
            return Err(BoxError::BuildError(
                "ext4 epoch is outside the superblock timestamp range".to_string(),
            ));
        }
        Ok(())
    }
}

/// Durable metadata committed in the same atomically published directory as
/// the raw filesystem image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ext4ArtifactManifest {
    pub schema: String,
    pub builder: String,
    pub format: String,
    pub capacity_bytes: u64,
    pub fs_uuid: String,
}

/// Paths and metadata for one complete, validated artifact generation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ext4Artifact {
    pub directory: PathBuf,
    pub disk: PathBuf,
    pub manifest: Ext4ArtifactManifest,
}

/// Build, validate, and atomically publish a raw ext4 filesystem.
///
/// `destination` is a generation directory, not the disk file itself. The
/// directory rename is the commit record: consumers can never observe an image
/// without its matching schema and builder identity.
pub fn publish_ext4_artifact(
    source: &Path,
    destination: &Path,
    options: Ext4ArtifactOptions,
) -> Result<Ext4Artifact> {
    options.validate()?;
    let parent = destination.parent().ok_or_else(|| {
        BoxError::BuildError(format!(
            "ext4 artifact destination has no parent: {}",
            destination.display()
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to create ext4 artifact parent {}: {error}",
            parent.display()
        ))
    })?;
    validate_source_and_destination(source, destination)?;

    let temporary = tempfile::Builder::new()
        .prefix(STAGING_DIRECTORY_PREFIX)
        .tempdir_in(parent)
        .map_err(|error| {
            BoxError::BuildError(format!(
                "Failed to create ext4 artifact staging directory in {}: {error}",
                parent.display()
            ))
        })?;
    let temporary_disk = temporary.path().join(DISK_FILE_NAME);

    let (builder, fills) = declare_source_tree(source, options)?;
    let layout = builder.seal().map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to lay out {}-byte ext4 artifact: {error}",
            options.capacity_bytes
        ))
    })?;
    let mut sink = FileSink::create(&temporary_disk, layout.image_len()).map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to create sparse ext4 artifact {}: {error}",
            temporary_disk.display()
        ))
    })?;
    let mut writer = layout.writer(&mut sink).map_err(mkext4_build_error)?;
    for fill in fills {
        fill.write_into(&mut writer)?;
    }
    let summary = writer.finish().map_err(mkext4_build_error)?;
    if summary.image_len != options.capacity_bytes {
        return Err(BoxError::BuildError(format!(
            "ext4 writer returned unexpected image length {} (expected {})",
            summary.image_len, options.capacity_bytes
        )));
    }
    sink.into_file().sync_all().map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to sync ext4 artifact {}: {error}",
            temporary_disk.display()
        ))
    })?;
    validate_ext4_image(&temporary_disk, options.capacity_bytes)?;

    let manifest = Ext4ArtifactManifest {
        schema: EXT4_ARTIFACT_SCHEMA.to_string(),
        builder: EXT4_BUILDER_ID.to_string(),
        format: "raw-ext4".to_string(),
        capacity_bytes: options.capacity_bytes,
        fs_uuid: hex::encode(options.fs_uuid),
    };
    let manifest_bytes = serde_json::to_vec_pretty(&manifest).map_err(|error| {
        BoxError::BuildError(format!("Failed to encode ext4 artifact manifest: {error}"))
    })?;
    let temporary_manifest = temporary.path().join(MANIFEST_FILE_NAME);
    std::fs::write(&temporary_manifest, manifest_bytes).map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to write ext4 artifact manifest {}: {error}",
            temporary_manifest.display()
        ))
    })?;
    File::open(&temporary_manifest)
        .and_then(|file| file.sync_all())
        .map_err(|error| {
            BoxError::BuildError(format!(
                "Failed to sync ext4 artifact manifest {}: {error}",
                temporary_manifest.display()
            ))
        })?;
    sync_directory(temporary.path())?;

    let temporary_path = temporary.keep();
    if let Err(error) = std::fs::rename(&temporary_path, destination) {
        let _ = std::fs::remove_dir_all(&temporary_path);
        return Err(BoxError::BuildError(format!(
            "Failed to atomically publish ext4 artifact {}: {error}",
            destination.display()
        )));
    }
    sync_directory(parent)?;

    Ok(Ext4Artifact {
        directory: destination.to_path_buf(),
        disk: destination.join(DISK_FILE_NAME),
        manifest,
    })
}

fn validate_source_and_destination(source: &Path, destination: &Path) -> Result<()> {
    let source_metadata = std::fs::symlink_metadata(source).map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to inspect ext4 source {}: {error}",
            source.display()
        ))
    })?;
    if !source_metadata.is_dir() || source_metadata.file_type().is_symlink() {
        return Err(BoxError::BuildError(format!(
            "ext4 source is not a plain directory: {}",
            source.display()
        )));
    }
    match std::fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(BoxError::BuildError(format!(
                "ext4 artifact generation already exists: {}",
                destination.display()
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(BoxError::BuildError(format!(
                "Failed to inspect ext4 artifact destination {}: {error}",
                destination.display()
            )))
        }
    }

    let canonical_source = source.canonicalize().map_err(BoxError::IoError)?;
    if let Some(parent) = destination.parent() {
        if let Ok(canonical_parent) = parent.canonicalize() {
            if canonical_parent.starts_with(&canonical_source) {
                return Err(BoxError::BuildError(
                    "ext4 artifact destination must be outside the source tree".to_string(),
                ));
            }
        }
    }
    Ok(())
}

fn declare_source_tree(
    source: &Path,
    options: Ext4ArtifactOptions,
) -> Result<(FsBuilder, Vec<FileFill>)> {
    let mut mkfs_options = Options::new(options.capacity_bytes, options.fs_uuid, options.epoch);
    mkfs_options.label = Some("a3s-rootfs".to_string());
    mkfs_options.reserved_percent = 0;
    let mut builder = FsBuilder::new(mkfs_options).map_err(mkext4_build_error)?;
    let metadata = ImageMetadata::load(source)?;
    let root_metadata = std::fs::symlink_metadata(source).map_err(BoxError::IoError)?;
    builder
        .set_meta(
            ROOT,
            node_meta(source, Path::new(""), &root_metadata, &metadata)?,
        )
        .map_err(mkext4_build_error)?;
    apply_xattrs(&mut builder, ROOT, source)?;

    let mut hardlinks = HashMap::new();
    let mut fills = Vec::new();
    let mut state = TreeDeclarationState {
        image_metadata: &metadata,
        hardlinks: &mut hardlinks,
        fills: &mut fills,
    };
    declare_directory(
        &mut builder,
        ROOT,
        source,
        Path::new(""),
        Path::new(""),
        &mut state,
    )?;
    Ok((builder, fills))
}

struct TreeDeclarationState<'a> {
    image_metadata: &'a ImageMetadata,
    hardlinks: &'a mut HashMap<(u64, u64), InodeHandle>,
    fills: &'a mut Vec<FileFill>,
}

fn declare_directory(
    builder: &mut FsBuilder,
    parent: InodeHandle,
    directory: &Path,
    physical_relative_directory: &Path,
    logical_relative_directory: &Path,
    state: &mut TreeDeclarationState<'_>,
) -> Result<()> {
    let mut entries = std::fs::read_dir(directory)
        .map_err(|error| {
            BoxError::BuildError(format!(
                "Failed to read ext4 source directory {}: {error}",
                directory.display()
            ))
        })?
        .collect::<std::result::Result<Vec<_>, _>>()
        .map_err(BoxError::IoError)?;
    entries.sort_by(|left, right| {
        left.file_name()
            .as_bytes()
            .cmp(right.file_name().as_bytes())
    });

    for entry in entries {
        let name_os = entry.file_name();
        let path = entry.path();
        let physical_relative = physical_relative_directory.join(&name_os);
        let logical_relative = state.image_metadata.logical_path(
            &physical_relative,
            logical_relative_directory,
            &name_os,
        )?;
        let name = logical_relative
            .file_name()
            .ok_or_else(|| {
                BoxError::BuildError(format!(
                    "Guest path has no directory entry name: {}",
                    logical_relative.display()
                ))
            })?
            .as_bytes();
        let filesystem = std::fs::symlink_metadata(&path).map_err(BoxError::IoError)?;
        let file_type = filesystem.file_type();

        if !file_type.is_dir() && filesystem.nlink() > 1 {
            let identity = (filesystem.dev(), filesystem.ino());
            if let Some(existing) = state.hardlinks.get(&identity).copied() {
                builder
                    .hardlink(parent, name, existing)
                    .map_err(mkext4_build_error)?;
                continue;
            }
        }

        let metadata = node_meta(&path, &logical_relative, &filesystem, state.image_metadata)?;
        let handle = if file_type.is_dir() {
            builder
                .mkdir(parent, name, metadata)
                .map_err(mkext4_build_error)?
        } else if file_type.is_symlink() {
            let target = state
                .image_metadata
                .symlink_target(&logical_relative)?
                .unwrap_or(
                    std::fs::read_link(&path)
                        .map_err(BoxError::IoError)?
                        .as_os_str()
                        .as_bytes()
                        .to_vec(),
                );
            builder
                .symlink(parent, name, &target, metadata)
                .map_err(mkext4_build_error)?
        } else if file_type.is_file() {
            let file = File::open(&path).map_err(BoxError::IoError)?;
            if let Some(sparse) = sparse_layout(&file, filesystem.len())? {
                let segments: Vec<_> = sparse
                    .segments
                    .iter()
                    .map(|segment| match *segment {
                        SourceSegment::Data { len, .. } => SparseSeg::Data(len),
                        SourceSegment::Hole { len } => SparseSeg::Hole(len),
                    })
                    .collect();
                let handle = builder
                    .file_sparse(parent, name, metadata, &segments)
                    .map_err(mkext4_build_error)?;
                if !sparse.data_ranges.is_empty() {
                    state.fills.push(FileFill::Sparse {
                        handle,
                        path: path.clone(),
                        ranges: sparse.data_ranges,
                    });
                }
                handle
            } else {
                let handle = builder
                    .file(parent, name, metadata, filesystem.len())
                    .map_err(mkext4_build_error)?;
                if filesystem.len() > 0 {
                    state.fills.push(FileFill::Dense {
                        handle,
                        path: path.clone(),
                    });
                }
                handle
            }
        } else if file_type.is_char_device() || file_type.is_block_device() {
            let (major, minor) = device_numbers(filesystem.rdev());
            let kind = if file_type.is_char_device() {
                SpecialKind::Char { major, minor }
            } else {
                SpecialKind::Block { major, minor }
            };
            builder
                .mknod(parent, name, metadata, kind)
                .map_err(mkext4_build_error)?
        } else if file_type.is_fifo() {
            builder
                .mknod(parent, name, metadata, SpecialKind::Fifo)
                .map_err(mkext4_build_error)?
        } else if file_type.is_socket() {
            builder
                .mknod(parent, name, metadata, SpecialKind::Socket)
                .map_err(mkext4_build_error)?
        } else {
            return Err(BoxError::BuildError(format!(
                "Unsupported rootfs entry type at {}",
                path.display()
            )));
        };

        apply_xattrs(builder, handle, &path)?;
        if !file_type.is_dir() && filesystem.nlink() > 1 {
            state
                .hardlinks
                .insert((filesystem.dev(), filesystem.ino()), handle);
        }
        if file_type.is_dir() {
            declare_directory(
                builder,
                handle,
                &path,
                &physical_relative,
                &logical_relative,
                state,
            )?;
        }
    }
    Ok(())
}

fn node_meta(
    path: &Path,
    relative: &Path,
    filesystem: &std::fs::Metadata,
    image_metadata: &ImageMetadata,
) -> Result<Meta> {
    let runtime_mode = a3s_box_core::rootfs_metadata::runtime_managed_rootfs_mode(relative)
        .or_else(|| {
            (relative == Path::new(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/')))
                .then_some(0o600)
        });
    // Runtime-owned replacements are a different generation than any OCI
    // entry at the same path. Replaying the image entry's kind, owner, mode, or
    // timestamp would make the generated base depend on stale image metadata
    // and can even reject a regular guest-init that replaced an image symlink.
    let override_entry = runtime_mode
        .is_none()
        .then(|| image_metadata.entries.get(relative))
        .flatten();
    if let Some(entry) = override_entry {
        let actual_kind = if filesystem.file_type().is_dir() {
            Some(RootfsEntryKind::Directory)
        } else if filesystem.file_type().is_file() {
            Some(RootfsEntryKind::Regular)
        } else if filesystem.file_type().is_symlink() {
            Some(RootfsEntryKind::Symlink)
        } else {
            None
        };
        if actual_kind != Some(entry.kind) {
            return Err(BoxError::BuildError(format!(
                "OCI metadata kind does not match staged rootfs entry {}",
                path.display()
            )));
        }
    }

    let mode = runtime_mode
        .or_else(|| override_entry.map(|entry| entry.mode))
        .unwrap_or_else(|| filesystem.mode())
        & 0o7777;
    let uid = match (runtime_mode, override_entry) {
        (Some(_), _) => 0,
        (None, Some(entry)) => u32::try_from(entry.uid).map_err(|_| {
            BoxError::BuildError(format!("UID exceeds ext4 range at {}", path.display()))
        })?,
        (None, None) => filesystem.uid(),
    };
    let gid = match (runtime_mode, override_entry) {
        (Some(_), _) => 0,
        (None, Some(entry)) => u32::try_from(entry.gid).map_err(|_| {
            BoxError::BuildError(format!("GID exceeds ext4 range at {}", path.display()))
        })?,
        (None, None) => filesystem.gid(),
    };
    let (mtime, mtime_nsec) = match (runtime_mode, override_entry) {
        (Some(_), _) => (0, 0),
        (None, Some(entry)) => (
            i64::try_from(entry.mtime).map_err(|_| {
                BoxError::BuildError(format!("mtime exceeds ext4 range at {}", path.display()))
            })?,
            0,
        ),
        (None, None) => (filesystem.mtime(), filesystem.mtime_nsec()),
    };
    let mtime_nsec = u32::try_from(mtime_nsec)
        .ok()
        .filter(|value| *value < 1_000_000_000)
        .ok_or_else(|| {
            BoxError::BuildError(format!("Invalid mtime nanoseconds at {}", path.display()))
        })?;
    Ok(Meta::new(mode as u16, uid, gid, (mtime, mtime_nsec)))
}

fn apply_xattrs(builder: &mut FsBuilder, handle: InodeHandle, path: &Path) -> Result<()> {
    let mut names = xattr::list(path)
        .map_err(|error| {
            BoxError::BuildError(format!(
                "Failed to list xattrs for {}: {error}",
                path.display()
            ))
        })?
        .collect::<Vec<_>>();
    names.sort_by(|left, right| left.as_bytes().cmp(right.as_bytes()));
    for name in names {
        let raw_name = name.as_bytes();
        if raw_name.starts_with(b"com.apple.") {
            // APFS/Finder metadata is a property of the host staging view, not
            // Linux image content. Never bake it into the guest filesystem.
            continue;
        }
        let name = name.to_str().ok_or_else(|| {
            BoxError::BuildError(format!(
                "Non-UTF-8 xattr name cannot be represented at {}",
                path.display()
            ))
        })?;
        if !is_linux_xattr_name(name) {
            return Err(BoxError::BuildError(format!(
                "Unsupported xattr namespace {name:?} at {}",
                path.display()
            )));
        }
        let value = xattr::get(path, name)
            .map_err(|error| {
                BoxError::BuildError(format!(
                    "Failed to read xattr {name:?} at {}: {error}",
                    path.display()
                ))
            })?
            .ok_or_else(|| {
                BoxError::BuildError(format!(
                    "xattr {name:?} disappeared while building {}",
                    path.display()
                ))
            })?;
        builder
            .set_xattr(handle, name, &value)
            .map_err(mkext4_build_error)?;
    }
    Ok(())
}

fn is_linux_xattr_name(name: &str) -> bool {
    name.strip_prefix("user.")
        .is_some_and(|suffix| !suffix.is_empty())
        || name
            .strip_prefix("trusted.")
            .is_some_and(|suffix| !suffix.is_empty())
        || name
            .strip_prefix("security.")
            .is_some_and(|suffix| !suffix.is_empty())
        || name == "system.posix_acl_access"
        || name == "system.posix_acl_default"
        || name
            .strip_prefix("system.")
            .is_some_and(|suffix| !suffix.is_empty())
}

#[derive(Default)]
struct ImageMetadata {
    entries: BTreeMap<PathBuf, RootfsMetadataEntry>,
    staging_to_logical: BTreeMap<PathBuf, PathBuf>,
}

impl ImageMetadata {
    fn load(root: &Path) -> Result<Self> {
        let path = root.join(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/'));
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default())
            }
            Err(error) => return Err(BoxError::IoError(error)),
        };
        let length = file.metadata().map_err(BoxError::IoError)?.len();
        if length > MAX_IMAGE_METADATA_BYTES {
            return Err(BoxError::BuildError(format!(
                "Image metadata {} exceeds {} bytes",
                path.display(),
                MAX_IMAGE_METADATA_BYTES
            )));
        }
        let mut bytes = Vec::with_capacity(length as usize);
        file.take(MAX_IMAGE_METADATA_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(BoxError::IoError)?;
        if bytes.len() as u64 > MAX_IMAGE_METADATA_BYTES {
            return Err(BoxError::BuildError(
                "Image metadata grew beyond its byte limit while reading".to_string(),
            ));
        }
        let manifest: RootfsMetadataManifest = serde_json::from_slice(&bytes).map_err(|error| {
            BoxError::BuildError(format!(
                "Invalid image metadata {}: {error}",
                path.display()
            ))
        })?;
        manifest.validate().map_err(BoxError::BuildError)?;

        let mut entries = BTreeMap::new();
        for entry in manifest.entries {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(&entry.path_base64)
                .map_err(|error| {
                    BoxError::BuildError(format!("Invalid image metadata path: {error}"))
                })?;
            let path = normalize_manifest_path(Path::new(&std::ffi::OsString::from_vec(raw)))?;
            if entries.insert(path, entry).is_some() {
                return Err(BoxError::BuildError(
                    "Duplicate path in image metadata".to_string(),
                ));
            }
        }
        let staging_to_logical = super::staging_path_map(entries.keys())?;
        Ok(Self {
            entries,
            staging_to_logical,
        })
    }

    fn logical_path(
        &self,
        physical: &Path,
        logical_parent: &Path,
        physical_name: &std::ffi::OsStr,
    ) -> Result<PathBuf> {
        super::logical_path_for_staged_child(
            &self.staging_to_logical,
            physical,
            logical_parent,
            physical_name,
        )
    }

    fn symlink_target(&self, relative: &Path) -> Result<Option<Vec<u8>>> {
        let Some(encoded) = self
            .entries
            .get(relative)
            .and_then(|entry| entry.link_target_base64.as_ref())
        else {
            return Ok(None);
        };
        base64::engine::general_purpose::STANDARD
            .decode(encoded)
            .map(Some)
            .map_err(|error| {
                BoxError::BuildError(format!(
                    "Invalid symlink target in image metadata at {}: {error}",
                    relative.display()
                ))
            })
    }
}

fn normalize_manifest_path(path: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            std::path::Component::CurDir => {}
            std::path::Component::Normal(name) => normalized.push(name),
            _ => {
                return Err(BoxError::BuildError(
                    "Unsafe path in image metadata".to_string(),
                ))
            }
        }
    }
    Ok(normalized)
}

pub(super) fn validate_ext4_image(path: &Path, expected_length: u64) -> Result<()> {
    let file = File::open(path).map_err(BoxError::IoError)?;
    let actual_length = file.metadata().map_err(BoxError::IoError)?.len();
    if actual_length != expected_length {
        return Err(BoxError::BuildError(format!(
            "ext4 artifact {} has length {} instead of {}",
            path.display(),
            actual_length,
            expected_length
        )));
    }
    let filesystem = mkext4::reader::Fs::open(&file).map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to reopen ext4 artifact {}: {error}",
            path.display()
        ))
    })?;
    let issues = filesystem.verify().map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to verify ext4 artifact {}: {error}",
            path.display()
        ))
    })?;
    if !issues.is_empty() {
        let details = issues
            .iter()
            .take(8)
            .map(|issue| issue.what.as_str())
            .collect::<Vec<_>>()
            .join("; ");
        return Err(BoxError::BuildError(format!(
            "ext4 artifact {} failed structural verification: {details}",
            path.display()
        )));
    }
    Ok(())
}

/// Validate a mutable persistent disk without parsing unreplayed metadata.
///
/// A clean generation receives the same full reader verification as an
/// immutable cache entry. After a host crash, ext4 sets only the dynamic
/// `RECOVER` incompatibility bit. In that state the home metadata can lag the
/// journal, so a host-side tree reader is the wrong recovery mechanism. We
/// instead validate the exact A3S superblock envelope and let the isolated
/// guest kernel replay its own journal on the next read-write mount.
#[cfg(any(target_os = "macos", all(unix, test)))]
pub(super) fn validate_ext4_image_for_resume(
    path: &Path,
    expected_length: u64,
    expected_uuid: [u8; 16],
) -> Result<Ext4ResumeValidation> {
    let mut file = File::open(path).map_err(BoxError::IoError)?;
    let actual_length = file.metadata().map_err(BoxError::IoError)?.len();
    if actual_length != expected_length {
        return Err(BoxError::BuildError(format!(
            "ext4 artifact {} has length {} instead of {}",
            path.display(),
            actual_length,
            expected_length
        )));
    }

    let mut bytes = [0u8; mkext4::spec::Superblock::LEN];
    file.seek(SeekFrom::Start(1024))
        .and_then(|_| file.read_exact(&mut bytes))
        .map_err(BoxError::IoError)?;
    let superblock = mkext4::spec::Superblock::decode(&bytes).map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to decode ext4 artifact superblock {}: {error}",
            path.display()
        ))
    })?;
    let needs_recovery = superblock.feature_incompat & mkext4::spec::incompat::RECOVER != 0;
    if !needs_recovery {
        validate_ext4_image(path, expected_length)?;
        return Ok(Ext4ResumeValidation::Clean);
    }

    validate_recovery_superblock(&superblock, &bytes, expected_length, expected_uuid).map_err(
        |reason| {
            BoxError::BuildError(format!(
                "Refused crash recovery for ext4 artifact {}: {reason}",
                path.display()
            ))
        },
    )?;
    Ok(Ext4ResumeValidation::JournalRecoveryRequired)
}

#[cfg(any(target_os = "macos", all(unix, test)))]
fn validate_recovery_superblock(
    superblock: &mkext4::spec::Superblock,
    bytes: &[u8; mkext4::spec::Superblock::LEN],
    expected_length: u64,
    expected_uuid: [u8; 16],
) -> std::result::Result<(), String> {
    let expected_incompat = mkext4::spec::incompat::WRITER | mkext4::spec::incompat::RECOVER;
    if superblock.feature_compat != mkext4::spec::compat::WRITER
        || superblock.feature_incompat != expected_incompat
        || superblock.feature_ro_compat != mkext4::spec::ro_compat::WRITER
    {
        return Err(format!(
            "feature set changed (compat={:#x}, incompat={:#x}, ro_compat={:#x})",
            superblock.feature_compat, superblock.feature_incompat, superblock.feature_ro_compat
        ));
    }
    if superblock.uuid != expected_uuid {
        return Err("filesystem UUID no longer matches the artifact manifest".to_string());
    }
    if superblock.block_size() != mkext4::spec::BLOCK_SIZE as u64
        || superblock.blocks_per_group != mkext4::spec::BLOCKS_PER_GROUP
        || superblock.clusters_per_group != mkext4::spec::BLOCKS_PER_GROUP
        || superblock.inode_size != mkext4::spec::INODE_SIZE as u16
        || superblock.first_data_block != 0
    {
        return Err("filesystem geometry no longer matches the A3S ext4 contract".to_string());
    }
    let filesystem_length = superblock
        .blocks_count
        .checked_mul(superblock.block_size())
        .ok_or_else(|| "filesystem length overflow".to_string())?;
    if filesystem_length != expected_length {
        return Err(format!(
            "superblock describes {filesystem_length} bytes instead of {expected_length}"
        ));
    }
    if superblock.journal_inum != mkext4::spec::JOURNAL_INO
        || superblock.journal_dev != 0
        || superblock.journal_uuid != [0; 16]
    {
        return Err("journal identity no longer matches the A3S ext4 contract".to_string());
    }
    if mkext4::csum::superblock(bytes) != superblock.checksum {
        return Err("primary superblock checksum is invalid".to_string());
    }
    Ok(())
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| {
            BoxError::BuildError(format!(
                "Failed to sync ext4 artifact directory {}: {error}",
                path.display()
            ))
        })
}

fn mkext4_build_error(error: mkext4::Error) -> BoxError {
    BoxError::BuildError(format!("ext4 artifact builder failed: {error}"))
}

fn device_numbers(device: u64) -> (u32, u32) {
    (
        ((device >> 8) & 0x0fff) as u32,
        ((device & 0x00ff) | ((device >> 12) & 0x000f_ff00)) as u32,
    )
}

#[cfg(test)]
#[path = "ext4_tests.rs"]
mod tests;
