//! Portable OCI artifact emitted by the native layer-cache authority.

use std::collections::BTreeMap;
use std::path::Path;
use std::str::FromStr;

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::platform::Platform;
use oci_spec::image::{
    Arch, Descriptor, DescriptorBuilder, ImageIndex, ImageIndexBuilder, ImageManifest,
    ImageManifestBuilder, MediaType, Os, PlatformBuilder, Sha256Digest, SCHEMA_VERSION,
};
use serde::{Deserialize, Serialize};

use super::{cached_blob_is_valid, BuildCache, CachedLayer};
use crate::oci::build::layer::sha256_bytes;
use crate::oci::build::layout::{
    metadata_bytes, validate_blob_inventory, validate_exact_entries, EntryKind,
};
use crate::oci::build::BuildOutputDescriptor;
use crate::oci::image::{
    canonical_sha256_digest_hex, read_regular_file_bounded, read_verified_oci_blob,
    verify_oci_blob_file, MAX_OCI_CONFIG_BYTES, MAX_OCI_INDEX_BYTES, MAX_OCI_LAYER_BLOB_BYTES,
    MAX_OCI_MANIFEST_BYTES,
};

pub const BUILD_CACHE_ARTIFACT_MEDIA_TYPE: &str = "application/vnd.a3s.box.build-cache.v1";
pub const BUILD_CACHE_CONFIG_MEDIA_TYPE: &str =
    "application/vnd.a3s.box.build-cache.config.v1+json";

const CACHE_CONFIG_SCHEMA: &str = "a3s.box.build-cache-config.v1";
const MAX_CACHE_ENTRIES: usize = 16 * 1024;

mod model;
#[cfg(test)]
mod tests;

pub(in crate::oci::build) use model::BuildCacheExportIdentity;
pub use model::{BuildCacheReceipt, RecordedBuildCache};

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

        inspect_build_cache_export(staging, identity, None)
    }
}

pub(in crate::oci::build) fn inspect_build_cache_export(
    root: &Path,
    identity: &BuildCacheExportIdentity,
    expected: Option<&BuildCacheReceipt>,
) -> Result<RecordedBuildCache> {
    validate_exact_entries(
        root,
        &[
            ("blobs", EntryKind::Directory),
            ("index.json", EntryKind::File),
            ("oci-layout", EntryKind::File),
        ],
        "native cache OCI layout",
    )?;
    validate_exact_entries(
        &root.join("blobs"),
        &[("sha256", EntryKind::Directory)],
        "native cache OCI blob root",
    )?;
    let index_bytes =
        read_regular_file_bounded(&root.join("index.json"), MAX_OCI_INDEX_BYTES, "cache index")?;
    let index: ImageIndex = serde_json::from_slice(&index_bytes)
        .map_err(|error| cache_error(format!("native cache index is invalid: {error}")))?;
    require_index(&index, &identity.platform)?;
    let root_descriptor = index.manifests()[0].clone();
    let manifest_bytes = read_descriptor(
        root,
        &root_descriptor,
        MAX_OCI_MANIFEST_BYTES,
        "cache manifest",
    )?;
    let manifest: ImageManifest = serde_json::from_slice(&manifest_bytes)
        .map_err(|error| cache_error(format!("native cache manifest is invalid: {error}")))?;
    require_manifest(&manifest)?;
    let config_bytes = read_descriptor(
        root,
        manifest.config(),
        MAX_OCI_CONFIG_BYTES,
        "cache config",
    )?;
    let config: CacheConfig = serde_json::from_slice(&config_bytes)
        .map_err(|error| cache_error(format!("native cache config is invalid: {error}")))?;
    validate_config(&config, identity, manifest.layers())?;
    for layer in manifest.layers() {
        verify_descriptor_file(root, layer, "cache layer")?;
    }

    let mut descriptors = Vec::with_capacity(manifest.layers().len() + 2);
    descriptors.push(root_descriptor.clone());
    descriptors.push(manifest.config().clone());
    descriptors.extend(manifest.layers().iter().cloned());
    let (blob_count, blob_bytes, blob_inventory_digest) =
        validate_blob_inventory(root, &descriptors, "Native cache OCI artifact")?;
    let content_bytes = metadata_bytes(root, "Native cache OCI artifact")?
        .checked_add(blob_bytes)
        .ok_or_else(|| cache_error("native cache artifact byte count overflowed"))?;
    let receipt = BuildCacheReceipt {
        schema: BuildCacheReceipt::SCHEMA.to_string(),
        key: identity.key.clone(),
        source_digest: identity.source_digest.clone(),
        plan_digest: identity.plan_digest.clone(),
        descriptor: BuildOutputDescriptor {
            media_type: root_descriptor.media_type().as_ref().to_string(),
            digest: root_descriptor.digest().to_string(),
            size: root_descriptor.size(),
        },
        platform: identity.platform.clone(),
        content_bytes,
        entry_count: config.entries.len() as u64,
        blob_count: blob_count as u64,
        blob_inventory_digest,
    };
    receipt.validate()?;
    if expected.is_some_and(|expected| expected != &receipt) {
        return Err(cache_error(
            "revalidated native cache artifact differs from its receipt",
        ));
    }
    Ok(RecordedBuildCache {
        receipt,
        layout_directory: root.to_path_buf(),
    })
}

