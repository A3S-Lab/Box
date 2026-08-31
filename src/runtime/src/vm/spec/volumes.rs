//! User volume parsing, staging, and anonymous volume ownership.

use super::*;
use sha2::{Digest, Sha256};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;

impl VmManager {
    /// Parse a volume mount string from the right so colons in a host path do
    /// not consume the host/guest separator. The guest always uses an absolute
    /// Linux path, even when the host path is a Windows drive or UNC path.
    pub(crate) fn parse_volume_spec(volume: &str) -> Result<ParsedVolumeMount> {
        let (mount, read_only) = match volume.rsplit_once(':') {
            Some((mount, "ro")) => (mount, true),
            Some((mount, "rw")) => (mount, false),
            Some((mount, mode)) if mount.contains(':') && !mode.starts_with('/') => {
                return Err(BoxError::ConfigError(format!(
                    "Invalid volume mode '{}' (expected 'ro' or 'rw'): {}",
                    mode, volume
                )));
            }
            _ => (volume, false),
        };

        let (host_path, guest_path) = mount.rsplit_once(':').ok_or_else(|| {
            BoxError::ConfigError(format!(
                "Invalid volume format (expected host:guest[:ro|rw]): {}",
                volume
            ))
        })?;
        if host_path.is_empty() || !guest_path.starts_with('/') {
            return Err(BoxError::ConfigError(format!(
                "Invalid volume format (expected host:guest[:ro|rw]): {}",
                volume
            )));
        }
        Self::validate_guest_mount_path(guest_path)?;

        Ok(ParsedVolumeMount {
            host_path: PathBuf::from(host_path),
            guest_path: guest_path.to_string(),
            read_only,
            copy_up: false,
        })
    }

    pub(super) fn validate_guest_mount_path(guest_path: &str) -> Result<()> {
        if !guest_path.starts_with('/')
            || guest_path.contains('\0')
            || guest_path.split('/').any(|component| component == "..")
        {
            return Err(BoxError::ConfigError(format!(
                "Invalid guest volume path: {guest_path}"
            )));
        }

        let normalized = format!(
            "/{}",
            guest_path
                .split('/')
                .filter(|component| !component.is_empty() && *component != ".")
                .collect::<Vec<_>>()
                .join("/")
        );
        if normalized == "/"
            || normalized == "/run"
            || normalized == "/run/a3s-box"
            || normalized.starts_with("/run/a3s-box/")
        {
            return Err(BoxError::ConfigError(format!(
                "Guest volume path {guest_path:?} overlaps reserved runtime state /run/a3s-box"
            )));
        }
        Ok(())
    }

    pub(super) fn prepare_volume_mount(
        volume: &ParsedVolumeMount,
        index: usize,
        filemounts_dir: &Path,
        managed_secret_root: Option<&Path>,
        box_id: &str,
    ) -> Result<FsMount> {
        let host_path = volume.host_path.clone();
        let managed_secret_root = managed_secret_root.filter(|root| host_path.starts_with(root));
        if !host_path.exists() {
            if managed_secret_root.is_some() {
                return Err(BoxError::ConfigError(format!(
                    "Managed transient Secret source is missing: {}",
                    host_path.display()
                )));
            }
            std::fs::create_dir_all(&host_path).map_err(|e| BoxError::BoxBootError {
                message: format!(
                    "Failed to create volume host directory {}: {}",
                    host_path.display(),
                    e
                ),
                hint: None,
            })?;
        }
        if managed_secret_root.is_some() {
            let metadata = std::fs::symlink_metadata(&host_path).map_err(BoxError::IoError)?;
            if !volume.read_only
                || !metadata.file_type().is_file()
                || metadata.file_type().is_symlink()
            {
                return Err(BoxError::ConfigError(
                    "Managed transient Secret mounts must be read-only regular files".into(),
                ));
            }
        }
        let host_path = host_path
            .canonicalize()
            .map_err(|e| BoxError::BoxBootError {
                message: format!(
                    "Failed to resolve volume path {}: {}",
                    host_path.display(),
                    e
                ),
                hint: None,
            })?;

        let host_path = if host_path.is_file() {
            if let Some(root) = managed_secret_root.filter(|root| host_path.starts_with(root)) {
                Self::stage_managed_secret_file_mount(
                    &host_path,
                    &volume.guest_path,
                    index,
                    root,
                    box_id,
                )?
            } else {
                Self::stage_single_file_mount(
                    &host_path,
                    &volume.guest_path,
                    index,
                    filemounts_dir,
                )?
            }
        } else {
            host_path
        };
        let tag = format!("vol{}", index);

        tracing::info!(
            tag = %tag,
            host = %host_path.display(),
            guest = %volume.guest_path,
            read_only = volume.read_only,
            "Adding user volume mount"
        );

        Ok(FsMount {
            tag,
            host_path,
            read_only: volume.read_only,
        })
    }

    #[cfg(test)]
    pub(super) fn parse_volume_mount(
        volume: &str,
        index: usize,
        filemounts_dir: &Path,
    ) -> Result<FsMount> {
        let parsed_volume = Self::parse_volume_spec(volume)?;
        Self::prepare_volume_mount(&parsed_volume, index, filemounts_dir, None, "test-box")
    }

