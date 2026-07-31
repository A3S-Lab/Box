//! Single validation and publication authority for native OCI build outputs.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::platform::Platform;
use a3s_box_core::StoredImage;
use serde::{Deserialize, Serialize};

use crate::oci::image::canonical_sha256_digest_hex;
use crate::oci::ImageStore;

mod validation;

use validation::inspect_build_output_layout;

/// OCI media type emitted for a multi-platform image-index root.
pub const OCI_IMAGE_INDEX_MEDIA_TYPE: &str = "application/vnd.oci.image.index.v1+json";
/// OCI media type emitted for a single-platform image-manifest root.
pub const OCI_IMAGE_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

const MAX_MULTI_PLATFORM_OUTPUTS: usize = 8;

/// Result of a successful single-platform build.
#[derive(Debug)]
pub struct BuildResult {
    /// Image reference stored in the image store.
    pub reference: String,
    /// Content digest.
    pub digest: String,
    /// Total image size in bytes.
    pub size: u64,
    /// Number of layers.
    pub layer_count: usize,
    /// Typed root descriptor published by the OCI layout.
    pub descriptor: BuildOutputDescriptor,
    /// Exact platform represented by the single-platform output.
    pub platform: Platform,
    /// Durable local OCI image-layout directory owned by the image store.
    pub layout_directory: PathBuf,
    /// Number of unique content-addressed blobs in the output layout.
    pub blob_count: usize,
    /// Canonical digest of the sorted digest-and-size blob inventory.
    pub blob_inventory_digest: String,
}

impl BuildResult {
    /// Total bytes occupied by the durable OCI layout.
    pub const fn content_bytes(&self) -> u64 {
        self.size
    }
}

/// Result of deterministically assembling recorded single-platform outputs.
#[derive(Debug)]
pub struct MultiPlatformBuildResult {
    /// Image reference stored in the one image store.
    pub reference: String,
    /// Content digest of the root OCI image index.
    pub digest: String,
    /// Total bytes occupied by the durable OCI layout.
    pub size: u64,
    /// Exact root OCI image-index descriptor.
    pub descriptor: BuildOutputDescriptor,
    /// Canonically sorted platforms represented by the image index.
    pub platforms: Vec<Platform>,
    /// Number of platform-specific image manifests.
    pub manifest_count: usize,
    /// Durable local OCI image-layout directory owned by the image store.
    pub layout_directory: PathBuf,
    /// Number of unique content-addressed blobs in the output layout.
    pub blob_count: usize,
    /// Canonical digest of the sorted digest-and-size blob inventory.
    pub blob_inventory_digest: String,
}

impl MultiPlatformBuildResult {
    /// Total bytes occupied by the durable OCI layout.
    pub const fn content_bytes(&self) -> u64 {
        self.size
    }
}

/// Typed root descriptor for a native Box build output.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
pub struct BuildOutputDescriptor {
    /// OCI descriptor media type.
    pub media_type: String,
    /// Canonical lowercase SHA-256 content digest.
    pub digest: String,
    /// Exact descriptor payload size.
    pub size: u64,
}

#[derive(Debug)]
struct ValidatedBuildLayout {
    descriptor: BuildOutputDescriptor,
    platforms: Vec<Platform>,
    layer_counts: BTreeMap<String, usize>,
    content_bytes: u64,
    blob_count: usize,
    blob_inventory_digest: String,
}

#[derive(Debug)]
struct InspectedStoredBuildOutput {
    reference: String,
    layout_directory: PathBuf,
    layout: ValidatedBuildLayout,
}

#[derive(Clone)]
enum BuildOutputExpectation {
    Single(Platform),
    Multi(Vec<Platform>),
}

impl BuildOutputExpectation {
    fn require(&self, layout: &ValidatedBuildLayout) -> Result<()> {
        match self {
            Self::Single(platform) => {
                if layout.descriptor.media_type != OCI_IMAGE_MANIFEST_MEDIA_TYPE
                    || layout.platforms.as_slice() != std::slice::from_ref(platform)
                {
                    return Err(output_error(
                        "native single-platform output differs from its requested platform",
                    ));
                }
            }
            Self::Multi(platforms) => {
                if layout.descriptor.media_type != OCI_IMAGE_INDEX_MEDIA_TYPE
                    || layout.platforms != *platforms
                {
                    return Err(output_error(
                        "assembled OCI index differs from its requested platforms",
                    ));
                }
            }
        }
        Ok(())
    }
}

/// Reconstruct one single-platform result through the same complete validation
/// used before native publication and during durable replay.
pub(super) fn inspect_stored_build_output(
    reference: &str,
    stored: StoredImage,
    store_root: &Path,
) -> Result<BuildResult> {
    inspect_stored_output(reference, stored, store_root)?.into_single()
}

/// Publish one engine-produced manifest through the sole ImageStore commit
/// boundary used by both native builds and deterministic index assembly.
pub(super) async fn publish_single_build_output(
    reference: &str,
    digest: &str,
    source_dir: &Path,
    store: &Arc<ImageStore>,
    platform: &Platform,
) -> Result<BuildResult> {
    publish_build_output(
        reference,
        digest,
        source_dir,
        store,
        BuildOutputExpectation::Single(platform.clone()),
    )
    .await?
    .into_single()
}

