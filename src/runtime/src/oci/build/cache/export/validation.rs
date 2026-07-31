//! Sole parser and validator for portable native cache artifacts.

use std::collections::BTreeMap;
use std::path::Path;

use a3s_box_core::error::Result;
use a3s_box_core::platform::Platform;
use oci_spec::image::{Descriptor, ImageIndex, ImageManifest, MediaType, SCHEMA_VERSION};

use super::{
    cache_error, BuildCacheExportIdentity, BuildCacheReceipt, CacheConfig, RecordedBuildCache,
    BUILD_CACHE_ARTIFACT_MEDIA_TYPE, BUILD_CACHE_CONFIG_MEDIA_TYPE, CACHE_CONFIG_SCHEMA,
    MAX_CACHE_ENTRIES,
};
use crate::oci::build::layout::{
    metadata_bytes, validate_blob_inventory, validate_exact_entries, EntryKind,
};
use crate::oci::build::BuildOutputDescriptor;
use crate::oci::image::{
    canonical_sha256_digest_hex, read_regular_file_bounded, read_verified_oci_blob,
    verify_oci_blob_file, MAX_OCI_CONFIG_BYTES, MAX_OCI_INDEX_BYTES, MAX_OCI_LAYER_BLOB_BYTES,
    MAX_OCI_MANIFEST_BYTES,
};

/// One cache entry returned only after the complete artifact is revalidated.
#[derive(Debug)]
pub(in crate::oci::build::cache) struct ValidatedCacheEntry {
    pub(in crate::oci::build::cache) key: String,
    pub(in crate::oci::build::cache) blob_path: std::path::PathBuf,
    pub(in crate::oci::build::cache) digest: String,
    pub(in crate::oci::build::cache) diff_id: String,
    pub(in crate::oci::build::cache) size: u64,
}

/// The sole parsed form shared by receipt replay and cache hydration.
#[derive(Debug)]
pub(in crate::oci::build::cache) struct ValidatedBuildCacheArtifact {
    pub(in crate::oci::build::cache) recorded: RecordedBuildCache,
    pub(in crate::oci::build::cache) entries: Vec<ValidatedCacheEntry>,
}

pub(in crate::oci::build) fn inspect_build_cache_artifact(
    root: &Path,
    identity: &BuildCacheExportIdentity,
    expected: Option<&BuildCacheReceipt>,
) -> Result<RecordedBuildCache> {
    validate_build_cache_artifact(root, identity, expected).map(|artifact| artifact.recorded)
}

pub(in crate::oci::build::cache) fn validate_build_cache_artifact(
    root: &Path,
    identity: &BuildCacheExportIdentity,
    expected: Option<&BuildCacheReceipt>,
) -> Result<ValidatedBuildCacheArtifact> {
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
    let entries = config
        .entries
        .into_iter()
        .map(|entry| {
            let key = canonical_sha256_digest_hex(&entry.key).map(str::to_string)?;
            let digest = canonical_sha256_digest_hex(&entry.layer.digest).map(str::to_string)?;
            let diff_id = canonical_sha256_digest_hex(&entry.diff_id).map(str::to_string)?;
            Ok(ValidatedCacheEntry {
                key,
                blob_path: root.join("blobs").join("sha256").join(&digest),
                digest,
                diff_id,
                size: entry.layer.size,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    Ok(ValidatedBuildCacheArtifact {
        recorded: RecordedBuildCache {
            receipt,
            layout_directory: root.to_path_buf(),
        },
        entries,
    })
}

pub(super) fn validate_config(
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
    let mut expected_diff_ids = BTreeMap::new();
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
        let layer_identity = (entry.layer.media_type.clone(), entry.layer.size);
        if expected_layers
            .insert(entry.layer.digest.clone(), layer_identity.clone())
            .is_some_and(|existing| existing != layer_identity)
            || expected_diff_ids
                .insert(entry.layer.digest.clone(), entry.diff_id.clone())
                .is_some_and(|existing| existing != entry.diff_id)
        {
            return Err(cache_error(
                "native cache entries disagree about one layer identity",
            ));
        }
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

pub(super) fn require_descriptor(
    descriptor: &Descriptor,
    media_type: MediaType,
    root: bool,
) -> Result<()> {
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
