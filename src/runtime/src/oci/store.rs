//! Disk-based OCI image store with LRU eviction.
//!
//! Stores pulled OCI images on disk with an in-memory index backed by
//! a persistent `index.json` file. Supports LRU eviction when the store
//! exceeds a configured maximum size.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::{ImageStoreBackend, StoredImage};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;

mod blob_reuse;

/// Per-process counter for unique staging-dir names in `put`.
static PUT_SEQ: AtomicU64 = AtomicU64::new(0);

/// Persistent index stored as JSON on disk.
#[derive(Debug, Default, Serialize, Deserialize)]
struct StoreIndex {
    images: Vec<StoredImage>,
}

/// Disk-based image store with in-memory index and LRU eviction.
pub struct ImageStore {
    /// Root directory for image storage
    store_dir: PathBuf,
    /// In-memory index: reference → StoredImage
    index: Arc<RwLock<HashMap<String, StoredImage>>>,
    /// Maximum total size in bytes
    max_size_bytes: u64,
}

fn state_dir_hint() -> &'static str {
    "Set A3S_HOME to a writable directory to change the A3S Box state directory."
}

impl ImageStore {
    /// Create a new image store.
    ///
    /// Creates the store directory if it doesn't exist and loads
    /// any existing index from disk.
    pub fn new(store_dir: &Path, max_size_bytes: u64) -> Result<Self> {
        std::fs::create_dir_all(store_dir).map_err(|e| {
            BoxError::OciImageError(format!(
                "Failed to create image store directory {}: {}. {}",
                store_dir.display(),
                e,
                state_dir_hint()
            ))
        })?;

        let mut store = Self {
            store_dir: store_dir.to_path_buf(),
            index: Arc::new(RwLock::new(HashMap::new())),
            max_size_bytes,
        };

        store.load_index()?;
        Ok(store)
    }

    /// Get a stored image by reference.
    pub async fn get(&self, reference: &str) -> Option<StoredImage> {
        let mut index = self.index.write().await;
        if let Some(image) = index.get_mut(reference) {
            image.last_used = Utc::now();
            let updated = image.clone();
            drop(index);
            // Best-effort save of updated last_used; log on failure so staleness is visible.
            if let Err(e) = self.save_index_inner().await {
                tracing::warn!(error = %e, "Failed to persist image store index (last_used may be stale)");
            }
            Some(updated)
        } else {
            None
        }
    }

    /// Get a stored image by digest.
    pub async fn get_by_digest(&self, digest: &str) -> Option<StoredImage> {
        let mut index = self.index.write().await;
        let found = index.values_mut().find(|img| img.digest == digest);
        if let Some(image) = found {
            image.last_used = Utc::now();
            let updated = image.clone();
            drop(index);
            if let Err(e) = self.save_index_inner().await {
                tracing::warn!(error = %e, "Failed to persist image store index (last_used may be stale)");
            }
            Some(updated)
        } else {
            None
        }
    }

    /// Resolve an image reference to a stored image.
    ///
    /// CRI callers may address an image by an exact stored reference, by its
    /// image id (a bare `sha256:...` or a `name@sha256:...` digest pin), or by
    /// an unnormalized name (e.g. a tagless name that defaults to `:latest`).
    pub async fn resolve(&self, image: &str) -> Option<StoredImage> {
        if let Some(found) = self.get(image).await {
            return Some(found);
        }
        let digest_part = image.rsplit_once('@').map_or(image, |(_, digest)| digest);
        if let Some(found) = self.get_by_digest(digest_part).await {
            return Some(found);
        }
        match super::ImageReference::parse(image) {
            Ok(parsed) => self.get(&parsed.full_reference()).await,
            Err(_) => None,
        }
    }

