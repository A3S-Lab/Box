//! Rootfs provider — stages a rootfs and finalizes its VMM transport.
//!
//! Portable providers:
//! - `CopyProvider` — full recursive copy (works everywhere, current default)
//! - `OverlayProvider` — Linux overlayfs mount (near-instant, CoW)
//!
//! macOS also has a case-sensitive APFS compatibility provider. The finalizer
//! boundary allows that mounted staging model to be replaced by a guest-native
//! block artifact without teaching OCI preparation about VMM transports.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::vmm::RootfsSource;

#[cfg(target_os = "macos")]
pub(crate) use super::apfs::CaseSensitiveApfsProvider;
#[cfg(target_os = "macos")]
use super::guest_native_ext4::GuestNativeExt4Provider;

/// Lifecycle constraints that a provider must honor before handing the
/// rootfs to the VMM.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootfsFinalizeOptions {
    pub disk_mib: u32,
    pub persistent: bool,
    pub snapshot: bool,
    pub artifact_cache: Option<RootfsArtifactCacheOptions>,
}

/// Constraints for reopening an already guest-owned rootfs generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RootfsResumeOptions {
    pub disk_mib: u32,
    pub persistent: bool,
    pub snapshot: bool,
}

/// A validated rootfs generation that no longer has a host directory view.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResumedRootfs {
    pub source: RootfsSource,
    pub guest_init_exec: String,
}

/// Exact identity and resource bounds for an immutable provider artifact.
///
/// This deliberately excludes the mutable image reference: the resolved OCI
/// manifest digest is the content authority. The guest-init digest is separate
/// because A3S installs that runtime binary after OCI extraction.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RootfsArtifactCacheOptions {
    pub directory: PathBuf,
    pub oci_manifest_digest: String,
    pub platform: String,
    pub guest_init_sha256: String,
    pub max_entries: usize,
    pub max_allocated_bytes: u64,
}

/// Abstracts how a rootfs directory is prepared for a box from a cached lower layer.
pub trait RootfsProvider: Send + Sync {
    /// Reopen a durable guest-owned generation without reconstructing a host
    /// staging tree. Directory providers return `None`.
    fn resume_for_boot(
        &self,
        box_dir: &Path,
        options: RootfsResumeOptions,
    ) -> Result<Option<ResumedRootfs>> {
        let _ = (box_dir, options);
        Ok(None)
    }

    /// Prepare a rootfs at `box_dir` from the cached read-only layer at `cache_dir`.
    ///
    /// The returned directory is the host-side staging view. Runtime code may
    /// still inspect and update it until [`Self::finalize_for_boot`] is called.
    fn prepare(&self, box_dir: &Path, cache_dir: &Path) -> Result<PathBuf>;

    /// Prepare an empty writable rootfs for an OCI cache miss.
    fn prepare_empty(&self, box_dir: &Path) -> Result<PathBuf> {
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).map_err(|error| {
            BoxError::BuildError(format!(
                "Failed to create rootfs {}: {error}",
                rootfs.display()
            ))
        })?;
        Ok(rootfs)
    }

    /// Finalize the staged tree and choose the root filesystem transport.
    ///
    /// This is called exactly after the last host-side rootfs mutation and
    /// before the VMM starts. Directory providers keep the existing virtio-fs
    /// behavior. A guest-native provider can atomically publish a raw ext4
    /// artifact here, detach any temporary host staging mount, and return an
    /// [`RootfsSource::Ext4Disk`].
    ///
    /// `disk_mib` is the configured logical capacity, not a request to eagerly
    /// allocate every byte on the host.
    fn finalize_for_boot(
        &self,
        box_dir: &Path,
        staged_rootfs: &Path,
        options: RootfsFinalizeOptions,
    ) -> Result<RootfsSource> {
        let _ = (box_dir, options);
        Ok(RootfsSource::directory(staged_rootfs))
    }

    /// Cleanup after box stops.
    ///
    /// When `persistent` is true, the writable layer (overlay upper dir or copy
    /// rootfs) is preserved on disk so changes survive the next start.
    /// When false, the writable layer is wiped for a clean slate.
    fn cleanup(&self, box_dir: &Path, persistent: bool) -> Result<()>;

    /// Whether a failed boot must retain the provider's rootfs generations.
    ///
    /// Most providers can discard a partially prepared first boot. A provider
    /// performing an in-place migration must keep both the rollback source and
    /// the atomically published target until the migration is verified.
    fn preserve_on_boot_failure(&self, box_dir: &Path) -> bool {
        let _ = box_dir;
        false
    }

    /// Record that a guest-owned rootfs completed a verified clean stop.
    ///
    /// The default is a no-op. Migration providers use this hook to advance a
    /// durable transaction only after the runtime has observed the guest's
    /// read-only handoff acknowledgement.
    fn record_clean_stop(&self, box_dir: &Path) -> Result<()> {
        let _ = box_dir;
        Ok(())
    }

    /// Human-readable name for logging.
    fn name(&self) -> &'static str;

    /// Whether this provider can consume the immutable artifact cache contract.
    fn supports_artifact_cache(&self) -> bool {
        false
    }

    /// Whether guest-init, rather than the host staging view, owns terminal
    /// metadata invalidation for this provider's supported lifecycle modes.
    fn guest_owns_terminal_fencing(&self) -> bool {
        false
    }

    /// Whether guest-init must capture the pristine diff baseline because the
    /// finalized rootfs has no host-visible directory.
    fn guest_owns_diff_baseline(&self) -> bool {
        false
    }
}

