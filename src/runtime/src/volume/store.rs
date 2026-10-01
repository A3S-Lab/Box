//! Persistent storage for volume configurations.
//!
//! Volumes are stored as JSON in `~/.a3s/volumes.json` with atomic writes
//! (write to tmp file, then rename) to prevent corruption.
//! Volume data is stored under `~/.a3s/volumes/<name>/`.

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::volume::VolumeConfig;
use std::collections::HashMap;
use std::path::{Path, PathBuf};

/// Persistent store for volume configurations.
#[derive(Debug)]
pub struct VolumeStore {
    /// Path to the JSON file.
    path: PathBuf,
    /// Base directory for volume data (~/.a3s/volumes/).
    volumes_dir: PathBuf,
}

/// Serializable wrapper for the volumes file.
#[derive(Debug, serde::Serialize, serde::Deserialize, Default)]
struct VolumesFile {
    volumes: HashMap<String, VolumeConfig>,
}

const ANONYMOUS_LABEL: &str = "anonymous";
const ANONYMOUS_KIND_LABEL: &str = "a3s.box.volume.kind";
const ANONYMOUS_KIND: &str = "anonymous-v1";
const ANONYMOUS_OWNER_LABEL: &str = "a3s.box.volume.owner";

impl VolumeStore {
    /// Create a new store at the given path.
    pub fn new(path: impl Into<PathBuf>, volumes_dir: impl Into<PathBuf>) -> Self {
        Self {
            path: path.into(),
            volumes_dir: volumes_dir.into(),
        }
    }

    /// Create a store at the default location (`~/.a3s/volumes.json`).
    pub fn default_path() -> Result<Self> {
        let home = a3s_box_core::dirs_home();
        Ok(Self::new(home.join("volumes.json"), home.join("volumes")))
    }

    /// Load all volumes from disk.
    pub fn load(&self) -> Result<HashMap<String, VolumeConfig>> {
        #[cfg(windows)]
        {
            let mut prefix = PathBuf::new();
            for component in self.path.components() {
                prefix.push(component);
                crate::vm::refuse_directory_reparse(&prefix)?;
            }
        }
        if !self.path.exists() {
            return Ok(HashMap::new());
        }

        let data = std::fs::read_to_string(&self.path).map_err(|e| {
            BoxError::ConfigError(format!(
                "failed to read volumes file {}: {}",
                self.path.display(),
                e
            ))
        })?;

        // A corrupt/old-schema volumes file must not brick the runtime: quarantine
        // it and start from an empty set (create repopulates) rather than failing
        // every volume operation. Mirrors the boxes.json hardening.
        let file: VolumesFile = match serde_json::from_str(&data) {
            Ok(f) => f,
            Err(e) => {
                let preserved = crate::store_io::quarantine_label(&self.path);
                tracing::warn!(
                    "volumes file {} is corrupt ({e}); preserved a copy at {preserved} \
                     and started from an empty volume set",
                    self.path.display(),
                );
                return Ok(HashMap::new());
            }
        };

        Ok(file.volumes)
    }

