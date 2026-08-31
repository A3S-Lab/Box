//! Durable migration from the legacy APFS rootfs to guest-owned ext4.

use std::fs::File;
use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use serde::{Deserialize, Serialize};

use super::guest_native_ext4::GuestNativeExt4Provider;
use super::provider::{CaseSensitiveApfsProvider, RootfsProvider};

const MIGRATION_SCHEMA: &str = "a3s.box.rootfs-migration.v1";
const MIGRATION_FILE_NAME: &str = "rootfs-migration-v1.json";
const MIGRATION_SOURCE_FORMAT: &str = "case-sensitive-apfs-sparseimage-v2";
pub(super) const MIGRATION_STAGING_PREFIX: &str = ".a3s-rootfs-migration-";
const MAX_MIGRATION_MANIFEST_BYTES: u64 = 64 * 1024;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(super) enum MigrationState {
    Building,
    ArtifactReady,
    CleanStopVerified,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct MigrationManifest {
    schema: String,
    source_format: String,
    source_file: String,
    target_schema: String,
    target_directory: String,
    pub(super) state: MigrationState,
}

impl MigrationManifest {
    pub(super) fn new() -> Self {
        Self {
            schema: MIGRATION_SCHEMA.to_string(),
            source_format: MIGRATION_SOURCE_FORMAT.to_string(),
            source_file: CaseSensitiveApfsProvider::IMAGE_NAME.to_string(),
            target_schema: super::EXT4_ARTIFACT_SCHEMA.to_string(),
            target_directory: GuestNativeExt4Provider::ARTIFACT_DIRECTORY.to_string(),
            state: MigrationState::Building,
        }
    }

    fn validate(&self, path: &Path) -> Result<()> {
        if self.schema != MIGRATION_SCHEMA
            || self.source_format != MIGRATION_SOURCE_FORMAT
            || self.source_file != CaseSensitiveApfsProvider::IMAGE_NAME
            || self.target_schema != super::EXT4_ARTIFACT_SCHEMA
            || self.target_directory != GuestNativeExt4Provider::ARTIFACT_DIRECTORY
        {
            return Err(BoxError::BuildError(format!(
                "Unsupported rootfs migration contract in {}",
                path.display()
            )));
        }
        Ok(())
    }
}

pub(super) fn path(box_dir: &Path) -> PathBuf {
    box_dir.join(MIGRATION_FILE_NAME)
}

pub(super) fn remove_stale_publications(box_dir: &Path) -> Result<()> {
    let entries = match std::fs::read_dir(box_dir) {
        Ok(entries) => entries,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(BoxError::IoError(error)),
    };
    for entry in entries {
        let entry = entry.map_err(BoxError::IoError)?;
        let name = entry.file_name();
        let Some(name) = name.to_str() else {
            continue;
        };
        if !name.starts_with(super::ext4::STAGING_DIRECTORY_PREFIX)
            && !name.starts_with(MIGRATION_STAGING_PREFIX)
        {
            continue;
        }
        let path = entry.path();
        let metadata = std::fs::symlink_metadata(&path).map_err(BoxError::IoError)?;
        if metadata.is_dir() && !metadata.file_type().is_symlink() {
            std::fs::remove_dir_all(&path).map_err(BoxError::IoError)?;
        } else {
            std::fs::remove_file(&path).map_err(BoxError::IoError)?;
        }
    }
    Ok(())
}

pub(super) fn load(box_dir: &Path) -> Result<Option<MigrationManifest>> {
    let path = path(box_dir);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            return Err(BoxError::BuildError(format!(
                "Failed to inspect rootfs migration manifest {}: {error}",
                path.display()
            )))
        }
    };
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() == 0
        || metadata.len() > MAX_MIGRATION_MANIFEST_BYTES
    {
        return Err(BoxError::BuildError(format!(
            "Rootfs migration manifest is not a bounded plain file: {}",
            path.display()
        )));
    }

    let mut options = std::fs::OpenOptions::new();
    options.read(true);
    use std::os::unix::fs::OpenOptionsExt;
    options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let file = options.open(&path).map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to open rootfs migration manifest {}: {error}",
            path.display()
        ))
    })?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_MIGRATION_MANIFEST_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|error| {
            BoxError::BuildError(format!(
                "Failed to read rootfs migration manifest {}: {error}",
                path.display()
            ))
        })?;
    if bytes.len() as u64 > MAX_MIGRATION_MANIFEST_BYTES {
        return Err(BoxError::BuildError(format!(
            "Rootfs migration manifest grew beyond its limit: {}",
            path.display()
        )));
    }
    let manifest = serde_json::from_slice::<MigrationManifest>(&bytes).map_err(|error| {
        BoxError::BuildError(format!(
            "Invalid rootfs migration manifest {}: {error}",
            path.display()
        ))
    })?;
    manifest.validate(&path)?;
    Ok(Some(manifest))
}

