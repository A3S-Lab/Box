//! Versioned rootfs payloads for filesystem snapshots.
//!
//! Directory snapshots retain the historical shared-lower layout. On macOS,
//! a clean guest-native ext4 generation is captured and restored as an
//! immutable raw artifact clone, so neither operation attaches a host volume.

use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::snapshot::SnapshotMetadata;
use serde::{Deserialize, Serialize};

use super::SnapshotStore;

pub const SNAPSHOT_ROOTFS_SCHEMA: &str = "a3s.box.snapshot-rootfs.v1";
const ROOTFS_MANIFEST_NAME: &str = "rootfs.json";
const DIRECTORY_NAME: &str = "rootfs";
const RAW_EXT4_DIRECTORY_NAME: &str = "rootfs-ext4-v1";
const RAW_EXT4_ARTIFACT_SCHEMA: &str = "a3s.box.rootfs-ext4.v1";
const MAX_ROOTFS_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_SNAPSHOT_METADATA_BYTES: u64 = 1024 * 1024;

/// Storage representation of a published filesystem snapshot.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotRootfsFormat {
    /// A host directory shared as an immutable lower by restored boxes.
    Directory,
    /// A clean raw ext4 artifact cloned privately into every restored box.
    GuestNativeExt4,
}

/// Metadata and rootfs representation materialized for one restored box.
#[derive(Debug, Clone)]
pub struct RestoredSnapshotRootfs {
    pub metadata: SnapshotMetadata,
    pub format: SnapshotRootfsFormat,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct SnapshotRootfsManifest {
    schema: String,
    snapshot_id: String,
    source_box_id: String,
    rootfs: SnapshotRootfsPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "format", rename_all = "kebab-case", deny_unknown_fields)]
enum SnapshotRootfsPayload {
    Directory,
    RawExt4 {
        artifact: Ext4ArtifactIdentity,
        sparse_sha256: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ext4ArtifactIdentity {
    schema: String,
    builder: String,
    format: String,
    capacity_bytes: u64,
    fs_uuid: String,
}

impl SnapshotRootfsManifest {
    fn directory(metadata: &SnapshotMetadata) -> Self {
        Self {
            schema: SNAPSHOT_ROOTFS_SCHEMA.to_string(),
            snapshot_id: metadata.id.clone(),
            source_box_id: metadata.source_box_id.clone(),
            rootfs: SnapshotRootfsPayload::Directory,
        }
    }

    #[cfg(target_os = "macos")]
    fn raw_ext4(
        metadata: &SnapshotMetadata,
        artifact: &crate::rootfs::Ext4Artifact,
        sparse_sha256: String,
    ) -> Self {
        Self {
            schema: SNAPSHOT_ROOTFS_SCHEMA.to_string(),
            snapshot_id: metadata.id.clone(),
            source_box_id: metadata.source_box_id.clone(),
            rootfs: SnapshotRootfsPayload::RawExt4 {
                artifact: Ext4ArtifactIdentity {
                    schema: artifact.manifest.schema.clone(),
                    builder: artifact.manifest.builder.clone(),
                    format: artifact.manifest.format.clone(),
                    capacity_bytes: artifact.manifest.capacity_bytes,
                    fs_uuid: artifact.manifest.fs_uuid.clone(),
                },
                sparse_sha256,
            },
        }
    }

    fn validate_for(&self, metadata: &SnapshotMetadata) -> Result<()> {
        if self.schema != SNAPSHOT_ROOTFS_SCHEMA
            || self.snapshot_id != metadata.id
            || self.source_box_id != metadata.source_box_id
        {
            return Err(snapshot_error(format!(
                "rootfs manifest identity does not match snapshot {}",
                metadata.id
            )));
        }
        if let SnapshotRootfsPayload::RawExt4 {
            artifact,
            sparse_sha256,
        } = &self.rootfs
        {
            if artifact.schema != RAW_EXT4_ARTIFACT_SCHEMA
                || artifact.format != "raw-ext4"
                || artifact.capacity_bytes == 0
                || artifact.fs_uuid.len() != 32
                || !artifact
                    .fs_uuid
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit())
                || !is_lower_sha256(sparse_sha256)
            {
                return Err(snapshot_error(format!(
                    "raw-ext4 identity is invalid for snapshot {}",
                    metadata.id
                )));
            }
        }
        Ok(())
    }
}

impl SnapshotStore {
    /// Save a clean guest-native ext4 generation without exposing it as a host
    /// directory. The snapshot owns an immutable clone; later restores own
    /// separate writable clones and therefore do not retain a store reference.
    #[cfg(target_os = "macos")]
    pub fn save_guest_native_ext4(
        &self,
        mut metadata: SnapshotMetadata,
        box_dir: &Path,
    ) -> Result<SnapshotMetadata> {
        validate_snapshot_id(&metadata.id)?;
        let _lock = self.acquire_exclusive_lock()?;
        let snapshot_dir = self.base_dir.join(&metadata.id);
        require_absent(&snapshot_dir, "snapshot")?;

        let staging_prefix = format!(".staging-{}-{}-", metadata.id, std::process::id());
        let staging = tempfile::Builder::new()
            .prefix(&staging_prefix)
            .tempdir_in(&self.base_dir)
            .map_err(|error| {
                snapshot_error(format!(
                    "failed to create snapshot staging directory in {}: {error}",
                    self.base_dir.display()
                ))
            })?;

        let source = box_dir.join(RAW_EXT4_DIRECTORY_NAME);
        let destination = staging.path().join(RAW_EXT4_DIRECTORY_NAME);
        let artifact =
            crate::rootfs::clone_clean_guest_native_ext4_artifact(&source, &destination)?;
        let digest = crate::rootfs::guest_native_ext4_sparse_digest(&artifact)?;
        metadata.size_bytes = crate::rootfs::guest_native_ext4_allocated_bytes(&destination)?;
        let manifest = SnapshotRootfsManifest::raw_ext4(&metadata, &artifact, digest);
        write_manifest(staging.path(), &manifest)?;
        write_metadata(staging.path(), &metadata)?;
        sync_directory(staging.path())?;
        publish_staging(staging, &snapshot_dir, &self.base_dir)?;
        Ok(metadata)
    }