    /// Save all volumes to disk (atomic write).
    pub fn save(&self, volumes: &HashMap<String, VolumeConfig>) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            #[cfg(windows)]
            {
                let mut prefix = PathBuf::new();
                for component in parent.components() {
                    prefix.push(component);
                    crate::vm::refuse_directory_reparse(&prefix)?;
                }
            }
            std::fs::create_dir_all(parent).map_err(|e| {
                BoxError::ConfigError(format!(
                    "failed to create directory {}: {}",
                    parent.display(),
                    e
                ))
            })?;
        }

        let file = VolumesFile {
            volumes: volumes.clone(),
        };

        let json = serde_json::to_string_pretty(&file).map_err(|e| {
            BoxError::SerializationError(format!("failed to serialize volumes: {}", e))
        })?;

        let tmp_path = self.path.with_extension("json.tmp");
        std::fs::write(&tmp_path, &json).map_err(|e| {
            BoxError::ConfigError(format!(
                "failed to write tmp file {}: {}",
                tmp_path.display(),
                e
            ))
        })?;

        std::fs::rename(&tmp_path, &self.path).map_err(|e| {
            BoxError::ConfigError(format!(
                "failed to rename {} → {}: {}",
                tmp_path.display(),
                self.path.display(),
                e
            ))
        })?;

        Ok(())
    }

    /// Run `f` over the volume map under a cross-process advisory lock,
    /// re-loading fresh from disk inside the lock and saving the result.
    ///
    /// `create`/`remove`/`update`/`modify`/`get_or_create` all funnel through
    /// here so concurrent `a3s-box` processes cannot lose each other's writes.
    /// The atomic tmp+rename in `save` only prevents a *torn* read — two
    /// processes that both load, mutate a different entry, and save would still
    /// clobber one update (and, for attach/detach, silently drop a volume's
    /// `in_use_by` entry, letting `prune`/`remove` delete data a live box still
    /// has mounted). `save` itself stays lock-free: the guard is held here for
    /// the whole load → mutate → save, and the lock is non-reentrant.
    fn with_write_lock<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut HashMap<String, VolumeConfig>) -> Result<R>,
    {
        let _lock = crate::file_lock::FileLock::acquire(&self.path).map_err(|e| {
            BoxError::ConfigError(format!(
                "failed to lock volumes file {}: {e}",
                self.path.display()
            ))
        })?;
        let mut volumes = self.load()?;
        let r = f(&mut volumes)?;
        self.save(&volumes)?;
        Ok(r)
    }

    /// Get a single volume by name.
    pub fn get(&self, name: &str) -> Result<Option<VolumeConfig>> {
        let volumes = self.load()?;
        Ok(volumes.get(name).cloned())
    }

    /// Create a new named volume. Returns the host mount point path.
    ///
    /// Creates the volume data directory under `~/.a3s/volumes/<name>/`.
    /// Errors if the name already exists (use [`Self::get_or_create`] for the
    /// idempotent auto-create on the `run -v name:/path` path).
    pub fn create(&self, config: VolumeConfig) -> Result<VolumeConfig> {
        self.with_write_lock(|volumes| {
            if volumes.contains_key(&config.name) {
                return Err(BoxError::ConfigError(format!(
                    "volume '{}' already exists",
                    config.name
                )));
            }
            self.materialize(config, volumes)
        })
    }

    /// Return the existing volume, or create it if absent — atomic under the
    /// cross-process lock. Two concurrent first-time `run -v name:/path` then
    /// share one volume instead of one racing to an "already exists" error.
    pub fn get_or_create(&self, config: VolumeConfig) -> Result<VolumeConfig> {
        self.with_write_lock(|volumes| {
            if let Some(existing) = volumes.get(&config.name) {
                validate_managed_volume_name(&config.name)?;
                reject_same_directory_alias(volumes, &config.name, &self.volumes_dir)?;
                require_managed_mount_point(
                    &self.volumes_dir,
                    &config.name,
                    &existing.mount_point,
                )?;
                ensure_managed_volume_directory(&self.volumes_dir.join(&config.name))?;
                return Ok(existing.clone());
            }
            self.materialize(config, volumes)
        })
    }

    /// Atomically create or reclaim one Box-owned anonymous volume.
    ///
    /// Anonymous identities are single-owner capabilities. An existing named
    /// volume, a volume owned by another execution, or metadata pointing away
    /// from the canonical managed directory all fail closed without mutation.
    pub(crate) fn claim_anonymous(&self, name: &str, owner: &str) -> Result<(VolumeConfig, bool)> {
        validate_anonymous_identity(name, owner)?;

        let expected_mount_point = self.volumes_dir.join(name).to_string_lossy().into_owned();
        self.with_write_lock(|volumes| {
            reject_same_directory_alias(volumes, name, &self.volumes_dir)?;
            if let Some(existing) = volumes.get_mut(name) {
                validate_anonymous_config(existing, name, owner, &expected_mount_point)?;
                ensure_managed_volume_directory(Path::new(&expected_mount_point))?;
                existing.attach(owner);
                existing
                    .labels
                    .insert(ANONYMOUS_KIND_LABEL.to_string(), ANONYMOUS_KIND.to_string());
                existing
                    .labels
                    .insert(ANONYMOUS_OWNER_LABEL.to_string(), owner.to_string());
                return Ok((existing.clone(), false));
            }

            let mut config = VolumeConfig::new(name, "");
            config
                .labels
                .insert(ANONYMOUS_LABEL.to_string(), "true".to_string());
            config
                .labels
                .insert(ANONYMOUS_KIND_LABEL.to_string(), ANONYMOUS_KIND.to_string());
            config
                .labels
                .insert(ANONYMOUS_OWNER_LABEL.to_string(), owner.to_string());
            config.attach(owner);
            self.materialize(config, volumes)
                .map(|created| (created, true))
        })
    }

    /// Remove only the anonymous volume capability owned by `owner`.
    ///
    /// The directory is removed while the metadata lock is held, before the
    /// atomic metadata update. If a previous claim created the deterministic
    /// directory but crashed before publishing metadata, the durable Box
    /// record can use this operation to remove that orphan safely.
    pub fn remove_anonymous(&self, name: &str, owner: &str) -> Result<bool> {
        validate_anonymous_identity(name, owner)?;
        let expected_mount_point = self.volumes_dir.join(name).to_string_lossy().into_owned();
        self.with_write_lock(|volumes| {
            if let Some(existing) =
                other_volume_for_same_directory(volumes, name, Path::new(&expected_mount_point))
            {
                if let Some(config) = volumes.get(name).cloned() {
                    validate_anonymous_config(&config, name, owner, &expected_mount_point)?;
                    volumes.remove(name);
                    return Ok(true);
                }
                return Err(BoxError::ConfigError(format!(
                    "volume '{name}' names the same directory as volume '{existing}'"
                )));
            }
            let existed = if let Some(existing) = volumes.get(name) {
                validate_anonymous_config(existing, name, owner, &expected_mount_point)?;
                true
            } else {
                false
            };

            remove_managed_volume_path(Path::new(&expected_mount_point))?;
            if existed {
                volumes.remove(name);
            }
            Ok(existed)
        })
    }

    /// Create the volume's data directory, set its mount point, and insert it
    /// into `volumes`. Caller must already hold the write lock.
    fn materialize(
        &self,
        mut config: VolumeConfig,
        volumes: &mut HashMap<String, VolumeConfig>,
    ) -> Result<VolumeConfig> {
        validate_managed_volume_name(&config.name)?;
        let vol_dir = self.volumes_dir.join(&config.name);
        if let Some(existing) = volume_name_for_same_directory(volumes, &vol_dir) {
            return Err(BoxError::ConfigError(format!(
                "volume '{}' names the same directory as volume '{existing}'",
                config.name
            )));
        }
        ensure_managed_volume_directory(&vol_dir)?;
        config.mount_point = vol_dir.to_string_lossy().into_owned();
        volumes.insert(config.name.clone(), config.clone());
        Ok(config)
    }

    /// Remove a volume by name. Returns error if in use.
    pub fn remove(&self, name: &str, force: bool) -> Result<VolumeConfig> {
        self.with_write_lock(|volumes| {
            let config = volumes
                .get(name)
                .cloned()
                .ok_or_else(|| BoxError::ConfigError(format!("volume '{}' not found", name)))?;

            if config.is_in_use() && !force {
                return Err(BoxError::ConfigError(format!(
                    "volume '{}' is in use by {} box(es); use --force to remove",
                    name,
                    config.in_use_by.len()
                )));
            }

            let vol_dir = self.volumes_dir.join(name);
            // An invalid name can be the sibling sidecar file of another
            // volume. Drop the catalog entry and delete only a real directory.
            if other_volume_for_same_directory(volumes, name, &vol_dir).is_some() {
                volumes.remove(name);
                return Ok(config);
            }

            if validate_managed_volume_name(name).is_err() {
                // `join` follows `..` and absolute names. An invalid catalog
                // key must not delete a directory outside this volume store.
                if a3s_box_core::volume_posix::managed_volume_directory(&self.volumes_dir, &vol_dir)
                    .is_some()
                    && !managed_volume_ancestor_link(&vol_dir).unwrap_or(true)
                {
                    match std::fs::symlink_metadata(&vol_dir) {
                        Ok(metadata)
                            if metadata.is_dir()
                                && !metadata.file_type().is_symlink()
                                && !managed_volume_is_reparse_point(&metadata) =>
                        {
                            std::fs::remove_dir_all(&vol_dir).map_err(|error| {
                                BoxError::ConfigError(format!(
                                    "failed to remove volume '{name}' data directory {}: {error}",
                                    vol_dir.display()
                                ))
                            })?;
                        }
                        Ok(_) => {}
                        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                        Err(error) => {
                            return Err(BoxError::ConfigError(format!(
                                "failed to remove volume '{name}' data directory {}: {error}",
                                vol_dir.display()
                            )))
                        }
                    }
                }
                volumes.remove(name);
                return Ok(config);
            }

            // Keep the directory removal under the same lock as the metadata
            // mutation. If this happened after the lock were released, a
            // concurrent get_or_create could materialize a new volume with the
            // same name and this stale cleanup would delete its data.
            remove_managed_volume_path(&vol_dir).map_err(|error| {
                BoxError::ConfigError(format!(
                    "failed to remove volume '{}' data directory {}: {error}",
                    name,
                    vol_dir.display()
                ))
            })?;

            volumes.remove(name);
            Ok(config)
        })
    }

    /// List all volumes.
    pub fn list(&self) -> Result<Vec<VolumeConfig>> {
        let volumes = self.load()?;
        Ok(volumes.into_values().collect())
    }

    /// Replace a volume's config wholesale under the cross-process lock.
    ///
    /// For attach/detach prefer [`Self::modify`]: a `get` → mutate → `update`
    /// reads outside the lock and would lose a concurrent update made between
    /// the two calls.
    pub fn update(&self, config: &VolumeConfig) -> Result<()> {
        self.with_write_lock(|volumes| {
            if !volumes.contains_key(&config.name) {
                return Err(BoxError::ConfigError(format!(
                    "volume '{}' not found",
                    config.name
                )));
            }
            validate_managed_volume_name(&config.name)?;
            reject_same_directory_alias(volumes, &config.name, &self.volumes_dir)?;
            require_managed_mount_point(&self.volumes_dir, &config.name, &config.mount_point)?;
            ensure_managed_volume_directory(&self.volumes_dir.join(&config.name))?;
            volumes.insert(config.name.clone(), config.clone());
            Ok(())
        })
    }

    /// Atomically mutate one volume's config under the cross-process lock.
    ///
    /// Re-reads the current entry inside the lock so concurrent attach/detach
    /// accumulate correctly — the canonical fix for the split `get` → mutate →
    /// `update` race that could drop a volume's `in_use_by` entry. Returns
    /// `false` if the volume does not exist.
    pub fn modify<F>(&self, name: &str, f: F) -> Result<bool>
    where
        F: FnOnce(&mut VolumeConfig),
    {
        self.with_write_lock(|volumes| {
            if !volumes.contains_key(name) {
                return Ok(false);
            }
            validate_managed_volume_name(name)?;
            reject_same_directory_alias(volumes, name, &self.volumes_dir)?;
            let Some(config) = volumes.get_mut(name) else {
                return Ok(false);
            };
            f(config);
            if config.name != name {
                return Err(BoxError::ConfigError(format!(
                    "volume '{name}' cannot be renamed"
                )));
            }
            require_managed_mount_point(&self.volumes_dir, name, &config.mount_point)?;
            ensure_managed_volume_directory(&self.volumes_dir.join(name))?;
            Ok(true)
        })
    }

    /// Remove all volumes that are not in use. Returns names of removed volumes.
    pub fn prune(&self) -> Result<Vec<String>> {
        let volumes = self.load()?;
        let candidates: Vec<String> = volumes
            .iter()
            .filter(|(_, config)| !config.is_in_use())
            .map(|(name, _)| name.clone())
            .collect();

        let mut pruned = Vec::new();
        for name in candidates {
            match self.remove(&name, false) {
                Ok(_) => pruned.push(name),
                // Raced to in-use or already gone: do not claim removal.
                Err(error)
                    if error.to_string().contains("not found")
                        || error.to_string().contains("in use") => {}
                Err(error) => return Err(error),
            }
        }

        Ok(pruned)
    }

    /// Get the volume data directory for a named volume.
    pub fn volume_dir(&self, name: &str) -> PathBuf {
        self.volumes_dir.join(name)
    }

    /// Get the store file path.
    pub fn path(&self) -> &Path {
        &self.path
    }

    pub(crate) fn mount_point_is_managed(&self, name: &str, mount_point: &str) -> bool {
        let _ = name;
        let mount_point = Path::new(mount_point);
        let Some(directory_name) = mount_point
            .file_name()
            .and_then(|component| component.to_str())
        else {
            return false;
        };
        validate_managed_volume_name(directory_name).is_ok()
            && a3s_box_core::volume_posix::managed_volume_directory(&self.volumes_dir, mount_point)
                .is_some()
            && managed_path_can_be_a_volume_directory(mount_point)
    }
}

fn validate_managed_volume_name(name: &str) -> Result<()> {
    if !name.is_empty()
        && name.chars().count() <= 255
        && !name.contains(['\0', '/', '\\', ':'])
        && name != "."
        && name != ".."
        && !windows_name_ends_with_dot_or_space(name)
        && !windows_reserved_device_name(name)
        && !name_contains_control_character(name)
        && !windows_forbidden_filename_character(name)
        && !name_is_volume_posix_sidecar(name)
    {
        return Ok(());
    }
    Err(BoxError::ConfigError(format!(
        "invalid volume name {name:?}"
    )))
}

fn windows_name_ends_with_dot_or_space(name: &str) -> bool {
    #[cfg(windows)]
    {
        name.ends_with(['.', ' '])
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        false
    }
}

fn name_contains_control_character(name: &str) -> bool {
    name.chars().any(char::is_control)
}

fn name_is_volume_posix_sidecar(name: &str) -> bool {
    const SUFFIX: &str = a3s_box_core::volume_posix::VOLUME_POSIX_SIDECAR_SUFFIX;
    #[cfg(windows)]
    {
        name.len() >= SUFFIX.len()
            && name
                .get(name.len() - SUFFIX.len()..)
                .is_some_and(|tail| tail.eq_ignore_ascii_case(SUFFIX))
    }
    #[cfg(not(windows))]
    {
        name.ends_with(SUFFIX)
    }
}

fn windows_forbidden_filename_character(name: &str) -> bool {
    #[cfg(windows)]
    {
        name.chars()
            .any(|character| matches!(character, '<' | '>' | '"' | '|' | '?' | '*'))
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        false
    }
}