/// Publish one assembled image index through the same sole ImageStore commit
/// boundary as single-platform native build output.
pub(super) async fn publish_multi_platform_build_output(
    reference: &str,
    digest: &str,
    source_dir: &Path,
    store: &Arc<ImageStore>,
    platforms: &[Platform],
) -> Result<MultiPlatformBuildResult> {
    publish_build_output(
        reference,
        digest,
        source_dir,
        store,
        BuildOutputExpectation::Multi(platforms.to_vec()),
    )
    .await?
    .into_multi()
}

async fn publish_build_output(
    reference: &str,
    digest: &str,
    source_dir: &Path,
    store: &Arc<ImageStore>,
    expectation: BuildOutputExpectation,
) -> Result<InspectedStoredBuildOutput> {
    let source = source_dir.to_path_buf();
    let expected_digest = digest.to_string();
    let preflight_expectation = expectation.clone();
    tokio::task::spawn_blocking(move || {
        let layout = inspect_build_output_layout(&source)?;
        if layout.descriptor.digest != expected_digest {
            return Err(output_error(
                "native output digest differs from its validated root descriptor",
            ));
        }
        preflight_expectation.require(&layout)
    })
    .await
    .map_err(|error| output_error(format!("OCI output preflight task failed: {error}")))??;

    let stored = store.put(reference, digest, source_dir).await?;
    let reference = reference.to_string();
    let store_root = store.store_dir().to_path_buf();
    let inspected =
        tokio::task::spawn_blocking(move || inspect_stored_output(&reference, stored, &store_root))
            .await
            .map_err(|error| {
                output_error(format!("OCI output publication task failed: {error}"))
            })??;
    expectation.require(&inspected.layout)?;
    Ok(inspected)
}

fn inspect_stored_output(
    reference: &str,
    stored: StoredImage,
    store_root: &Path,
) -> Result<InspectedStoredBuildOutput> {
    if stored.reference != reference {
        return Err(output_error(format!(
            "ImageStore returned reference {:?} for requested build output {reference:?}",
            stored.reference
        )));
    }
    canonical_sha256_digest_hex(&stored.digest)?;

    let store_root = store_root.canonicalize().map_err(|error| {
        output_error(format!(
            "failed to canonicalize ImageStore root {}: {error}",
            store_root.display()
        ))
    })?;
    let layout_directory = stored.path.canonicalize().map_err(|error| {
        output_error(format!(
            "failed to canonicalize stored OCI build output {}: {error}",
            stored.path.display()
        ))
    })?;
    if !layout_directory.starts_with(&store_root) {
        return Err(output_error(format!(
            "stored OCI build output {} escaped ImageStore {}",
            layout_directory.display(),
            store_root.display()
        )));
    }

    let layout = inspect_build_output_layout(&layout_directory)?;
    if stored.digest != layout.descriptor.digest {
        return Err(output_error(
            "ImageStore digest differs from the validated root descriptor",
        ));
    }
    if stored.size_bytes != layout.content_bytes {
        return Err(output_error(format!(
            "ImageStore reports {} bytes but the validated OCI output contains {}",
            stored.size_bytes, layout.content_bytes
        )));
    }
    Ok(InspectedStoredBuildOutput {
        reference: reference.to_string(),
        layout_directory,
        layout,
    })
}

impl InspectedStoredBuildOutput {
    fn into_single(self) -> Result<BuildResult> {
        if self.layout.descriptor.media_type != OCI_IMAGE_MANIFEST_MEDIA_TYPE
            || self.layout.platforms.len() != 1
        {
            return Err(output_error(
                "native single-platform output must contain exactly one image manifest",
            ));
        }
        let platform =
            self.layout.platforms.first().cloned().ok_or_else(|| {
                output_error("native single-platform output omitted its platform")
            })?;
        let layer_count = self
            .layout
            .layer_counts
            .get(&platform.to_string())
            .copied()
            .ok_or_else(|| output_error("native single-platform output omitted its layers"))?;
        Ok(BuildResult {
            reference: self.reference,
            digest: self.layout.descriptor.digest.clone(),
            size: self.layout.content_bytes,
            layer_count,
            descriptor: self.layout.descriptor,
            platform,
            layout_directory: self.layout_directory,
            blob_count: self.layout.blob_count,
            blob_inventory_digest: self.layout.blob_inventory_digest,
        })
    }

    fn into_multi(self) -> Result<MultiPlatformBuildResult> {
        if self.layout.descriptor.media_type != OCI_IMAGE_INDEX_MEDIA_TYPE
            || !(2..=MAX_MULTI_PLATFORM_OUTPUTS).contains(&self.layout.platforms.len())
        {
            return Err(output_error(
                "multi-platform output must contain one bounded OCI image index",
            ));
        }
        Ok(MultiPlatformBuildResult {
            reference: self.reference,
            digest: self.layout.descriptor.digest.clone(),
            size: self.layout.content_bytes,
            descriptor: self.layout.descriptor,
            manifest_count: self.layout.platforms.len(),
            platforms: self.layout.platforms,
            layout_directory: self.layout_directory,
            blob_count: self.layout.blob_count,
            blob_inventory_digest: self.layout.blob_inventory_digest,
        })
    }
}

fn output_error(message: impl Into<String>) -> BoxError {
    BoxError::BuildError(message.into())
}
