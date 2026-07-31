//! Portable OCI artifact emitted by the native layer-cache authority.

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::platform::Platform;
use oci_spec::image::{
    Arch, Descriptor, DescriptorBuilder, ImageIndexBuilder, ImageManifestBuilder, MediaType, Os,
    PlatformBuilder, Sha256Digest, SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};

use super::{cached_blob_is_valid, BuildCache, CachedLayer};
use crate::oci::build::layer::sha256_bytes;
use crate::oci::build::BuildOutputDescriptor;
use crate::oci::image::canonical_sha256_digest_hex;

pub const BUILD_CACHE_ARTIFACT_MEDIA_TYPE: &str = "application/vnd.a3s.box.build-cache.v1";
pub const BUILD_CACHE_CONFIG_MEDIA_TYPE: &str =
    "application/vnd.a3s.box.build-cache.config.v1+json";

const CACHE_CONFIG_SCHEMA: &str = "a3s.box.build-cache-config.v1";
const MAX_CACHE_ENTRIES: usize = 16 * 1024;

mod model;
#[cfg(test)]
mod tests;
mod validation;

pub(in crate::oci::build) use model::BuildCacheExportIdentity;
pub use model::{BuildCacheReceipt, RecordedBuildCache};
pub(in crate::oci::build) use validation::inspect_build_cache_artifact;
pub(super) use validation::{validate_build_cache_artifact, ValidatedCacheEntry};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CacheEntry {
    key: String,
    layer: BuildOutputDescriptor,
    diff_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct CacheConfig {
    schema: String,
    key: String,
    source_digest: String,
    plan_digest: String,
    platform: Platform,
    entries: Vec<CacheEntry>,
}

/// Exact layer keys observed through the one native cache implementation.
#[derive(Debug, Default)]
pub(in crate::oci::build) struct BuildCacheTrace {
    entries: BTreeMap<String, CacheEntry>,
}

impl BuildCacheTrace {
    pub(in crate::oci::build) fn record(
        &mut self,
        raw_key: &str,
        layer: &CachedLayer,
    ) -> Result<()> {
        let key = prefixed_digest(raw_key, "native cache chain key")?;
        let entry = CacheEntry {
            key,
            layer: BuildOutputDescriptor {
                media_type: MediaType::ImageLayerGzip.as_ref().to_string(),
                digest: prefixed_digest(&layer.digest, "native cache layer digest")?,
                size: layer.size,
            },
            diff_id: prefixed_digest(&layer.diff_id, "native cache diff ID")?,
        };
        if let Some(existing) = self.entries.insert(raw_key.to_string(), entry.clone()) {
            if existing != entry {
                return Err(cache_error(
                    "one native cache chain key resolved to conflicting content",
                ));
            }
        }
        if self.entries.len() > MAX_CACHE_ENTRIES {
            return Err(cache_error("native build cache entry bound was exceeded"));
        }
        Ok(())
    }
}

impl BuildCache {
    pub(in crate::oci::build) fn stage_export(
        &self,
        trace: &BuildCacheTrace,
        identity: &BuildCacheExportIdentity,
        staging: &Path,
    ) -> Result<RecordedBuildCache> {
        let _lock = self
            .lock()
            .map_err(|error| cache_error(format!("failed to lock native cache: {error}")))?;
        match std::fs::symlink_metadata(staging) {
            Ok(_) => {
                return Err(cache_error(
                    "native cache export staging path already exists",
                ))
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(cache_error(format!(
                    "failed to inspect native cache export staging path: {error}"
                )))
            }
        }
        let blob_root = staging.join("blobs").join("sha256");
        std::fs::create_dir_all(&blob_root).map_err(|error| {
            cache_error(format!("failed to create native cache export: {error}"))
        })?;

        let mut entries = Vec::with_capacity(trace.entries.len());
        let mut layers = BTreeMap::<String, Descriptor>::new();
        for (raw_key, expected) in &trace.entries {
            let actual = self.lookup_unlocked(raw_key).ok_or_else(|| {
                cache_error(format!(
                    "native cache entry {} disappeared before export",
                    expected.key
                ))
            })?;
            let actual_entry = CacheEntry {
                key: expected.key.clone(),
                layer: BuildOutputDescriptor {
                    media_type: MediaType::ImageLayerGzip.as_ref().to_string(),
                    digest: prefixed_digest(&actual.digest, "native cache layer digest")?,
                    size: actual.size,
                },
                diff_id: prefixed_digest(&actual.diff_id, "native cache diff ID")?,
            };
            if &actual_entry != expected {
                return Err(cache_error(format!(
                    "native cache entry {} changed before export",
                    expected.key
                )));
            }
            let target = blob_root.join(&actual.digest);
            if !target.exists() {
                // The export is operation-owned evidence, not another view of
                // the mutable BuildCache authority. A copy prevents replay
                // validation, publication, or cleanup from mutating the cache
                // blob through a shared inode.
                std::fs::copy(&actual.blob_path, &target).map_err(|error| {
                    cache_error(format!(
                        "failed to copy native cache layer {}: {error}",
                        actual_entry.layer.digest
                    ))
                })?;
            }
            if !cached_blob_is_valid(&target, &actual.digest, actual.size) {
                return Err(cache_error(format!(
                    "exported native cache layer {} failed verification",
                    actual_entry.layer.digest
                )));
            }
            layers
                .entry(actual_entry.layer.digest.clone())
                .or_insert(build_descriptor(
                    MediaType::ImageLayerGzip,
                    actual.size,
                    &actual.digest,
                )?);
            entries.push(actual_entry);
        }

        let config = CacheConfig {
            schema: CACHE_CONFIG_SCHEMA.to_string(),
            key: identity.key.clone(),
            source_digest: identity.source_digest.clone(),
            plan_digest: identity.plan_digest.clone(),
            platform: identity.platform.clone(),
            entries,
        };
        let config_bytes = serde_json::to_vec(&config)
            .map_err(|error| cache_error(format!("failed to encode cache config: {error}")))?;
        let config_descriptor =
            write_json_blob(&blob_root, BUILD_CACHE_CONFIG_MEDIA_TYPE, &config_bytes)?;
        let manifest = ImageManifestBuilder::default()
            .schema_version(SCHEMA_VERSION)
            .media_type(MediaType::ImageManifest)
            .artifact_type(MediaType::from(BUILD_CACHE_ARTIFACT_MEDIA_TYPE))
            .config(config_descriptor)
            .layers(layers.into_values().collect::<Vec<_>>())
            .build()
            .map_err(|error| cache_error(format!("failed to build cache manifest: {error}")))?;
        let manifest_bytes = serde_json::to_vec(&manifest)
            .map_err(|error| cache_error(format!("failed to encode cache manifest: {error}")))?;
        let manifest_hex = sha256_bytes(&manifest_bytes);
        std::fs::write(blob_root.join(&manifest_hex), &manifest_bytes).map_err(|error| {
            cache_error(format!("failed to write native cache manifest: {error}"))
        })?;

        let mut platform = PlatformBuilder::default()
            .architecture(Arch::from(identity.platform.architecture.as_str()))
            .os(Os::from(identity.platform.os.as_str()));
        if let Some(variant) = &identity.platform.variant {
            platform = platform.variant(variant.clone());
        }
        let root_descriptor = DescriptorBuilder::default()
            .media_type(MediaType::ImageManifest)
            .artifact_type(MediaType::from(BUILD_CACHE_ARTIFACT_MEDIA_TYPE))
            .digest(parse_digest(&manifest_hex)?)
            .size(manifest_bytes.len() as u64)
            .platform(
                platform
                    .build()
                    .map_err(|error| cache_error(format!("invalid cache platform: {error}")))?,
            )
            .build()
            .map_err(|error| cache_error(format!("invalid cache root descriptor: {error}")))?;
        let index = ImageIndexBuilder::default()
            .schema_version(SCHEMA_VERSION)
            .media_type(MediaType::ImageIndex)
            .artifact_type(MediaType::from(BUILD_CACHE_ARTIFACT_MEDIA_TYPE))
            .manifests(vec![root_descriptor])
            .build()
            .map_err(|error| cache_error(format!("failed to build cache index: {error}")))?;
        std::fs::write(
            staging.join("index.json"),
            serde_json::to_vec(&index)
                .map_err(|error| cache_error(format!("failed to encode cache index: {error}")))?,
        )
        .map_err(|error| cache_error(format!("failed to write cache index: {error}")))?;
        std::fs::write(
            staging.join("oci-layout"),
            br#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .map_err(|error| cache_error(format!("failed to write cache layout marker: {error}")))?;

        inspect_build_cache_artifact(staging, identity, None)
    }
}

fn write_json_blob(root: &Path, media_type: &str, bytes: &[u8]) -> Result<Descriptor> {
    let digest = sha256_bytes(bytes);
    std::fs::write(root.join(&digest), bytes)
        .map_err(|error| cache_error(format!("failed to write native cache blob: {error}")))?;
    build_descriptor(MediaType::from(media_type), bytes.len() as u64, &digest)
}

fn build_descriptor(media_type: MediaType, size: u64, digest: &str) -> Result<Descriptor> {
    DescriptorBuilder::default()
        .media_type(media_type)
        .digest(parse_digest(digest)?)
        .size(size)
        .build()
        .map_err(|error| cache_error(format!("invalid native cache descriptor: {error}")))
}

fn parse_digest(hex: &str) -> Result<Sha256Digest> {
    Sha256Digest::from_str(hex)
        .map_err(|error| cache_error(format!("invalid native cache digest: {error}")))
}

fn prefixed_digest(hex: &str, label: &str) -> Result<String> {
    let digest = format!("sha256:{hex}");
    canonical_sha256_digest_hex(&digest)
        .map_err(|_| cache_error(format!("{label} is not canonical SHA-256")))?;
    Ok(digest)
}

fn cache_error(message: impl Into<String>) -> BoxError {
    BoxError::BuildError(message.into())
}