    /// Store an image from a source directory.
    ///
    /// Copies the OCI image layout from `source_dir` into the store
    /// under `sha256/<digest>/`.
    pub async fn put(
        &self,
        reference: &str,
        digest: &str,
        source_dir: &Path,
    ) -> Result<StoredImage> {
        // A digest becomes both a directory name and part of the staging name.
        // Accept only canonical content digests before either path is built.
        let digest_hex = super::registry::validated_digest_hex(digest)?;
        let digest_root = self.store_dir.join("sha256");
        std::fs::create_dir_all(&digest_root).map_err(|error| {
            BoxError::OciImageError(format!(
                "Failed to create image content directory {}: {error}",
                digest_root.display()
            ))
        })?;
        require_real_directory(&digest_root).map_err(|error| {
            BoxError::OciImageError(format!(
                "Unsafe image content directory {}: {error}",
                digest_root.display()
            ))
        })?;
        let target_dir = digest_root.join(digest_hex);

        // Copy source to target if not already present. Stage into a unique temp
        // dir then atomically rename into place, so a concurrent put() for the
        // same digest — or a copy that fails partway — can never leave a
        // half-populated content-addressed dir that a later caller mistakes for a
        // complete image (the bare check-then-copy raced on both counts).
        if !real_directory_exists(&target_dir).map_err(|error| {
            BoxError::OciImageError(format!(
                "Unsafe existing image directory {}: {error}",
                target_dir.display()
            ))
        })? {
            // Reserve the staging directory atomically. Never remove a guessed
            // path first: a local reparse point at that name must not turn
            // cleanup into traversal outside the store.
            let staging = loop {
                let seq = PUT_SEQ.fetch_add(1, Ordering::Relaxed);
                let candidate = digest_root.join(format!(
                    ".staging-{}-{}-{}",
                    digest_hex,
                    std::process::id(),
                    seq
                ));
                match std::fs::create_dir(&candidate) {
                    Ok(()) => break candidate,
                    Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => continue,
                    Err(error) => {
                        return Err(BoxError::OciImageError(format!(
                            "Failed to reserve image staging directory {}: {error}",
                            candidate.display()
                        )))
                    }
                }
            };
            copy_dir_contents_no_follow(source_dir, &staging).map_err(|e| {
                let _ = std::fs::remove_dir_all(&staging);
                BoxError::OciImageError(format!("Failed to copy image to store: {}", e))
            })?;
            if let Err(e) = std::fs::rename(&staging, &target_dir) {
                let _ = std::fs::remove_dir_all(&staging);
                // A concurrent put() may have populated target_dir first (rename
                // onto a non-empty dir fails); that's fine — only propagate if the
                // image still isn't there.
                if !real_directory_exists(&target_dir).map_err(|error| {
                    BoxError::OciImageError(format!(
                        "Unsafe concurrently published image directory {}: {error}",
                        target_dir.display()
                    ))
                })? {
                    return Err(BoxError::OciImageError(format!(
                        "Failed to publish image to store: {}",
                        e
                    )));
                }
            }
        }

        let size_bytes = dir_size(&target_dir);
        let now = Utc::now();

        let stored = StoredImage {
            reference: reference.to_string(),
            digest: digest.to_string(),
            size_bytes,
            pulled_at: now,
            last_used: now,
            path: target_dir,
        };

        self.with_index_lock(|index| {
            // Docker parity: if this reference already points at a DIFFERENT
            // digest and that old digest is about to lose its last reference,
            // keep the displaced image as a dangling entry (keyed by its digest)
            // instead of dropping it. This makes a rebuilt/re-tagged image show
            // up as `<none>` in `images`, be removable by `image prune`, and
            // prevents silently orphaning its on-disk layout.
            if let Some(old) = index.get(reference).cloned() {
                if old.digest != digest {
                    let still_referenced = index
                        .iter()
                        .any(|(key, img)| key.as_str() != reference && img.digest == old.digest);
                    if !still_referenced && !index.contains_key(&old.digest) {
                        let mut dangling = old.clone();
                        dangling.reference = old.digest.clone();
                        index.insert(old.digest.clone(), dangling);
                    }
                }
            }

            index.insert(reference.to_string(), stored.clone());
            Ok(())
        })
        .await?;

        Ok(stored)
    }

    /// Remove an image by reference or by image ID (digest).
    ///
    /// The CRI `RemoveImage` may identify an image either by a repo
    /// reference/tag or by its image ID (`sha256:<digest>`, as returned in
    /// `ImageStatus`). When `image` does not match a stored reference key,
    /// fall back to removing every reference that points at the matching
    /// digest.
    pub async fn remove(&self, image: &str) -> Result<()> {
        let store_dir = self.store_dir.clone();
        self.with_index_lock(move |index| {
            // Resolve the reference keys to remove: the exact reference if it is
            // a known key, otherwise every key sharing the requested digest.
            let keys: Vec<String> = if index.contains_key(image) {
                vec![image.to_string()]
            } else {
                index
                    .values()
                    .filter(|img| img.digest == image)
                    .map(|img| img.reference.clone())
                    .collect()
            };

            if keys.is_empty() {
                return Err(BoxError::OciImageError(format!(
                    "Image not found: {}",
                    image
                )));
            }

            // Stored paths are persisted data. Re-derive every deletion target
            // from a validated digest instead of trusting a serialized path.
            for key in &keys {
                let img = index.get(key).ok_or_else(|| {
                    BoxError::OciImageError(format!("Image index entry disappeared: {key}"))
                })?;
                let digest_hex = super::registry::validated_digest_hex(&img.digest)?;
                let expected = store_dir.join("sha256").join(digest_hex);
                if img.path != expected {
                    return Err(BoxError::OciImageError(format!(
                        "Refusing unsafe image path {} for digest {} (expected {})",
                        img.path.display(),
                        img.digest,
                        expected.display()
                    )));
                }
            }

            let removed: Vec<StoredImage> = keys.iter().filter_map(|k| index.remove(k)).collect();

            // Delete each image's on-disk layout once no remaining reference
            // points at the same digest. References sharing a digest share the
            // same directory, so the `path.exists()` guard makes this idempotent.
            for img in removed {
                let digest_still_used = index.values().any(|other| other.digest == img.digest);
                if !digest_still_used
                    && real_directory_exists(&img.path).map_err(|error| {
                        BoxError::OciImageError(format!(
                            "Refusing unsafe image directory {}: {error}",
                            img.path.display()
                        ))
                    })?
                {
                    std::fs::remove_dir_all(&img.path).map_err(|e| {
                        BoxError::OciImageError(format!(
                            "Failed to remove image directory {}: {}",
                            img.path.display(),
                            e
                        ))
                    })?;
                }
            }
            Ok(())
        })
        .await
    }

    /// List all stored images.
    pub async fn list(&self) -> Vec<StoredImage> {
        let index = self.index.read().await;
        index.values().cloned().collect()
    }

    /// Evict least-recently-used images until total size is under the limit.
    ///
    /// Returns the references of evicted images.
    pub async fn evict(&self) -> Result<Vec<String>> {
        let mut evicted = Vec::new();
        let mut total = self.total_size().await;

        while total > self.max_size_bytes {
            // Find the least recently used image
            let lru_ref = {
                let index = self.index.read().await;
                index
                    .values()
                    .min_by_key(|img| img.last_used)
                    .map(|img| img.reference.clone())
            };

            match lru_ref {
                Some(reference) => {
                    self.remove(&reference).await?;
                    evicted.push(reference);
                    total = self.total_size().await;
                }
                None => break,
            }
        }

        Ok(evicted)
    }