/// Full recursive copy provider — works on all platforms.
///
/// This is the original behavior: copies the entire cached rootfs into
/// `box_dir/rootfs/`. Safe but slow for large images.
pub struct CopyProvider;

impl RootfsProvider for CopyProvider {
    fn prepare(&self, box_dir: &Path, cache_dir: &Path) -> Result<PathBuf> {
        let rootfs = box_dir.join("rootfs");
        // Reuse existing rootfs when persistent and already populated
        if rootfs.exists() {
            tracing::info!(path = %rootfs.display(), "Reusing persistent rootfs");
            return Ok(rootfs);
        }
        crate::cache::layer_cache::copy_dir_recursive(cache_dir, &rootfs)?;
        Ok(rootfs)
    }

    fn cleanup(&self, box_dir: &Path, persistent: bool) -> Result<()> {
        if persistent {
            tracing::info!("Persistent box: keeping rootfs on disk");
            return Ok(());
        }
        let rootfs = box_dir.join("rootfs");
        if rootfs.exists() {
            std::fs::remove_dir_all(&rootfs).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to remove rootfs {}: {}",
                    rootfs.display(),
                    e
                ))
            })?;
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "copy"
    }
}

/// Overlayfs provider — near-instant CoW mounts (Linux only).
///
/// Layout:
/// ```text
/// cache_dir/           ← lower (read-only, shared across boxes)
/// box_dir/upper/       ← upper (per-box writes)
/// box_dir/work/        ← overlayfs workdir
/// box_dir/merged/      ← merged view → RootfsSource::Directory
/// ```
pub struct OverlayProvider;

