//! Layer-level build cache for the image build engine.
//!
//! Implements a Docker/BuildKit-style cache keyed on a running "chain key":
//! a rolling hash of every instruction (and, for COPY/ADD, the content of the
//! source files) executed so far. When a layer-producing instruction
//! (RUN/COPY/ADD) is reached with a chain key that has been seen before, the
//! previously produced layer is reused instead of re-executing the instruction.
//!
//! The cache lives under `~/.a3s/buildcache`:
//! - `blobs/<digest>`  — the cached layer tar.gz, content-addressed.
//! - `keys/<chain-key>` — a small JSON record `{digest, diff_id, size}` pointing
//!   at the blob.
//!
//! All cache I/O is best-effort: any failure leaves the build uncached but does
//! NOT fail the build.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use a3s_box_core::dirs_home;
use serde::{Deserialize, Serialize};

use super::layer::{sha256_bytes, sha256_file, LayerInfo};

mod export;
mod import;

pub(super) use export::{inspect_build_cache_artifact, BuildCacheExportIdentity, BuildCacheTrace};
pub use export::{
    BuildCacheReceipt, RecordedBuildCache, BUILD_CACHE_ARTIFACT_MEDIA_TYPE,
    BUILD_CACHE_CONFIG_MEDIA_TYPE,
};
pub use import::hydrate_recorded_build_cache;

/// Per-process counter for unique staging-file names in `store`.
static STORE_SEQ: AtomicU64 = AtomicU64::new(0);

/// Default cap on the total size of cached layer blobs (2 GiB). Override with
/// `A3S_BOX_BUILDCACHE_MAX_BYTES`. When the cap is exceeded after a store, the
/// oldest blobs are evicted (FIFO) until the total is back under the cap; a
/// later build that needs an evicted layer simply re-runs the instruction.
const DEFAULT_MAX_BYTES: u64 = 2 * 1024 * 1024 * 1024;

fn configured_max_bytes() -> u64 {
    std::env::var("A3S_BOX_BUILDCACHE_MAX_BYTES")
        .ok()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(DEFAULT_MAX_BYTES)
}

/// On-disk record stored at `keys/<chain-key>`.
#[derive(Debug, Serialize, Deserialize)]
struct KeyRecord {
    digest: String,
    diff_id: String,
    size: u64,
}

/// A cache hit: a previously produced layer that can be reused.
#[derive(Debug, Clone)]
pub(crate) struct CachedLayer {
    /// Path to the cached layer tar.gz blob.
    pub(crate) blob_path: PathBuf,
    /// SHA256 digest (hex, no prefix) of the compressed layer.
    pub(crate) digest: String,
    /// diff_id (SHA256 of the uncompressed content).
    pub(crate) diff_id: String,
    /// Size in bytes of the compressed layer.
    pub(crate) size: u64,
}

/// Layer-level build cache rooted at `~/.a3s/buildcache`.
pub(crate) struct BuildCache {
    dir: PathBuf,
}

impl BuildCache {
    /// Open the build cache, creating its directory layout if needed.
    ///
    /// Returns `None` (so the build proceeds uncached) if the directories
    /// cannot be created.
    pub(crate) fn open() -> Option<Self> {
        Self::open_in(dirs_home().join("buildcache"))
    }

    /// Open a build cache rooted at an explicit directory.
    fn open_in(dir: PathBuf) -> Option<Self> {
        std::fs::create_dir_all(dir.join("blobs")).ok()?;
        std::fs::create_dir_all(dir.join("keys")).ok()?;
        Some(Self { dir })
    }

    fn lock(&self) -> std::io::Result<crate::file_lock::FileLock> {
        crate::file_lock::FileLock::acquire(&self.dir.join("cache"))
    }

    /// Compute the next chain key from the previous key, the canonical
    /// instruction representation, and an optional input hash.
    ///
    /// The key is `sha256(prev_key + "\n" + instruction_repr + ("\n" + input_hash)?)`.
    /// This makes the key order-sensitive and sensitive to every instruction
    /// (including config-only ones like ENV/WORKDIR, which affect later RUNs).
    pub(crate) fn chain(
        prev_key: &str,
        instruction_repr: &str,
        input_hash: Option<&str>,
    ) -> String {
        let mut buf = String::with_capacity(prev_key.len() + instruction_repr.len() + 1);
        buf.push_str(prev_key);
        buf.push('\n');
        buf.push_str(instruction_repr);
        if let Some(h) = input_hash {
            buf.push('\n');
            buf.push_str(h);
        }
        sha256_bytes(buf.as_bytes())
    }