    /// Inspect and validate the rootfs representation of a published snapshot.
    pub fn rootfs_format(&self, id: &str) -> Result<SnapshotRootfsFormat> {
        validate_snapshot_id(id)?;
        let _lock = self.acquire_exclusive_lock()?;
        let (snapshot_dir, metadata) = load_snapshot_bundle(&self.base_dir, id)?;
        inspect_payload(&snapshot_dir, &metadata).map(|payload| payload.format())
    }

    /// Materialize a published snapshot into a newly-created box directory.
    ///
    /// Directory payloads publish the historical `.snapshot-lower` reference
    /// while the store lock is held. Raw payloads clone and validate a private
    /// writable artifact, so deleting the snapshot later cannot affect the box.
    pub fn restore_rootfs_to_box(
        &self,
        id: &str,
        box_dir: &Path,
    ) -> Result<RestoredSnapshotRootfs> {
        validate_snapshot_id(id)?;
        let _lock = self.acquire_exclusive_lock()?;
        let (snapshot_dir, metadata) = load_snapshot_bundle(&self.base_dir, id)?;
        let payload = inspect_payload(&snapshot_dir, &metadata)?;
        validate_box_directory(box_dir)?;

        let image_config = metadata.require_image_config()?.clone();

        match payload {
            ValidatedPayload::Directory(rootfs) => {
                let marker = box_dir.join(".snapshot-lower");
                require_absent(&marker, "snapshot lower marker")?;
                let temporary = box_dir.join(format!(
                    ".snapshot-lower.{}.tmp",
                    uuid::Uuid::new_v4().simple()
                ));
                a3s_box_core::fs_atomic::write_durable(
                    &temporary,
                    &marker,
                    rootfs.to_string_lossy().as_bytes(),
                )
                .map_err(BoxError::IoError)?;
                if let Err(error) =
                    crate::resolved_image::persist_snapshot_image_config(box_dir, &image_config)
                {
                    let _ = std::fs::remove_file(&marker);
                    return Err(error);
                }
                Ok(RestoredSnapshotRootfs {
                    metadata,
                    format: SnapshotRootfsFormat::Directory,
                })
            }
            ValidatedPayload::RawExt4 {
                source,
                #[cfg(target_os = "macos")]
                identity,
            } => {
                #[cfg(target_os = "macos")]
                {
                    let destination = box_dir.join(RAW_EXT4_DIRECTORY_NAME);
                    require_absent(&destination, "restored raw-ext4 artifact")?;
                    let artifact = crate::rootfs::clone_clean_guest_native_ext4_artifact(
                        &source,
                        &destination,
                    )?;
                    if let Err(error) = validate_ext4_identity(&artifact, &identity) {
                        let _ = std::fs::remove_dir_all(&destination);
                        return Err(error);
                    }
                    let digest = match crate::rootfs::guest_native_ext4_sparse_digest(&artifact) {
                        Ok(digest) => digest,
                        Err(error) => {
                            let _ = std::fs::remove_dir_all(&destination);
                            return Err(error);
                        }
                    };
                    if digest != identity.sparse_sha256 {
                        let _ = std::fs::remove_dir_all(&destination);
                        return Err(snapshot_error(format!(
                            "restored raw-ext4 integrity mismatch for snapshot {}",
                            metadata.id
                        )));
                    }
                    if let Err(error) =
                        crate::resolved_image::persist_snapshot_image_config(box_dir, &image_config)
                    {
                        let _ = std::fs::remove_dir_all(&destination);
                        return Err(error);
                    }
                    Ok(RestoredSnapshotRootfs {
                        metadata,
                        format: SnapshotRootfsFormat::GuestNativeExt4,
                    })
                }
                #[cfg(not(target_os = "macos"))]
                {
                    let _ = source;
                    Err(snapshot_error(format!(
                        "snapshot {} contains a guest-native macOS rootfs that this host cannot restore",
                        metadata.id
                    )))
                }
            }
        }
    }
}

pub(super) fn write_directory_manifest(
    snapshot_directory: &Path,
    metadata: &SnapshotMetadata,
) -> Result<()> {
    write_manifest(
        snapshot_directory,
        &SnapshotRootfsManifest::directory(metadata),
    )
}

enum ValidatedPayload {
    Directory(PathBuf),
    RawExt4 {
        source: PathBuf,
        #[cfg(target_os = "macos")]
        identity: RawExt4Identity,
    },
}

impl ValidatedPayload {
    fn format(&self) -> SnapshotRootfsFormat {
        match self {
            Self::Directory(_) => SnapshotRootfsFormat::Directory,
            Self::RawExt4 { .. } => SnapshotRootfsFormat::GuestNativeExt4,
        }
    }
}

#[cfg(target_os = "macos")]
struct RawExt4Identity {
    artifact: Ext4ArtifactIdentity,
    sparse_sha256: String,
}

fn inspect_payload(snapshot_dir: &Path, metadata: &SnapshotMetadata) -> Result<ValidatedPayload> {
    let manifest = read_manifest(snapshot_dir)?;
    let Some(manifest) = manifest else {
        // Legacy snapshots predate the versioned rootfs manifest. Preserve
        // compatibility only for the exact historical directory payload.
        reject_alternate_payload(snapshot_dir, RAW_EXT4_DIRECTORY_NAME, "legacy directory")?;
        return validate_directory_payload(snapshot_dir).map(ValidatedPayload::Directory);
    };
    manifest.validate_for(metadata)?;
    match manifest.rootfs {
        SnapshotRootfsPayload::Directory => {
            reject_alternate_payload(snapshot_dir, RAW_EXT4_DIRECTORY_NAME, "directory")?;
            validate_directory_payload(snapshot_dir).map(ValidatedPayload::Directory)
        }
        SnapshotRootfsPayload::RawExt4 {
            artifact,
            sparse_sha256,
        } => {
            reject_alternate_payload(snapshot_dir, DIRECTORY_NAME, "raw-ext4")?;
            let source = snapshot_dir.join(RAW_EXT4_DIRECTORY_NAME);
            validate_raw_payload(&source, &artifact, &sparse_sha256)?;
            Ok(ValidatedPayload::RawExt4 {
                source,
                #[cfg(target_os = "macos")]
                identity: RawExt4Identity {
                    artifact,
                    sparse_sha256,
                },
            })
        }
    }
}

fn reject_alternate_payload(
    snapshot_dir: &Path,
    alternate_name: &str,
    selected_format: &str,
) -> Result<()> {
    let alternate = snapshot_dir.join(alternate_name);
    match std::fs::symlink_metadata(&alternate) {
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Ok(_) => Err(snapshot_error(format!(
            "snapshot bundle declares {selected_format} rootfs but also contains alternate payload {}",
            alternate.display()
        ))),
        Err(error) => Err(BoxError::IoError(error)),
    }
}

fn validate_directory_payload(snapshot_dir: &Path) -> Result<PathBuf> {
    let rootfs = snapshot_dir.join(DIRECTORY_NAME);
    let metadata = std::fs::symlink_metadata(&rootfs).map_err(|error| {
        snapshot_error(format!(
            "failed to inspect snapshot rootfs {}: {error}",
            rootfs.display()
        ))
    })?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(snapshot_error(format!(
            "snapshot rootfs is not a plain directory: {}",
            rootfs.display()
        )));
    }
    rootfs
        .canonicalize()
        .map_err(BoxError::IoError)
        .and_then(|rootfs| {
            if rootfs.parent() != Some(snapshot_dir) {
                return Err(snapshot_error(format!(
                    "snapshot rootfs escapes its bundle: {}",
                    rootfs.display()
                )));
            }
            Ok(rootfs)
        })
}

