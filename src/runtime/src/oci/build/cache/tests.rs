use std::fs;
use std::path::Path;

use tempfile::TempDir;

use super::*;

/// Test constructor: open a cache at an explicit directory.
fn open_at(dir: &Path) -> BuildCache {
    BuildCache::open_in(dir.to_path_buf()).expect("open build cache at temp dir")
}

#[test]
fn test_chain_is_deterministic() {
    let a = BuildCache::chain("prev", "RUN echo hi", None);
    let b = BuildCache::chain("prev", "RUN echo hi", None);
    assert_eq!(a, b);
    assert_eq!(a.len(), 64);
}

#[test]
fn test_chain_is_order_sensitive() {
    let a = BuildCache::chain("prev", "RUN echo a", None);
    let b = BuildCache::chain("prev", "RUN echo b", None);
    assert_ne!(a, b);

    let c = BuildCache::chain("prev1", "RUN echo a", None);
    let d = BuildCache::chain("prev2", "RUN echo a", None);
    assert_ne!(c, d);
}

#[test]
fn test_chain_input_hash_changes_key() {
    let none = BuildCache::chain("prev", "COPY . /app", None);
    let some = BuildCache::chain("prev", "COPY . /app", Some("deadbeef"));
    assert_ne!(none, some);

    let other = BuildCache::chain("prev", "COPY . /app", Some("cafebabe"));
    assert_ne!(some, other);
}

#[test]
fn test_store_then_lookup_round_trips() {
    let tmp = TempDir::new().unwrap();
    let cache_dir = tmp.path().join("buildcache");
    let cache = open_at(&cache_dir);

    let layer_path = tmp.path().join("layer.tar.gz");
    let contents = b"fake layer contents";
    fs::write(&layer_path, contents).unwrap();
    let layer = LayerInfo {
        path: layer_path,
        digest: sha256_bytes(contents),
        size: contents.len() as u64,
    };

    let key = BuildCache::chain("", "RUN echo hi", None);
    assert!(cache.lookup(&key).is_none());

    cache.store(&key, &layer, "diff-id-xyz");

    let hit = cache
        .lookup(&key)
        .expect("expected a cache hit after store");
    assert_eq!(hit.digest, sha256_bytes(contents));
    assert_eq!(hit.diff_id, "diff-id-xyz");
    assert_eq!(hit.size, contents.len() as u64);
    assert!(hit.blob_path.exists());
    assert_eq!(fs::read(&hit.blob_path).unwrap(), b"fake layer contents");
}

#[test]
fn prune_evicts_orphan_key_records() {
    let tmp = TempDir::new().unwrap();
    let cache_dir = tmp.path().join("buildcache");
    let cache = open_at(&cache_dir);

    let layer_path = tmp.path().join("layer.tar.gz");
    let contents = vec![0u8; 4096];
    fs::write(&layer_path, &contents).unwrap();
    let layer = LayerInfo {
        path: layer_path,
        digest: sha256_bytes(&contents),
        size: 4096,
    };
    let key = BuildCache::chain("", "RUN make", None);
    cache.store(&key, &layer, "diff");

    let blob = cache_dir.join("blobs").join(&layer.digest);
    let key_file = cache_dir.join("keys").join(&key);
    assert!(blob.exists() && key_file.exists());

    cache.prune_to(0);
    assert!(!blob.exists(), "blob should be evicted");
    assert!(!key_file.exists(), "orphaned key record should be pruned");
}

#[test]
fn test_lookup_misses_when_blob_removed() {
    let tmp = TempDir::new().unwrap();
    let cache = open_at(&tmp.path().join("buildcache"));

    let layer_path = tmp.path().join("layer.tar.gz");
    let contents = b"data";
    fs::write(&layer_path, contents).unwrap();
    let layer = LayerInfo {
        path: layer_path,
        digest: sha256_bytes(contents),
        size: contents.len() as u64,
    };
    let key = BuildCache::chain("", "RUN x", None);
    cache.store(&key, &layer, "diff");

    fs::remove_file(tmp.path().join("buildcache/blobs").join(&layer.digest)).unwrap();
    assert!(cache.lookup(&key).is_none());
}