    /// Stage a managed Secret file only inside its validated tmpfs identity.
    /// A cross-filesystem fallback into the ordinary per-box directory would
    /// turn transient Secret bytes into durable host data and is forbidden.
    fn stage_managed_secret_file_mount(
        source: &Path,
        guest_path: &str,
        index: usize,
        root: &Path,
        box_id: &str,
    ) -> Result<PathBuf> {
        let relative = source.strip_prefix(root).map_err(|_| {
            BoxError::ConfigError("Managed Secret source escaped its configured root".into())
        })?;
        let components = relative.components().collect::<Vec<_>>();
        let identity = match components.as_slice() {
            [std::path::Component::Normal(identity), std::path::Component::Normal(_)]
                if identity.len() == 64
                    && identity
                        .as_encoded_bytes()
                        .iter()
                        .all(|byte| byte.is_ascii_hexdigit()) =>
            {
                identity
            }
            _ => {
                return Err(BoxError::ConfigError(
                    "Managed Secret source has an invalid identity path".into(),
                ))
            }
        };
        let basename = Path::new(guest_path).file_name().ok_or_else(|| {
            BoxError::ConfigError(format!(
                "Single-file bind guest path has no file name: {guest_path}"
            ))
        })?;
        let owner = hex::encode(Sha256::digest(box_id.as_bytes()));
        let stage_dir = root
            .join(identity)
            .join(".mounts")
            .join(owner)
            .join(index.to_string());
        std::fs::create_dir_all(&stage_dir).map_err(BoxError::IoError)?;
        #[cfg(unix)]
        std::fs::set_permissions(&stage_dir, std::fs::Permissions::from_mode(0o700))
            .map_err(BoxError::IoError)?;

        let staged = stage_dir.join(basename);
        let temporary = stage_dir.join(format!(".{}.tmp", uuid::Uuid::new_v4().simple()));
        let result = (|| -> std::io::Result<()> {
            std::fs::copy(source, &temporary)?;
            #[cfg(unix)]
            std::fs::set_permissions(&temporary, std::fs::Permissions::from_mode(0o400))?;
            std::fs::File::open(&temporary)?.sync_all()?;
            std::fs::rename(&temporary, &staged)?;
            std::fs::File::open(&stage_dir)?.sync_all()
        })();
        if result.is_err() {
            let _ = std::fs::remove_file(&temporary);
        }
        result.map_err(BoxError::IoError)?;
        Ok(stage_dir)
    }

    /// Stage a single-file bind source into a per-box directory so virtio-fs (which
    /// shares directories, not bare files) can expose it. Returns the directory to
    /// share; it contains exactly one entry — the file under the guest path's
    /// basename, which `mount_user_volumes` then binds onto the guest path. The
    /// file is hard-linked to keep the bind live in both directions; across
    /// filesystems it falls back to a copy (host-side writes then do not propagate).
    fn stage_single_file_mount(
        source: &Path,
        guest_path: &str,
        index: usize,
        filemounts_dir: &Path,
    ) -> Result<PathBuf> {
        let basename = Path::new(guest_path).file_name().ok_or_else(|| {
            BoxError::ConfigError(format!(
                "Single-file bind guest path has no file name: {guest_path}"
            ))
        })?;
        let stage_dir = filemounts_dir.join(index.to_string());
        std::fs::create_dir_all(&stage_dir).map_err(|e| BoxError::BoxBootError {
            message: format!(
                "Failed to create file-mount staging dir {}: {}",
                stage_dir.display(),
                e
            ),
            hint: None,
        })?;
        let staged = stage_dir.join(basename);
        let _ = std::fs::remove_file(&staged); // idempotent across restarts
        if std::fs::hard_link(source, &staged).is_err() {
            std::fs::copy(source, &staged).map_err(|e| BoxError::BoxBootError {
                message: format!(
                    "Failed to stage single-file mount {} -> {}: {}",
                    source.display(),
                    staged.display(),
                    e
                ),
                hint: None,
            })?;
            tracing::warn!(
                source = %source.display(),
                "Single-file bind staged by copy (source on a different filesystem); \
                 host-side writes will not propagate to the container"
            );
        }
        Ok(stage_dir)
    }

    /// Create an anonymous volume via VolumeStore.
    ///
    /// Returns the host path of the created volume.
    pub(super) fn create_anonymous_volume(&self, name: &str) -> Result<(String, bool)> {
        use crate::volume::VolumeStore;

        let store = VolumeStore::new(
            self.home_dir.join("volumes.json"),
            self.home_dir.join("volumes"),
        );

        // If the volume already exists (e.g., from a previous run), reuse it
        if let Some(existing) = store.get(name)? {
            return Ok((existing.mount_point, false));
        }

        let mut config = a3s_box_core::volume::VolumeConfig::new(name, "");
        config
            .labels
            .insert("anonymous".to_string(), "true".to_string());
        config.attach(&self.box_id);
        let created = store.create(config)?;
        Ok((created.mount_point, true))
    }
}