fn validate_raw_payload(
    source: &Path,
    identity: &Ext4ArtifactIdentity,
    sparse_sha256: &str,
) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let artifact = crate::rootfs::open_clean_guest_native_ext4_artifact(source)?;
        validate_ext4_identity(
            &artifact,
            &RawExt4Identity {
                artifact: identity.clone(),
                sparse_sha256: sparse_sha256.to_string(),
            },
        )?;
        let actual = crate::rootfs::guest_native_ext4_sparse_digest(&artifact)?;
        if actual != sparse_sha256 {
            return Err(snapshot_error(format!(
                "raw-ext4 snapshot integrity mismatch at {}",
                artifact.disk.display()
            )));
        }
        Ok(())
    }
    #[cfg(not(target_os = "macos"))]
    {
        let metadata = std::fs::symlink_metadata(source).map_err(BoxError::IoError)?;
        if !metadata.is_dir() || metadata.file_type().is_symlink() {
            return Err(snapshot_error(format!(
                "raw-ext4 snapshot payload is not a plain directory: {}",
                source.display()
            )));
        }
        let _ = (identity, sparse_sha256);
        Ok(())
    }
}

#[cfg(target_os = "macos")]
fn validate_ext4_identity(
    artifact: &crate::rootfs::Ext4Artifact,
    expected: &RawExt4Identity,
) -> Result<()> {
    let actual = Ext4ArtifactIdentity {
        schema: artifact.manifest.schema.clone(),
        builder: artifact.manifest.builder.clone(),
        format: artifact.manifest.format.clone(),
        capacity_bytes: artifact.manifest.capacity_bytes,
        fs_uuid: artifact.manifest.fs_uuid.clone(),
    };
    if actual != expected.artifact {
        return Err(snapshot_error(format!(
            "raw-ext4 artifact identity mismatch at {}",
            artifact.directory.display()
        )));
    }
    Ok(())
}