#[test]
fn test_lookup_rejects_and_store_repairs_corrupt_blob() {
    let tmp = TempDir::new().unwrap();
    let cache_dir = tmp.path().join("buildcache");
    let cache = open_at(&cache_dir);
    let contents = b"verified layer";
    let layer_path = tmp.path().join("layer.tar.gz");
    fs::write(&layer_path, contents).unwrap();
    let layer = LayerInfo {
        path: layer_path,
        digest: sha256_bytes(contents),
        size: contents.len() as u64,
    };
    let key = BuildCache::chain("", "COPY value /value", None);
    cache.store(&key, &layer, "diff");

    let blob = cache_dir.join("blobs").join(&layer.digest);
    fs::write(&blob, b"corrupted data").unwrap();
    assert!(cache.lookup(&key).is_none());

    cache.store(&key, &layer, "diff");
    assert_eq!(fs::read(&blob).unwrap(), contents);
    assert!(cache.lookup(&key).is_some());
}

#[test]
fn test_hash_context_sources_detects_change() {
    let ctx = TempDir::new().unwrap();
    fs::write(ctx.path().join("a.txt"), "hello").unwrap();
    fs::create_dir(ctx.path().join("sub")).unwrap();
    fs::write(ctx.path().join("sub/b.txt"), "world").unwrap();

    let srcs = vec![".".to_string()];
    let h1 = hash_context_sources(ctx.path(), &srcs).unwrap();
    let h2 = hash_context_sources(ctx.path(), &srcs).unwrap();
    assert_eq!(h1, h2, "stable hash for unchanged content");

    fs::write(ctx.path().join("a.txt"), "HELLO").unwrap();
    let h3 = hash_context_sources(ctx.path(), &srcs).unwrap();
    assert_ne!(h1, h3, "changed content must change the hash");
}

#[test]
fn test_prune_evicts_until_under_cap() {
    let tmp = TempDir::new().unwrap();
    let cache = open_at(&tmp.path().join("buildcache"));

    for i in 0..3 {
        let payload = vec![b'x' + i as u8; 100];
        let src = tmp.path().join(format!("src{i}"));
        fs::write(&src, &payload).unwrap();
        let layer = LayerInfo {
            path: src,
            digest: sha256_bytes(&payload),
            size: payload.len() as u64,
        };
        cache.store(
            &BuildCache::chain("", &format!("RUN step {i}"), None),
            &layer,
            "d",
        );
    }

    let blobs_dir = tmp.path().join("buildcache/blobs");
    let total = |dir: &Path| -> u64 {
        fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.metadata().unwrap().len())
            .sum()
    };
    assert_eq!(total(&blobs_dir), 300, "three blobs stored");

    cache.prune_to(150);
    assert!(
        total(&blobs_dir) <= 150,
        "prune must bring total under the cap"
    );
    assert!(total(&blobs_dir) > 0, "prune must keep what fits");
}

#[test]
fn hydration_prune_preserves_the_imported_layer_set() {
    let tmp = TempDir::new().unwrap();
    let cache_dir = tmp.path().join("buildcache");
    let cache = open_at(&cache_dir);
    let mut digests = Vec::new();

    for i in 0..3 {
        let payload = vec![b'a' + i as u8; 100];
        let src = tmp.path().join(format!("protected-src-{i}"));
        fs::write(&src, &payload).unwrap();
        let layer = LayerInfo {
            path: src,
            digest: sha256_bytes(&payload),
            size: payload.len() as u64,
        };
        cache.store(
            &BuildCache::chain("", &format!("COPY protected {i}"), None),
            &layer,
            "d",
        );
        digests.push(layer.digest);
    }

    let preserved = BTreeSet::from([digests[0].clone()]);
    let _lock = cache.lock().unwrap();
    assert!(cache.prune_to_unlocked_preserving(100, &preserved));
    assert!(cache_dir.join("blobs").join(&digests[0]).exists());
    assert!(!cache_dir.join("blobs").join(&digests[1]).exists());
    assert!(!cache_dir.join("blobs").join(&digests[2]).exists());
}

#[test]
fn test_hash_context_sources_missing_source_is_none() {
    let ctx = TempDir::new().unwrap();
    let srcs = vec!["does-not-exist".to_string()];
    assert!(hash_context_sources(ctx.path(), &srcs).is_none());
}
