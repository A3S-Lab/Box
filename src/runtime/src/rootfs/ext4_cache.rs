//! Immutable cache for guest-native ext4 base images.
//!
//! Cache entries are content-addressed by the resolved OCI manifest, guest
//! platform, exact guest-init binary, ext4 writer contract, and disk capacity.
//! The cached disk is never attached writable: every box receives a private
//! copy-on-write clone before the VMM can mutate it.

use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::os::fd::AsRawFd;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::ext4::{
    publish_ext4_artifact, Ext4Artifact, Ext4ArtifactOptions, EXT4_ARTIFACT_SCHEMA, EXT4_BUILDER_ID,
};
use super::ext4_artifact::open_ext4_artifact;

pub const EXT4_CACHE_SCHEMA: &str = "a3s.box.rootfs-ext4-cache.v1";
const CACHE_MANIFEST_NAME: &str = "cache.json";
const ARTIFACT_DIRECTORY_NAME: &str = "artifact";
pub(super) const CACHE_STAGING_PREFIX: &str = ".a3s-rootfs-ext4-cache-";
pub(super) const CLONE_STAGING_PREFIX: &str = ".a3s-rootfs-ext4-clone-";
const MAX_CACHE_MANIFEST_BYTES: u64 = 64 * 1024;
const MAX_IDENTITY_FIELD_BYTES: usize = 512;
const COPY_BUFFER_BYTES: usize = 1024 * 1024;

/// Immutable source identity for a reusable ext4 base image.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Ext4CacheIdentity {
    pub schema: String,
    pub oci_manifest_digest: String,
    pub platform: String,
    pub guest_init_sha256: String,
}

impl Ext4CacheIdentity {
    pub fn new(
        oci_manifest_digest: impl Into<String>,
        platform: impl Into<String>,
        guest_init_sha256: impl Into<String>,
    ) -> Result<Self> {
        let identity = Self {
            schema: EXT4_CACHE_SCHEMA.to_string(),
            oci_manifest_digest: oci_manifest_digest.into(),
            platform: platform.into(),
            guest_init_sha256: guest_init_sha256.into(),
        };
        identity.validate()?;
        Ok(identity)
    }

    fn validate(&self) -> Result<()> {
        if self.schema != EXT4_CACHE_SCHEMA {
            return Err(cache_error("unsupported ext4 cache identity schema"));
        }
        validate_identity_field("OCI manifest digest", &self.oci_manifest_digest)?;
        validate_identity_field("platform", &self.platform)?;
        if !self.platform.starts_with("linux/") || self.platform.matches('/').count() > 2 {
            return Err(cache_error(
                "ext4 cache platform must be a Linux OCI platform",
            ));
        }
        let digest = self
            .guest_init_sha256
            .strip_prefix("sha256:")
            .ok_or_else(|| cache_error("guest-init identity must use sha256"))?;
        if digest.len() != 64
            || !digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        {
            return Err(cache_error(
                "guest-init identity must be a lowercase sha256 digest",
            ));
        }
        Ok(())
    }

    fn key(&self, capacity_bytes: u64) -> String {
        let mut hasher = Sha256::new();
        hash_field(&mut hasher, EXT4_CACHE_SCHEMA.as_bytes());
        hash_field(&mut hasher, EXT4_ARTIFACT_SCHEMA.as_bytes());
        hash_field(&mut hasher, EXT4_BUILDER_ID.as_bytes());
        hash_field(&mut hasher, b"raw-ext4");
        hash_field(&mut hasher, &capacity_bytes.to_le_bytes());
        hash_field(&mut hasher, self.oci_manifest_digest.as_bytes());
        hash_field(&mut hasher, self.platform.as_bytes());
        hash_field(&mut hasher, self.guest_init_sha256.as_bytes());
        hex::encode(hasher.finalize())
    }
}