fn require_managed_mount_point(volumes_dir: &Path, name: &str, mount_point: &str) -> Result<()> {
    let expected = volumes_dir.join(name);
    let matches = {
        #[cfg(windows)]
        {
            a3s_box_core::volume_posix::same_windows_directory(Path::new(mount_point), &expected)
        }
        #[cfg(not(windows))]
        {
            Path::new(mount_point) == expected
        }
    };
    if matches {
        return Ok(());
    }
    Err(BoxError::ConfigError(format!(
        "volume '{name}' mount point is not its managed directory"
    )))
}

fn windows_reserved_device_name(name: &str) -> bool {
    #[cfg(windows)]
    {
        let stem = name.split('.').next().unwrap_or(name);
        let stem = stem.trim_end_matches(['.', ' ']);
        const RESERVED: &[&str] = &[
            "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7",
            "COM8", "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
        ];
        RESERVED
            .iter()
            .any(|reserved| stem.eq_ignore_ascii_case(reserved))
    }
    #[cfg(not(windows))]
    {
        let _ = name;
        false
    }
}

fn valid_anonymous_volume_name(name: &str) -> bool {
    name.starts_with("anon_")
        && name.len() <= 128
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
}

fn validate_anonymous_identity(name: &str, owner: &str) -> Result<()> {
    if !valid_anonymous_volume_name(name) {
        return Err(BoxError::ConfigError(format!(
            "invalid anonymous volume name {name:?}"
        )));
    }
    if owner.is_empty() || owner.contains('\0') {
        return Err(BoxError::ConfigError(
            "anonymous volume owner must be a non-empty execution identity".to_string(),
        ));
    }
    Ok(())
}

fn validate_anonymous_config(
    config: &VolumeConfig,
    name: &str,
    owner: &str,
    expected_mount_point: &str,
) -> Result<()> {
    if config.labels.get(ANONYMOUS_LABEL).map(String::as_str) != Some("true") {
        return Err(BoxError::ConfigError(format!(
            "volume {name:?} is not an anonymous volume"
        )));
    }
    if config.driver != "local" {
        return Err(BoxError::ConfigError(format!(
            "anonymous volume {name:?} does not use the local driver"
        )));
    }
    if config.mount_point != expected_mount_point {
        return Err(BoxError::ConfigError(format!(
            "anonymous volume {name:?} does not use its canonical managed directory"
        )));
    }
    let exact_owner = config.in_use_by.len() == 1
        && config
            .in_use_by
            .first()
            .is_some_and(|current| current == owner);
    if config.in_use_by.iter().any(|current| current != owner) {
        return Err(BoxError::ConfigError(format!(
            "anonymous volume {name:?} is owned by another execution"
        )));
    }
    match config.labels.get(ANONYMOUS_KIND_LABEL).map(String::as_str) {
        Some(ANONYMOUS_KIND)
            if exact_owner
                && config
                    .labels
                    .get(ANONYMOUS_OWNER_LABEL)
                    .is_some_and(|current| current == owner) =>
        {
            Ok(())
        }
        None if exact_owner => Ok(()),
        _ => Err(BoxError::ConfigError(format!(
            "anonymous volume {name:?} has no compatible ownership contract"
        ))),
    }
}

/// Windows named volumes share one directory when only case, a short name,
/// or another Win32 alias differs. A second catalog entry would delete the
/// first volume's files on remove.
/// A second catalog key can name the directory Win32 already opened for
/// another volume. Mounting or deleting that key would share or destroy
/// the first volume's files.
fn other_volume_for_same_directory<'a>(
    volumes: &'a HashMap<String, VolumeConfig>,
    name: &str,
    vol_dir: &Path,
) -> Option<&'a str> {
    #[cfg(windows)]
    {
        return volumes.iter().find_map(|(other, config)| {
            if other == name {
                return None;
            }
            a3s_box_core::volume_posix::same_windows_directory(
                Path::new(&config.mount_point),
                vol_dir,
            )
            .then_some(other.as_str())
        });
    }
    #[cfg(not(windows))]
    {
        let _ = (volumes, name, vol_dir);
        None
    }
}

fn reject_same_directory_alias(
    volumes: &HashMap<String, VolumeConfig>,
    name: &str,
    volumes_dir: &Path,
) -> Result<()> {
    if let Some(existing) = other_volume_for_same_directory(volumes, name, &volumes_dir.join(name))
    {
        return Err(BoxError::ConfigError(format!(
            "volume '{name}' names the same directory as volume '{existing}'"
        )));
    }
    Ok(())
}

fn volume_name_for_same_directory<'a>(
    volumes: &'a HashMap<String, VolumeConfig>,
    vol_dir: &Path,
) -> Option<&'a str> {
    #[cfg(windows)]
    {
        return volumes.iter().find_map(|(name, config)| {
            a3s_box_core::volume_posix::same_windows_directory(
                Path::new(&config.mount_point),
                vol_dir,
            )
            .then_some(name.as_str())
        });
    }
    #[cfg(not(windows))]
    {
        let _ = (volumes, vol_dir);
        None
    }
}

/// A missing managed path can still be created at boot. A file, symlink,
/// or reparse point already occupies the name and is not a volume directory.
/// A symlink or reparse ancestor is the same refusal: the leaf can look like
/// a real directory after the link is followed.
pub(crate) fn managed_path_can_be_a_volume_directory(path: &Path) -> bool {
    if managed_volume_ancestor_link(path).unwrap_or(true) {
        return false;
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata) => {
            metadata.is_dir()
                && !metadata.file_type().is_symlink()
                && !managed_volume_is_reparse_point(&metadata)
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => true,
        Err(_) => false,
    }
}

pub(crate) fn managed_volume_ancestor_is_link(path: &Path) -> bool {
    managed_volume_ancestor_link(path).unwrap_or(true)
}