    /// Get total size of all stored images in bytes.
    pub async fn total_size(&self) -> u64 {
        let index = self.index.read().await;
        index.values().map(|img| img.size_bytes).sum()
    }

    /// Load index from disk.
    fn load_index(&mut self) -> Result<()> {
        // Construction-time load; reuse the shared disk reader.
        self.index = Arc::new(RwLock::new(self.read_index_from_disk()?));
        Ok(())
    }

    /// Read and parse `index.json` from disk into a fresh map (entries whose
    /// content dir vanished are dropped). Does NOT touch `self.index`.
    fn read_index_from_disk(&self) -> Result<HashMap<String, StoredImage>> {
        let index_path = self.store_dir.join("index.json");
        if !index_path.exists() {
            return Ok(HashMap::new());
        }

        let data = std::fs::read_to_string(&index_path).map_err(|e| {
            BoxError::OciImageError(format!(
                "Failed to read image store index {}: {}",
                index_path.display(),
                e
            ))
        })?;

        // Parse resiliently so a corrupt/old-schema index never bricks the whole
        // catalog or blocks CRI/CLI startup. First read the `{ images: [...] }`
        // envelope leniently (entries as raw values); if even that fails the file
        // is unusable, so quarantine it and start from an empty catalog that
        // re-pulls repopulate. Then deserialize each entry independently, skipping
        // (not failing on) any one corrupt/incompatible record.
        #[derive(serde::Deserialize)]
        struct RawIndex {
            #[serde(default)]
            images: Vec<serde_json::Value>,
        }

        let raw: RawIndex = match serde_json::from_str(&data) {
            Ok(raw) => raw,
            Err(err) => {
                let preserved = crate::store_io::quarantine_label(&index_path);
                tracing::warn!(
                    "image store index {} is corrupt ({err}); preserved a copy at \
                     {preserved} and started from an empty catalog (re-pulled images \
                     will repopulate it)",
                    index_path.display(),
                );
                return Ok(HashMap::new());
            }
        };

        let mut index = HashMap::new();
        let mut skipped = 0usize;
        for value in raw.images {
            match serde_json::from_value::<StoredImage>(value) {
                Ok(mut image) => {
                    // The serialized path is compatibility metadata, not an
                    // authority for filesystem access. Windows can persist an
                    // equivalent 8.3 spelling (for example `WODEDI~1`) and then
                    // reopen the store through the long spelling. Lexical path
                    // equality rejects that valid entry. Re-derive the only
                    // permitted location from the validated digest and replace
                    // the persisted spelling before exposing the record.
                    let expected = super::registry::validated_digest_hex(&image.digest)
                        .map(|digest_hex| self.store_dir.join("sha256").join(digest_hex));
                    match expected {
                        Ok(expected) if real_directory_exists(&expected).unwrap_or(false) => {
                            image.path = expected;
                            index.insert(image.reference.clone(), image);
                        }
                        _ => {
                            skipped += 1;
                            tracing::warn!(
                                reference = %image.reference,
                                digest = %image.digest,
                                path = %image.path.display(),
                                "skipping image index entry with malformed digest or unsafe path"
                            );
                        }
                    }
                }
                Err(err) => {
                    skipped += 1;
                    tracing::warn!("skipping unreadable image index entry ({err})");
                }
            }
        }
        if skipped > 0 {
            // Preserve the original (with the un-deserializable entries) before the
            // next save rewrites index.json with only the survivors — otherwise the
            // skipped records are erased with no backup, unlike the whole-file path.
            let preserved = crate::store_io::quarantine_copy(&index_path)
                .map(|p| p.display().to_string())
                .unwrap_or_else(|| "<backup failed>".to_string());
            tracing::warn!(
                "{skipped} image index entr{} skipped as unreadable; preserved a copy at \
                 {preserved}; affected images will be re-pulled on demand",
                if skipped == 1 { "y" } else { "ies" },
            );
        }
        Ok(index)
    }

    /// Apply `f` to the image index under the **cross-process write lock**:
    /// reload `index.json` from disk (so this process observes other processes'
    /// pulls/removes), let `f` mutate the map, then save. Without this, two
    /// processes pulling concurrently each load their own snapshot and the
    /// second `save` drops the first's entry (and leaks its content dir).
    ///
    /// The blocking `flock` is acquired off the runtime via `spawn_blocking`;
    /// `save_index_inner` is lock-free, so there is no re-entrant `flock`.
    async fn with_index_lock<F, R>(&self, f: F) -> Result<R>
    where
        F: FnOnce(&mut HashMap<String, StoredImage>) -> Result<R>,
    {
        let index_path = self.store_dir.join("index.json");
        let _lock = {
            let p = index_path.clone();
            tokio::task::spawn_blocking(move || crate::file_lock::FileLock::acquire(&p))
                .await
                .map_err(|e| BoxError::OciImageError(format!("index lock task failed: {e}")))?
                .map_err(|e| {
                    BoxError::OciImageError(format!(
                        "failed to lock image index {}: {e}. {}",
                        index_path.display(),
                        state_dir_hint()
                    ))
                })?
        };
        // Sync the in-memory index with disk (pick up other processes' writes).
        let fresh = self.read_index_from_disk()?;
        let result = {
            let mut idx = self.index.write().await;
            *idx = fresh;
            f(&mut idx)?
        };
        self.save_index_inner().await?;
        Ok(result)
    }

