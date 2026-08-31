//! Experimental macOS provider for a guest-owned ext4 root filesystem.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::vmm::RootfsSource;

use super::guest_native_migration::{self as migration, MigrationState};
use super::provider::{
    CaseSensitiveApfsProvider, ResumedRootfs, RootfsFinalizeOptions, RootfsProvider,
    RootfsResumeOptions,
};

/// Uses APFS only as a private construction workspace, then hands a raw ext4
/// disk to the guest and detaches APFS before the VMM starts.
///
/// Persistent generations reuse the validated raw disk directly. Stopped
/// archive operations use a separate read-only maintenance VM; snapshot-backed
/// generations remain disabled until disk and memory identity are coupled.
pub struct GuestNativeExt4Provider;

impl GuestNativeExt4Provider {
    pub(super) const ARTIFACT_DIRECTORY: &'static str = "rootfs-ext4-v1";

    pub(crate) fn artifact_directory(box_dir: &Path) -> PathBuf {
        box_dir.join(Self::ARTIFACT_DIRECTORY)
    }

    pub(super) fn migration_path(box_dir: &Path) -> PathBuf {
        migration::path(box_dir)
    }

    fn remove_artifact(box_dir: &Path) -> Result<()> {
        let artifact = Self::artifact_directory(box_dir);
        match std::fs::symlink_metadata(&artifact) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                std::fs::remove_dir_all(&artifact).map_err(|error| {
                    BoxError::BuildError(format!(
                        "Failed to remove ext4 artifact {}: {error}",
                        artifact.display()
                    ))
                })
            }
            Ok(_) => std::fs::remove_file(&artifact).map_err(|error| {
                BoxError::BuildError(format!(
                    "Failed to remove invalid ext4 artifact path {}: {error}",
                    artifact.display()
                ))
            }),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(error) => Err(BoxError::BuildError(format!(
                "Failed to inspect ext4 artifact {}: {error}",
                artifact.display()
            ))),
        }
    }

    fn filesystem_uuid(box_dir: &Path, staged_rootfs: &Path, disk_mib: u32) -> Result<[u8; 16]> {
        use sha2::{Digest, Sha256};
        use std::io::Read;
        use std::os::unix::ffi::OsStrExt;

        let mut hasher = Sha256::new();
        hasher.update(super::EXT4_BUILDER_ID.as_bytes());
        hasher.update(disk_mib.to_le_bytes());
        let metadata_path = staged_rootfs.join(
            a3s_box_core::rootfs_metadata::IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/'),
        );
        match std::fs::File::open(&metadata_path) {
            Ok(file) => {
                const MAX_IDENTITY_BYTES: u64 = 64 * 1024 * 1024;
                let length = file.metadata().map_err(BoxError::IoError)?.len();
                if length > MAX_IDENTITY_BYTES {
                    return Err(BoxError::BuildError(format!(
                        "Image metadata exceeds the ext4 identity limit at {}",
                        metadata_path.display()
                    )));
                }
                let mut bytes = Vec::with_capacity(length as usize);
                file.take(MAX_IDENTITY_BYTES + 1)
                    .read_to_end(&mut bytes)
                    .map_err(BoxError::IoError)?;
                if bytes.len() as u64 > MAX_IDENTITY_BYTES {
                    return Err(BoxError::BuildError(format!(
                        "Image metadata grew beyond the ext4 identity limit at {}",
                        metadata_path.display()
                    )));
                }
                hasher.update(bytes);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                hasher.update(box_dir.as_os_str().as_bytes())
            }
            Err(error) => return Err(BoxError::IoError(error)),
        }
        let digest = hasher.finalize();
        let mut uuid = [0u8; 16];
        uuid.copy_from_slice(&digest[..16]);
        uuid[6] = (uuid[6] & 0x0f) | 0x50;
        uuid[8] = (uuid[8] & 0x3f) | 0x80;
        Ok(uuid)
    }
}

impl RootfsProvider for GuestNativeExt4Provider {
    fn resume_for_boot(
        &self,
        box_dir: &Path,
        options: RootfsResumeOptions,
    ) -> Result<Option<ResumedRootfs>> {
        if !options.persistent {
            return Ok(None);
        }
        if options.snapshot {
            return Err(BoxError::BuildError(
                "guest-native ext4 is not yet enabled for snapshot-backed boxes; use the APFS compatibility provider"
                    .to_string(),
            ));
        }

        let directory = Self::artifact_directory(box_dir);
        match std::fs::symlink_metadata(&directory) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(BoxError::IoError(error)),
            Ok(_) => {}
        }
        let (artifact, validation) =
            super::ext4_artifact::open_ext4_artifact_for_resume(&directory)?;
        let expected_capacity = u64::from(options.disk_mib)
            .checked_mul(1024 * 1024)
            .ok_or_else(|| BoxError::BuildError("ext4 capacity overflow".to_string()))?;
        if artifact.manifest.capacity_bytes != expected_capacity {
            return Err(BoxError::BuildError(format!(
                "persistent ext4 capacity is {} bytes but the box requests {} bytes; online root disk resize is not implemented",
                artifact.manifest.capacity_bytes, expected_capacity
            )));
        }
        if validation == super::ext4::Ext4ResumeValidation::JournalRecoveryRequired {
            tracing::warn!(
                disk = %artifact.disk.display(),
                "Persistent ext4 generation was not cleanly handed off; delegating journal replay to the guest kernel"
            );
        }