    /// Look up a cached layer by chain key.
    ///
    /// Returns the cached layer only if its key record exists and the
    /// referenced blob is a regular file with the recorded size and digest.
    pub(crate) fn lookup(&self, key: &str) -> Option<CachedLayer> {
        let _lock = self.lock().ok()?;
        self.lookup_unlocked(key)
    }

    fn lookup_unlocked(&self, key: &str) -> Option<CachedLayer> {
        let key_path = self.dir.join("keys").join(key);
        let bytes = std::fs::read(&key_path).ok()?;
        let record: KeyRecord = serde_json::from_slice(&bytes).ok()?;

        let blob_path = self.dir.join("blobs").join(&record.digest);
        if !cached_blob_is_valid(&blob_path, &record.digest, record.size) {
            return None;
        }

        Some(CachedLayer {
            blob_path,
            digest: record.digest,
            diff_id: record.diff_id,
            size: record.size,
        })
    }

    /// Store a produced layer under the given chain key.
    ///
    /// Copies `layer.path` to `blobs/<layer.digest>` when the current blob is
    /// absent or invalid, then writes the `keys/<key>` record. Best-effort: I/O
    /// errors are ignored.
    pub(crate) fn store(&self, key: &str, layer: &LayerInfo, diff_id: &str) {
        let Ok(_lock) = self.lock() else {
            return;
        };
        self.store_unlocked(key, layer, diff_id);
    }

    fn store_unlocked(&self, key: &str, layer: &LayerInfo, diff_id: &str) {
        if self.publish_entry_unlocked(key, layer, diff_id) {
            self.prune_to_unlocked(configured_max_bytes());
        }
    }

    /// Publish one entry through the native cache's sole blob/key write
    /// boundary. The caller must hold the cache lock.
    fn publish_entry_unlocked(&self, key: &str, layer: &LayerInfo, diff_id: &str) -> bool {
        if !cached_blob_is_valid(&layer.path, &layer.digest, layer.size) {
            return false;
        }
        let blob_path = self.dir.join("blobs").join(&layer.digest);
        if !cached_blob_is_valid(&blob_path, &layer.digest, layer.size) {
            // Copy to a unique temp file then atomically rename into place, so a
            // concurrent build (or a copy that fails partway) never publishes a
            // half-written blob that a key record then points at.
            let seq = STORE_SEQ.fetch_add(1, Ordering::Relaxed);
            let staging = self.dir.join("blobs").join(format!(
                ".staging-{}-{}-{}",
                layer.digest,
                std::process::id(),
                seq
            ));
            if std::fs::copy(&layer.path, &staging).is_err() {
                let _ = std::fs::remove_file(&staging);
                return false;
            }
            if !cached_blob_is_valid(&staging, &layer.digest, layer.size) {
                let _ = std::fs::remove_file(&staging);
                return false;
            }
            // Windows cannot rename over an existing file. Removing a corrupt
            // destination first is safe because readers independently validate
            // cache blobs and rebuild on a miss or race.
            if blob_path.exists() && std::fs::remove_file(&blob_path).is_err() {
                let _ = std::fs::remove_file(&staging);
                return false;
            }
            if std::fs::rename(&staging, &blob_path).is_err() {
                let _ = std::fs::remove_file(&staging);
                return false;
            }
        }

        let record = KeyRecord {
            digest: layer.digest.clone(),
            diff_id: diff_id.to_string(),
            size: layer.size,
        };
        if let Ok(bytes) = serde_json::to_vec(&record) {
            let target = self.dir.join("keys").join(key);
            let temporary = target.with_extension("tmp");
            if a3s_box_core::fs_atomic::write_durable(&temporary, &target, &bytes).is_ok() {
                return true;
            }
        }
        false
    }

    /// Evict oldest layer blobs (by modification time) until the total blob
    /// size is at or below `cap` bytes. Best-effort; key records that point at
    /// an evicted blob simply miss on the next lookup (and the instruction is
    /// re-run), so eviction can never corrupt a build.
    #[cfg(test)]
    fn prune_to(&self, cap: u64) {
        let Ok(_lock) = self.lock() else {
            return;
        };
        self.prune_to_unlocked(cap);
    }

    fn prune_to_unlocked(&self, cap: u64) {
        let _ = self.prune_to_unlocked_preserving(cap, &BTreeSet::new());
    }