fn managed_volume_ancestor_link(path: &Path) -> std::io::Result<bool> {
    let Some(parent) = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
    else {
        return Ok(false);
    };
    // Stat every ancestor as its own final component. The nearest existing
    // directory can be a real directory reached by following a link higher up.
    for ancestor in parent.ancestors() {
        if ancestor.as_os_str().is_empty() {
            break;
        }
        match std::fs::symlink_metadata(ancestor) {
            Ok(metadata) => {
                if metadata.file_type().is_symlink() || managed_volume_is_reparse_point(&metadata) {
                    return Ok(true);
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    Ok(false)
}

fn ensure_managed_volume_directory(path: &Path) -> Result<()> {
    match managed_volume_ancestor_link(path) {
        Ok(true) => {
            return Err(BoxError::ConfigError(format!(
                "managed volume path {} is not a directory",
                path.display()
            )));
        }
        Ok(false) => {}
        Err(error) => {
            return Err(BoxError::ConfigError(format!(
                "failed to inspect volume directory {}: {error}",
                path.display()
            )));
        }
    }
    match std::fs::symlink_metadata(path) {
        Ok(metadata)
            if metadata.file_type().is_symlink()
                || managed_volume_is_reparse_point(&metadata)
                || !metadata.is_dir() =>
        {
            return Err(BoxError::ConfigError(format!(
                "managed volume path {} is not a directory",
                path.display()
            )))
        }
        Ok(_) => return Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => {
            return Err(BoxError::ConfigError(format!(
                "failed to inspect volume directory {}: {error}",
                path.display()
            )))
        }
    }
    std::fs::create_dir_all(path).map_err(|error| {
        BoxError::ConfigError(format!(
            "failed to create volume directory {}: {error}",
            path.display()
        ))
    })?;
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        BoxError::ConfigError(format!(
            "failed to verify volume directory {}: {error}",
            path.display()
        ))
    })?;
    if metadata.file_type().is_symlink()
        || managed_volume_is_reparse_point(&metadata)
        || !metadata.is_dir()
    {
        return Err(BoxError::ConfigError(format!(
            "managed volume path {} is not a directory",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn managed_volume_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;
    metadata.file_attributes() & 0x0000_0400 != 0
}

#[cfg(not(windows))]
fn managed_volume_is_reparse_point(_metadata: &std::fs::Metadata) -> bool {
    false
}

fn remove_managed_volume_path(path: &Path) -> Result<()> {
    // A symlink or reparse ancestor means this path is not the store's own
    // directory. Dropping the catalog entry must not delete or rewrite the
    // directory named by the link.
    if managed_volume_ancestor_link(path).unwrap_or(true) {
        return Ok(());
    }
    // Drop the sidecar before the directory. A later recreate of the same
    // name must not inherit uid/gid from the deleted volume.
    a3s_box_core::volume_posix::remove_volume_posix_sidecar(path).map_err(BoxError::IoError)?;
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(BoxError::IoError(error)),
    };
    if metadata.is_dir()
        && !metadata.file_type().is_symlink()
        && !managed_volume_is_reparse_point(&metadata)
    {
        std::fs::remove_dir_all(path).map_err(BoxError::IoError)
    } else {
        std::fs::remove_file(path).map_err(BoxError::IoError)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_store() -> (tempfile::TempDir, VolumeStore) {
        let dir = tempfile::tempdir().unwrap();
        let store = VolumeStore::new(dir.path().join("volumes.json"), dir.path().join("volumes"));
        (dir, store)
    }

    #[test]
    fn test_load_empty() {
        let (_dir, store) = temp_store();
        let volumes = store.load().unwrap();
        assert!(volumes.is_empty());
    }

    #[test]
    fn test_create_and_load() {
        let (_dir, store) = temp_store();
        let vol = VolumeConfig::new("mydata", "");
        store.create(vol).unwrap();

        let volumes = store.load().unwrap();
        assert_eq!(volumes.len(), 1);
        assert!(volumes.contains_key("mydata"));
    }

    #[test]
    fn test_create_sets_mount_point() {
        let (_dir, store) = temp_store();
        let vol = VolumeConfig::new("mydata", "");
        let created = store.create(vol).unwrap();

        assert!(created.mount_point.contains("mydata"));
        assert!(PathBuf::from(&created.mount_point).exists());
    }

    #[test]
    fn remove_deletes_volume_posix_sidecar() {
        use a3s_box_core::rootfs_metadata::{RootfsEntryKind, RootfsMetadataEntry};
        use a3s_box_core::volume_posix::{
            read_volume_posix_sidecar, volume_posix_sidecar_path, write_volume_posix_sidecar,
            VolumePosixSidecar,
        };

        let (_dir, store) = temp_store();
        let created = store.create(VolumeConfig::new("mydata", "")).unwrap();
        let volume_dir = PathBuf::from(&created.mount_point);
        write_volume_posix_sidecar(
            &volume_dir,
            &VolumePosixSidecar::new(vec![RootfsMetadataEntry {
                path_base64: "Lg==".to_string(),
                kind: RootfsEntryKind::Directory,
                mode: 0o750,
                uid: 1000,
                gid: 1000,
                mtime: 0,
                size: 0,
                link_target_base64: None,
            }]),
        )
        .unwrap();
        let sidecar = volume_posix_sidecar_path(&volume_dir).unwrap();
        assert!(sidecar.is_file());

        store.remove("mydata", false).unwrap();
        assert!(!sidecar.exists());
        assert!(!volume_dir.exists());

        let recreated = store.create(VolumeConfig::new("mydata", "")).unwrap();
        assert!(read_volume_posix_sidecar(Path::new(&recreated.mount_point))
            .unwrap()
            .is_none());
    }

    #[test]
    fn create_rejects_a_name_that_leaves_the_volume_directory() {
        let (dir, store) = temp_store();
        let error = store
            .create(VolumeConfig::new("nested/../../outside", ""))
            .expect_err("a volume name is one directory, not a path");

        assert!(error.to_string().contains("invalid volume name"), "{error}");
        assert!(!dir.path().join("outside").exists());
        assert!(!dir.path().join("volumes").join("nested").exists());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn prune_drops_an_escaping_volume_name_without_deleting_outside() {
        let (dir, store) = temp_store();
        let outside = dir.path().join("outside");
        std::fs::create_dir(&outside).unwrap();
        std::fs::write(outside.join("keep.txt"), b"kept").unwrap();
        let name = if cfg!(windows) {
            r"..\outside"
        } else {
            "../outside"
        };
        let mut volumes = store.load().unwrap();
        volumes.insert(
            name.to_string(),
            VolumeConfig::new(name, &outside.to_string_lossy()),
        );
        store.save(&volumes).unwrap();

        let pruned = store
            .prune()
            .expect("pruning an escaping name must not delete a directory outside the store");
        assert!(pruned.iter().any(|pruned_name| pruned_name == name));
        assert!(store.get(name).unwrap().is_none());
        assert_eq!(std::fs::read(outside.join("keep.txt")).unwrap(), b"kept");
        assert!(outside.is_dir());
    }

    #[test]
    fn get_or_create_rejects_a_managed_path_that_is_not_a_directory() {
        let (_dir, store) = temp_store();
        let created = store.create(VolumeConfig::new("data", "")).unwrap();
        let volume_dir = PathBuf::from(&created.mount_point);
        std::fs::remove_dir_all(&volume_dir).unwrap();
        std::fs::write(&volume_dir, b"not-a-directory").unwrap();

        let error = store
            .get_or_create(VolumeConfig::new("data", ""))
            .expect_err("a named volume path must be a directory");
        assert!(error.to_string().contains("not a directory"), "{error}");

        let mut attached = store.get("data").unwrap().unwrap();
        attached.attach("box-1");
        let updated = store.update(&attached);
        assert!(updated.is_err(), "{updated:?}");
        let modified = store.modify("data", |config| {
            config.attach("box-1");
        });
        assert!(modified.is_err(), "{modified:?}");
        assert!(store.get("data").unwrap().unwrap().in_use_by.is_empty());
        assert_eq!(std::fs::read(&volume_dir).unwrap(), b"not-a-directory");
    }

    #[cfg(windows)]
    #[test]
    fn save_does_not_write_through_a_directory_junction() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("secret.txt"), b"keep").unwrap();
        let parent = dir.path().join("catalog");
        use std::os::windows::process::CommandExt;
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            parent.display(),
            outside.display()
        ));
        let status = command.status().expect("mklink");
        assert!(status.success(), "mklink /J failed: {status}");
        let store = VolumeStore::new(parent.join("volumes.json"), parent.join("volumes"));
        let saved = store.save(&HashMap::new());
        assert!(
            saved.is_err(),
            "save followed a directory junction: {saved:?}"
        );
        assert!(
            !outside.join("volumes.json").exists(),
            "volumes.json was written through the junction"
        );
        assert!(
            !outside.join("volumes.json.tmp").exists(),
            "volumes.json.tmp was written through the junction"
        );
        assert_eq!(std::fs::read(outside.join("secret.txt")).unwrap(), b"keep");
    }

    #[cfg(windows)]
    #[test]
    fn load_does_not_read_through_an_ancestor_junction() {
        use std::os::windows::process::CommandExt;

        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        let volumes = outside.join("volumes");
        std::fs::create_dir_all(&volumes).unwrap();
        let real = VolumeStore::new(outside.join("volumes.json"), &volumes);
        real.create(VolumeConfig::new("secret-vol", "")).unwrap();
        let parent = dir.path().join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let link = parent.join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let store = VolumeStore::new(link.join("volumes.json"), link.join("volumes"));
        let loaded = store.load();
        match loaded {
            Ok(volumes) => panic!(
                "loaded volumes through a junction: {:?}",
                volumes.keys().collect::<Vec<_>>()
            ),
            Err(error) => assert!(
                error.to_string().contains("junction"),
                "expected a junction refusal, got {error}"
            ),
        }
        assert!(outside.join("volumes.json").is_file());
    }

    #[test]
    fn create_rejects_a_volume_store_directory_that_is_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(outside.join("data")).unwrap();
        std::fs::write(outside.join("data").join("secret.txt"), b"secret").unwrap();
        let volumes = dir.path().join("volumes");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &volumes).unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let mut command = std::process::Command::new("cmd");
            command.raw_arg(format!(
                "/C mklink /J \"{}\" \"{}\"",
                volumes.display(),
                outside.display()
            ));
            let status = command.status().expect("mklink");
            assert!(status.success(), "mklink /J failed: {status}");
        }
        let store = VolumeStore::new(dir.path().join("volumes.json"), &volumes);
        let error = store
            .create(VolumeConfig::new("data", ""))
            .expect_err("a linked volume store must not become a managed volume");
        assert!(error.to_string().contains("not a directory"), "{error}");
        assert!(!store.mount_point_is_managed("data", &volumes.join("data").to_string_lossy()));
        assert_eq!(
            std::fs::read(outside.join("data").join("secret.txt")).unwrap(),
            b"secret"
        );
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn remove_drops_a_catalog_entry_when_the_volume_store_is_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(outside.join("data")).unwrap();
        std::fs::write(outside.join("data").join("secret.txt"), b"secret").unwrap();
        std::fs::write(outside.join("data.a3s-volume-posix.v1.json"), b"sidecar").unwrap();
        let volumes = dir.path().join("volumes");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &volumes).unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let mut command = std::process::Command::new("cmd");
            command.raw_arg(format!(
                "/C mklink /J \"{}\" \"{}\"",
                volumes.display(),
                outside.display()
            ));
            let status = command.status().expect("mklink");
            assert!(status.success(), "mklink /J failed: {status}");
        }
        let store = VolumeStore::new(dir.path().join("volumes.json"), &volumes);
        let mount_point = volumes.join("data");
        let mut volumes_catalog = store.load().unwrap();
        volumes_catalog.insert(
            "data".to_string(),
            VolumeConfig::new("data", &mount_point.to_string_lossy()),
        );
        store.save(&volumes_catalog).unwrap();

        store
            .remove("data", false)
            .expect("removing a linked store entry must not delete the directory it names");
        assert!(store.get("data").unwrap().is_none());
        assert_eq!(
            std::fs::read(outside.join("data").join("secret.txt")).unwrap(),
            b"secret"
        );
        assert_eq!(
            std::fs::read(outside.join("data.a3s-volume-posix.v1.json")).unwrap(),
            b"sidecar"
        );
    }

    #[test]
    fn create_rejects_a_volume_store_reached_through_a_link() {
        let dir = tempfile::tempdir().unwrap();
        let outside = dir.path().join("outside");
        let real_store = outside.join("store");
        std::fs::create_dir_all(&real_store).unwrap();
        std::fs::write(real_store.join("keep.txt"), b"kept").unwrap();
        std::fs::create_dir_all(real_store.join("data")).unwrap();
        std::fs::write(real_store.join("data").join("secret.txt"), b"secret").unwrap();
        std::fs::write(real_store.join("data.a3s-volume-posix.v1.json"), b"sidecar").unwrap();
        let link = dir.path().join("link");
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).unwrap();
        #[cfg(windows)]
        {
            use std::os::windows::process::CommandExt;
            let mut command = std::process::Command::new("cmd");
            command.raw_arg(format!(
                "/C mklink /J \"{}\" \"{}\"",
                link.display(),
                outside.display()
            ));
            let status = command.status().expect("mklink");
            assert!(status.success(), "mklink /J failed: {status}");
        }
        let volumes = link.join("store");
        let store = VolumeStore::new(dir.path().join("volumes.json"), &volumes);
        let error = store
            .create(VolumeConfig::new("data", ""))
            .expect_err("a volume store reached through a link must not become a managed volume");
        assert!(error.to_string().contains("not a directory"), "{error}");
        assert!(!store.mount_point_is_managed("data", &volumes.join("data").to_string_lossy()));
        assert!(store.list().unwrap().is_empty());

        let mut volumes_catalog = store.load().unwrap();
        volumes_catalog.insert(
            "data".to_string(),
            VolumeConfig::new("data", &volumes.join("data").to_string_lossy()),
        );
        store.save(&volumes_catalog).unwrap();
        store.remove("data", false).expect(
            "removing a store reached through a link must not delete the directory it names",
        );
        assert!(store.get("data").unwrap().is_none());
        assert_eq!(std::fs::read(real_store.join("keep.txt")).unwrap(), b"kept");
        assert_eq!(
            std::fs::read(real_store.join("data").join("secret.txt")).unwrap(),
            b"secret"
        );
        assert_eq!(
            std::fs::read(real_store.join("data.a3s-volume-posix.v1.json")).unwrap(),
            b"sidecar"
        );
    }

    #[cfg(windows)]
    #[test]
    fn create_rejects_a_name_whose_trailing_dot_or_space_is_not_the_directory() {
        let (dir, store) = temp_store();
        let dotted = store
            .create(VolumeConfig::new("data.", ""))
            .expect_err("a trailing dot is not the directory Windows creates");
        assert!(
            dotted.to_string().contains("invalid volume name"),
            "{dotted}"
        );
        let spaced = store
            .create(VolumeConfig::new("data ", ""))
            .expect_err("a trailing space is not the directory Windows creates");
        assert!(
            spaced.to_string().contains("invalid volume name"),
            "{spaced}"
        );
        assert!(!dir.path().join("volumes").join("data").exists());
        assert!(store.list().unwrap().is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn create_rejects_a_windows_reserved_device_name() {
        let (dir, store) = temp_store();
        for name in ["NUL", "con", "COM1", "NUL.txt"] {
            let error = store
                .create(VolumeConfig::new(name, ""))
                .expect_err("a reserved device name is not a volume directory");
            assert!(
                error.to_string().contains("invalid volume name"),
                "{name}: {error}"
            );
        }
        assert!(!dir.path().join("volumes").join("NUL").exists());
        assert!(store.list().unwrap().is_empty());
    }

    #[test]
    fn create_rejects_a_name_with_a_control_character() {
        let (dir, store) = temp_store();
        let error = store
            .create(VolumeConfig::new("data\nextra", ""))
            .expect_err("a volume name cannot contain a control character");
        assert!(error.to_string().contains("invalid volume name"), "{error}");
        assert!(store.list().unwrap().is_empty());
        assert!(!dir.path().join("volumes").join("data\nextra").exists());
    }

    #[cfg(windows)]
    #[test]
    fn create_rejects_a_windows_forbidden_filename_character() {
        let (dir, store) = temp_store();
        for name in ["data?", "data*", "a|b", "a<b", "a>b", "a\"b"] {
            let error = store
                .create(VolumeConfig::new(name, ""))
                .expect_err("a Windows volume name cannot contain a forbidden filename character");
            assert!(
                error.to_string().contains("invalid volume name"),
                "{name}: {error}"
            );
        }
        assert!(store.list().unwrap().is_empty());
        assert!(!dir.path().join("volumes").join("data").exists());
    }

    #[test]
    fn create_rejects_a_name_that_is_a_volume_posix_sidecar() {
        let (dir, store) = temp_store();
        let suffix = a3s_box_core::volume_posix::VOLUME_POSIX_SIDECAR_SUFFIX;
        let name = format!("data{suffix}");
        let error = store
            .create(VolumeConfig::new(&name, ""))
            .expect_err("a volume directory cannot occupy a posix sidecar filename");
        assert!(error.to_string().contains("invalid volume name"), "{error}");
        assert!(store.list().unwrap().is_empty());
        assert!(!dir.path().join("volumes").join(&name).exists());

        #[cfg(windows)]
        {
            let folded = "data.A3S-VOLUME-POSIX.V1.JSON";
            let error = store
                .create(VolumeConfig::new(folded, ""))
                .expect_err("a Windows volume directory cannot occupy a posix sidecar filename");
            assert!(
                error.to_string().contains("invalid volume name"),
                "{folded}: {error}"
            );
            assert!(!dir.path().join("volumes").join(folded).exists());
        }
    }

    #[test]
    fn get_or_create_rejects_a_stored_sidecar_filename() {
        let (dir, store) = temp_store();
        let data = store.create(VolumeConfig::new("data", "")).unwrap();
        let suffix = a3s_box_core::volume_posix::VOLUME_POSIX_SIDECAR_SUFFIX;
        let name = format!("data{suffix}");
        let mount = dir.path().join("volumes").join(&name);
        let mut volumes = store.load().unwrap();
        volumes.insert(
            name.clone(),
            VolumeConfig::new(&name, &mount.to_string_lossy()),
        );
        store.save(&volumes).unwrap();

        let error = store
            .get_or_create(VolumeConfig::new(&name, ""))
            .expect_err("a stored sidecar filename is not a usable volume");
        assert!(error.to_string().contains("invalid volume name"), "{error}");
        assert_eq!(
            store.get("data").unwrap().unwrap().mount_point,
            data.mount_point
        );
        assert!(store.get(&name).unwrap().is_some());
        assert!(!mount.exists());
        assert!(PathBuf::from(&data.mount_point).is_dir());
    }

    #[test]
    fn prune_keeps_another_volumes_posix_sidecar_when_a_record_uses_that_filename() {
        use a3s_box_core::rootfs_metadata::{RootfsEntryKind, RootfsMetadataEntry};
        use a3s_box_core::volume_posix::{
            read_volume_posix_sidecar, volume_posix_sidecar_path, write_volume_posix_sidecar,
            VolumePosixSidecar,
        };

        let (dir, store) = temp_store();
        let mut data = store.create(VolumeConfig::new("data", "")).unwrap();
        let volume_dir = PathBuf::from(&data.mount_point);
        write_volume_posix_sidecar(
            &volume_dir,
            &VolumePosixSidecar::new(vec![RootfsMetadataEntry {
                path_base64: "Lg==".to_string(),
                kind: RootfsEntryKind::Directory,
                mode: 0o640,
                uid: 1000,
                gid: 1000,
                mtime: 0,
                size: 0,
                link_target_base64: None,
            }]),
        )
        .unwrap();
        data.attach("box-1");
        store.update(&data).unwrap();

        let suffix = a3s_box_core::volume_posix::VOLUME_POSIX_SIDECAR_SUFFIX;
        let name = format!("data{suffix}");
        let sidecar = volume_posix_sidecar_path(&volume_dir).unwrap();
        let mut volumes = store.load().unwrap();
        volumes.insert(
            name.clone(),
            VolumeConfig::new(&name, &sidecar.to_string_lossy()),
        );
        store.save(&volumes).unwrap();

        let pruned = store
            .prune()
            .expect("pruning a sidecar filename must not delete another volume's metadata");
        assert!(pruned.contains(&name));
        assert!(!pruned.iter().any(|pruned_name| pruned_name == "data"));
        assert!(store.get(&name).unwrap().is_none());
        assert!(store.get("data").unwrap().is_some());
        assert!(sidecar.is_file());
        let kept = read_volume_posix_sidecar(&volume_dir)
            .unwrap()
            .expect("data sidecar");
        assert_eq!(kept.entries[0].mode, 0o640);
        assert!(volume_dir.is_dir());
        assert!(!dir.path().join("volumes").join("data").join(&name).exists());

        #[cfg(windows)]
        {
            let folded = "data.A3S-VOLUME-POSIX.V1.JSON";
            let mut volumes = store.load().unwrap();
            volumes.insert(
                folded.to_string(),
                VolumeConfig::new(folded, &sidecar.to_string_lossy()),
            );
            store.save(&volumes).unwrap();
            let pruned = store.prune().expect("a case-different sidecar filename");
            assert!(pruned.contains(&folded.to_string()));
            assert!(sidecar.is_file());
            assert_eq!(
                read_volume_posix_sidecar(&volume_dir)
                    .unwrap()
                    .expect("data sidecar")
                    .entries[0]
                    .mode,
                0o640
            );
        }
    }

    #[test]
    fn update_rejects_a_stored_sidecar_filename() {
        use a3s_box_core::rootfs_metadata::{RootfsEntryKind, RootfsMetadataEntry};
        use a3s_box_core::volume_posix::{
            read_volume_posix_sidecar, volume_posix_sidecar_path, write_volume_posix_sidecar,
            VolumePosixSidecar,
        };

        let (_dir, store) = temp_store();
        let data = store.create(VolumeConfig::new("data", "")).unwrap();
        let volume_dir = PathBuf::from(&data.mount_point);
        write_volume_posix_sidecar(
            &volume_dir,
            &VolumePosixSidecar::new(vec![RootfsMetadataEntry {
                path_base64: "Lg==".to_string(),
                kind: RootfsEntryKind::Directory,
                mode: 0o640,
                uid: 1000,
                gid: 1000,
                mtime: 0,
                size: 0,
                link_target_base64: None,
            }]),
        )
        .unwrap();
        let suffix = a3s_box_core::volume_posix::VOLUME_POSIX_SIDECAR_SUFFIX;
        let name = format!("data{suffix}");
        let sidecar = volume_posix_sidecar_path(&volume_dir).unwrap();
        let mut volumes = store.load().unwrap();
        volumes.insert(
            name.clone(),
            VolumeConfig::new(&name, &sidecar.to_string_lossy()),
        );
        store.save(&volumes).unwrap();

        let mut attached = store.get(&name).unwrap().unwrap();
        attached.attach("box-1");
        let error = store
            .update(&attached)
            .expect_err("a sidecar filename cannot be marked in use");
        assert!(error.to_string().contains("invalid volume name"), "{error}");
        let modified = store.modify(&name, |config| {
            config.attach("box-1");
        });
        assert!(modified.is_err(), "{modified:?}");
        assert!(store.get(&name).unwrap().unwrap().in_use_by.is_empty());
        assert!(sidecar.is_file());
        assert_eq!(
            read_volume_posix_sidecar(&volume_dir)
                .unwrap()
                .expect("data sidecar")
                .entries[0]
                .mode,
            0o640
        );
        assert_eq!(
            store.get("data").unwrap().unwrap().mount_point,
            data.mount_point
        );
    }

    #[test]
    fn modify_rejects_a_renamed_volume() {
        let (_dir, store) = temp_store();
        let created = store.create(VolumeConfig::new("data", "")).unwrap();
        let suffix = a3s_box_core::volume_posix::VOLUME_POSIX_SIDECAR_SUFFIX;
        let renamed = format!("data{suffix}");
        let error = store
            .modify("data", |config| {
                config.name = renamed;
            })
            .expect_err("a volume record cannot take a sidecar filename");
        assert!(error.to_string().contains("cannot be renamed"), "{error}");
        assert_eq!(store.get("data").unwrap().unwrap().name, "data");
        assert_eq!(
            store.get("data").unwrap().unwrap().mount_point,
            created.mount_point
        );

        let error = store
            .modify("data", |config| {
                config.name = "other".to_string();
            })
            .expect_err("a volume record cannot be renamed");
        assert!(error.to_string().contains("cannot be renamed"), "{error}");
        assert_eq!(store.get("data").unwrap().unwrap().name, "data");
    }

    #[cfg(windows)]
    #[test]
    fn prune_drops_a_same_directory_alias_without_deleting_the_volume() {
        use a3s_box_core::rootfs_metadata::{RootfsEntryKind, RootfsMetadataEntry};
        use a3s_box_core::volume_posix::{
            read_volume_posix_sidecar, write_volume_posix_sidecar, VolumePosixSidecar,
        };

        let (dir, store) = temp_store();
        let mut data = store.create(VolumeConfig::new("data", "")).unwrap();
        let volume_dir = PathBuf::from(&data.mount_point);
        std::fs::write(volume_dir.join("keep.txt"), b"kept").unwrap();
        write_volume_posix_sidecar(
            &volume_dir,
            &VolumePosixSidecar::new(vec![RootfsMetadataEntry {
                path_base64: "Lg==".to_string(),
                kind: RootfsEntryKind::Directory,
                mode: 0o640,
                uid: 1000,
                gid: 1000,
                mtime: 0,
                size: 0,
                link_target_base64: None,
            }]),
        )
        .unwrap();
        data.attach("box-1");
        store.update(&data).unwrap();

        let alias_mount = dir.path().join("volumes").join("Data");
        let mut volumes = store.load().unwrap();
        volumes.insert(
            "Data".to_string(),
            VolumeConfig::new("Data", &alias_mount.to_string_lossy()),
        );
        store.save(&volumes).unwrap();

        let mut alias = store.get("Data").unwrap().unwrap();
        alias.attach("box-1");
        let error = store
            .update(&alias)
            .expect_err("a same-directory alias cannot be marked in use");
        assert!(error.to_string().contains("same directory"), "{error}");
        let modified = store.modify("Data", |config| {
            config.attach("box-1");
        });
        assert!(modified.is_err(), "{modified:?}");
        let resolved = store.get_or_create(VolumeConfig::new("Data", ""));
        assert!(resolved.is_err(), "{resolved:?}");
        assert!(resolved.unwrap_err().to_string().contains("same directory"));
        let resolved = store.get_or_create(VolumeConfig::new("data", ""));
        assert!(
            resolved.is_err(),
            "the stored volume is not mounted while an alias names its directory: {resolved:?}"
        );

        let pruned = store
            .prune()
            .expect("pruning a same-directory alias must keep the other volume");
        assert!(pruned.contains(&"Data".to_string()));
        assert!(!pruned.iter().any(|name| name == "data"));
        assert!(store.get("Data").unwrap().is_none());
        assert!(store.get("data").unwrap().unwrap().is_in_use());
        assert_eq!(std::fs::read(volume_dir.join("keep.txt")).unwrap(), b"kept");
        assert_eq!(
            read_volume_posix_sidecar(&volume_dir)
                .unwrap()
                .expect("data sidecar")
                .entries[0]
                .mode,
            0o640
        );

        let short_mount = dir.path().join("volumes").join("LONGVO~1");
        let mut long = store
            .create(VolumeConfig::new("LongVolumeName", ""))
            .unwrap();
        let long_dir = PathBuf::from(&long.mount_point);
        std::fs::write(long_dir.join("keep.txt"), b"kept").unwrap();
        long.attach("box-1");
        store.update(&long).unwrap();
        let mut volumes = store.load().unwrap();
        volumes.insert(
            "LONGVO~1".to_string(),
            VolumeConfig::new("LONGVO~1", &short_mount.to_string_lossy()),
        );
        store.save(&volumes).unwrap();
        let pruned = store
            .prune()
            .expect("pruning a generated short name must keep the long volume");
        assert!(pruned.contains(&"LONGVO~1".to_string()));
        assert!(store.get("LongVolumeName").unwrap().unwrap().is_in_use());
        assert_eq!(std::fs::read(long_dir.join("keep.txt")).unwrap(), b"kept");
    }

    #[test]
    fn test_create_duplicate() {
        let (_dir, store) = temp_store();
        let v1 = VolumeConfig::new("mydata", "");
        let v2 = VolumeConfig::new("mydata", "");

        store.create(v1).unwrap();
        assert!(store.create(v2).is_err());
    }

    #[cfg(windows)]
    #[test]
    fn create_rejects_a_name_that_differs_only_by_case() {
        let (_dir, store) = temp_store();
        let created = store.create(VolumeConfig::new("data", "")).unwrap();
        let error = store.create(VolumeConfig::new("Data", "")).unwrap_err();
        assert!(error.to_string().contains("same directory"), "{error}");
        assert!(store.get("data").unwrap().is_some());
        assert!(store.get("Data").unwrap().is_none());
        let again = store
            .get_or_create(VolumeConfig::new("Data", ""))
            .unwrap_err();
        assert!(again.to_string().contains("same directory"), "{again}");
        assert_eq!(store.list().unwrap().len(), 1);
        assert!(std::path::Path::new(&created.mount_point).is_dir());
    }

    #[cfg(windows)]
    #[test]
    fn create_rejects_a_name_that_differs_only_by_unicode_case() {
        let (_dir, store) = temp_store();
        store.create(VolumeConfig::new("Größe", "")).unwrap();
        let error = store.create(VolumeConfig::new("GrÖße", "")).unwrap_err();
        assert!(error.to_string().contains("same directory"), "{error}");
        assert!(store.get("Größe").unwrap().is_some());
        assert!(store.get("GrÖße").unwrap().is_none());
    }

    #[cfg(windows)]
    #[test]
    fn create_rejects_a_generated_short_name_of_an_existing_volume() {
        let (_dir, store) = temp_store();
        store
            .create(VolumeConfig::new("LongVolumeName", ""))
            .unwrap();
        let error = store.create(VolumeConfig::new("LONGVO~1", "")).unwrap_err();
        assert!(error.to_string().contains("same directory"), "{error}");
        assert!(store.get("LongVolumeName").unwrap().is_some());
        assert!(store.get("LONGVO~1").unwrap().is_none());
    }

    #[test]
    fn test_get_existing() {
        let (_dir, store) = temp_store();
        store.create(VolumeConfig::new("mydata", "")).unwrap();

        let found = store.get("mydata").unwrap();
        assert!(found.is_some());
        assert_eq!(found.unwrap().name, "mydata");
    }

    #[test]
    fn test_get_nonexistent() {
        let (_dir, store) = temp_store();
        let found = store.get("nope").unwrap();
        assert!(found.is_none());
    }

    #[test]
    fn test_remove() {
        let (_dir, store) = temp_store();
        store.create(VolumeConfig::new("mydata", "")).unwrap();

        let removed = store.remove("mydata", false).unwrap();
        assert_eq!(removed.name, "mydata");

        let volumes = store.load().unwrap();
        assert!(volumes.is_empty());
    }

    #[test]
    fn test_remove_nonexistent() {
        let (_dir, store) = temp_store();
        assert!(store.remove("nope", false).is_err());
    }

    #[test]
    fn test_remove_in_use_fails() {
        let (_dir, store) = temp_store();
        let mut vol = VolumeConfig::new("mydata", "");
        vol.attach("box-1");
        // Manually insert since create() doesn't set in_use_by
        let created = store.create(VolumeConfig::new("mydata", "")).unwrap();
        let mut updated = created;
        updated.attach("box-1");
        store.update(&updated).unwrap();

        assert!(store.remove("mydata", false).is_err());
    }

    #[test]
    fn test_remove_in_use_force() {
        let (_dir, store) = temp_store();
        let created = store.create(VolumeConfig::new("mydata", "")).unwrap();
        let mut updated = created;
        updated.attach("box-1");
        store.update(&updated).unwrap();

        let removed = store.remove("mydata", true).unwrap();
        assert_eq!(removed.name, "mydata");
    }

    #[test]
    fn test_list() {
        let (_dir, store) = temp_store();
        store.create(VolumeConfig::new("vol1", "")).unwrap();
        store.create(VolumeConfig::new("vol2", "")).unwrap();

        let list = store.list().unwrap();
        assert_eq!(list.len(), 2);
    }

    #[test]
    fn test_update() {
        let (_dir, store) = temp_store();
        let created = store.create(VolumeConfig::new("mydata", "")).unwrap();

        let mut updated = created;
        updated.attach("box-1");
        store.update(&updated).unwrap();

        let loaded = store.get("mydata").unwrap().unwrap();
        assert_eq!(loaded.in_use_by, vec!["box-1"]);
    }

    #[test]
    fn update_rejects_a_mount_point_outside_the_managed_directory() {
        let (dir, store) = temp_store();
        let created = store.create(VolumeConfig::new("mydata", "")).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let mut updated = created.clone();
        updated.mount_point = outside.to_string_lossy().into_owned();

        let error = store
            .update(&updated)
            .expect_err("a volume mount point stays inside the store");
        assert!(error.to_string().contains("managed directory"), "{error}");
        assert_eq!(
            store.get("mydata").unwrap().unwrap().mount_point,
            created.mount_point
        );

        let modified = store.modify("mydata", |config| {
            config.mount_point = outside.to_string_lossy().into_owned();
        });
        assert!(modified.is_err(), "{modified:?}");
        assert_eq!(
            store.get("mydata").unwrap().unwrap().mount_point,
            created.mount_point
        );
        assert!(outside.is_dir());
    }

    #[test]
    fn get_or_create_rejects_a_stored_mount_point_outside_the_managed_directory() {
        let (dir, store) = temp_store();
        let created = store.create(VolumeConfig::new("mydata", "")).unwrap();
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        let mut volumes = store.load().unwrap();
        volumes.get_mut("mydata").unwrap().mount_point = outside.to_string_lossy().into_owned();
        store.save(&volumes).unwrap();

        let error = store
            .get_or_create(VolumeConfig::new("mydata", ""))
            .expect_err("a stored mount point outside the volume directory is not usable");
        assert!(error.to_string().contains("managed directory"), "{error}");
        assert_eq!(
            store.get("mydata").unwrap().unwrap().mount_point,
            outside.to_string_lossy()
        );
        assert!(PathBuf::from(&created.mount_point).is_dir());
        assert!(outside.is_dir());
    }

    #[test]
    fn test_update_nonexistent() {
        let (_dir, store) = temp_store();
        let vol = VolumeConfig::new("nope", "/tmp");
        assert!(store.update(&vol).is_err());
    }

    #[test]
    fn test_prune() {
        let (_dir, store) = temp_store();
        store.create(VolumeConfig::new("unused1", "")).unwrap();
        store.create(VolumeConfig::new("unused2", "")).unwrap();

        let created = store.create(VolumeConfig::new("in_use", "")).unwrap();
        let mut updated = created;
        updated.attach("box-1");
        store.update(&updated).unwrap();

        let pruned = store.prune().unwrap();
        assert_eq!(pruned.len(), 2);
        assert!(pruned.contains(&"unused1".to_string()));
        assert!(pruned.contains(&"unused2".to_string()));

        // in_use should remain
        let remaining = store.list().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].name, "in_use");
    }

    #[test]
    fn test_atomic_write() {
        let (_dir, store) = temp_store();
        store.create(VolumeConfig::new("mydata", "")).unwrap();

        let data = std::fs::read_to_string(store.path()).unwrap();
        let _: serde_json::Value = serde_json::from_str(&data).unwrap();

        let tmp = store.path().with_extension("json.tmp");
        assert!(!tmp.exists());
    }

    #[test]
    fn test_creates_parent_directory() {
        let dir = tempfile::tempdir().unwrap();
        let store = VolumeStore::new(
            dir.path().join("subdir").join("volumes.json"),
            dir.path().join("subdir").join("volumes"),
        );

        store.create(VolumeConfig::new("mydata", "")).unwrap();
        assert!(store.path().exists());
    }

    #[test]
    fn test_remove_cleans_up_directory() {
        let (_dir, store) = temp_store();
        let created = store.create(VolumeConfig::new("mydata", "")).unwrap();
        let vol_dir = PathBuf::from(&created.mount_point);
        assert!(vol_dir.exists());

        store.remove("mydata", false).unwrap();
        assert!(!vol_dir.exists());
    }

    #[test]
    fn test_get_or_create_is_idempotent() {
        let (_dir, store) = temp_store();
        let first = store
            .get_or_create(VolumeConfig::new("shared", ""))
            .unwrap();
        let second = store
            .get_or_create(VolumeConfig::new("shared", ""))
            .unwrap();
        assert_eq!(first.mount_point, second.mount_point);
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn test_get_or_create_preserves_existing_config() {
        let (_dir, store) = temp_store();
        let created = store
            .get_or_create(VolumeConfig::with_size_limit("shared", "", 4096))
            .unwrap();
        let mut updated = created;
        updated.attach("box-1");
        store.update(&updated).unwrap();

        let reused = store
            .get_or_create(VolumeConfig::with_size_limit("shared", "/ignored", 8192))
            .unwrap();

        assert_eq!(reused.size_limit, 4096);
        assert_eq!(reused.in_use_by, vec!["box-1"]);
        assert!(PathBuf::from(&reused.mount_point).exists());
    }

    #[test]
    fn claim_anonymous_is_idempotent_for_the_exact_owner() {
        let (_dir, store) = temp_store();

        let (first, first_created) = store.claim_anonymous("anon_owned", "box-owner").unwrap();
        let (second, second_created) = store.claim_anonymous("anon_owned", "box-owner").unwrap();

        assert!(first_created);
        assert!(!second_created);
        assert_eq!(first.mount_point, second.mount_point);
        assert_eq!(second.in_use_by, vec!["box-owner"]);
        assert_eq!(
            second.labels.get(ANONYMOUS_LABEL).map(String::as_str),
            Some("true")
        );
        assert_eq!(
            second.labels.get(ANONYMOUS_KIND_LABEL).map(String::as_str),
            Some(ANONYMOUS_KIND)
        );
        assert_eq!(
            second.labels.get(ANONYMOUS_OWNER_LABEL).map(String::as_str),
            Some("box-owner")
        );
    }

    #[test]
    fn claim_anonymous_upgrades_an_exact_owner_legacy_volume() {
        let (_dir, store) = temp_store();
        let mut legacy = VolumeConfig::new("anon_legacy", "");
        legacy
            .labels
            .insert(ANONYMOUS_LABEL.to_string(), "true".to_string());
        legacy.attach("box-owner");
        store.create(legacy).unwrap();

        let (claimed, created) = store.claim_anonymous("anon_legacy", "box-owner").unwrap();

        assert!(!created);
        assert_eq!(
            claimed.labels.get(ANONYMOUS_KIND_LABEL).map(String::as_str),
            Some(ANONYMOUS_KIND)
        );
        assert_eq!(
            claimed
                .labels
                .get(ANONYMOUS_OWNER_LABEL)
                .map(String::as_str),
            Some("box-owner")
        );
    }

    #[test]
    fn claim_anonymous_rejects_named_volume_collision_without_mutation() {
        let (_dir, store) = temp_store();
        let named = store
            .create(VolumeConfig::new("anon_collision", ""))
            .unwrap();

        let error = store
            .claim_anonymous("anon_collision", "box-owner")
            .expect_err("a named volume must never become anonymously owned");

        assert!(error.to_string().contains("not an anonymous volume"));
        assert_eq!(
            store.get("anon_collision").unwrap().unwrap().in_use_by,
            named.in_use_by
        );
    }

    #[test]
    fn claim_anonymous_rejects_a_different_owner_without_mutation() {
        let (_dir, store) = temp_store();
        store.claim_anonymous("anon_owned", "box-one").unwrap();

        let error = store
            .claim_anonymous("anon_owned", "box-two")
            .expect_err("anonymous volumes have exactly one Box owner");

        assert!(error.to_string().contains("owned by another execution"));
        assert_eq!(
            store.get("anon_owned").unwrap().unwrap().in_use_by,
            vec!["box-one"]
        );
    }

    #[test]
    fn claim_anonymous_rejects_unsafe_identity_before_creating_a_directory() {
        let (dir, store) = temp_store();

        let error = store
            .claim_anonymous("../escaped", "box-owner")
            .expect_err("managed identities may not escape the volume root");

        assert!(error.to_string().contains("invalid anonymous volume name"));
        assert!(!dir.path().join("escaped").exists());
        assert!(store.load().unwrap().is_empty());
    }

    #[test]
    fn concurrent_anonymous_claims_publish_one_exact_owner() {
        use std::sync::Arc;
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(VolumeStore::new(
            dir.path().join("volumes.json"),
            dir.path().join("volumes"),
        ));
        let handles = (0..16)
            .map(|_| {
                let store = Arc::clone(&store);
                thread::spawn(move || {
                    store
                        .claim_anonymous("anon_concurrent", "box-owner")
                        .unwrap()
                        .1
                })
            })
            .collect::<Vec<_>>();

        let created = handles
            .into_iter()
            .map(|handle| usize::from(handle.join().unwrap()))
            .sum::<usize>();
        let claimed = store.get("anon_concurrent").unwrap().unwrap();

        assert_eq!(created, 1);
        assert_eq!(claimed.in_use_by, vec!["box-owner"]);
        assert_eq!(store.list().unwrap().len(), 1);
    }

    #[test]
    fn remove_anonymous_requires_and_removes_the_exact_owner() {
        let (_dir, store) = temp_store();
        let (claimed, _) = store.claim_anonymous("anon_owned", "box-owner").unwrap();

        let removed = store.remove_anonymous("anon_owned", "box-owner").unwrap();

        assert!(removed);
        assert!(store.get("anon_owned").unwrap().is_none());
        assert!(!PathBuf::from(claimed.mount_point).exists());
    }

    #[test]
    fn remove_anonymous_rejects_named_and_different_owner_collisions() {
        let (_dir, store) = temp_store();
        let named = store.create(VolumeConfig::new("anon_named", "")).unwrap();
        store.claim_anonymous("anon_owned", "box-one").unwrap();

        let named_error = store
            .remove_anonymous("anon_named", "box-one")
            .expect_err("named volume metadata must fail closed");
        let owner_error = store
            .remove_anonymous("anon_owned", "box-two")
            .expect_err("another owner must fail closed");

        assert!(named_error.to_string().contains("not an anonymous volume"));
        assert!(owner_error
            .to_string()
            .contains("owned by another execution"));
        assert!(PathBuf::from(named.mount_point).exists());
        assert!(store.get("anon_owned").unwrap().is_some());
    }

    #[cfg(windows)]
    #[test]
    fn remove_anonymous_rejects_a_name_that_differs_only_by_case() {
        let (_dir, store) = temp_store();
        let (claimed, _) = store.claim_anonymous("anon_data", "box-owner").unwrap();
        let marker = PathBuf::from(&claimed.mount_point).join("keep.txt");
        std::fs::write(&marker, b"keep").unwrap();

        let error = store
            .remove_anonymous("anon_Data", "box-owner")
            .expect_err("a case alias must not delete the existing volume");

        assert!(error.to_string().contains("same directory"), "{error}");
        assert!(store.get("anon_data").unwrap().is_some());
        assert!(marker.is_file());
    }

    #[cfg(windows)]
    #[test]
    fn claim_anonymous_rejects_a_stored_same_directory_alias() {
        use a3s_box_core::rootfs_metadata::{RootfsEntryKind, RootfsMetadataEntry};
        use a3s_box_core::volume_posix::{
            read_volume_posix_sidecar, write_volume_posix_sidecar, VolumePosixSidecar,
        };

        let (dir, store) = temp_store();
        let mut named = store.create(VolumeConfig::new("anon_data", "")).unwrap();
        let volume_dir = PathBuf::from(&named.mount_point);
        std::fs::write(volume_dir.join("keep.txt"), b"kept").unwrap();
        write_volume_posix_sidecar(
            &volume_dir,
            &VolumePosixSidecar::new(vec![RootfsMetadataEntry {
                path_base64: "Lg==".to_string(),
                kind: RootfsEntryKind::Directory,
                mode: 0o640,
                uid: 1000,
                gid: 1000,
                mtime: 0,
                size: 0,
                link_target_base64: None,
            }]),
        )
        .unwrap();
        named.attach("box-1");
        store.update(&named).unwrap();

        let alias_mount = dir.path().join("volumes").join("anon_Data");
        let mut alias = VolumeConfig::new("anon_Data", &alias_mount.to_string_lossy());
        alias
            .labels
            .insert(ANONYMOUS_LABEL.to_string(), "true".to_string());
        alias
            .labels
            .insert(ANONYMOUS_KIND_LABEL.to_string(), ANONYMOUS_KIND.to_string());
        alias
            .labels
            .insert(ANONYMOUS_OWNER_LABEL.to_string(), "box-owner".to_string());
        alias.attach("box-owner");
        let mut volumes = store.load().unwrap();
        volumes.insert("anon_Data".to_string(), alias);
        store.save(&volumes).unwrap();

        let error = store
            .claim_anonymous("anon_Data", "box-owner")
            .expect_err("a stored anonymous alias cannot claim another volume's directory");
        assert!(error.to_string().contains("same directory"), "{error}");
        assert!(store.get("anon_Data").unwrap().unwrap().in_use_by == vec!["box-owner"]);
        assert_eq!(
            store.get("anon_data").unwrap().unwrap().in_use_by,
            vec!["box-1".to_string()]
        );

        let removed = store
            .remove_anonymous("anon_Data", "box-owner")
            .expect("removing the alias drops the catalog entry");
        assert!(removed);
        assert!(store.get("anon_Data").unwrap().is_none());
        assert!(store.get("anon_data").unwrap().unwrap().is_in_use());
        assert_eq!(std::fs::read(volume_dir.join("keep.txt")).unwrap(), b"kept");
        assert_eq!(
            read_volume_posix_sidecar(&volume_dir)
                .unwrap()
                .expect("named sidecar")
                .entries[0]
                .mode,
            0o640
        );
    }

    #[test]
    fn remove_anonymous_cleans_an_unpublished_deterministic_directory() {
        let (_dir, store) = temp_store();
        let orphan = store.volume_dir("anon_unpublished");
        std::fs::create_dir_all(&orphan).unwrap();
        std::fs::write(orphan.join("partial"), b"partial").unwrap();

        let removed = store
            .remove_anonymous("anon_unpublished", "box-owner")
            .unwrap();

        assert!(!removed);
        assert!(!orphan.exists());
        assert!(store.get("anon_unpublished").unwrap().is_none());
    }

    #[test]
    fn reclaim_anonymous_recreates_a_missing_owned_directory() {
        let (_dir, store) = temp_store();
        let (claimed, _) = store.claim_anonymous("anon_owned", "box-owner").unwrap();
        std::fs::remove_dir_all(&claimed.mount_point).unwrap();

        let (reclaimed, created) = store.claim_anonymous("anon_owned", "box-owner").unwrap();

        assert!(!created);
        assert!(PathBuf::from(reclaimed.mount_point).is_dir());
    }

    #[test]
    fn test_modify_missing_returns_false() {
        let (_dir, store) = temp_store();
        assert!(!store.modify("nope", |c| c.attach("box-1")).unwrap());
    }

    #[test]
    fn test_modify_existing_returns_true_and_persists() {
        let (_dir, store) = temp_store();
        store.create(VolumeConfig::new("shared", "")).unwrap();

        let modified = store.modify("shared", |c| c.attach("box-1")).unwrap();

        assert!(modified);
        assert_eq!(
            store.get("shared").unwrap().unwrap().in_use_by,
            vec!["box-1"]
        );
    }

    #[test]
    fn test_volume_dir_joins_name_under_base_directory() {
        let (dir, store) = temp_store();
        assert_eq!(
            store.volume_dir("mydata"),
            dir.path().join("volumes").join("mydata")
        );
    }

    // The advisory lock is per-open-file-description, so separate
    // FileLock::acquire calls serialize even across threads in one process —
    // which is exactly what lets this exercise the lost-update fix in-process.
    #[test]
    fn concurrent_attaches_accumulate_without_lost_update() {
        use std::sync::Arc;
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(VolumeStore::new(
            dir.path().join("volumes.json"),
            dir.path().join("volumes"),
        ));
        store.create(VolumeConfig::new("shared", "")).unwrap();

        let n = 16;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let store = Arc::clone(&store);
                thread::spawn(move || {
                    store
                        .modify("shared", |c| c.attach(&format!("box-{i}")))
                        .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        let cfg = store.get("shared").unwrap().unwrap();
        assert_eq!(
            cfg.in_use_by.len(),
            n,
            "every concurrent attach must persist (no lost update): {:?}",
            cfg.in_use_by
        );
        for i in 0..n {
            assert!(cfg.in_use_by.contains(&format!("box-{i}")));
        }
    }

    #[test]
    fn concurrent_creates_persist_every_volume() {
        use std::sync::Arc;
        use std::thread;

        let dir = tempfile::tempdir().unwrap();
        let store = Arc::new(VolumeStore::new(
            dir.path().join("volumes.json"),
            dir.path().join("volumes"),
        ));

        let n = 16;
        let handles: Vec<_> = (0..n)
            .map(|i| {
                let store = Arc::clone(&store);
                thread::spawn(move || {
                    store
                        .create(VolumeConfig::new(&format!("vol-{i}"), ""))
                        .unwrap();
                })
            })
            .collect();
        for h in handles {
            h.join().unwrap();
        }

        assert_eq!(
            store.list().unwrap().len(),
            n,
            "every concurrent create must persist (no lost update)"
        );
    }

    #[test]
    fn corrupt_volumes_file_is_quarantined_not_fatal() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("volumes.json");
        std::fs::write(&path, "{ not valid json").unwrap();
        let store = VolumeStore::new(path.clone(), dir.path().join("volumes"));

        // load() must succeed (empty) instead of erroring every volume op.
        assert!(store.load().unwrap().is_empty());
        let quarantined = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("volumes.json.corrupt-")
            });
        assert!(quarantined, "corrupt volumes.json must be quarantined");
    }
}