/// Bounded immutable cache used by the experimental guest-native provider.
pub struct Ext4ArtifactCache {
    root: PathBuf,
    max_entries: usize,
    max_allocated_bytes: u64,
}

impl Ext4ArtifactCache {
    pub fn new(root: impl Into<PathBuf>, max_entries: usize, max_allocated_bytes: u64) -> Self {
        Self {
            root: root.into(),
            max_entries,
            max_allocated_bytes,
        }
    }

    /// Materialize a private writable generation from an immutable cache base.
    ///
    /// The cache lock covers lookup, first publication, cloning, and pruning.
    /// This intentionally favors a simple crash-safe protocol over parallel
    /// cache mutation while the provider remains experimental.
    pub fn materialize(
        &self,
        source: &Path,
        destination: &Path,
        disk_mib: u32,
        identity: &Ext4CacheIdentity,
    ) -> Result<Ext4Artifact> {
        self.materialize_with(destination, disk_mib, identity, |destination, options| {
            publish_ext4_artifact(source, destination, options)
        })
    }

    /// Materialize through a publisher that owns its source representation.
    /// Cache hits never invoke the publisher, so OCI layers do not need to be
    /// decoded or spooled when an immutable ext4 base already exists.
    pub(super) fn materialize_with<F>(
        &self,
        destination: &Path,
        disk_mib: u32,
        identity: &Ext4CacheIdentity,
        publish: F,
    ) -> Result<Ext4Artifact>
    where
        F: FnOnce(&Path, Ext4ArtifactOptions) -> Result<Ext4Artifact>,
    {
        identity.validate()?;
        let provisional = Ext4ArtifactOptions::from_disk_mib(disk_mib, [0; 16])?;
        let key = identity.key(provisional.capacity_bytes);
        let options = Ext4ArtifactOptions::from_disk_mib(
            disk_mib,
            deterministic_uuid(&key).ok_or_else(|| cache_error("invalid ext4 cache key"))?,
        )?;

        std::fs::create_dir_all(&self.root).map_err(|error| {
            cache_error(format!(
                "failed to create ext4 cache {}: {error}",
                self.root.display()
            ))
        })?;
        validate_plain_directory(&self.root, "ext4 cache root")?;
        let lock_target = self.root.join(".cache-index");
        let _lock = crate::file_lock::FileLock::acquire(&lock_target).map_err(|error| {
            cache_error(format!(
                "failed to lock ext4 cache {}: {error}",
                self.root.display()
            ))
        })?;
        remove_stale_cache_staging(&self.root)?;

        let cache_entry = self.root.join(&key);
        let mut publish = Some(publish);
        let cached = match std::fs::symlink_metadata(&cache_entry) {
            Ok(_) => match open_cache_entry(&cache_entry, &key, identity, options) {
                Ok(artifact) => artifact,
                Err(error) => {
                    tracing::warn!(
                        path = %cache_entry.display(),
                        %error,
                        "Discarding invalid ext4 cache entry before rebuilding"
                    );
                    remove_cache_entry(&cache_entry)?;
                    publish_cache_entry_with(
                        &self.root,
                        &cache_entry,
                        &key,
                        identity,
                        options,
                        publish.take().expect("ext4 publisher is called once"),
                    )?
                }
            },
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => publish_cache_entry_with(
                &self.root,
                &cache_entry,
                &key,
                identity,
                options,
                publish.take().expect("ext4 publisher is called once"),
            )?,
            Err(error) => {
                return Err(cache_error(format!(
                    "failed to inspect ext4 cache entry {}: {error}",
                    cache_entry.display()
                )))
            }
        };
        touch_cache_entry(&cache_entry);
        let private = clone_artifact(&cached, destination)?;
        self.prune(&key)?;
        Ok(private)
    }

