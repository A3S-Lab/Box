//! Loading and verification for published guest-native ext4 artifacts.

use std::fs::File;
use std::io::Read;
use std::path::Path;

use a3s_box_core::error::{BoxError, Result};

use super::ext4::{
    validate_ext4_image, validate_ext4_image_for_resume, Ext4Artifact, Ext4ArtifactManifest,
    Ext4ArtifactOptions, Ext4ResumeValidation, DISK_FILE_NAME, EXT4_ARTIFACT_SCHEMA,
    EXT4_BUILDER_ID, LEGACY_EXT4_BUILDER_IDS, MANIFEST_FILE_NAME,
};

const MAX_ARTIFACT_MANIFEST_BYTES: u64 = 64 * 1024;

/// Open and structurally verify an already published artifact generation.
///
/// Consumers never trust a cache directory based only on its name. The
/// manifest contract, disk type and capacity, and ext4 structure are all
/// revalidated before the image can be cloned or handed to the VMM.
pub(super) fn open_ext4_artifact(directory: &Path) -> Result<Ext4Artifact> {
    open_ext4_artifact_inner(directory, false).map(|(artifact, _)| artifact)
}

/// Open a mutable persistent generation for boot.
///
/// Clean filesystems receive full structural verification. A generation left
/// mounted by a host crash is accepted only when its primary superblock still
/// matches the exact A3S artifact contract; the guest kernel then owns journal
/// replay inside the VM.
#[cfg(target_os = "macos")]
pub(super) fn open_ext4_artifact_for_resume(
    directory: &Path,
) -> Result<(Ext4Artifact, Ext4ResumeValidation)> {
    open_ext4_artifact_inner(directory, true)
}

fn open_ext4_artifact_inner(
    directory: &Path,
    allow_journal_recovery: bool,
) -> Result<(Ext4Artifact, Ext4ResumeValidation)> {
    let directory_metadata = std::fs::symlink_metadata(directory).map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to inspect ext4 artifact directory {}: {error}",
            directory.display()
        ))
    })?;
    if !directory_metadata.is_dir() || directory_metadata.file_type().is_symlink() {
        return Err(BoxError::BuildError(format!(
            "ext4 artifact path is not a plain directory: {}",
            directory.display()
        )));
    }

    let manifest_path = directory.join(MANIFEST_FILE_NAME);
    let manifest_metadata = std::fs::symlink_metadata(&manifest_path).map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to inspect ext4 artifact manifest {}: {error}",
            manifest_path.display()
        ))
    })?;
    if !manifest_metadata.is_file() || manifest_metadata.file_type().is_symlink() {
        return Err(BoxError::BuildError(format!(
            "ext4 artifact manifest is not a plain file: {}",
            manifest_path.display()
        )));
    }
    if manifest_metadata.len() > MAX_ARTIFACT_MANIFEST_BYTES {
        return Err(BoxError::BuildError(format!(
            "ext4 artifact manifest {} exceeds {} bytes",
            manifest_path.display(),
            MAX_ARTIFACT_MANIFEST_BYTES
        )));
    }
    let mut manifest_bytes = Vec::with_capacity(manifest_metadata.len() as usize);
    File::open(&manifest_path)
        .and_then(|file| {
            file.take(MAX_ARTIFACT_MANIFEST_BYTES + 1)
                .read_to_end(&mut manifest_bytes)
        })
        .map_err(|error| {
            BoxError::BuildError(format!(
                "Failed to read ext4 artifact manifest {}: {error}",
                manifest_path.display()
            ))
        })?;
    if manifest_bytes.len() as u64 > MAX_ARTIFACT_MANIFEST_BYTES {
        return Err(BoxError::BuildError(format!(
            "ext4 artifact manifest {} grew beyond {} bytes",
            manifest_path.display(),
            MAX_ARTIFACT_MANIFEST_BYTES
        )));
    }
    let manifest: Ext4ArtifactManifest =
        serde_json::from_slice(&manifest_bytes).map_err(|error| {
            BoxError::BuildError(format!(
                "Invalid ext4 artifact manifest {}: {error}",
                manifest_path.display()
            ))
        })?;
    let fs_uuid = validate_artifact_manifest(&manifest, &manifest_path)?;

    let disk = directory.join(DISK_FILE_NAME);
    let disk_metadata = std::fs::symlink_metadata(&disk).map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to inspect ext4 artifact disk {}: {error}",
            disk.display()
        ))
    })?;
    if !disk_metadata.is_file() || disk_metadata.file_type().is_symlink() {
        return Err(BoxError::BuildError(format!(
            "ext4 artifact disk is not a plain file: {}",
            disk.display()
        )));
    }
    let validation = if allow_journal_recovery {
        validate_ext4_image_for_resume(&disk, manifest.capacity_bytes, fs_uuid)?
    } else {
        validate_ext4_image(&disk, manifest.capacity_bytes)?;
        Ext4ResumeValidation::Clean
    };

    Ok((
        Ext4Artifact {
            directory: directory.to_path_buf(),
            disk,
            manifest,
        },
        validation,
    ))
}

fn validate_artifact_manifest(manifest: &Ext4ArtifactManifest, path: &Path) -> Result<[u8; 16]> {
    let supported_builder = manifest.builder == EXT4_BUILDER_ID
        || LEGACY_EXT4_BUILDER_IDS.contains(&manifest.builder.as_str());
    if manifest.schema != EXT4_ARTIFACT_SCHEMA
        || !supported_builder
        || manifest.format != "raw-ext4"
    {
        return Err(BoxError::BuildError(format!(
            "Unsupported ext4 artifact contract in {}",
            path.display()
        )));
    }
    Ext4ArtifactOptions {
        capacity_bytes: manifest.capacity_bytes,
        fs_uuid: [0; 16],
        epoch: 0,
    }
    .validate()?;
    let uuid = hex::decode(&manifest.fs_uuid).map_err(|error| {
        BoxError::BuildError(format!(
            "Invalid ext4 artifact UUID in {}: {error}",
            path.display()
        ))
    })?;
    let uuid: [u8; 16] = uuid.try_into().map_err(|uuid: Vec<u8>| {
        BoxError::BuildError(format!(
            "Invalid ext4 artifact UUID length {} in {}",
            uuid.len(),
            path.display()
        ))
    })?;
    Ok(uuid)
}