    /// Save index to disk (async inner helper).
    async fn save_index_inner(&self) -> Result<()> {
        let index = self.index.read().await;
        let store_index = StoreIndex {
            images: index.values().cloned().collect(),
        };
        drop(index);

        let data = serde_json::to_string_pretty(&store_index)?;
        let index_path = self.store_dir.join("index.json");
        // Write atomically (tmp + rename) so a concurrent reader (e.g. another
        // process running `create`/`run`) never observes a truncated/empty
        // index.json mid-write — which previously surfaced as
        // "Failed to parse image store index: EOF".
        let tmp_path = self.store_dir.join("index.json.tmp");
        tokio::fs::write(&tmp_path, data).await.map_err(|e| {
            BoxError::OciImageError(format!(
                "Failed to write image store index {}: {}. {}",
                tmp_path.display(),
                e,
                state_dir_hint()
            ))
        })?;
        tokio::fs::rename(&tmp_path, &index_path)
            .await
            .map_err(|e| {
                BoxError::OciImageError(format!(
                    "Failed to commit image store index {}: {}. {}",
                    index_path.display(),
                    e,
                    state_dir_hint()
                ))
            })?;

        Ok(())
    }

    /// Get the store directory path.
    pub fn store_dir(&self) -> &Path {
        &self.store_dir
    }
}

#[async_trait::async_trait]
impl ImageStoreBackend for ImageStore {
    async fn get(&self, reference: &str) -> Option<StoredImage> {
        self.get(reference).await
    }

    async fn get_by_digest(&self, digest: &str) -> Option<StoredImage> {
        self.get_by_digest(digest).await
    }

    async fn put(&self, reference: &str, digest: &str, source_dir: &Path) -> Result<StoredImage> {
        self.put(reference, digest, source_dir).await
    }

    async fn remove(&self, reference: &str) -> Result<()> {
        self.remove(reference).await
    }

    async fn list(&self) -> Vec<StoredImage> {
        self.list().await
    }

    async fn evict(&self) -> Result<Vec<String>> {
        self.evict().await
    }

    async fn total_size(&self) -> u64 {
        self.total_size().await
    }
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt;

    // FILE_ATTRIBUTE_REPARSE_POINT. Keep the value local so runtime does not
    // need another windows-sys feature solely to classify metadata.
    metadata.file_attributes() & 0x0000_0400 != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(_metadata: &std::fs::Metadata) -> bool {
    false
}

fn require_real_directory(path: &Path) -> std::io::Result<()> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || metadata_is_reparse_point(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::PermissionDenied,
            format!(
                "refusing symbolic link or reparse-point directory {}",
                path.display()
            ),
        ));
    }
    if !metadata.is_dir() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("expected a directory at {}", path.display()),
        ));
    }
    Ok(())
}

fn real_directory_exists(path: &Path) -> std::io::Result<bool> {
    match require_real_directory(path) {
        Ok(()) => Ok(true),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(error),
    }
}

#[cfg(unix)]
fn open_regular_source_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    use std::os::unix::fs::OpenOptionsExt;

    let mut options = std::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    let file = options.open(path)?;
    if !file.metadata()?.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("expected a regular file at {}", path.display()),
        ));
    }
    Ok(file)
}

#[cfg(windows)]
fn open_regular_source_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    a3s_box_core::windows_file::open_regular_file(path, None).map(|(file, _)| file)
}

#[cfg(not(any(unix, windows)))]
fn open_regular_source_no_follow(path: &Path) -> std::io::Result<std::fs::File> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink() || !metadata.is_file() {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("expected a regular file at {}", path.display()),
        ));
    }
    std::fs::File::open(path)
}

fn copy_regular_file_no_follow(src: &Path, dst: &Path) -> std::io::Result<()> {
    let mut source = open_regular_source_no_follow(src)?;
    let mut destination = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(dst)?;
    std::io::copy(&mut source, &mut destination)?;
    Ok(())
}

/// Copy directory contents while rejecting every source link, junction,
/// reparse point, and special file. OCI layout symlinks are never required:
/// guest symlinks live inside opaque layer tar blobs instead.
fn copy_dir_contents_no_follow(src: &Path, dst: &Path) -> std::io::Result<()> {
    require_real_directory(src)?;
    require_real_directory(dst)?;

    for entry in std::fs::read_dir(src)? {
        let entry = entry?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());
        let metadata = std::fs::symlink_metadata(&src_path)?;

        if metadata.file_type().is_symlink() || metadata_is_reparse_point(&metadata) {
            return Err(std::io::Error::new(
                std::io::ErrorKind::PermissionDenied,
                format!(
                    "refusing symbolic link or reparse point in OCI layout: {}",
                    src_path.display()
                ),
            ));
        }
        if metadata.is_dir() {
            std::fs::create_dir(&dst_path)?;
            copy_dir_contents_no_follow(&src_path, &dst_path)?;
        } else if metadata.is_file() {
            copy_regular_file_no_follow(&src_path, &dst_path)?;
        } else {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                format!(
                    "refusing special file in OCI layout: {}",
                    src_path.display()
                ),
            ));
        }
    }
    Ok(())
}

/// Recursively copy a directory into a newly created destination.
#[cfg(test)]
fn copy_dir_recursive(src: &Path, dst: &Path) -> std::io::Result<()> {
    std::fs::create_dir(dst)?;
    copy_dir_contents_no_follow(src, dst)
}