        // A resumed generation is authoritative. A legacy migration source is
        // retained as rollback evidence but must be detached; an unrelated
        // construction image is stale and can be removed.
        if !migration::reconcile_for_resume(box_dir)? {
            CaseSensitiveApfsProvider.cleanup(box_dir, false)?;
        }
        Ok(Some(ResumedRootfs {
            source: RootfsSource::ext4_disk(artifact.disk, false),
            guest_init_exec: "/sbin/init".to_string(),
        }))
    }

    fn prepare(&self, box_dir: &Path, cache_dir: &Path) -> Result<PathBuf> {
        migration::begin_if_needed(box_dir)?;
        CaseSensitiveApfsProvider.prepare(box_dir, cache_dir)
    }

    fn prepare_empty(&self, box_dir: &Path) -> Result<PathBuf> {
        migration::begin_if_needed(box_dir)?;
        CaseSensitiveApfsProvider.prepare_empty(box_dir)
    }

    fn finalize_for_boot(
        &self,
        box_dir: &Path,
        staged_rootfs: &Path,
        options: RootfsFinalizeOptions,
    ) -> Result<RootfsSource> {
        if options.snapshot {
            return Err(BoxError::BuildError(
                "guest-native ext4 is not yet enabled for snapshot-backed boxes; use the APFS compatibility provider"
                    .to_string(),
            ));
        }

        let migration = migration::load(box_dir)?;
        if let Some(manifest) = migration.as_ref() {
            if manifest.state != MigrationState::Building {
                return Err(BoxError::StateError(format!(
                    "Cannot rebuild rootfs migration in {:?} state",
                    manifest.state
                )));
            }
            migration::validate_legacy_source(box_dir, true)?;
        }

        Self::remove_artifact(box_dir)?;
        migration::remove_stale_publications(box_dir)?;
        let destination = Self::artifact_directory(box_dir);
        // A migrated tree may contain guest writes that are intentionally not
        // represented by the immutable OCI cache identity. Always build it from
        // the exact attached legacy filesystem.
        let artifact = match options
            .artifact_cache
            .as_ref()
            .filter(|_| migration.is_none())
        {
            Some(cache_options) => {
                let identity = super::Ext4CacheIdentity::new(
                    cache_options.oci_manifest_digest.clone(),
                    cache_options.platform.clone(),
                    cache_options.guest_init_sha256.clone(),
                )?;
                super::Ext4ArtifactCache::new(
                    &cache_options.directory,
                    cache_options.max_entries,
                    cache_options.max_allocated_bytes,
                )
                .materialize(
                    staged_rootfs,
                    &destination,
                    options.disk_mib,
                    &identity,
                )?
            }
            None => {
                let uuid = Self::filesystem_uuid(box_dir, staged_rootfs, options.disk_mib)?;
                super::publish_ext4_artifact(
                    staged_rootfs,
                    &destination,
                    super::Ext4ArtifactOptions::from_disk_mib(options.disk_mib, uuid)?,
                )?
            }
        };

        super::unmount_box_rootfs_for_handoff(staged_rootfs)?;
        if migration.is_some() {
            migration::detach_legacy_source(box_dir)?;
            migration::publish_state(box_dir, MigrationState::ArtifactReady)?;
        } else {
            // The raw artifact is now the only rootfs generation. Keeping a new
            // construction sparseimage would create a misleading second source
            // of truth and consume host space until stop.
            CaseSensitiveApfsProvider.cleanup(box_dir, false)?;
        }
        Ok(RootfsSource::ext4_disk(artifact.disk, false))
    }

    fn cleanup(&self, box_dir: &Path, persistent: bool) -> Result<()> {
        let staging_result = CaseSensitiveApfsProvider.cleanup(box_dir, persistent);
        let stale_result = migration::remove_stale_publications(box_dir);
        let artifact_result = if persistent {
            Ok(())
        } else {
            Self::remove_artifact(box_dir)
        };
        staging_result?;
        stale_result?;
        artifact_result?;
        if !persistent {
            migration::remove(box_dir)?;
        }
        Ok(())
    }

    fn preserve_on_boot_failure(&self, box_dir: &Path) -> bool {
        migration::preserve_on_boot_failure(box_dir)
    }

    fn record_clean_stop(&self, box_dir: &Path) -> Result<()> {
        migration::record_clean_stop(box_dir)
    }

    fn name(&self) -> &'static str {
        "guest-native-ext4"
    }

    fn supports_artifact_cache(&self) -> bool {
        true
    }

    fn guest_owns_terminal_fencing(&self) -> bool {
        true
    }

    fn guest_owns_diff_baseline(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::super::guest_native_migration::{
        self as migration, MigrationManifest, MigrationState, MIGRATION_STAGING_PREFIX,
    };
    use super::*;

    #[test]
    fn migration_intent_is_durable_and_idempotent() {
        let temporary = tempfile::tempdir().unwrap();
        let source = CaseSensitiveApfsProvider::image_path(temporary.path());
        std::fs::write(&source, b"legacy").unwrap();

        assert!(migration::begin_if_needed(temporary.path()).unwrap());
        assert!(migration::begin_if_needed(temporary.path()).unwrap());
        let manifest = migration::load(temporary.path()).unwrap().unwrap();
        assert_eq!(manifest.state, MigrationState::Building);
        assert!(GuestNativeExt4Provider.preserve_on_boot_failure(temporary.path()));
    }

    #[test]
    fn migration_refuses_to_rebuild_a_missing_published_generation() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::write(
            CaseSensitiveApfsProvider::image_path(temporary.path()),
            b"legacy",
        )
        .unwrap();
        let mut manifest = MigrationManifest::new();
        manifest.state = MigrationState::ArtifactReady;
        migration::store(temporary.path(), &manifest).unwrap();

        let error = migration::begin_if_needed(temporary.path())
            .unwrap_err()
            .to_string();
        assert!(error.contains("refusing to overwrite"), "{error}");
    }

    #[test]
    fn verified_migration_manifest_survives_explicit_source_gc() {
        let temporary = tempfile::tempdir().unwrap();
        let mut manifest = MigrationManifest::new();
        manifest.state = MigrationState::CleanStopVerified;
        migration::store(temporary.path(), &manifest).unwrap();

        assert!(!migration::validate_legacy_source(temporary.path(), false).unwrap());
        assert_eq!(
            migration::load(temporary.path()).unwrap().unwrap().state,
            MigrationState::CleanStopVerified
        );
    }

    #[test]
    fn clean_ext4_handoff_verifies_the_migration_transaction() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::write(
            CaseSensitiveApfsProvider::image_path(temporary.path()),
            b"legacy",
        )
        .unwrap();
        let source = temporary.path().join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("data"), b"migrated").unwrap();
        super::super::publish_ext4_artifact(
            &source,
            &GuestNativeExt4Provider::artifact_directory(temporary.path()),
            super::super::Ext4ArtifactOptions::from_disk_mib(16, [7; 16]).unwrap(),
        )
        .unwrap();
        let mut manifest = MigrationManifest::new();
        manifest.state = MigrationState::ArtifactReady;
        migration::store(temporary.path(), &manifest).unwrap();

        GuestNativeExt4Provider
            .record_clean_stop(temporary.path())
            .unwrap();

        assert_eq!(
            migration::load(temporary.path()).unwrap().unwrap().state,
            MigrationState::CleanStopVerified
        );
    }

    #[test]
    fn published_artifact_resumes_a_crash_before_state_advance() {
        let temporary = tempfile::tempdir().unwrap();
        std::fs::write(
            CaseSensitiveApfsProvider::image_path(temporary.path()),
            b"legacy",
        )
        .unwrap();
        let source = temporary.path().join("source");
        std::fs::create_dir(&source).unwrap();
        std::fs::write(source.join("data"), b"migrated").unwrap();
        super::super::publish_ext4_artifact(
            &source,
            &GuestNativeExt4Provider::artifact_directory(temporary.path()),
            super::super::Ext4ArtifactOptions::from_disk_mib(16, [9; 16]).unwrap(),
        )
        .unwrap();
        migration::store(temporary.path(), &MigrationManifest::new()).unwrap();

        let resumed = GuestNativeExt4Provider
            .resume_for_boot(
                temporary.path(),
                RootfsResumeOptions {
                    disk_mib: 16,
                    persistent: true,
                    snapshot: false,
                },
            )
            .unwrap()
            .unwrap();

        assert!(matches!(resumed.source, RootfsSource::Ext4Disk { .. }));
        assert_eq!(
            migration::load(temporary.path()).unwrap().unwrap().state,
            MigrationState::ArtifactReady
        );
        assert!(CaseSensitiveApfsProvider::image_path(temporary.path()).is_file());
    }

    #[test]
    fn stale_publication_cleanup_never_follows_symlinks() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let stale_directory = temporary.path().join(format!(
            "{}directory",
            super::super::ext4::STAGING_DIRECTORY_PREFIX
        ));
        let stale_file = temporary
            .path()
            .join(format!("{MIGRATION_STAGING_PREFIX}file"));
        let outside = temporary.path().join("outside");
        let stale_link = temporary.path().join(format!(
            "{}link",
            super::super::ext4::STAGING_DIRECTORY_PREFIX
        ));
        std::fs::create_dir(&stale_directory).unwrap();
        std::fs::write(stale_directory.join("partial"), b"partial").unwrap();
        std::fs::write(&stale_file, b"partial").unwrap();
        std::fs::write(&outside, b"keep").unwrap();
        symlink(&outside, &stale_link).unwrap();

        migration::remove_stale_publications(temporary.path()).unwrap();

        assert!(!stale_directory.exists());
        assert!(!stale_file.exists());
        assert!(std::fs::symlink_metadata(&stale_link).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
    }
}