fn validate_config(
    config: &CacheConfig,
    identity: &BuildCacheExportIdentity,
    layers: &[Descriptor],
) -> Result<()> {
    if config.schema != CACHE_CONFIG_SCHEMA
        || config.key != identity.key
        || config.source_digest != identity.source_digest
        || config.plan_digest != identity.plan_digest
        || config.platform != identity.platform
        || config.entries.len() > MAX_CACHE_ENTRIES
    {
        return Err(cache_error("native cache config identity is invalid"));
    }
    let mut previous = None;
    let mut expected_layers = BTreeMap::new();
    for entry in &config.entries {
        if previous.as_ref().is_some_and(|key| key >= &entry.key)
            || canonical_sha256_digest_hex(&entry.key).is_err()
            || canonical_sha256_digest_hex(&entry.diff_id).is_err()
            || canonical_sha256_digest_hex(&entry.layer.digest).is_err()
            || entry.layer.media_type != MediaType::ImageLayerGzip.as_ref()
            || entry.layer.size == 0
        {
            return Err(cache_error("native cache config entry is invalid"));
        }
        previous = Some(entry.key.clone());
        expected_layers.insert(
            entry.layer.digest.clone(),
            (entry.layer.media_type.clone(), entry.layer.size),
        );
    }
    let actual_layers = layers
        .iter()
        .map(|layer| {
            (
                layer.digest().to_string(),
                (layer.media_type().as_ref().to_string(), layer.size()),
            )
        })
        .collect::<Vec<_>>();
    let expected_layers = expected_layers.into_iter().collect::<Vec<_>>();
    if actual_layers != expected_layers {
        return Err(cache_error(
            "native cache manifest layers differ from the cache config",
        ));
    }
    Ok(())
}

fn require_index(index: &ImageIndex, platform: &Platform) -> Result<()> {
    if index.schema_version() != SCHEMA_VERSION
        || index.media_type().as_ref() != Some(&MediaType::ImageIndex)
        || index.artifact_type().as_ref() != Some(&MediaType::from(BUILD_CACHE_ARTIFACT_MEDIA_TYPE))
        || index.subject().is_some()
        || index.annotations().is_some()
        || index.manifests().len() != 1
    {
        return Err(cache_error("native cache OCI index shape is invalid"));
    }
    let descriptor = &index.manifests()[0];
    require_descriptor(descriptor, MediaType::ImageManifest, true)?;
    let Some(actual) = descriptor.platform() else {
        return Err(cache_error("native cache root descriptor has no platform"));
    };
    if actual.os().to_string() != platform.os
        || actual.architecture().to_string() != platform.architecture
        || actual.variant() != &platform.variant
        || descriptor.artifact_type().as_ref()
            != Some(&MediaType::from(BUILD_CACHE_ARTIFACT_MEDIA_TYPE))
    {
        return Err(cache_error("native cache root platform is invalid"));
    }
    Ok(())
}

fn require_manifest(manifest: &ImageManifest) -> Result<()> {
    if manifest.schema_version() != SCHEMA_VERSION
        || manifest.media_type().as_ref() != Some(&MediaType::ImageManifest)
        || manifest.artifact_type().as_ref()
            != Some(&MediaType::from(BUILD_CACHE_ARTIFACT_MEDIA_TYPE))
        || manifest.subject().is_some()
        || manifest.annotations().is_some()
        || manifest.config().media_type().as_ref() != BUILD_CACHE_CONFIG_MEDIA_TYPE
    {
        return Err(cache_error("native cache OCI manifest shape is invalid"));
    }
    require_descriptor(
        manifest.config(),
        MediaType::from(BUILD_CACHE_CONFIG_MEDIA_TYPE),
        false,
    )?;
    for layer in manifest.layers() {
        require_descriptor(layer, MediaType::ImageLayerGzip, false)?;
    }
    Ok(())
}

fn require_descriptor(descriptor: &Descriptor, media_type: MediaType, root: bool) -> Result<()> {
    if descriptor.media_type() != &media_type
        || descriptor.size() == 0
        || canonical_sha256_digest_hex(descriptor.digest().as_ref()).is_err()
        || descriptor.urls().is_some()
        || descriptor.annotations().is_some()
        || descriptor.data().is_some()
        || (!root && descriptor.platform().is_some())
        || (!root && descriptor.artifact_type().is_some())
    {
        return Err(cache_error("native cache OCI descriptor is invalid"));
    }
    Ok(())
}

fn read_descriptor(
    root: &Path,
    descriptor: &Descriptor,
    limit: u64,
    label: &str,
) -> Result<Vec<u8>> {
    let size = i64::try_from(descriptor.size())
        .map_err(|_| cache_error(format!("{label} size exceeds the supported range")))?;
    read_verified_oci_blob(root, descriptor.digest().as_ref(), size, limit, label)
}

fn verify_descriptor_file(root: &Path, descriptor: &Descriptor, label: &str) -> Result<()> {
    let size = i64::try_from(descriptor.size())
        .map_err(|_| cache_error(format!("{label} size exceeds the supported range")))?;
    verify_oci_blob_file(
        root,
        descriptor.digest().as_ref(),
        size,
        MAX_OCI_LAYER_BLOB_BYTES,
        label,
    )
    .map(|_| ())
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