    fn prune(&self, protected_key: &str) -> Result<()> {
        let mut entries = Vec::new();
        for item in std::fs::read_dir(&self.root).map_err(BoxError::IoError)? {
            let item = item.map_err(BoxError::IoError)?;
            let name = item.file_name();
            let Some(key) = name.to_str() else {
                continue;
            };
            if !is_cache_key(key) || !item.file_type().map_err(BoxError::IoError)?.is_dir() {
                continue;
            }
            let path = item.path();
            let accessed = std::fs::metadata(path.join(CACHE_MANIFEST_NAME))
                .and_then(|metadata| metadata.modified())
                .unwrap_or(std::time::UNIX_EPOCH);
            entries.push(CacheEntryUsage {
                allocated_bytes: allocated_bytes(&path)?,
                path,
                key: key.to_string(),
                accessed,
            });
        }
        entries.sort_by_key(|entry| entry.accessed);
        let mut count = entries.len();
        let mut bytes = entries
            .iter()
            .map(|entry| entry.allocated_bytes)
            .sum::<u64>();
        for entry in entries {
            if count <= self.max_entries && bytes <= self.max_allocated_bytes {
                break;
            }
            if entry.key == protected_key {
                continue;
            }
            validate_plain_directory(&entry.path, "ext4 cache entry")?;
            match std::fs::remove_dir_all(&entry.path) {
                Ok(()) => {}
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
                Err(error) => {
                    return Err(cache_error(format!(
                        "failed to prune ext4 cache entry {}: {error}",
                        entry.path.display()
                    )))
                }
            }
            count = count.saturating_sub(1);
            bytes = bytes.saturating_sub(entry.allocated_bytes);
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Ext4CacheManifest {
    schema: String,
    key: String,
    identity: Ext4CacheIdentity,
    artifact_schema: String,
    builder: String,
    format: String,
    capacity_bytes: u64,
    fs_uuid: String,
    sparse_sha256: String,
}

struct CacheEntryUsage {
    path: PathBuf,
    key: String,
    accessed: std::time::SystemTime,
    allocated_bytes: u64,
}

fn publish_cache_entry_with<F>(
    root: &Path,
    destination: &Path,
    key: &str,
    identity: &Ext4CacheIdentity,
    options: Ext4ArtifactOptions,
    publish: F,
) -> Result<Ext4Artifact>
where
    F: FnOnce(&Path, Ext4ArtifactOptions) -> Result<Ext4Artifact>,
{
    let temporary = tempfile::Builder::new()
        .prefix(CACHE_STAGING_PREFIX)
        .tempdir_in(root)
        .map_err(|error| cache_error(format!("failed to stage ext4 cache entry: {error}")))?;
    let artifact_directory = temporary.path().join(ARTIFACT_DIRECTORY_NAME);
    let artifact = publish(&artifact_directory, options)?;
    if artifact.directory != artifact_directory {
        return Err(cache_error(
            "ext4 cache publisher returned an artifact outside its assigned directory",
        ));
    }
    let manifest = Ext4CacheManifest {
        schema: EXT4_CACHE_SCHEMA.to_string(),
        key: key.to_string(),
        identity: identity.clone(),
        artifact_schema: artifact.manifest.schema.clone(),
        builder: artifact.manifest.builder.clone(),
        format: artifact.manifest.format.clone(),
        capacity_bytes: artifact.manifest.capacity_bytes,
        fs_uuid: artifact.manifest.fs_uuid.clone(),
        sparse_sha256: sparse_sha256(&artifact.disk, artifact.manifest.capacity_bytes)?,
    };
    write_cache_manifest(&temporary.path().join(CACHE_MANIFEST_NAME), &manifest)?;
    sync_directory(temporary.path())?;

    let temporary_path = temporary.keep();
    if let Err(error) = std::fs::rename(&temporary_path, destination) {
        let _ = std::fs::remove_dir_all(&temporary_path);
        return Err(cache_error(format!(
            "failed to publish ext4 cache entry {}: {error}",
            destination.display()
        )));
    }
    sync_directory(root)?;
    open_cache_entry(destination, key, identity, options)
}

fn open_cache_entry(
    directory: &Path,
    key: &str,
    identity: &Ext4CacheIdentity,
    options: Ext4ArtifactOptions,
) -> Result<Ext4Artifact> {
    validate_plain_directory(directory, "ext4 cache entry")?;
    let manifest_path = directory.join(CACHE_MANIFEST_NAME);
    let manifest: Ext4CacheManifest = read_bounded_json(&manifest_path)?;
    let expected = Ext4CacheManifest {
        schema: EXT4_CACHE_SCHEMA.to_string(),
        key: key.to_string(),
        identity: identity.clone(),
        artifact_schema: EXT4_ARTIFACT_SCHEMA.to_string(),
        builder: EXT4_BUILDER_ID.to_string(),
        format: "raw-ext4".to_string(),
        capacity_bytes: options.capacity_bytes,
        fs_uuid: hex::encode(options.fs_uuid),
        sparse_sha256: manifest.sparse_sha256.clone(),
    };
    if manifest != expected || !is_sha256_hex(&manifest.sparse_sha256) {
        return Err(cache_error(format!(
            "ext4 cache manifest does not match requested identity at {}",
            manifest_path.display()
        )));
    }
    let artifact = open_ext4_artifact(&directory.join(ARTIFACT_DIRECTORY_NAME))?;
    if artifact.manifest.capacity_bytes != manifest.capacity_bytes
        || artifact.manifest.fs_uuid != manifest.fs_uuid
        || artifact.manifest.schema != manifest.artifact_schema
        || artifact.manifest.builder != manifest.builder
        || artifact.manifest.format != manifest.format
    {
        return Err(cache_error(format!(
            "ext4 cache artifact disagrees with {}",
            manifest_path.display()
        )));
    }
    let actual_digest = sparse_sha256(&artifact.disk, artifact.manifest.capacity_bytes)?;
    if actual_digest != manifest.sparse_sha256 {
        return Err(cache_error(format!(
            "ext4 cache artifact integrity mismatch at {}",
            artifact.disk.display()
        )));
    }
    Ok(artifact)
}

fn clone_artifact(source: &Ext4Artifact, destination: &Path) -> Result<Ext4Artifact> {
    let parent = destination.parent().ok_or_else(|| {
        cache_error(format!(
            "ext4 clone destination has no parent: {}",
            destination.display()
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(BoxError::IoError)?;
    match std::fs::symlink_metadata(destination) {
        Ok(_) => {
            return Err(cache_error(format!(
                "ext4 clone destination already exists: {}",
                destination.display()
            )))
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(BoxError::IoError(error)),
    }
    let temporary = tempfile::Builder::new()
        .prefix(CLONE_STAGING_PREFIX)
        .tempdir_in(parent)
        .map_err(BoxError::IoError)?;
    let disk = temporary.path().join("rootfs.ext4");
    clone_disk(&source.disk, &disk)?;
    File::open(&disk)
        .and_then(|file| file.sync_all())
        .map_err(BoxError::IoError)?;
    let manifest_path = temporary.path().join("artifact.json");
    let bytes = serde_json::to_vec_pretty(&source.manifest)
        .map_err(|error| cache_error(format!("failed to encode cloned ext4 manifest: {error}")))?;
    let mut manifest_file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&manifest_path)
        .map_err(BoxError::IoError)?;
    manifest_file.write_all(&bytes).map_err(BoxError::IoError)?;
    manifest_file.sync_all().map_err(BoxError::IoError)?;
    open_ext4_artifact(temporary.path())?;
    sync_directory(temporary.path())?;

    let temporary_path = temporary.keep();
    if let Err(error) = std::fs::rename(&temporary_path, destination) {
        let _ = std::fs::remove_dir_all(&temporary_path);
        return Err(cache_error(format!(
            "failed to publish private ext4 clone {}: {error}",
            destination.display()
        )));
    }
    sync_directory(parent)?;
    open_ext4_artifact(destination)
}

#[cfg(target_os = "macos")]
fn clone_disk(source: &Path, destination: &Path) -> Result<()> {
    let output = std::process::Command::new("cp")
        .arg("-c")
        .arg(source)
        .arg(destination)
        .output()
        .map_err(|error| cache_error(format!("failed to start APFS clone: {error}")))?;
    if !output.status.success() {
        return Err(cache_error(format!(
            "failed to clone immutable ext4 base {}: {}",
            source.display(),
            String::from_utf8_lossy(&output.stderr).trim()
        )));
    }
    Ok(())
}

#[cfg(not(target_os = "macos"))]
fn clone_disk(source: &Path, destination: &Path) -> Result<()> {
    std::fs::copy(source, destination)
        .map(|_| ())
        .map_err(BoxError::IoError)
}

fn write_cache_manifest(path: &Path, manifest: &Ext4CacheManifest) -> Result<()> {
    let bytes = serde_json::to_vec_pretty(manifest)
        .map_err(|error| cache_error(format!("failed to encode ext4 cache manifest: {error}")))?;
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(BoxError::IoError)?;
    file.write_all(&bytes).map_err(BoxError::IoError)?;
    file.sync_all().map_err(BoxError::IoError)
}

fn read_bounded_json<T: for<'de> Deserialize<'de>>(path: &Path) -> Result<T> {
    let metadata = std::fs::symlink_metadata(path).map_err(BoxError::IoError)?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(cache_error(format!(
            "ext4 cache manifest is not a plain file: {}",
            path.display()
        )));
    }
    if metadata.len() > MAX_CACHE_MANIFEST_BYTES {
        return Err(cache_error(format!(
            "ext4 cache manifest exceeds {} bytes: {}",
            MAX_CACHE_MANIFEST_BYTES,
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    File::open(path)
        .and_then(|file| {
            file.take(MAX_CACHE_MANIFEST_BYTES + 1)
                .read_to_end(&mut bytes)
        })
        .map_err(BoxError::IoError)?;
    if bytes.len() as u64 > MAX_CACHE_MANIFEST_BYTES {
        return Err(cache_error(format!(
            "ext4 cache manifest grew beyond {} bytes: {}",
            MAX_CACHE_MANIFEST_BYTES,
            path.display()
        )));
    }
    serde_json::from_slice(&bytes).map_err(|error| {
        cache_error(format!(
            "invalid ext4 cache manifest {}: {error}",
            path.display()
        ))
    })
}

fn sparse_sha256(path: &Path, expected_length: u64) -> Result<String> {
    let mut file = File::open(path).map_err(BoxError::IoError)?;
    let length = file.metadata().map_err(BoxError::IoError)?.len();
    if length != expected_length {
        return Err(cache_error(format!(
            "ext4 cache disk {} has unexpected length {length}",
            path.display()
        )));
    }
    let mut hasher = Sha256::new();
    hash_field(&mut hasher, b"a3s.box.sparse-file-sha256.v1");
    hash_field(&mut hasher, &length.to_le_bytes());
    let mut offset = 0u64;
    let mut buffer = vec![0u8; COPY_BUFFER_BYTES];
    while offset < length {
        let Some(data) = seek_extent(&file, offset, libc::SEEK_DATA, path)? else {
            hash_sparse_range(&mut hasher, b'H', offset, length - offset);
            break;
        };
        if data > offset {
            hash_sparse_range(&mut hasher, b'H', offset, data - offset);
        }
        let hole = seek_extent(&file, data, libc::SEEK_HOLE, path)?.unwrap_or(length);
        if hole <= data || hole > length {
            return Err(cache_error(format!(
                "invalid sparse extent in ext4 cache disk {}",
                path.display()
            )));
        }
        hash_sparse_range(&mut hasher, b'D', data, hole - data);
        file.seek(SeekFrom::Start(data))
            .map_err(BoxError::IoError)?;
        let mut remaining = hole - data;
        while remaining > 0 {
            let requested = buffer.len().min(remaining as usize);
            let read = file
                .read(&mut buffer[..requested])
                .map_err(BoxError::IoError)?;
            if read == 0 {
                return Err(cache_error(format!(
                    "unexpected EOF in ext4 cache disk {}",
                    path.display()
                )));
            }
            hasher.update(&buffer[..read]);
            remaining -= read as u64;
        }
        offset = hole;
    }
    Ok(hex::encode(hasher.finalize()))
}

fn seek_extent(file: &File, offset: u64, whence: i32, path: &Path) -> Result<Option<u64>> {
    let offset =
        i64::try_from(offset).map_err(|_| cache_error("ext4 cache extent offset exceeds off_t"))?;
    let result = unsafe { libc::lseek(file.as_raw_fd(), offset, whence) };
    if result >= 0 {
        return Ok(Some(result as u64));
    }
    let error = std::io::Error::last_os_error();
    if error.raw_os_error() == Some(libc::ENXIO) {
        return Ok(None);
    }
    Err(cache_error(format!(
        "failed to inspect sparse extents for {}: {error}",
        path.display()
    )))
}

fn allocated_bytes(path: &Path) -> Result<u64> {
    let metadata = std::fs::symlink_metadata(path).map_err(BoxError::IoError)?;
    if metadata.file_type().is_symlink() {
        return Err(cache_error(format!(
            "symlink found inside ext4 cache entry: {}",
            path.display()
        )));
    }
    if metadata.is_file() {
        return Ok(metadata.blocks().saturating_mul(512));
    }
    if !metadata.is_dir() {
        return Err(cache_error(format!(
            "unsupported object inside ext4 cache entry: {}",
            path.display()
        )));
    }
    let mut total = metadata.blocks().saturating_mul(512);
    for entry in std::fs::read_dir(path).map_err(BoxError::IoError)? {
        total = total.saturating_add(allocated_bytes(&entry.map_err(BoxError::IoError)?.path())?);
    }
    Ok(total)
}

fn validate_plain_directory(path: &Path, label: &str) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(BoxError::IoError)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(cache_error(format!(
            "{label} is not a plain directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn remove_stale_cache_staging(root: &Path) -> Result<()> {
    for entry in std::fs::read_dir(root).map_err(BoxError::IoError)? {
        let entry = entry.map_err(BoxError::IoError)?;
        let name = entry.file_name();
        if name
            .to_str()
            .is_some_and(|name| name.starts_with(CACHE_STAGING_PREFIX))
        {
            remove_cache_entry(&entry.path())?;
        }
    }
    Ok(())
}

fn remove_cache_entry(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => {
            std::fs::remove_dir_all(path).map_err(BoxError::IoError)?;
        }
        Ok(_) => std::fs::remove_file(path).map_err(BoxError::IoError)?,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(BoxError::IoError(error)),
    }
    if let Some(parent) = path.parent() {
        sync_directory(parent)?;
    }
    Ok(())
}

fn touch_cache_entry(directory: &Path) {
    let manifest = directory.join(CACHE_MANIFEST_NAME);
    if let Ok(file) = OpenOptions::new().write(true).open(manifest) {
        let now = std::time::SystemTime::now();
        let times = std::fs::FileTimes::new()
            .set_accessed(now)
            .set_modified(now);
        let _ = file.set_times(times);
    }
}

fn deterministic_uuid(key: &str) -> Option<[u8; 16]> {
    let bytes = hex::decode(key).ok()?;
    let mut uuid = [0u8; 16];
    uuid.copy_from_slice(bytes.get(..16)?);
    uuid[6] = (uuid[6] & 0x0f) | 0x50;
    uuid[8] = (uuid[8] & 0x3f) | 0x80;
    Some(uuid)
}

fn hash_field(hasher: &mut Sha256, bytes: &[u8]) {
    hasher.update((bytes.len() as u64).to_le_bytes());
    hasher.update(bytes);
}

fn hash_sparse_range(hasher: &mut Sha256, kind: u8, offset: u64, length: u64) {
    hasher.update([kind]);
    hasher.update(offset.to_le_bytes());
    hasher.update(length.to_le_bytes());
}

fn validate_identity_field(label: &str, value: &str) -> Result<()> {
    if value.is_empty()
        || value.len() > MAX_IDENTITY_FIELD_BYTES
        || !value.bytes().all(|byte| byte.is_ascii_graphic())
    {
        return Err(cache_error(format!("invalid ext4 cache {label}")));
    }
    Ok(())
}

fn is_cache_key(value: &str) -> bool {
    value.len() == 64
        && value
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn is_sha256_hex(value: &str) -> bool {
    is_cache_key(value)
}

fn sync_directory(path: &Path) -> Result<()> {
    File::open(path)
        .and_then(|directory| directory.sync_all())
        .map_err(BoxError::IoError)
}

fn cache_error(message: impl Into<String>) -> BoxError {
    BoxError::CacheError(message.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn identity(name: &str) -> Ext4CacheIdentity {
        Ext4CacheIdentity::new(
            format!("sha256:{}", hex::encode(Sha256::digest(name.as_bytes()))),
            "linux/arm64",
            format!("sha256:{}", "42".repeat(32)),
        )
        .unwrap()
    }

    fn source(root: &Path, value: &[u8]) {
        std::fs::create_dir_all(root.join("etc")).unwrap();
        std::fs::write(root.join("etc/value"), value).unwrap();
    }

    #[test]
    fn cache_key_covers_every_immutable_input() {
        let first = identity("first");
        let second = identity("second");
        assert_ne!(first.key(16 * 1024 * 1024), second.key(16 * 1024 * 1024));
        assert_ne!(first.key(16 * 1024 * 1024), first.key(32 * 1024 * 1024));
        let changed_init = Ext4CacheIdentity::new(
            first.oci_manifest_digest.clone(),
            first.platform.clone(),
            format!("sha256:{}", "24".repeat(32)),
        )
        .unwrap();
        assert_ne!(
            first.key(16 * 1024 * 1024),
            changed_init.key(16 * 1024 * 1024)
        );
    }

    #[test]
    fn cache_hit_materializes_without_source_tree() {
        let temporary = tempfile::tempdir().unwrap();
        let source_path = temporary.path().join("source");
        source(&source_path, b"cached");
        let cache = Ext4ArtifactCache::new(temporary.path().join("cache"), 4, u64::MAX);
        let first = cache
            .materialize(
                &source_path,
                &temporary.path().join("first"),
                16,
                &identity("image"),
            )
            .unwrap();
        std::fs::remove_dir_all(&source_path).unwrap();
        let second = cache
            .materialize(
                &source_path,
                &temporary.path().join("second"),
                16,
                &identity("image"),
            )
            .unwrap();
        assert_eq!(first.manifest, second.manifest);
        assert!(second.disk.is_file());
    }

    #[test]
    fn cache_hit_does_not_invoke_source_publisher() {
        let temporary = tempfile::tempdir().unwrap();
        let source_path = temporary.path().join("source");
        source(&source_path, b"cached");
        let cache = Ext4ArtifactCache::new(temporary.path().join("cache"), 4, u64::MAX);
        let identity = identity("direct-image");
        cache
            .materialize(&source_path, &temporary.path().join("first"), 16, &identity)
            .unwrap();

        let second = cache
            .materialize_with(
                &temporary.path().join("second"),
                16,
                &identity,
                |_destination, _options| {
                    panic!("cache hit must not decode OCI layers");
                },
            )
            .unwrap();
        let filesystem = mkext4::reader::Fs::open(File::open(second.disk).unwrap()).unwrap();
        let value = filesystem.resolve("/etc/value").unwrap();
        assert_eq!(filesystem.read_file(value).unwrap(), b"cached");
    }

    #[test]
    fn cache_lock_reclaims_crash_staging_without_following_links() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let source_path = temporary.path().join("source");
        source(&source_path, b"cached");
        let cache_root = temporary.path().join("cache");
        std::fs::create_dir(&cache_root).unwrap();
        let stale_directory = cache_root.join(format!("{CACHE_STAGING_PREFIX}directory"));
        std::fs::create_dir(&stale_directory).unwrap();
        std::fs::write(stale_directory.join("partial"), b"partial").unwrap();
        let outside = temporary.path().join("outside");
        std::fs::write(&outside, b"keep").unwrap();
        let stale_link = cache_root.join(format!("{CACHE_STAGING_PREFIX}link"));
        symlink(&outside, &stale_link).unwrap();
        let cache = Ext4ArtifactCache::new(&cache_root, 4, u64::MAX);

        cache
            .materialize(
                &source_path,
                &temporary.path().join("box"),
                16,
                &identity("image"),
            )
            .unwrap();

        assert!(!stale_directory.exists());
        assert!(std::fs::symlink_metadata(&stale_link).is_err());
        assert_eq!(std::fs::read(outside).unwrap(), b"keep");
    }

    #[test]
    fn corrupted_cache_entry_is_never_cloned_and_is_rebuilt() {
        let temporary = tempfile::tempdir().unwrap();
        let source_path = temporary.path().join("source");
        source(&source_path, b"cached");
        let cache_root = temporary.path().join("cache");
        let cache = Ext4ArtifactCache::new(&cache_root, 4, u64::MAX);
        let identity = identity("image");
        cache
            .materialize(&source_path, &temporary.path().join("first"), 16, &identity)
            .unwrap();
        let key = identity.key(16 * 1024 * 1024);
        let disk = cache_root
            .join(key)
            .join(ARTIFACT_DIRECTORY_NAME)
            .join("rootfs.ext4");
        let mut file = OpenOptions::new().write(true).open(disk).unwrap();
        file.seek(SeekFrom::Start(4096)).unwrap();
        file.write_all(b"corrupt").unwrap();
        file.sync_all().unwrap();

        let repaired = cache
            .materialize(
                &source_path,
                &temporary.path().join("second"),
                16,
                &identity,
            )
            .unwrap();
        let filesystem = mkext4::reader::Fs::open(File::open(repaired.disk).unwrap()).unwrap();
        let value = filesystem.resolve("/etc/value").unwrap();
        assert_eq!(filesystem.read_file(value).unwrap(), b"cached");
    }

    #[test]
    fn pruning_counts_allocated_sparse_bytes_and_protects_current_entry() {
        let temporary = tempfile::tempdir().unwrap();
        let source_path = temporary.path().join("source");
        source(&source_path, b"cached");
        let cache_root = temporary.path().join("cache");
        let cache = Ext4ArtifactCache::new(&cache_root, 1, u64::MAX);
        for name in ["first", "second"] {
            cache
                .materialize(
                    &source_path,
                    &temporary.path().join(format!("box-{name}")),
                    16,
                    &identity(name),
                )
                .unwrap();
        }
        let entries = std::fs::read_dir(&cache_root)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .filter(|entry| {
                entry.file_type().is_ok_and(|kind| kind.is_dir())
                    && entry.file_name().to_str().is_some_and(is_cache_key)
            })
            .collect::<Vec<_>>();
        assert_eq!(entries.len(), 1);
        assert_eq!(
            entries[0].file_name().to_str().unwrap(),
            identity("second").key(16 * 1024 * 1024)
        );
    }
}