fn load_snapshot_bundle(base_dir: &Path, id: &str) -> Result<(PathBuf, SnapshotMetadata)> {
    load_snapshot_metadata(base_dir, id)?
        .ok_or_else(|| snapshot_error(format!("snapshot '{id}' does not exist")))
}

pub(super) fn load_snapshot_metadata(
    base_dir: &Path,
    id: &str,
) -> Result<Option<(PathBuf, SnapshotMetadata)>> {
    validate_snapshot_id(id)?;
    let base = base_dir.canonicalize().map_err(BoxError::IoError)?;
    let snapshot = base_dir.join(id);
    let file_type = match std::fs::symlink_metadata(&snapshot) {
        Ok(metadata) => metadata.file_type(),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(BoxError::IoError(error)),
    };
    if !file_type.is_dir() || file_type.is_symlink() {
        return Err(snapshot_error(format!(
            "snapshot bundle is not a plain directory: {}",
            snapshot.display()
        )));
    }
    let snapshot = snapshot.canonicalize().map_err(BoxError::IoError)?;
    if snapshot.parent() != Some(base.as_path()) {
        return Err(snapshot_error(format!(
            "snapshot bundle escapes its store: {}",
            snapshot.display()
        )));
    }
    let metadata_path = snapshot.join("metadata.json");
    let metadata_file = std::fs::symlink_metadata(&metadata_path).map_err(BoxError::IoError)?;
    if !metadata_file.file_type().is_file()
        || metadata_file.file_type().is_symlink()
        || metadata_file.len() > MAX_SNAPSHOT_METADATA_BYTES
    {
        return Err(snapshot_error(format!(
            "snapshot metadata is not a bounded plain file: {}",
            metadata_path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(metadata_file.len() as usize);
    File::open(&metadata_path)
        .and_then(|file| {
            file.take(MAX_SNAPSHOT_METADATA_BYTES + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(BoxError::IoError)?;
    if bytes.len() as u64 > MAX_SNAPSHOT_METADATA_BYTES {
        return Err(snapshot_error(format!(
            "snapshot metadata grew beyond its limit: {}",
            metadata_path.display()
        )));
    }
    let metadata: SnapshotMetadata = serde_json::from_slice(&bytes).map_err(|error| {
        snapshot_error(format!(
            "invalid snapshot metadata {}: {error}",
            metadata_path.display()
        ))
    })?;
    if metadata.id != id {
        return Err(snapshot_error(format!(
            "snapshot metadata identity '{}' does not match requested id '{id}'",
            metadata.id
        )));
    }
    Ok(Some((snapshot, metadata)))
}

fn validate_box_directory(box_dir: &Path) -> Result<()> {
    std::fs::create_dir_all(box_dir).map_err(BoxError::IoError)?;
    let metadata = std::fs::symlink_metadata(box_dir).map_err(BoxError::IoError)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(snapshot_error(format!(
            "snapshot restore destination is not a plain directory: {}",
            box_dir.display()
        )));
    }
    Ok(())
}

fn read_manifest(snapshot_directory: &Path) -> Result<Option<SnapshotRootfsManifest>> {
    let path = snapshot_directory.join(ROOTFS_MANIFEST_NAME);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(BoxError::IoError(error)),
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_ROOTFS_MANIFEST_BYTES
    {
        return Err(snapshot_error(format!(
            "snapshot rootfs manifest is not a bounded plain file: {}",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(&path)
        .and_then(|file| {
            file.take(MAX_ROOTFS_MANIFEST_BYTES + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(BoxError::IoError)?;
    if bytes.len() as u64 > MAX_ROOTFS_MANIFEST_BYTES {
        return Err(snapshot_error(format!(
            "snapshot rootfs manifest grew beyond its limit: {}",
            path.display()
        )));
    }
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        snapshot_error(format!(
            "invalid snapshot rootfs manifest {}: {error}",
            path.display()
        ))
    })
}

fn write_manifest(directory: &Path, manifest: &SnapshotRootfsManifest) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest).map_err(|error| {
        BoxError::SerializationError(format!(
            "Failed to encode snapshot rootfs manifest: {error}"
        ))
    })?;
    write_new_synced(&directory.join(ROOTFS_MANIFEST_NAME), &bytes)
}

#[cfg(target_os = "macos")]
fn write_metadata(directory: &Path, metadata: &SnapshotMetadata) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(metadata).map_err(|error| {
        BoxError::SerializationError(format!("Failed to serialize snapshot metadata: {error}"))
    })?;
    write_new_synced(&directory.join("metadata.json"), &bytes)
}

fn write_new_synced(path: &Path, bytes: &[u8]) -> Result<()> {
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(BoxError::IoError)?;
    file.write_all(bytes).map_err(BoxError::IoError)?;
    file.sync_all().map_err(BoxError::IoError)
}

#[cfg(target_os = "macos")]
fn publish_staging(staging: tempfile::TempDir, destination: &Path, base_dir: &Path) -> Result<()> {
    let staging_path = staging.keep();
    if let Err(error) = std::fs::rename(&staging_path, destination) {
        let _ = std::fs::remove_dir_all(&staging_path);
        return Err(snapshot_error(format!(
            "failed to publish snapshot {}: {error}",
            destination.display()
        )));
    }
    sync_directory(base_dir)
}

#[cfg(target_os = "macos")]
fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(BoxError::IoError)
}

fn require_absent(path: &Path, description: &str) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(_) => Err(snapshot_error(format!(
            "{description} already exists: {}",
            path.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(BoxError::IoError(error)),
    }
}

pub(super) fn validate_snapshot_id(id: &str) -> Result<()> {
    if id.is_empty()
        || id.len() > 255
        || id == "."
        || id == ".."
        || Path::new(id).components().count() != 1
        || id.contains(std::path::MAIN_SEPARATOR)
        || id.as_bytes().contains(&0)
    {
        return Err(snapshot_error(format!("invalid snapshot id '{id}'")));
    }
    Ok(())
}

fn is_lower_sha256(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn snapshot_error(message: impl Into<String>) -> BoxError {
    BoxError::CacheError(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn metadata(id: &str) -> SnapshotMetadata {
        let mut metadata = SnapshotMetadata::new(
            id.to_string(),
            id.to_string(),
            "source-box".to_string(),
            "alpine:3.20".to_string(),
        );
        metadata.image_config = Some(a3s_box_core::SnapshotImageConfig::default());
        metadata
    }

    #[test]
    fn directory_snapshot_manifest_is_explicit_and_restore_is_durable() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("state"), b"directory-generation").unwrap();
        let store = SnapshotStore::new(&temporary.path().join("snapshots")).unwrap();

        store.save(metadata("directory"), &source).unwrap();
        assert_eq!(
            store.rootfs_format("directory").unwrap(),
            SnapshotRootfsFormat::Directory
        );

        let box_dir = temporary.path().join("boxes/restored");
        let restored = store.restore_rootfs_to_box("directory", &box_dir).unwrap();
        assert_eq!(restored.metadata.id, "directory");
        assert_eq!(restored.format, SnapshotRootfsFormat::Directory);
        assert_eq!(
            PathBuf::from(std::fs::read_to_string(box_dir.join(".snapshot-lower")).unwrap()),
            store.rootfs_path("directory").canonicalize().unwrap()
        );
        assert!(box_dir
            .join(crate::resolved_image::RESOLVED_IMAGE_CONFIG_FILE)
            .is_file());
    }

    #[test]
    fn legacy_directory_snapshot_without_manifest_remains_supported() {
        let temporary = tempfile::tempdir().unwrap();
        let store = SnapshotStore::new(&temporary.path().join("snapshots")).unwrap();
        let snapshot = temporary.path().join("snapshots/legacy");
        std::fs::create_dir_all(snapshot.join("rootfs")).unwrap();
        std::fs::write(snapshot.join("rootfs/state"), b"legacy").unwrap();
        std::fs::write(
            snapshot.join("metadata.json"),
            serde_json::to_vec_pretty(&metadata("legacy")).unwrap(),
        )
        .unwrap();

        assert_eq!(
            store.rootfs_format("legacy").unwrap(),
            SnapshotRootfsFormat::Directory
        );
    }

    #[test]
    fn rootfs_manifest_cannot_be_rebound_to_other_metadata() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let store = SnapshotStore::new(&temporary.path().join("snapshots")).unwrap();
        store.save(metadata("bound"), &source).unwrap();

        let manifest = temporary.path().join("snapshots/bound/rootfs.json");
        let mut value: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest).unwrap()).unwrap();
        value["source_box_id"] = serde_json::Value::String("other-box".to_string());
        std::fs::write(&manifest, serde_json::to_vec_pretty(&value).unwrap()).unwrap();

        let error = store.rootfs_format("bound").unwrap_err().to_string();
        assert!(error.contains("identity does not match"), "{error}");
    }

    #[test]
    fn directory_snapshot_rejects_an_ambiguous_raw_payload() {
        let temporary = tempfile::tempdir().unwrap();
        let source = temporary.path().join("source");
        std::fs::create_dir(&source).unwrap();
        let store = SnapshotStore::new(&temporary.path().join("snapshots")).unwrap();
        store.save(metadata("ambiguous"), &source).unwrap();
        std::fs::create_dir(
            temporary
                .path()
                .join("snapshots/ambiguous")
                .join(RAW_EXT4_DIRECTORY_NAME),
        )
        .unwrap();

        let error = store.rootfs_format("ambiguous").unwrap_err().to_string();

        assert!(error.contains("alternate payload"), "{error}");
    }

    #[cfg(target_os = "macos")]
    fn publish_raw_box(box_dir: &Path) -> crate::rootfs::Ext4Artifact {
        let source = box_dir.join("logical-rootfs");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("state"), b"raw-generation").unwrap();
        crate::rootfs::publish_ext4_artifact(
            &source,
            &box_dir.join(RAW_EXT4_DIRECTORY_NAME),
            crate::rootfs::Ext4ArtifactOptions::from_disk_mib(16, [7; 16]).unwrap(),
        )
        .unwrap()
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn raw_ext4_snapshot_restores_an_independent_verified_clone() {
        use std::io::{Seek, SeekFrom, Write};

        let temporary = tempfile::tempdir().unwrap();
        let source_box = temporary.path().join("boxes/source");
        let source_artifact = publish_raw_box(&source_box);
        let store = SnapshotStore::new(&temporary.path().join("snapshots")).unwrap();

        let saved = store
            .save_guest_native_ext4(metadata("raw"), &source_box)
            .unwrap();
        assert!(saved.size_bytes > 0);
        assert_eq!(
            store.rootfs_format("raw").unwrap(),
            SnapshotRootfsFormat::GuestNativeExt4
        );
        assert!(!store.rootfs_path("raw").exists());

        // Mutating the source clone after capture must not alter the immutable
        // snapshot generation even though APFS initially shares its blocks.
        let mut source = OpenOptions::new()
            .write(true)
            .open(&source_artifact.disk)
            .unwrap();
        source.seek(SeekFrom::Start(0)).unwrap();
        source.write_all(b"changed-source").unwrap();
        source.sync_all().unwrap();

        let restored_box = temporary.path().join("boxes/restored");
        let restored = store.restore_rootfs_to_box("raw", &restored_box).unwrap();
        assert_eq!(restored.format, SnapshotRootfsFormat::GuestNativeExt4);
        assert!(!restored_box.join(".snapshot-lower").exists());
        let restored_artifact = crate::rootfs::open_clean_guest_native_ext4_artifact(
            &restored_box.join(RAW_EXT4_DIRECTORY_NAME),
        )
        .unwrap();
        let filesystem =
            mkext4::reader::Fs::open(File::open(&restored_artifact.disk).unwrap()).unwrap();
        let state = filesystem.resolve("/state").unwrap();
        assert_eq!(filesystem.read_file(state).unwrap(), b"raw-generation");

        assert!(store.delete("raw").unwrap());
        assert!(restored_artifact.disk.is_file());
        assert_eq!(filesystem.read_file(state).unwrap(), b"raw-generation");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn raw_ext4_restore_rejects_payload_tampering_before_publication() {
        use std::io::{Seek, SeekFrom, Write};

        let temporary = tempfile::tempdir().unwrap();
        let source_box = temporary.path().join("boxes/source");
        publish_raw_box(&source_box);
        let store = SnapshotStore::new(&temporary.path().join("snapshots")).unwrap();
        store
            .save_guest_native_ext4(metadata("tampered"), &source_box)
            .unwrap();

        let disk = temporary
            .path()
            .join("snapshots/tampered/rootfs-ext4-v1/rootfs.ext4");
        let mut disk = OpenOptions::new().write(true).open(disk).unwrap();
        disk.seek(SeekFrom::Start(0)).unwrap();
        disk.write_all(b"tampered").unwrap();
        disk.sync_all().unwrap();

        let restored_box = temporary.path().join("boxes/restored");
        let error = store
            .restore_rootfs_to_box("tampered", &restored_box)
            .unwrap_err()
            .to_string();
        assert!(error.contains("integrity mismatch"), "{error}");
        assert!(!restored_box.join(RAW_EXT4_DIRECTORY_NAME).exists());
    }
}
