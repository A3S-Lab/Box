//! Default macOS provider for a guest-owned ext4 root filesystem.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::vmm::RootfsSource;

use super::guest_native_migration::{self as migration, MigrationState};
use super::provider::{
    CaseSensitiveApfsProvider, ResumedRootfs, RootfsFinalizeOptions, RootfsOciPrepareOptions,
    RootfsProvider, RootfsResumeOptions,
};

/// Builds new OCI generations directly into ext4 and uses APFS only when a
/// legacy writable generation must be migrated without losing guest state.
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

    fn direct_filesystem_uuid(identity: &super::Ext4CacheIdentity, disk_mib: u32) -> [u8; 16] {
        use sha2::{Digest, Sha256};

        let mut hasher = Sha256::new();
        for field in [
            super::EXT4_BUILDER_ID.as_bytes(),
            identity.schema.as_bytes(),
            identity.oci_manifest_digest.as_bytes(),
            identity.platform.as_bytes(),
            identity.guest_init_sha256.as_bytes(),
            &disk_mib.to_le_bytes(),
        ] {
            hasher.update((field.len() as u64).to_le_bytes());
            hasher.update(field);
        }
        let digest = hasher.finalize();
        let mut uuid = [0; 16];
        uuid.copy_from_slice(&digest[..16]);
        uuid[6] = (uuid[6] & 0x0f) | 0x50;
        uuid[8] = (uuid[8] & 0x3f) | 0x80;
        uuid
    }

    fn host_directory_generation_exists(box_dir: &Path) -> Result<bool> {
        for directory in [
            box_dir.join("rootfs"),
            box_dir.join("upper"),
            box_dir.join("merged"),
        ] {
            match std::fs::read_dir(&directory) {
                Ok(mut entries) => {
                    if entries.next().is_some() {
                        return Ok(true);
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(BoxError::BuildError(format!(
                        "Failed to inspect legacy rootfs directory {}: {error}",
                        directory.display()
                    )))
                }
            }
        }
        Ok(false)
    }

    fn remove_compatibility_staging_directories(box_dir: &Path) -> Result<()> {
        for path in [
            box_dir.join("rootfs"),
            box_dir.join("upper"),
            box_dir.join("work"),
            box_dir.join("merged"),
        ] {
            if super::is_mountpoint(&path) {
                return Err(BoxError::BuildError(format!(
                    "Compatibility rootfs remained mounted after cleanup: {}",
                    path.display()
                )));
            }
            match std::fs::symlink_metadata(&path) {
                Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
                    std::fs::remove_dir_all(&path).map_err(BoxError::IoError)?;
                }
                Ok(_) => std::fs::remove_file(&path).map_err(BoxError::IoError)?,
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => return Err(BoxError::IoError(error)),
            }
        }
        Ok(())
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
        migration::remove_stale_publications(box_dir)?;

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

    fn prepare_oci_for_boot(
        &self,
        box_dir: &Path,
        options: RootfsOciPrepareOptions<'_>,
    ) -> Result<Option<ResumedRootfs>> {
        if options.snapshot {
            return Err(BoxError::BuildError(
                "guest-native ext4 is not yet enabled for snapshot-backed boxes; use the APFS compatibility provider"
                    .to_string(),
            ));
        }
        // A legacy sparse image is mutable user state, not an OCI cache miss.
        // Keep the existing attach-and-migrate transaction for that one case.
        if options.persistent && migration::begin_if_needed(box_dir)? {
            return Ok(None);
        }
        if !options.persistent {
            migration::remove_stale_publications(box_dir)?;
        }
        if options.persistent && Self::host_directory_generation_exists(box_dir)? {
            return Err(BoxError::StateError(
                "Persistent host-directory rootfs state cannot be replaced by a new OCI ext4 generation; select the compatibility provider and migrate or commit that state first"
                    .to_string(),
            ));
        }

        let identity = super::Ext4CacheIdentity::new(
            options.image.manifest_digest(),
            options.platform,
            options.guest_init_sha256,
        )?;
        if let Some(cache) = options.artifact_cache.as_ref() {
            if cache.oci_manifest_digest != identity.oci_manifest_digest
                || cache.platform != identity.platform
                || cache.guest_init_sha256 != identity.guest_init_sha256
            {
                return Err(BoxError::BuildError(
                    "Direct OCI rootfs identity disagrees with its ext4 cache identity".to_string(),
                ));
            }
        }

        // No compatibility construction image is authoritative here. Remove
        // any abandoned plain staging state before publishing the sole rootfs
        // generation for this box.
        CaseSensitiveApfsProvider.cleanup(box_dir, false)?;
        Self::remove_compatibility_staging_directories(box_dir)?;
        if !options.persistent {
            migration::remove(box_dir)?;
        }
        Self::remove_artifact(box_dir)?;
        migration::remove_stale_publications(box_dir)?;
        let destination = Self::artifact_directory(box_dir);
        let artifact = match options.artifact_cache.as_ref() {
            Some(cache) => super::Ext4ArtifactCache::new(
                &cache.directory,
                cache.max_entries,
                cache.max_allocated_bytes,
            )
            .materialize_with(
                &destination,
                options.disk_mib,
                &identity,
                |cache_destination, artifact_options| {
                    super::oci_ext4::publish_oci_layers_ext4(
                        options.image,
                        options.guest_init,
                        options.guest_init_sha256,
                        cache_destination,
                        artifact_options,
                    )
                },
            )?,
            None => super::oci_ext4::publish_oci_layers_ext4(
                options.image,
                options.guest_init,
                options.guest_init_sha256,
                &destination,
                super::Ext4ArtifactOptions::from_disk_mib(
                    options.disk_mib,
                    Self::direct_filesystem_uuid(&identity, options.disk_mib),
                )?,
            )?,
        };
        tracing::info!(
            disk = %artifact.disk.display(),
            layers = options.image.layer_paths().len(),
            "Prepared guest-native ext4 directly from OCI layers"
        );
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

    fn supports_direct_oci_assembly(&self) -> bool {
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
    use sha2::{Digest, Sha256};

    fn write_blob(root: &Path, bytes: &[u8]) -> String {
        let digest = hex::encode(Sha256::digest(bytes));
        let path = root.join("blobs/sha256").join(&digest);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, bytes).unwrap();
        format!("sha256:{digest}")
    }

    fn direct_test_image(root: &Path) -> crate::oci::OciImage {
        let mut layer = tar::Builder::new(Vec::new());
        let content = b"mount-free";
        let mut header = tar::Header::new_gnu();
        header.set_size(content.len() as u64);
        header.set_mode(0o640);
        header.set_uid(101);
        header.set_gid(202);
        header.set_mtime(1_704_067_200);
        header.set_cksum();
        layer
            .append_data(&mut header, "payload", content.as_slice())
            .unwrap();
        layer.finish().unwrap();
        let layer = layer.into_inner().unwrap();
        let layer_digest = write_blob(root, &layer);
        let config = serde_json::to_vec(&serde_json::json!({
            "architecture": "arm64",
            "os": "linux",
            "config": {"Cmd": ["/bin/true"]},
            "rootfs": {
                "type": "layers",
                "diff_ids": [format!("sha256:{}", hex::encode(Sha256::digest(&layer)))]
            }
        }))
        .unwrap();
        let config_digest = write_blob(root, &config);
        let manifest = serde_json::to_vec(&serde_json::json!({
            "schemaVersion": 2,
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "config": {
                "mediaType": "application/vnd.oci.image.config.v1+json",
                "digest": config_digest,
                "size": config.len()
            },
            "layers": [{
                "mediaType": "application/vnd.oci.image.layer.v1.tar",
                "digest": layer_digest,
                "size": layer.len()
            }]
        }))
        .unwrap();
        let manifest_digest = write_blob(root, &manifest);
        std::fs::write(
            root.join("oci-layout"),
            br#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(
            root.join("index.json"),
            serde_json::to_vec(&serde_json::json!({
                "schemaVersion": 2,
                "manifests": [{
                    "mediaType": "application/vnd.oci.image.manifest.v1+json",
                    "digest": manifest_digest,
                    "size": manifest.len(),
                    "platform": {"os": "linux", "architecture": "arm64"}
                }]
            }))
            .unwrap(),
        )
        .unwrap();
        crate::oci::OciImage::from_path(root).unwrap()
    }

    #[test]
    fn new_oci_generation_is_published_without_apfs_or_a_host_rootfs() {
        let temporary = tempfile::tempdir().unwrap();
        let image = direct_test_image(&temporary.path().join("image"));
        let box_dir = temporary.path().join("box");
        let guest_init = temporary.path().join("guest-init");
        std::fs::write(&guest_init, b"guest-init").unwrap();
        let guest_init_sha256 = format!("sha256:{}", hex::encode(Sha256::digest(b"guest-init")));

        let prepared = GuestNativeExt4Provider
            .prepare_oci_for_boot(
                &box_dir,
                RootfsOciPrepareOptions {
                    image: &image,
                    guest_init: &guest_init,
                    guest_init_sha256: &guest_init_sha256,
                    platform: "linux/arm64",
                    disk_mib: 16,
                    persistent: false,
                    snapshot: false,
                    artifact_cache: None,
                },
            )
            .unwrap()
            .unwrap();

        let RootfsSource::Ext4Disk { path, read_only } = prepared.source else {
            panic!("direct OCI preparation must return an ext4 disk");
        };
        assert!(!read_only);
        assert!(path.is_file());
        assert!(!box_dir.join("rootfs").exists());
        assert!(!CaseSensitiveApfsProvider::image_path(&box_dir).exists());
        let filesystem = mkext4::reader::Fs::open(std::fs::File::open(path).unwrap()).unwrap();
        let payload = filesystem.resolve("/payload").unwrap();
        assert_eq!(filesystem.read_file(payload).unwrap(), b"mount-free");
        let inode = filesystem.inode(payload).unwrap();
        assert_eq!((inode.uid, inode.gid), (101, 202));
        let init = filesystem.resolve("/sbin/init").unwrap();
        assert_eq!(filesystem.read_file(init).unwrap(), b"guest-init");
    }

    #[test]
    fn direct_preparation_never_discards_persistent_directory_state() {
        let temporary = tempfile::tempdir().unwrap();
        let image = direct_test_image(&temporary.path().join("image"));
        let box_dir = temporary.path().join("box");
        let legacy_file = box_dir.join("rootfs/guest-write");
        std::fs::create_dir_all(legacy_file.parent().unwrap()).unwrap();
        std::fs::write(&legacy_file, b"keep").unwrap();
        let guest_init = temporary.path().join("guest-init");
        std::fs::write(&guest_init, b"guest-init").unwrap();
        let guest_init_sha256 = format!("sha256:{}", hex::encode(Sha256::digest(b"guest-init")));

        let error = GuestNativeExt4Provider
            .prepare_oci_for_boot(
                &box_dir,
                RootfsOciPrepareOptions {
                    image: &image,
                    guest_init: &guest_init,
                    guest_init_sha256: &guest_init_sha256,
                    platform: "linux/arm64",
                    disk_mib: 16,
                    persistent: true,
                    snapshot: false,
                    artifact_cache: None,
                },
            )
            .unwrap_err()
            .to_string();

        assert!(error.contains("cannot be replaced"), "{error}");
        assert_eq!(std::fs::read(legacy_file).unwrap(), b"keep");
        assert!(!GuestNativeExt4Provider::artifact_directory(&box_dir).exists());
    }

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
        let stale_content = temporary.path().join(format!(
            "{}content",
            super::super::oci_ext4::CONTENT_STAGING_PREFIX
        ));
        let stale_clone = temporary.path().join(format!(
            "{}clone",
            super::super::ext4_cache::CLONE_STAGING_PREFIX
        ));
        let outside = temporary.path().join("outside");
        let stale_link = temporary.path().join(format!(
            "{}link",
            super::super::ext4::STAGING_DIRECTORY_PREFIX
        ));
        std::fs::create_dir(&stale_directory).unwrap();
        std::fs::write(stale_directory.join("partial"), b"partial").unwrap();
        std::fs::create_dir(&stale_content).unwrap();
        std::fs::write(stale_content.join("payload"), b"partial").unwrap();
        std::fs::write(&stale_clone, b"partial").unwrap();
        std::fs::write(&stale_file, b"partial").unwrap();
        std::fs::write(&outside, b"keep").unwrap();
        symlink(&outside, &stale_link).unwrap();

        migration::remove_stale_publications(temporary.path()).unwrap();

        assert!(!stale_directory.exists());
        assert!(!stale_file.exists());
        assert!(!stale_content.exists());
        assert!(!stale_clone.exists());
        assert!(std::fs::symlink_metadata(&stale_link).is_err());
        assert_eq!(std::fs::read(&outside).unwrap(), b"keep");
    }
}