impl OverlayProvider {
    fn lower_dir(box_dir: &Path, cache_dir: &Path) -> Result<PathBuf> {
        let rootfs = box_dir.join("rootfs");
        match std::fs::read_dir(&rootfs) {
            Ok(mut entries) => {
                if entries.next().is_some() {
                    // A cache miss builds the first generation directly in `rootfs`.
                    // Once that generation has run, the cache will usually be warm.
                    // Keep the original writable tree as the overlay lower instead
                    // of switching the next generation to the immutable image cache;
                    // otherwise persistent guest writes silently disappear on restart.
                    Ok(rootfs)
                } else {
                    Ok(cache_dir.to_path_buf())
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                Ok(cache_dir.to_path_buf())
            }
            Err(error) => Err(BoxError::BuildError(format!(
                "Failed to inspect existing rootfs {}: {error}",
                rootfs.display()
            ))),
        }
    }
}

impl RootfsProvider for OverlayProvider {
    fn prepare(&self, box_dir: &Path, cache_dir: &Path) -> Result<PathBuf> {
        let lower = Self::lower_dir(box_dir, cache_dir)?;
        let upper = box_dir.join("upper");
        let work = box_dir.join("work");
        let merged = box_dir.join("merged");

        for dir in [&upper, &work, &merged] {
            std::fs::create_dir_all(dir).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to create overlay dir {}: {}",
                    dir.display(),
                    e
                ))
            })?;
        }

        // Idempotent: a restart re-runs prepare(); without this guard each call
        // stacks another overlay on `merged` (the leaked double/triple mounts).
        if super::is_mountpoint(&merged) {
            tracing::debug!(merged = %merged.display(), "Overlay already mounted; reusing");
            return Ok(merged);
        }

        super::overlay::overlay_mount(&lower, &upper, &work, &merged)?;

        tracing::info!(
            lower = %lower.display(),
            merged = %merged.display(),
            "Overlay mount ready"
        );

        Ok(merged)
    }

    fn cleanup(&self, box_dir: &Path, persistent: bool) -> Result<()> {
        let merged = box_dir.join("merged");

        if persistent {
            // A retained upper is about to become the next generation's
            // writable layer. Fully release every old mount before reuse;
            // lazy detach can keep an old namespace writer alive and make the
            // replacement generation observe stale rootfs state.
            super::unmount_box_overlay_for_reuse(&merged)?;
            // Keep both possible persistent generations: a cache-miss generation
            // lives in `rootfs`, while later overlay writes live in `upper`.
            // The next prepare mounts their union again.
            tracing::info!("Persistent box: keeping rootfs and overlay upper on disk");
            for dir_name in &["merged", "work"] {
                let dir = box_dir.join(dir_name);
                if dir.exists() {
                    if let Err(e) = std::fs::remove_dir_all(&dir) {
                        tracing::warn!(path = %dir.display(), error = %e, "Failed to remove overlay dir");
                    }
                }
            }
            return Ok(());
        }

        // A discarded rootfs can use bounded lazy unmount cleanup. It will
        // never be mounted as a replacement generation.
        super::unmount_box_overlay(&merged);

        for dir_name in &["rootfs", "upper", "work", "merged"] {
            let dir = box_dir.join(dir_name);
            if dir.exists() {
                if let Err(e) = std::fs::remove_dir_all(&dir) {
                    tracing::warn!(
                        path = %dir.display(),
                        error = %e,
                        "Failed to remove overlay dir"
                    );
                }
            }
        }

        Ok(())
    }

    fn name(&self) -> &'static str {
        "overlay"
    }
}

/// Auto-detect the best available rootfs provider for the current platform.
pub fn default_provider() -> Box<dyn RootfsProvider> {
    #[cfg(target_os = "macos")]
    {
        if std::env::var("A3S_BOX_EXPERIMENTAL_GUEST_NATIVE_ROOTFS")
            .is_ok_and(|value| matches!(value.as_str(), "1" | "true" | "yes"))
        {
            tracing::warn!(
                "Using experimental guest-native ext4 rootfs provider; snapshot-backed boxes remain disabled"
            );
            return Box::new(GuestNativeExt4Provider);
        }
        tracing::info!("Using case-sensitive APFS rootfs provider");
        Box::new(CaseSensitiveApfsProvider)
    }

    #[cfg(not(target_os = "macos"))]
    {
        if super::overlay::is_overlay_supported() {
            tracing::info!("Using overlayfs rootfs provider");
            return Box::new(OverlayProvider);
        }

        tracing::info!("Overlayfs not available, using copy provider");
        Box::new(CopyProvider)
    }
}

