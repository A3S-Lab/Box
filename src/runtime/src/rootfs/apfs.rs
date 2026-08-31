//! Case-sensitive APFS compatibility staging for macOS.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};

use super::provider::RootfsProvider;

/// A copy provider backed by a case-sensitive APFS sparse image.
///
/// macOS commonly stores `~/.a3s` on case-insensitive APFS. Passing a normal
/// host directory to libkrun as the guest root would then make Linux paths such
/// as `/bin` and `/BIN` aliases. Each box therefore owns a sparse, dynamically
/// allocated case-sensitive APFS image and exposes its mountpoint via virtiofs.
pub(crate) struct CaseSensitiveApfsProvider;

impl CaseSensitiveApfsProvider {
    // v2 stores the Linux tree below a private directory inside the volume.
    // APFS creates volume-management entries such as `.fseventsd` at the
    // volume root; exposing that root to the guest both leaks host artifacts
    // and can make recursive rootfs walks fail with EACCES.
    const IMAGE_STEM: &'static str = "rootfs-apfs-v2";
    pub(super) const IMAGE_NAME: &'static str = "rootfs-apfs-v2.sparseimage";
    const DATA_DIR: &'static str = ".a3s-rootfs";

    pub(super) fn image_path(box_dir: &Path) -> PathBuf {
        box_dir.join(Self::IMAGE_NAME)
    }

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

        let image = Self::image_path(box_dir);
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

impl RootfsProvider for CaseSensitiveApfsProvider {
    fn prepare(&self, box_dir: &Path, cache_dir: &Path) -> Result<PathBuf> {
        let image = Self::image_path(box_dir);
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
            let image = Self::image_path(box_dir);
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