/// Calculate total size without following links that may have appeared in a
/// corrupted or externally modified store.
fn dir_size(path: &Path) -> u64 {
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return 0;
    };
    if metadata.file_type().is_symlink() || metadata_is_reparse_point(&metadata) {
        return 0;
    }
    if metadata.is_file() {
        return metadata.len();
    }
    if !metadata.is_dir() {
        return 0;
    }

    std::fs::read_dir(path)
        .map(|entries| entries.flatten().map(|entry| dir_size(&entry.path())).sum())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn create_test_oci_layout(dir: &Path) {
        std::fs::create_dir_all(dir.join("blobs/sha256")).unwrap();
        std::fs::write(dir.join("oci-layout"), r#"{"imageLayoutVersion":"1.0.0"}"#).unwrap();
        std::fs::write(dir.join("index.json"), r#"{"manifests":[]}"#).unwrap();
        // Write some blob data to have measurable size
        std::fs::write(dir.join("blobs/sha256/testblob"), "x".repeat(1024)).unwrap();
    }

    fn stored_image(reference: &str, digest: &str, path: PathBuf) -> StoredImage {
        let now = Utc::now();
        StoredImage {
            reference: reference.to_string(),
            digest: digest.to_string(),
            size_bytes: 1024,
            pulled_at: now,
            last_used: now,
            path,
        }
    }

    #[tokio::test]
    async fn test_new_creates_directory() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("images");
        let store = ImageStore::new(&store_dir, 1024 * 1024).unwrap();
        assert!(store_dir.exists());
        assert_eq!(store.total_size().await, 0);
    }

    #[tokio::test]
    async fn test_new_keeps_existing_tmp_dir_for_concurrent_pulls() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("images");
        let tmp_dir = store_dir.join("tmp");
        std::fs::create_dir_all(tmp_dir.join("pull-1")).unwrap();
        std::fs::write(tmp_dir.join("pull-1/layer"), b"partial").unwrap();

        let store = ImageStore::new(&store_dir, 1024 * 1024).unwrap();

        assert!(
            tmp_dir.join("pull-1/layer").exists(),
            "constructing a store must not delete another process' active pull"
        );
        assert_eq!(store.total_size().await, 0);
    }

    #[tokio::test]
    async fn test_put_and_get() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        let store = ImageStore::new(&store_dir, 10 * 1024 * 1024).unwrap();

        let stored = store
            .put(
                "nginx:latest",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                &source_dir,
            )
            .await
            .unwrap();

        assert_eq!(stored.reference, "nginx:latest");
        assert_eq!(
            stored.digest,
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );
        assert!(stored.size_bytes > 0);
        assert!(stored.path.exists());

        // Get by reference
        let fetched = store.get("nginx:latest").await.unwrap();
        assert_eq!(
            fetched.digest,
            "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        );

        // Get by digest
        let fetched = store
            .get_by_digest(
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            )
            .await
            .unwrap();
        assert_eq!(fetched.reference, "nginx:latest");
    }

    #[tokio::test]
    async fn test_get_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let store = ImageStore::new(tmp.path(), 1024 * 1024).unwrap();
        assert!(store.get("nonexistent").await.is_none());
    }

    #[tokio::test]
    async fn test_remove() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        let store = ImageStore::new(&store_dir, 10 * 1024 * 1024).unwrap();
        store
            .put(
                "nginx:latest",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                &source_dir,
            )
            .await
            .unwrap();

        store.remove("nginx:latest").await.unwrap();
        assert!(store.get("nginx:latest").await.is_none());
    }

    #[tokio::test]
    async fn test_remove_one_tag_keeps_shared_digest_until_last_reference() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        let store = ImageStore::new(&store_dir, 10 * 1024 * 1024).unwrap();
        store
            .put(
                "img:v1",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                &source_dir,
            )
            .await
            .unwrap();
        let stored = store
            .put(
                "img:latest",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                &source_dir,
            )
            .await
            .unwrap();
        let path = stored.path.clone();

        store.remove("img:v1").await.unwrap();
        assert!(store.get("img:v1").await.is_none());
        assert!(store.get("img:latest").await.is_some());
        assert!(path.exists(), "shared layout should remain in use");

        store.remove("img:latest").await.unwrap();
        assert!(!path.exists(), "layout should be removed after final tag");
    }

    #[tokio::test]
    async fn test_retag_keeps_displaced_image_as_dangling() {
        // Docker parity: re-pointing a tag at a new digest leaves the old image
        // as a dangling entry (keyed by its digest), not silently dropped.
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        let store = ImageStore::new(&store_dir, 10 * 1024 * 1024).unwrap();
        store
            .put(
                "app:latest",
                "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee",
                &source_dir,
            )
            .await
            .unwrap();
        store
            .put(
                "app:latest",
                "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff",
                &source_dir,
            )
            .await
            .unwrap();

        // The tag now resolves to the new digest...
        assert_eq!(
            store.get("app:latest").await.unwrap().digest,
            "sha256:ffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffffff"
        );
        // ...and the displaced image survives as a digest-keyed dangling entry.
        let dangling = store
            .get("sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee")
            .await
            .unwrap();
        assert_eq!(
            dangling.digest,
            "sha256:eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee"
        );
        assert_eq!(store.list().await.len(), 2);
    }

    #[tokio::test]
    async fn test_reput_same_digest_does_not_create_dangling() {
        // Re-putting the same reference at the SAME digest (e.g. pulling latest
        // when content is unchanged) must not spawn a spurious dangling entry.
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        let store = ImageStore::new(&store_dir, 10 * 1024 * 1024).unwrap();
        store
            .put(
                "app:latest",
                "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                &source_dir,
            )
            .await
            .unwrap();
        store
            .put(
                "app:latest",
                "sha256:1111111111111111111111111111111111111111111111111111111111111111",
                &source_dir,
            )
            .await
            .unwrap();

        assert_eq!(store.list().await.len(), 1);
    }

    #[tokio::test]
    async fn test_remove_by_digest() {
        // CRI RemoveImage identifies the image by its ID (sha256 digest),
        // not its tag. Removing by digest must drop the reference + layout.
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        let store = ImageStore::new(&store_dir, 10 * 1024 * 1024).unwrap();
        let stored = store
            .put(
                "gcr.io/test/img:test",
                "sha256:2222222222222222222222222222222222222222222222222222222222222222",
                &source_dir,
            )
            .await
            .unwrap();
        let path = stored.path.clone();

        store
            .remove("sha256:2222222222222222222222222222222222222222222222222222222222222222")
            .await
            .unwrap();
        assert!(store.get("gcr.io/test/img:test").await.is_none());
        assert!(store
            .get_by_digest(
                "sha256:2222222222222222222222222222222222222222222222222222222222222222"
            )
            .await
            .is_none());
        assert!(!path.exists(), "on-disk layout should be deleted");
    }

    #[tokio::test]
    async fn test_remove_by_digest_removes_all_tags() {
        // Two tags sharing one digest: removing by digest drops both and
        // deletes the shared layout exactly once.
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        let store = ImageStore::new(&store_dir, 10 * 1024 * 1024).unwrap();
        store
            .put(
                "img:v1",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                &source_dir,
            )
            .await
            .unwrap();
        let stored = store
            .put(
                "img:latest",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                &source_dir,
            )
            .await
            .unwrap();
        let path = stored.path.clone();

        store
            .remove("sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb")
            .await
            .unwrap();
        assert!(store.get("img:v1").await.is_none());
        assert!(store.get("img:latest").await.is_none());
        assert!(!path.exists(), "shared layout should be deleted");
    }

    #[tokio::test]
    async fn test_resolve_by_name_digest_and_normalized() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        let store = ImageStore::new(&store_dir, 10 * 1024 * 1024).unwrap();
        store
            .put(
                "gcr.io/x/test-image-predefined-group:latest",
                "sha256:3333333333333333333333333333333333333333333333333333333333333333",
                &source_dir,
            )
            .await
            .unwrap();

        // Exact reference.
        assert!(store
            .resolve("gcr.io/x/test-image-predefined-group:latest")
            .await
            .is_some());
        // Unnormalized name (no tag -> :latest) — the CreateContainer case.
        assert_eq!(
            store
                .resolve("gcr.io/x/test-image-predefined-group")
                .await
                .map(|i| i.digest),
            Some(
                "sha256:3333333333333333333333333333333333333333333333333333333333333333"
                    .to_string()
            )
        );
        // Image id (bare digest) and a name@digest pin.
        assert!(store
            .resolve("sha256:3333333333333333333333333333333333333333333333333333333333333333")
            .await
            .is_some());
        assert!(store
            .resolve("gcr.io/x/test-image-predefined-group@sha256:3333333333333333333333333333333333333333333333333333333333333333")
            .await
            .is_some());
        // Unknown.
        assert!(store.resolve("nope:latest").await.is_none());
    }

    #[tokio::test]
    async fn test_resolve_invalid_reference_returns_none() {
        let tmp = TempDir::new().unwrap();
        let store = ImageStore::new(tmp.path(), 1024 * 1024).unwrap();

        assert!(store.resolve("registry.example.com/").await.is_none());
        assert!(store.resolve("busybox@not-a-digest").await.is_none());
    }

    #[tokio::test]
    async fn test_remove_nonexistent() {
        let tmp = TempDir::new().unwrap();
        let store = ImageStore::new(tmp.path(), 1024 * 1024).unwrap();
        assert!(store.remove("nonexistent").await.is_err());
    }

    #[tokio::test]
    async fn test_list() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        let store = ImageStore::new(&store_dir, 10 * 1024 * 1024).unwrap();
        store
            .put(
                "nginx:latest",
                "sha256:4444444444444444444444444444444444444444444444444444444444444444",
                &source_dir,
            )
            .await
            .unwrap();
        store
            .put(
                "alpine:3.18",
                "sha256:5555555555555555555555555555555555555555555555555555555555555555",
                &source_dir,
            )
            .await
            .unwrap();

        let images = store.list().await;
        assert_eq!(images.len(), 2);
    }

    #[tokio::test]
    async fn test_total_size() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        let store = ImageStore::new(&store_dir, 10 * 1024 * 1024).unwrap();
        store
            .put(
                "nginx:latest",
                "sha256:4444444444444444444444444444444444444444444444444444444444444444",
                &source_dir,
            )
            .await
            .unwrap();

        assert!(store.total_size().await > 0);
    }

    #[tokio::test]
    async fn test_lru_eviction() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        // Set max size very small to trigger eviction
        let store = ImageStore::new(&store_dir, 100).unwrap();

        store
            .put(
                "old:v1",
                "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
                &source_dir,
            )
            .await
            .unwrap();

        // Sleep briefly so timestamps differ
        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        store
            .put(
                "new:v2",
                "sha256:dddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddddd",
                &source_dir,
            )
            .await
            .unwrap();

        // Access the newer one to update its last_used
        store.get("new:v2").await;

        let evicted = store.evict().await.unwrap();
        // At least one image should be evicted (the older one first)
        assert!(!evicted.is_empty());
        assert!(evicted.contains(&"old:v1".to_string()));
    }

    #[tokio::test]
    async fn test_evict_empty_and_under_limit_returns_empty() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        let store = ImageStore::new(&store_dir, u64::MAX).unwrap();
        assert!(store.evict().await.unwrap().is_empty());

        store
            .put(
                "tiny:latest",
                "sha256:6666666666666666666666666666666666666666666666666666666666666666",
                &source_dir,
            )
            .await
            .unwrap();
        assert!(store.evict().await.unwrap().is_empty());
        assert!(store.get("tiny:latest").await.is_some());
    }

    #[tokio::test]
    async fn test_index_persistence() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        // Create store and add image
        {
            let store = ImageStore::new(&store_dir, 10 * 1024 * 1024).unwrap();
            store
                .put(
                    "nginx:latest",
                    "sha256:7777777777777777777777777777777777777777777777777777777777777777",
                    &source_dir,
                )
                .await
                .unwrap();
        }

        // Create new store from same directory — should load persisted index
        {
            let store = ImageStore::new(&store_dir, 10 * 1024 * 1024).unwrap();
            let image = store.get("nginx:latest").await;
            assert!(image.is_some());
            assert_eq!(
                image.unwrap().digest,
                "sha256:7777777777777777777777777777777777777777777777777777777777777777"
            );
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn concurrent_cross_instance_puts_persist_both() {
        use std::collections::HashSet;
        use std::sync::Arc;

        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        create_test_oci_layout(&source_dir);

        // Two ImageStore instances on the SAME dir simulate two processes, each
        // with its own in-memory index. Concurrent puts of distinct images must
        // BOTH persist — the lost-update bug dropped one. with_index_lock
        // reloads under the cross-process lock, so neither overwrites the other.
        let s1 = Arc::new(ImageStore::new(&store_dir, u64::MAX).unwrap());
        let s2 = Arc::new(ImageStore::new(&store_dir, u64::MAX).unwrap());
        let (src1, src2) = (source_dir.clone(), source_dir.clone());
        let h1 = {
            let s1 = Arc::clone(&s1);
            tokio::spawn(async move {
                s1.put(
                    "img:a",
                    "sha256:8888888888888888888888888888888888888888888888888888888888888888",
                    &src1,
                )
                .await
                .unwrap()
            })
        };
        let h2 = {
            let s2 = Arc::clone(&s2);
            tokio::spawn(async move {
                s2.put(
                    "img:b",
                    "sha256:9999999999999999999999999999999999999999999999999999999999999999",
                    &src2,
                )
                .await
                .unwrap()
            })
        };
        h1.await.unwrap();
        h2.await.unwrap();

        // A fresh instance reads index.json from disk: both must be there.
        let s3 = ImageStore::new(&store_dir, u64::MAX).unwrap();
        let refs: HashSet<String> = s3.list().await.into_iter().map(|i| i.reference).collect();
        assert!(refs.contains("img:a"), "img:a lost: {refs:?}");
        assert!(refs.contains("img:b"), "img:b lost: {refs:?}");
    }

    #[tokio::test]
    async fn load_index_skips_missing_paths_and_unreadable_entries() {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("images");
        let live_digest = format!("sha256:{}", "a".repeat(64));
        let missing_digest = format!("sha256:{}", "b".repeat(64));
        let live_path = store_dir.join("sha256").join("a".repeat(64));
        let missing_path = store_dir.join("sha256").join("b".repeat(64));
        create_test_oci_layout(&live_path);

        let live = stored_image("live:latest", &live_digest, live_path);
        let missing = stored_image("missing:latest", &missing_digest, missing_path);
        let index = serde_json::json!({
            "images": [
                serde_json::to_value(&live).unwrap(),
                serde_json::to_value(&missing).unwrap(),
                {
                    "reference": "broken:latest",
                    "digest": false
                }
            ]
        });
        std::fs::write(
            store_dir.join("index.json"),
            serde_json::to_vec_pretty(&index).unwrap(),
        )
        .unwrap();

        let store = ImageStore::new(&store_dir, u64::MAX).unwrap();
        let images = store.list().await;

        assert_eq!(images.len(), 1);
        assert_eq!(images[0].reference, "live:latest");
        assert!(store.get("missing:latest").await.is_none());
        assert!(std::fs::read_dir(&store_dir)
            .unwrap()
            .filter_map(|entry| entry.ok())
            .any(|entry| entry
                .file_name()
                .to_string_lossy()
                .contains("index.json.corrupt-")));
    }

    #[tokio::test]
    async fn corrupt_index_is_quarantined_not_fatal() {
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("images");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("index.json"), "{ not valid json").unwrap();

        // Construction must SUCCEED (start from an empty catalog) rather than
        // erroring and blocking CRI/CLI startup on a corrupt/old-schema index.
        let store = ImageStore::new(&store_dir, u64::MAX)
            .expect("corrupt index.json must not brick the image store");
        assert!(
            store.list().await.is_empty(),
            "store must start from an empty catalog after quarantine"
        );

        // The corrupt index is preserved as a timestamped sibling, not lost.
        let quarantined = std::fs::read_dir(&store_dir)
            .unwrap()
            .filter_map(|e| e.ok())
            .any(|e| {
                e.file_name()
                    .to_string_lossy()
                    .contains("index.json.corrupt-")
            });
        assert!(
            quarantined,
            "corrupt index.json must be quarantined to a sibling"
        );
    }

    #[test]
    fn copy_dir_recursive_copies_nested_files_and_dir_size_sums() {
        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("src");
        let dst = tmp.path().join("dst");
        std::fs::create_dir_all(src.join("nested")).unwrap();
        std::fs::write(src.join("root.txt"), b"abc").unwrap();
        std::fs::write(src.join("nested/leaf.txt"), b"hello").unwrap();

        copy_dir_recursive(&src, &dst).unwrap();

        assert_eq!(std::fs::read(dst.join("root.txt")).unwrap(), b"abc");
        assert_eq!(
            std::fs::read(dst.join("nested/leaf.txt")).unwrap(),
            b"hello"
        );
        assert_eq!(dir_size(&dst), 8);
        assert_eq!(dir_size(&tmp.path().join("missing")), 0);
    }

    #[test]
    fn copy_dir_recursive_fails_for_missing_source() {
        let tmp = TempDir::new().unwrap();
        let err = copy_dir_recursive(&tmp.path().join("missing"), &tmp.path().join("dst"))
            .expect_err("missing source should fail");

        assert_eq!(err.kind(), std::io::ErrorKind::NotFound);
    }

    #[tokio::test]
    async fn put_rejects_path_shaped_digest_without_touching_host_path() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        let host_dir = tmp.path().join("host-target");
        create_test_oci_layout(&source_dir);
        std::fs::create_dir_all(&host_dir).unwrap();
        std::fs::write(host_dir.join("keep.txt"), b"host data").unwrap();
        let store = ImageStore::new(&store_dir, u64::MAX).unwrap();

        let error = store
            .put("evil:latest", "sha256:../../host-target", &source_dir)
            .await
            .unwrap_err();

        assert!(error.to_string().contains("malformed content digest"));
        assert_eq!(
            std::fs::read(host_dir.join("keep.txt")).unwrap(),
            b"host data"
        );
        assert!(store.list().await.is_empty());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn put_rejects_source_symlink_without_copying_target() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let real_source = tmp.path().join("real-source");
        let source_link = tmp.path().join("source-link");
        create_test_oci_layout(&real_source);
        symlink(&real_source, &source_link).unwrap();
        let store = ImageStore::new(&store_dir, u64::MAX).unwrap();

        let error = store
            .put(
                "evil:latest",
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
                &source_link,
            )
            .await
            .unwrap_err();

        assert!(error.to_string().contains("symbolic link"));
        assert!(store.list().await.is_empty());
        assert!(real_source.join("index.json").is_file());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn put_rejects_extra_symlink_and_preserves_its_target() {
        use std::os::unix::fs::symlink;

        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        let host_file = tmp.path().join("host-secret.txt");
        create_test_oci_layout(&source_dir);
        std::fs::write(&host_file, b"secret").unwrap();
        symlink(&host_file, source_dir.join("extra-blob")).unwrap();
        let store = ImageStore::new(&store_dir, u64::MAX).unwrap();

        assert!(store
            .put(
                "evil:latest",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                &source_dir,
            )
            .await
            .is_err());
        assert_eq!(std::fs::read(&host_file).unwrap(), b"secret");
        assert!(store.list().await.is_empty());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn put_rejects_windows_source_reparse_point() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let source_dir = tmp.path().join("source");
        let host_file = tmp.path().join("host-secret.txt");
        let link = source_dir.join("extra-blob");
        create_test_oci_layout(&source_dir);
        std::fs::write(&host_file, b"secret").unwrap();
        match std::os::windows::fs::symlink_file(&host_file, &link) {
            Ok(()) => {}
            Err(error) if error.raw_os_error() == Some(1314) => return,
            Err(error) => panic!("failed to create Windows test symlink: {error}"),
        }
        let store = ImageStore::new(&store_dir, u64::MAX).unwrap();

        assert!(store
            .put(
                "evil:latest",
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
                &source_dir,
            )
            .await
            .is_err());
        assert_eq!(std::fs::read(&host_file).unwrap(), b"secret");
        assert!(store.list().await.is_empty());
    }

    #[tokio::test]
    async fn load_index_rejects_forged_deletion_path() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let victim = tmp.path().join("host-victim");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("keep.txt"), b"keep").unwrap();

        let forged = stored_image(
            "evil:latest",
            "sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc",
            victim.clone(),
        );
        let index = StoreIndex {
            images: vec![forged],
        };
        std::fs::write(
            store_dir.join("index.json"),
            serde_json::to_vec_pretty(&index).unwrap(),
        )
        .unwrap();

        let store = ImageStore::new(&store_dir, u64::MAX).unwrap();
        assert!(store.list().await.is_empty());
        assert!(store.remove("evil:latest").await.is_err());
        assert_eq!(std::fs::read(victim.join("keep.txt")).unwrap(), b"keep");
    }

    #[tokio::test]
    async fn load_index_rederives_path_and_never_uses_forged_spelling() {
        let tmp = TempDir::new().unwrap();
        let store_dir = tmp.path().join("store");
        let digest_hex = "d".repeat(64);
        let digest = format!("sha256:{digest_hex}");
        let expected = store_dir.join("sha256").join(&digest_hex);
        let victim = tmp.path().join("host-victim");
        create_test_oci_layout(&expected);
        std::fs::create_dir_all(&victim).unwrap();
        std::fs::write(victim.join("keep.txt"), b"keep").unwrap();

        // A persisted path may be an equivalent legacy spelling (notably an
        // 8.3 path on Windows), or it may be attacker-controlled. In either
        // case the digest-derived content directory is the only path to use.
        let forged = stored_image("safe:latest", &digest, victim.clone());
        let index = StoreIndex {
            images: vec![forged],
        };
        std::fs::write(
            store_dir.join("index.json"),
            serde_json::to_vec_pretty(&index).unwrap(),
        )
        .unwrap();

        let store = ImageStore::new(&store_dir, u64::MAX).unwrap();
        let loaded = store.get("safe:latest").await.unwrap();
        assert_eq!(loaded.path, expected);

        store.remove("safe:latest").await.unwrap();
        assert!(!expected.exists());
        assert_eq!(std::fs::read(victim.join("keep.txt")).unwrap(), b"keep");
    }
}