/// Select the provider for an existing box generation.
///
/// A raw disk is durable box state, so its provider identity must not depend on
/// whether the experimental opt-in environment variable is still present on a
/// later `start` invocation.
pub fn default_provider_for_box(box_dir: &Path) -> Box<dyn RootfsProvider> {
    #[cfg(target_os = "macos")]
    {
        for (path, state) in [
            (
                GuestNativeExt4Provider::artifact_directory(box_dir),
                "retained raw rootfs",
            ),
            (
                GuestNativeExt4Provider::migration_path(box_dir),
                "rootfs migration transaction",
            ),
        ] {
            match std::fs::symlink_metadata(&path) {
                Ok(_) => {
                    tracing::info!(
                        path = %path.display(),
                        %state,
                        "Selecting guest-native provider for durable rootfs state"
                    );
                    return Box::new(GuestNativeExt4Provider);
                }
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    tracing::warn!(
                        path = %path.display(),
                        %error,
                        %state,
                        "Cannot inspect durable rootfs state; selecting its provider to fail closed"
                    );
                    return Box::new(GuestNativeExt4Provider);
                }
            }
        }
    }

    let _ = box_dir;
    default_provider()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn make_sample_rootfs(dir: &Path) {
        std::fs::create_dir_all(dir.join("etc")).unwrap();
        std::fs::create_dir_all(dir.join("bin")).unwrap();
        std::fs::write(dir.join("etc/hostname"), "testbox").unwrap();
        std::fs::write(dir.join("bin/hello"), "#!/bin/sh\necho hi").unwrap();
    }

    #[test]
    fn test_copy_provider_prepare() {
        let tmp = TempDir::new().unwrap();
        let cache_dir = tmp.path().join("cache");
        let box_dir = tmp.path().join("box");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&box_dir).unwrap();
        make_sample_rootfs(&cache_dir);

        let provider = CopyProvider;
        let rootfs = provider.prepare(&box_dir, &cache_dir).unwrap();

        assert_eq!(rootfs, box_dir.join("rootfs"));
        assert!(rootfs.join("etc/hostname").exists());
        assert_eq!(
            std::fs::read_to_string(rootfs.join("etc/hostname")).unwrap(),
            "testbox"
        );
        assert!(rootfs.join("bin/hello").exists());
    }

    #[test]
    fn copy_provider_finalizes_to_directory_transport() {
        let tmp = TempDir::new().unwrap();
        let box_dir = tmp.path().join("box");
        let rootfs = box_dir.join("rootfs");

        let source = CopyProvider
            .finalize_for_boot(
                &box_dir,
                &rootfs,
                RootfsFinalizeOptions {
                    disk_mib: 4096,
                    persistent: false,
                    snapshot: false,
                    artifact_cache: None,
                },
            )
            .unwrap();

        assert_eq!(source, RootfsSource::directory(rootfs));
    }

    #[test]
    fn test_copy_provider_prepare_reuses_existing_rootfs_without_overwriting() {
        let tmp = TempDir::new().unwrap();
        let cache_dir = tmp.path().join("cache");
        let box_dir = tmp.path().join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(rootfs.join("etc")).unwrap();
        make_sample_rootfs(&cache_dir);
        std::fs::write(rootfs.join("etc/hostname"), "persistent-host").unwrap();

        let provider = CopyProvider;
        let prepared = provider.prepare(&box_dir, &cache_dir).unwrap();

        assert_eq!(prepared, rootfs);
        assert_eq!(
            std::fs::read_to_string(prepared.join("etc/hostname")).unwrap(),
            "persistent-host"
        );
        assert!(
            !prepared.join("bin/hello").exists(),
            "existing persistent rootfs must not be overwritten from cache"
        );
    }

    #[test]
    fn test_copy_provider_cleanup() {
        let tmp = TempDir::new().unwrap();
        let cache_dir = tmp.path().join("cache");
        let box_dir = tmp.path().join("box");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&box_dir).unwrap();
        make_sample_rootfs(&cache_dir);

        let provider = CopyProvider;
        let rootfs = provider.prepare(&box_dir, &cache_dir).unwrap();
        assert!(rootfs.exists());

        provider.cleanup(&box_dir, false).unwrap();
        assert!(!rootfs.exists());
    }

    #[test]
    fn test_copy_provider_cleanup_persistent_keeps_rootfs() {
        let tmp = TempDir::new().unwrap();
        let box_dir = tmp.path().join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(rootfs.join("etc")).unwrap();
        std::fs::write(rootfs.join("etc/hostname"), "kept").unwrap();

        CopyProvider.cleanup(&box_dir, true).unwrap();

        assert_eq!(
            std::fs::read_to_string(rootfs.join("etc/hostname")).unwrap(),
            "kept"
        );
    }

    #[test]
    fn test_copy_provider_cleanup_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let provider = CopyProvider;
        // Should not error on missing dir
        provider.cleanup(tmp.path(), false).unwrap();
    }

    #[test]
    fn test_copy_provider_name() {
        assert_eq!(CopyProvider.name(), "copy");
    }

    #[test]
    fn test_overlay_provider_name() {
        assert_eq!(OverlayProvider.name(), "overlay");
    }

    #[test]
    fn test_overlay_provider_uses_populated_rootfs_as_persistent_lower() {
        let tmp = TempDir::new().unwrap();
        let cache_dir = tmp.path().join("cache");
        let box_dir = tmp.path().join("box");
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::write(rootfs.join("restart-proof"), "generation-one").unwrap();

        assert_eq!(
            OverlayProvider::lower_dir(&box_dir, &cache_dir).unwrap(),
            rootfs
        );
    }

    #[test]
    fn test_overlay_provider_ignores_empty_rootfs_as_lower() {
        let tmp = TempDir::new().unwrap();
        let cache_dir = tmp.path().join("cache");
        let box_dir = tmp.path().join("box");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(box_dir.join("rootfs")).unwrap();

        assert_eq!(
            OverlayProvider::lower_dir(&box_dir, &cache_dir).unwrap(),
            cache_dir
        );
    }

    #[test]
    fn test_overlay_provider_cleanup_persistent_keeps_rootfs_and_upper() {
        let tmp = TempDir::new().unwrap();
        let box_dir = tmp.path().join("box");
        for dir in ["rootfs", "upper", "work", "merged"] {
            std::fs::create_dir_all(box_dir.join(dir)).unwrap();
        }
        std::fs::write(box_dir.join("rootfs/restart-proof"), "generation-one").unwrap();
        std::fs::write(box_dir.join("upper/data.txt"), "state").unwrap();
        std::fs::write(box_dir.join("work/scratch.txt"), "work").unwrap();
        std::fs::write(box_dir.join("merged/view.txt"), "merged").unwrap();

        OverlayProvider.cleanup(&box_dir, true).unwrap();

        assert_eq!(
            std::fs::read_to_string(box_dir.join("upper/data.txt")).unwrap(),
            "state"
        );
        assert_eq!(
            std::fs::read_to_string(box_dir.join("rootfs/restart-proof")).unwrap(),
            "generation-one"
        );
        assert!(!box_dir.join("work").exists());
        assert!(!box_dir.join("merged").exists());
    }

    #[test]
    fn test_overlay_provider_cleanup_nonpersistent_removes_all_overlay_dirs() {
        let tmp = TempDir::new().unwrap();
        let box_dir = tmp.path().join("box");
        for dir in ["rootfs", "upper", "work", "merged"] {
            std::fs::create_dir_all(box_dir.join(dir)).unwrap();
            std::fs::write(box_dir.join(dir).join("file.txt"), "data").unwrap();
        }

        OverlayProvider.cleanup(&box_dir, false).unwrap();

        assert!(!box_dir.join("rootfs").exists());
        assert!(!box_dir.join("upper").exists());
        assert!(!box_dir.join("work").exists());
        assert!(!box_dir.join("merged").exists());
    }

    #[test]
    fn test_default_provider_returns_something() {
        let provider = default_provider();
        // On any platform, we should get a provider
        assert!(!provider.name().is_empty());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn case_sensitive_apfs_provider_preserves_distinct_names() {
        use std::os::unix::fs::MetadataExt;

        let tmp = TempDir::new().unwrap();
        let box_dir = tmp.path().join("box");
        let provider = CaseSensitiveApfsProvider;
        let rootfs = provider.prepare_empty(&box_dir).unwrap();
        std::fs::write(rootfs.join("Foo"), "upper").unwrap();
        std::fs::write(rootfs.join("foo"), "lower").unwrap();

        assert_eq!(
            std::fs::read_to_string(rootfs.join("Foo")).unwrap(),
            "upper"
        );
        assert_eq!(
            std::fs::read_to_string(rootfs.join("foo")).unwrap(),
            "lower"
        );
        assert_ne!(
            std::fs::metadata(rootfs.join("Foo")).unwrap().ino(),
            std::fs::metadata(rootfs.join("foo")).unwrap().ino()
        );

        provider.cleanup(&box_dir, false).unwrap();
        assert!(!box_dir.join(CaseSensitiveApfsProvider::IMAGE_NAME).exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn guest_native_ext4_handoff_detaches_apfs_before_boot() {
        let tmp = TempDir::new().unwrap();
        let box_dir = tmp.path().join("box");
        let provider = GuestNativeExt4Provider;
        let staged = provider.prepare_empty(&box_dir).unwrap();
        std::fs::create_dir_all(staged.join("etc")).unwrap();
        std::fs::write(staged.join("etc/hostname"), "guest-native").unwrap();
        let mountpoint = staged.parent().unwrap().to_path_buf();
        assert!(super::super::is_mountpoint(&mountpoint));

        let source = provider
            .finalize_for_boot(
                &box_dir,
                &staged,
                RootfsFinalizeOptions {
                    disk_mib: 32,
                    persistent: false,
                    snapshot: false,
                    artifact_cache: None,
                },
            )
            .unwrap();
        let RootfsSource::Ext4Disk { path, read_only } = source else {
            panic!("guest-native provider returned a directory rootfs")
        };
        assert!(path.is_file());
        assert!(!read_only);
        assert!(
            !super::super::is_mountpoint(&mountpoint),
            "APFS staging mount must be gone before VMM handoff"
        );

        provider.cleanup(&box_dir, false).unwrap();
        assert!(!box_dir.join(CaseSensitiveApfsProvider::IMAGE_NAME).exists());
        assert!(!GuestNativeExt4Provider::artifact_directory(&box_dir).exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn guest_native_ext4_cache_never_shares_its_writable_disk() {
        use sha2::{Digest, Sha256};
        use std::io::{Read, Seek, SeekFrom, Write};

        fn digest(path: &Path) -> Vec<u8> {
            let mut file = std::fs::File::open(path).unwrap();
            let mut hasher = Sha256::new();
            let mut buffer = vec![0u8; 1024 * 1024];
            loop {
                let read = file.read(&mut buffer).unwrap();
                if read == 0 {
                    return hasher.finalize().to_vec();
                }
                hasher.update(&buffer[..read]);
            }
        }

        let tmp = TempDir::new().unwrap();
        let box_dir = tmp.path().join("box");
        let cache_dir = tmp.path().join("cache");
        let provider = GuestNativeExt4Provider;
        let staged = provider.prepare_empty(&box_dir).unwrap();
        std::fs::create_dir_all(staged.join("etc")).unwrap();
        std::fs::write(staged.join("etc/hostname"), "cached-base").unwrap();

        let source = provider
            .finalize_for_boot(
                &box_dir,
                &staged,
                RootfsFinalizeOptions {
                    disk_mib: 16,
                    persistent: false,
                    snapshot: false,
                    artifact_cache: Some(RootfsArtifactCacheOptions {
                        directory: cache_dir.clone(),
                        oci_manifest_digest: format!("sha256:{}", "11".repeat(32)),
                        platform: "linux/arm64".to_string(),
                        guest_init_sha256: format!("sha256:{}", "22".repeat(32)),
                        max_entries: 2,
                        max_allocated_bytes: u64::MAX,
                    }),
                },
            )
            .unwrap();
        let RootfsSource::Ext4Disk { path, .. } = source else {
            panic!("guest-native provider returned a directory rootfs")
        };
        assert!(path.starts_with(&box_dir));

        let cache_entry = std::fs::read_dir(&cache_dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .find(|entry| entry.file_type().is_ok_and(|kind| kind.is_dir()))
            .unwrap();
        let cached_disk = cache_entry.path().join("artifact/rootfs.ext4");
        let cached_before = digest(&cached_disk);
        let mut private = std::fs::OpenOptions::new().write(true).open(&path).unwrap();
        private.seek(SeekFrom::Start(4 * 1024 * 1024)).unwrap();
        private.write_all(b"private-generation").unwrap();
        private.sync_all().unwrap();
        assert_eq!(digest(&cached_disk), cached_before);

        provider.cleanup(&box_dir, false).unwrap();
        assert!(!path.exists());
        assert!(cached_disk.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn guest_native_ext4_persistent_generation_resumes_without_apfs() {
        let tmp = TempDir::new().unwrap();
        let box_dir = tmp.path().join("box");
        let provider = GuestNativeExt4Provider;
        let staged = provider.prepare_empty(&box_dir).unwrap();
        std::fs::create_dir_all(staged.join("sbin")).unwrap();
        std::fs::write(staged.join("sbin/init"), b"guest-init").unwrap();

        let first = provider
            .finalize_for_boot(
                &box_dir,
                &staged,
                RootfsFinalizeOptions {
                    disk_mib: 16,
                    persistent: true,
                    snapshot: false,
                    artifact_cache: None,
                },
            )
            .unwrap();
        assert!(!box_dir.join("rootfs-apfs-v2.sparseimage").exists());
        provider.cleanup(&box_dir, true).unwrap();

        let resumed = provider
            .resume_for_boot(
                &box_dir,
                RootfsResumeOptions {
                    disk_mib: 16,
                    persistent: true,
                    snapshot: false,
                },
            )
            .unwrap()
            .unwrap();
        assert_eq!(resumed.source, first);
        assert_eq!(resumed.guest_init_exec, "/sbin/init");

        provider.cleanup(&box_dir, false).unwrap();
        let RootfsSource::Ext4Disk { path, .. } = first else {
            panic!("guest-native provider returned a directory rootfs")
        };
        assert!(!path.exists());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn guest_native_ext4_still_rejects_snapshot_generations() {
        let tmp = TempDir::new().unwrap();
        let error = GuestNativeExt4Provider
            .finalize_for_boot(
                tmp.path(),
                tmp.path(),
                RootfsFinalizeOptions {
                    disk_mib: 32,
                    persistent: false,
                    snapshot: true,
                    artifact_cache: None,
                },
            )
            .unwrap_err()
            .to_string();

        assert!(error.contains("snapshot"), "{error}");
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn retained_raw_generation_selects_guest_native_provider() {
        let tmp = TempDir::new().unwrap();
        std::fs::create_dir_all(tmp.path().join("rootfs-ext4-v1")).unwrap();

        assert_eq!(
            default_provider_for_box(tmp.path()).name(),
            "guest-native-ext4"
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn retained_migration_transaction_selects_guest_native_provider() {
        let tmp = TempDir::new().unwrap();
        std::fs::write(tmp.path().join("rootfs-migration-v1.json"), b"incomplete").unwrap();

        assert_eq!(
            default_provider_for_box(tmp.path()).name(),
            "guest-native-ext4"
        );
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_overlay_provider_prepare_and_cleanup() {
        if !super::super::overlay::is_overlay_supported() {
            // Skip if overlay not available (e.g., in container without privileges)
            return;
        }

        let tmp = TempDir::new().unwrap();
        let cache_dir = tmp.path().join("cache");
        let box_dir = tmp.path().join("box");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&box_dir).unwrap();
        make_sample_rootfs(&cache_dir);

        let provider = OverlayProvider;
        let merged = provider.prepare(&box_dir, &cache_dir).unwrap();

        assert_eq!(merged, box_dir.join("merged"));
        assert!(merged.join("etc/hostname").exists());
        assert_eq!(
            std::fs::read_to_string(merged.join("etc/hostname")).unwrap(),
            "testbox"
        );

        // Write to merged — should go to upper
        std::fs::write(merged.join("etc/newfile"), "overlay write").unwrap();
        assert!(box_dir.join("upper/etc/newfile").exists());

        provider.cleanup(&box_dir, false).unwrap();
        assert!(!box_dir.join("merged").exists());
        assert!(!box_dir.join("upper").exists());
        assert!(!box_dir.join("work").exists());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn test_overlay_provider_persistent_cleanup_remounts_retained_upper() {
        if !super::super::overlay::is_overlay_supported() {
            return;
        }

        let tmp = TempDir::new().unwrap();
        let cache_dir = tmp.path().join("cache");
        let box_dir = tmp.path().join("box");
        std::fs::create_dir_all(&cache_dir).unwrap();
        std::fs::create_dir_all(&box_dir).unwrap();
        make_sample_rootfs(&cache_dir);

        let provider = OverlayProvider;
        let first = provider.prepare(&box_dir, &cache_dir).unwrap();
        std::fs::write(first.join("restart-proof"), "generation-one").unwrap();

        provider.cleanup(&box_dir, true).unwrap();
        assert_eq!(
            std::fs::read_to_string(box_dir.join("upper/restart-proof")).unwrap(),
            "generation-one"
        );

        let second = provider.prepare(&box_dir, &cache_dir).unwrap();
        assert_eq!(
            std::fs::read_to_string(second.join("restart-proof")).unwrap(),
            "generation-one"
        );

        provider.cleanup(&box_dir, false).unwrap();
    }
}