    /// Prune through the native cache authority while retaining the exact
    /// layer digests required by an in-progress hydration.
    ///
    /// Returns whether the configured cap could be reached. The caller must
    /// hold the cache lock.
    fn prune_to_unlocked_preserving(&self, cap: u64, preserved: &BTreeSet<String>) -> bool {
        let blobs_dir = self.dir.join("blobs");
        let Ok(read_dir) = std::fs::read_dir(&blobs_dir) else {
            return false;
        };

        let mut blobs: Vec<(std::time::SystemTime, u64, PathBuf, bool)> = Vec::new();
        let mut total: u64 = 0;
        for entry in read_dir.flatten() {
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            let len = meta.len();
            total = total.saturating_add(len);
            let mtime = meta.modified().unwrap_or(std::time::UNIX_EPOCH);
            let path = entry.path();
            let keep = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| preserved.contains(name));
            blobs.push((mtime, len, path, keep));
        }

        if total <= cap {
            return true;
        }

        blobs.sort_by_key(|(mtime, _, _, _)| *mtime); // oldest first
        for (_, len, path, keep) in blobs {
            if total <= cap {
                break;
            }
            if keep {
                continue;
            }
            if std::fs::remove_file(&path).is_ok() {
                total = total.saturating_sub(len);
            }
        }

        // Blobs were just evicted — drop key records that now point at nothing,
        // so the keys/ dir doesn't accumulate dangling pointers without bound.
        self.prune_orphan_keys();
        total <= cap
    }

    /// Remove key records whose referenced blob no longer exists. Each key is a
    /// small JSON record, but a long-lived build host edits-and-rebuilds many
    /// chain keys and `prune_to` only evicts blobs, so without this the keys/
    /// directory grows unbounded (and leaves dangling pointers after eviction).
    /// Best-effort; a missing keys/ dir is a no-op.
    fn prune_orphan_keys(&self) {
        let Ok(read_dir) = std::fs::read_dir(self.dir.join("keys")) else {
            return;
        };
        for entry in read_dir.flatten() {
            let path = entry.path();
            let keep = std::fs::read(&path)
                .ok()
                .and_then(|bytes| serde_json::from_slice::<KeyRecord>(&bytes).ok())
                .is_some_and(|record| self.dir.join("blobs").join(&record.digest).exists());
            if !keep {
                // Blob evicted, or an unparseable/truncated key record: cruft.
                let _ = std::fs::remove_file(&path);
            }
        }
    }
}

fn cached_blob_is_valid(path: &Path, digest: &str, size: u64) -> bool {
    if digest.len() != 64
        || !digest
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    {
        return false;
    }
    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    metadata.is_file()
        && !metadata.file_type().is_symlink()
        && metadata.len() == size
        && sha256_file(path).is_ok_and(|actual| actual == digest)
}

/// Hash the content of COPY/ADD source files for cache invalidation.
///
/// Each `src` pattern is resolved under `context_dir`. Files are hashed
/// recursively for directories, in a deterministic (sorted by relative path)
/// order; for each file the hash absorbs `relpath + len + bytes`. A changed
/// file therefore changes the resulting hash and invalidates the cache.
///
/// Returns `None` if any source cannot be read (so the caller treats it as a
/// cache miss and falls back to executing the instruction).
pub(crate) fn hash_context_sources(context_dir: &Path, src_patterns: &[String]) -> Option<String> {
    use sha2::{Digest, Sha256};

    let mut files: Vec<(PathBuf, PathBuf)> = Vec::new();
    for src in src_patterns {
        // Match handle_copy/handle_add: a leading slash is context-relative, not
        // a host absolute path (which `Path::join` would otherwise jump to).
        let src_path = context_dir.join(src.trim_start_matches('/'));
        if !src_path.exists() {
            return None;
        }
        if src_path.is_dir() {
            collect_files(&src_path, &src_path, &mut files)?;
        } else {
            let rel = PathBuf::from(src);
            files.push((rel, src_path));
        }
    }

    // Deterministic order regardless of filesystem traversal order.
    files.sort_by(|a, b| a.0.cmp(&b.0));

    let mut hasher = Sha256::new();
    for (rel, full) in &files {
        let bytes = std::fs::read(full).ok()?;
        hasher.update(rel.to_string_lossy().as_bytes());
        hasher.update(b"\0");
        hasher.update((bytes.len() as u64).to_le_bytes());
        hasher.update(&bytes);
    }
    Some(hex::encode(hasher.finalize()))
}

/// Recursively collect `(relative_path, full_path)` pairs for files under `root`.
fn collect_files(root: &Path, current: &Path, out: &mut Vec<(PathBuf, PathBuf)>) -> Option<()> {
    for entry in std::fs::read_dir(current).ok()? {
        let entry = entry.ok()?;
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else {
            let rel = path.strip_prefix(root).ok()?.to_path_buf();
            out.push((rel, path));
        }
    }
    Some(())
}

#[cfg(test)]
mod tests;
