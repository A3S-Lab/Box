//! Hydration of a validated portable cache artifact into `BuildCache`.

use std::collections::{BTreeMap, BTreeSet};

use a3s_box_core::error::{BoxError, Result};

use super::export::{validate_build_cache_artifact, ValidatedCacheEntry};
use super::{
    configured_max_bytes, BuildCache, BuildCacheExportIdentity, BuildCacheReceipt, CachedLayer,
    RecordedBuildCache,
};
use crate::oci::build::layer::LayerInfo;

/// Revalidate one portable cache artifact and hydrate it into the sole native
/// [`BuildCache`] authority.
///
/// The artifact is validated before the cache lock is acquired. Hydration then
/// serializes with native cache hits and stores through that existing lock.
/// Existing identical entries are idempotent; an existing valid key that maps
/// to different content is rejected before any imported key is published.
pub async fn hydrate_recorded_build_cache(
    recorded: &RecordedBuildCache,
) -> Result<BuildCacheReceipt> {
    let recorded = RecordedBuildCache {
        receipt: recorded.receipt.clone(),
        layout_directory: recorded.layout_directory.clone(),
    };
    tokio::task::spawn_blocking(move || {
        let cache = BuildCache::open().ok_or_else(|| {
            hydration_error("the native BuildCache directory could not be opened")
        })?;
        cache.hydrate_recorded(&recorded)
    })
    .await
    .map_err(|error| hydration_error(format!("cache hydration task failed: {error}")))?
}

impl BuildCache {
    fn hydrate_recorded(&self, recorded: &RecordedBuildCache) -> Result<BuildCacheReceipt> {
        let identity = BuildCacheExportIdentity::new(
            recorded.receipt.source_digest.clone(),
            recorded.receipt.plan_digest.clone(),
            recorded.receipt.platform.clone(),
        )?;
        let artifact = validate_build_cache_artifact(
            &recorded.layout_directory,
            &identity,
            Some(&recorded.receipt),
        )?;
        let layers = artifact
            .entries
            .iter()
            .map(|entry| (entry.digest.clone(), entry.size))
            .collect::<BTreeMap<_, _>>();
        let required_bytes = layers.values().try_fold(0_u64, |total, size| {
            total
                .checked_add(*size)
                .ok_or_else(|| hydration_error("the imported cache layer byte count overflowed"))
        })?;
        let capacity = configured_max_bytes();
        if required_bytes > capacity {
            return Err(hydration_error(format!(
                "the artifact requires {required_bytes} layer bytes but the BuildCache cap is {capacity}"
            )));
        }
        let _lock = self
            .lock()
            .map_err(|error| hydration_error(format!("failed to lock BuildCache: {error}")))?;

        for entry in &artifact.entries {
            if let Some(existing) = self.lookup_unlocked(&entry.key) {
                if !entry_matches(entry, &existing) {
                    return Err(hydration_error(format!(
                        "cache key sha256:{} already maps to different verified content",
                        entry.key
                    )));
                }
            }
        }

        for entry in &artifact.entries {
            let layer = LayerInfo {
                path: entry.blob_path.clone(),
                digest: entry.digest.clone(),
                size: entry.size,
            };
            if !self.publish_entry_unlocked(&entry.key, &layer, &entry.diff_id) {
                return Err(hydration_error(format!(
                    "failed to publish cache key sha256:{} through BuildCache",
                    entry.key
                )));
            }
            let hydrated = self.lookup_unlocked(&entry.key).ok_or_else(|| {
                hydration_error(format!(
                    "failed to publish cache key sha256:{} through BuildCache",
                    entry.key
                ))
            })?;
            if !entry_matches(entry, &hydrated) {
                return Err(hydration_error(format!(
                    "cache key sha256:{} changed while it was being hydrated",
                    entry.key
                )));
            }
        }

        let preserved = layers.into_keys().collect::<BTreeSet<_>>();
        if !self.prune_to_unlocked_preserving(capacity, &preserved) {
            return Err(hydration_error(format!(
                "the BuildCache cap of {capacity} bytes could not be enforced while retaining the imported artifact"
            )));
        }

        for entry in &artifact.entries {
            let hydrated = self.lookup_unlocked(&entry.key).ok_or_else(|| {
                hydration_error(format!(
                    "cache key sha256:{} was evicted before hydration completed",
                    entry.key
                ))
            })?;
            if !entry_matches(entry, &hydrated) {
                return Err(hydration_error(format!(
                    "cache key sha256:{} differs after hydration completed",
                    entry.key
                )));
            }
        }

        Ok(artifact.recorded.receipt)
    }
}

fn entry_matches(expected: &ValidatedCacheEntry, actual: &CachedLayer) -> bool {
    actual.digest == expected.digest
        && actual.diff_id == expected.diff_id
        && actual.size == expected.size
}