pub(super) fn store(box_dir: &Path, manifest: &MigrationManifest) -> Result<()> {
    let path = path(box_dir);
    manifest.validate(&path)?;
    std::fs::create_dir_all(box_dir).map_err(BoxError::IoError)?;
    let mut bytes = serde_json::to_vec_pretty(manifest).map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to encode rootfs migration manifest: {error}"
        ))
    })?;
    bytes.push(b'\n');

    let mut temporary = tempfile::Builder::new()
        .prefix(MIGRATION_STAGING_PREFIX)
        .tempfile_in(box_dir)
        .map_err(|error| {
            BoxError::BuildError(format!(
                "Failed to create rootfs migration manifest beside {}: {error}",
                path.display()
            ))
        })?;
    temporary.write_all(&bytes).map_err(BoxError::IoError)?;
    temporary.as_file().sync_all().map_err(BoxError::IoError)?;
    let published = temporary.persist(&path).map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to publish rootfs migration manifest {}: {}",
            path.display(),
            error.error
        ))
    })?;
    published.sync_all().map_err(BoxError::IoError)?;
    File::open(box_dir)
        .and_then(|directory| directory.sync_all())
        .map_err(BoxError::IoError)
}

pub(super) fn validate_legacy_source(box_dir: &Path, required: bool) -> Result<bool> {
    let source = CaseSensitiveApfsProvider::image_path(box_dir);
    match std::fs::symlink_metadata(&source) {
        Ok(metadata) if metadata.is_file() && !metadata.file_type().is_symlink() => Ok(true),
        Ok(_) => Err(BoxError::BuildError(format!(
            "Legacy APFS migration source is not a plain sparse image: {}",
            source.display()
        ))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound && !required => Ok(false),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            Err(BoxError::StateError(format!(
                "Legacy APFS migration source is missing: {}",
                source.display()
            )))
        }
        Err(error) => Err(BoxError::BuildError(format!(
            "Failed to inspect legacy APFS migration source {}: {error}",
            source.display()
        ))),
    }
}

/// Persist migration intent before the legacy image is attached. New
/// guest-native constructions do not have an image yet and therefore do not
/// create a migration transaction.
pub(super) fn begin_if_needed(box_dir: &Path) -> Result<bool> {
    remove_stale_publications(box_dir)?;
    let existing = load(box_dir)?;
    let source_exists = validate_legacy_source(box_dir, existing.is_some())?;
    if !source_exists {
        return Ok(false);
    }

    match existing {
        None => store(box_dir, &MigrationManifest::new())?,
        Some(manifest) if manifest.state == MigrationState::Building => {}
        Some(manifest) => {
            return Err(BoxError::StateError(format!(
                "Rootfs migration is {:?} but its ext4 generation is unavailable; refusing to overwrite the rollback source",
                manifest.state
            )))
        }
    }
    Ok(true)
}

pub(super) fn detach_legacy_source(box_dir: &Path) -> Result<()> {
    let mountpoint = box_dir.join("rootfs");
    if super::is_mountpoint(&mountpoint) {
        super::unmount_box_rootfs_for_handoff(&mountpoint)?;
    }
    CaseSensitiveApfsProvider.cleanup(box_dir, true)
}

pub(super) fn publish_state(box_dir: &Path, state: MigrationState) -> Result<()> {
    let mut manifest = load(box_dir)?.ok_or_else(|| {
        BoxError::StateError("Rootfs migration manifest disappeared during handoff".to_string())
    })?;
    manifest.state = state;
    store(box_dir, &manifest)
}

pub(super) fn reconcile_for_resume(box_dir: &Path) -> Result<bool> {
    let Some(manifest) = load(box_dir)? else {
        return Ok(false);
    };
    let source_required = manifest.state != MigrationState::CleanStopVerified;
    validate_legacy_source(box_dir, source_required)?;
    detach_legacy_source(box_dir)?;
    if manifest.state == MigrationState::Building {
        publish_state(box_dir, MigrationState::ArtifactReady)?;
    }
    Ok(true)
}

pub(super) fn remove(box_dir: &Path) -> Result<()> {
    let path = path(box_dir);
    match std::fs::remove_file(&path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(BoxError::BuildError(format!(
            "Failed to remove rootfs migration manifest {}: {error}",
            path.display()
        ))),
    }
}

pub(super) fn preserve_on_boot_failure(box_dir: &Path) -> bool {
    !matches!(
        std::fs::symlink_metadata(path(box_dir)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound
    )
}

pub(super) fn record_clean_stop(box_dir: &Path) -> Result<()> {
    let Some(manifest) = load(box_dir)? else {
        return Ok(());
    };
    if manifest.state == MigrationState::CleanStopVerified {
        return Ok(());
    }
    if manifest.state != MigrationState::ArtifactReady {
        return Err(BoxError::StateError(format!(
            "Cannot verify rootfs migration clean stop from {:?} state",
            manifest.state
        )));
    }
    validate_legacy_source(box_dir, true)?;
    super::ext4_artifact::open_ext4_artifact(&GuestNativeExt4Provider::artifact_directory(
        box_dir,
    ))?;
    publish_state(box_dir, MigrationState::CleanStopVerified)
}
