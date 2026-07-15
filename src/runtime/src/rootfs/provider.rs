//! Rootfs provider — abstracts how a rootfs directory is prepared for a box.
//!
//! Two built-in providers:
//! - `CopyProvider` — full recursive copy (works everywhere, current default)
//! - `OverlayProvider` — Linux overlayfs mount (near-instant, CoW)

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};

/// Abstracts how a rootfs directory is prepared for a box from a cached lower layer.
pub trait RootfsProvider: Send + Sync {
    /// Prepare a rootfs at `box_dir` from the cached read-only layer at `cache_dir`.
    /// Returns the path to use as `InstanceSpec.rootfs_path`.
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

    /// Cleanup after box stops.
    ///
    /// When `persistent` is true, the writable layer (overlay upper dir or copy
    /// rootfs) is preserved on disk so changes survive the next start.
    /// When false, the writable layer is wiped for a clean slate.
    fn cleanup(&self, box_dir: &Path, persistent: bool) -> Result<()>;

    /// Human-readable name for logging.
    fn name(&self) -> &'static str;
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

/// A copy provider backed by a case-sensitive APFS sparse image.
///
/// macOS commonly stores `~/.a3s` on case-insensitive APFS. Passing a normal
/// host directory to libkrun as the guest root would then make Linux paths such
/// as `/bin` and `/BIN` aliases. Each box therefore owns a sparse, dynamically
/// allocated case-sensitive APFS image and exposes its mountpoint via virtiofs.
#[cfg(target_os = "macos")]
pub struct CaseSensitiveApfsProvider;

#[cfg(target_os = "macos")]
impl CaseSensitiveApfsProvider {
    // v2 stores the Linux tree below a private directory inside the volume.
    // APFS creates volume-management entries such as `.fseventsd` at the
    // volume root; exposing that root to the guest both leaks host artifacts
    // and can make recursive rootfs walks fail with EACCES.
    const IMAGE_STEM: &'static str = "rootfs-apfs-v2";
    const IMAGE_NAME: &'static str = "rootfs-apfs-v2.sparseimage";
    const DATA_DIR: &'static str = ".a3s-rootfs";

    fn clone_image(source: &Path, destination: &Path) -> Result<()> {
        let output = std::process::Command::new("cp")
            .arg("-c")
            .arg(source)
            .arg(destination)
            .output()
            .map_err(|error| {
                BoxError::BuildError(format!("Failed to start APFS clone: {error}"))
            })?;
        if !output.status.success() {
            return Err(BoxError::BuildError(format!(
                "Failed to clone cached APFS rootfs {}: {}",
                source.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        Ok(())
    }

    fn mount(&self, box_dir: &Path) -> Result<PathBuf> {
        use std::process::Command;

        std::fs::create_dir_all(box_dir).map_err(|error| {
            BoxError::BuildError(format!(
                "Failed to create box directory {}: {error}",
                box_dir.display()
            ))
        })?;
        let rootfs = box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).map_err(|error| {
            BoxError::BuildError(format!(
                "Failed to create APFS mountpoint {}: {error}",
                rootfs.display()
            ))
        })?;
        if super::is_mountpoint(&rootfs) {
            return Self::data_dir(&rootfs);
        }

        let image = box_dir.join(Self::IMAGE_NAME);
        if !image.exists() {
            let stem = box_dir.join(Self::IMAGE_STEM);
            let output = Command::new("hdiutil")
                .args([
                    "create",
                    "-quiet",
                    "-size",
                    "64g",
                    "-type",
                    "SPARSE",
                    "-fs",
                    "Case-sensitive APFS",
                    "-volname",
                    "A3SRootfs",
                ])
                .arg(&stem)
                .output()
                .map_err(|error| {
                    BoxError::BuildError(format!("Failed to start hdiutil create: {error}"))
                })?;
            if !output.status.success() {
                return Err(BoxError::BuildError(format!(
                    "Failed to create case-sensitive APFS rootfs image: {}",
                    String::from_utf8_lossy(&output.stderr).trim()
                )));
            }
        }

        let output = Command::new("hdiutil")
            .args([
                "attach",
                "-quiet",
                "-nobrowse",
                "-owners",
                "on",
                "-mountpoint",
            ])
            .arg(&rootfs)
            .arg(&image)
            .output()
            .map_err(|error| {
                BoxError::BuildError(format!("Failed to start hdiutil attach: {error}"))
            })?;
        if !output.status.success() {
            return Err(BoxError::BuildError(format!(
                "Failed to mount case-sensitive APFS rootfs image {}: {}",
                image.display(),
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }
        if !super::is_mountpoint(&rootfs) {
            return Err(BoxError::BuildError(format!(
                "hdiutil did not mount the rootfs image at {}",
                rootfs.display()
            )));
        }
        Self::data_dir(&rootfs)
    }

    fn data_dir(mountpoint: &Path) -> Result<PathBuf> {
        let data = mountpoint.join(Self::DATA_DIR);
        std::fs::create_dir_all(&data).map_err(|error| {
            BoxError::BuildError(format!(
                "Failed to create APFS rootfs data directory {}: {error}",
                data.display()
            ))
        })?;
        Ok(data)
    }
}

#[cfg(target_os = "macos")]
impl RootfsProvider for CaseSensitiveApfsProvider {
    fn prepare(&self, box_dir: &Path, cache_dir: &Path) -> Result<PathBuf> {
        let image = box_dir.join(Self::IMAGE_NAME);
        if cache_dir.is_file() && !image.exists() {
            std::fs::create_dir_all(box_dir).map_err(BoxError::IoError)?;
            Self::clone_image(cache_dir, &image)?;
        }
        let rootfs = self.mount(box_dir)?;
        if cache_dir.is_file() {
            return Ok(rootfs);
        }
        if std::fs::read_dir(&rootfs)
            .map_err(|error| BoxError::BuildError(error.to_string()))?
            .next()
            .is_none()
        {
            crate::cache::layer_cache::copy_dir_recursive(cache_dir, &rootfs)?;
        } else {
            tracing::info!(path = %rootfs.display(), "Reusing persistent APFS rootfs");
        }
        Ok(rootfs)
    }

    fn prepare_empty(&self, box_dir: &Path) -> Result<PathBuf> {
        self.mount(box_dir)
    }

    fn cleanup(&self, box_dir: &Path, persistent: bool) -> Result<()> {
        super::unmount_box_rootfs(&box_dir.join("rootfs"));
        if !persistent {
            let image = box_dir.join(Self::IMAGE_NAME);
            if image.exists() {
                std::fs::remove_file(&image).map_err(|error| {
                    BoxError::BuildError(format!(
                        "Failed to remove rootfs image {}: {error}",
                        image.display()
                    ))
                })?;
            }
        }
        Ok(())
    }

    fn name(&self) -> &'static str {
        "case-sensitive-apfs"
    }
}

/// Overlayfs provider — near-instant CoW mounts (Linux only).
///
/// Layout:
/// ```text
/// cache_dir/           ← lower (read-only, shared across boxes)
/// box_dir/upper/       ← upper (per-box writes)
/// box_dir/work/        ← overlayfs workdir
/// box_dir/merged/      ← merged view → InstanceSpec.rootfs_path
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
        // Bounded unmount-retry rather than a single attempt: a transient EBUSY
        // must not leave the overlay mounted, or the remove_dir_all below would
        // recurse into the live mount and leak it. Mirrors the cleanup paths in
        // cleanup_stopped_box/cleanup_removed_box.
        super::unmount_box_overlay(&merged);

        if persistent {
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
}