fn hydration_error(message: impl Into<String>) -> BoxError {
    BoxError::BuildError(format!(
        "native BuildCache hydration failed: {}",
        message.into()
    ))
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::Path;

    use a3s_box_core::platform::Platform;
    use tempfile::TempDir;

    use super::super::{BuildCacheTrace, CachedLayer};
    use super::*;
    use crate::oci::build::layer::{sha256_bytes, LayerInfo};

    struct CacheFixture {
        recorded: RecordedBuildCache,
        first_key: String,
        first_layer: CachedLayer,
        second_key: String,
        second_layer: CachedLayer,
    }

    fn store_layer(
        cache: &BuildCache,
        root: &Path,
        name: &str,
        key: &str,
        contents: &[u8],
    ) -> CachedLayer {
        let path = root.join(name);
        fs::write(&path, contents).unwrap();
        let layer = LayerInfo {
            path,
            digest: sha256_bytes(contents),
            size: contents.len() as u64,
        };
        cache.store(
            key,
            &layer,
            &sha256_bytes(format!("{name}-diff").as_bytes()),
        );
        cache.lookup(key).expect("stored cache entry")
    }

    fn export_fixture(root: &Path) -> CacheFixture {
        let source = BuildCache::open_in(root.join("source-cache")).unwrap();
        let first_key = BuildCache::chain("", "COPY first /first", Some("first"));
        let second_key = BuildCache::chain(&first_key, "RUN second", None);
        let first_layer = store_layer(
            &source,
            root,
            "first-layer.tar.gz",
            &first_key,
            b"first portable layer",
        );
        let second_layer = store_layer(
            &source,
            root,
            "second-layer.tar.gz",
            &second_key,
            b"second portable layer",
        );
        let mut trace = BuildCacheTrace::default();
        trace.record(&first_key, &first_layer).unwrap();
        trace.record(&second_key, &second_layer).unwrap();
        let identity = BuildCacheExportIdentity::new(
            format!("sha256:{}", "1".repeat(64)),
            format!("sha256:{}", "2".repeat(64)),
            Platform::linux_amd64(),
        )
        .unwrap();
        let recorded = source
            .stage_export(&trace, &identity, &root.join("cache-artifact"))
            .unwrap();
        CacheFixture {
            recorded,
            first_key,
            first_layer,
            second_key,
            second_layer,
        }
    }

    #[test]
    fn hydrate_revalidates_and_populates_the_existing_cache_authority() {
        let tmp = TempDir::new().unwrap();
        let fixture = export_fixture(tmp.path());
        let target = BuildCache::open_in(tmp.path().join("target-cache")).unwrap();

        let receipt = target
            .hydrate_recorded(&fixture.recorded)
            .expect("hydrate validated cache artifact");

        assert_eq!(receipt, fixture.recorded.receipt);
        assert_eq!(
            target.lookup(&fixture.first_key).unwrap().digest,
            fixture.first_layer.digest
        );
        assert_eq!(
            target.lookup(&fixture.second_key).unwrap().digest,
            fixture.second_layer.digest
        );

        fs::remove_dir_all(&fixture.recorded.layout_directory).unwrap();
        assert!(target.lookup(&fixture.first_key).is_some());
        assert!(target.lookup(&fixture.second_key).is_some());
    }

    #[test]
    fn hydrate_is_idempotent() {
        let tmp = TempDir::new().unwrap();
        let fixture = export_fixture(tmp.path());
        let target_dir = tmp.path().join("target-cache");
        let target = BuildCache::open_in(target_dir.clone()).unwrap();

        let first = target.hydrate_recorded(&fixture.recorded).unwrap();
        let second = target.hydrate_recorded(&fixture.recorded).unwrap();

        assert_eq!(first, second);
        assert_eq!(fs::read_dir(target_dir.join("keys")).unwrap().count(), 2);
        assert_eq!(fs::read_dir(target_dir.join("blobs")).unwrap().count(), 2);
    }

    #[test]
    fn hydrate_rejects_artifact_tampering_before_writing_keys() {
        let tmp = TempDir::new().unwrap();
        let fixture = export_fixture(tmp.path());
        let target = BuildCache::open_in(tmp.path().join("target-cache")).unwrap();
        let layer = fixture
            .recorded
            .layout_directory
            .join("blobs/sha256")
            .join(&fixture.first_layer.digest);
        fs::write(layer, b"tampered").unwrap();

        assert!(target.hydrate_recorded(&fixture.recorded).is_err());
        assert!(target.lookup(&fixture.first_key).is_none());
        assert!(target.lookup(&fixture.second_key).is_none());
    }

    #[test]
    fn hydrate_rejects_a_valid_local_key_conflict_without_partial_import() {
        let tmp = TempDir::new().unwrap();
        let fixture = export_fixture(tmp.path());
        let target = BuildCache::open_in(tmp.path().join("target-cache")).unwrap();
        let conflict = store_layer(
            &target,
            tmp.path(),
            "conflicting-layer.tar.gz",
            &fixture.first_key,
            b"different deterministic output",
        );

        assert!(target.hydrate_recorded(&fixture.recorded).is_err());
        assert_eq!(
            target.lookup(&fixture.first_key).unwrap().digest,
            conflict.digest
        );
        assert!(target.lookup(&fixture.second_key).is_none());
    }
}
