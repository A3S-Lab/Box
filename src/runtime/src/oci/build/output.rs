//! Single validation and reconstruction path for native OCI build outputs.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::platform::Platform;
use a3s_box_core::StoredImage;
use serde::{Deserialize, Serialize};

use super::layout::{metadata_bytes, validate_blob_inventory, validate_exact_entries, EntryKind};
use crate::oci::image::canonical_sha256_digest_hex;
use crate::oci::OciImage;

/// OCI media type emitted for the root descriptor of a native build.
pub const OCI_IMAGE_MANIFEST_MEDIA_TYPE: &str = "application/vnd.oci.image.manifest.v1+json";

/// Result of a successful build.
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

/// Reconstruct one build result through the same complete validation used
/// immediately after native output publication and during durable replay.
pub(super) fn inspect_stored_build_output(
    reference: &str,
    stored: StoredImage,
    store_root: &Path,
) -> Result<BuildResult> {
    if stored.reference != reference {
        return Err(output_error(format!(
            "ImageStore returned reference {:?} for requested build output {reference:?}",
            stored.reference
        )));
    }
    canonical_sha256_digest_hex(&stored.digest)?;

    let store_root = store_root.canonicalize().map_err(|error| {
        output_error(format!(
            "Failed to canonicalize ImageStore root {}: {error}",
            store_root.display()
        ))
    })?;
    let layout_directory = stored.path.canonicalize().map_err(|error| {
        output_error(format!(
            "Failed to canonicalize stored OCI build output {}: {error}",
            stored.path.display()
        ))
    })?;
    if !layout_directory.starts_with(&store_root) {
        return Err(output_error(format!(
            "Stored OCI build output {} escaped ImageStore {}",
            layout_directory.display(),
            store_root.display()
        )));
    }

    let image = OciImage::from_path(&layout_directory)
        .map_err(|error| output_error(format!("OCI graph validation failed: {error}")))?;
    if image.index_manifest_count() != 1 {
        return Err(output_error(
            "Native single-platform build output must contain exactly one root manifest",
        ));
    }

    let root = image.manifest_descriptor();
    let descriptor = BuildOutputDescriptor {
        media_type: root.media_type().to_string(),
        digest: root.digest().to_string(),
        size: root.size(),
    };
    if descriptor.media_type != OCI_IMAGE_MANIFEST_MEDIA_TYPE
        || canonical_sha256_digest_hex(&descriptor.digest).is_err()
        || descriptor.size == 0
    {
        return Err(output_error(
            "Native build root descriptor is outside the closed output contract",
        ));
    }
    if image.manifest_digest() != descriptor.digest || stored.digest != descriptor.digest {
        return Err(output_error(
            "ImageStore, OCI image, and root descriptor digests differ",
        ));
    }

    let platform = root
        .platform()
        .as_ref()
        .ok_or_else(|| output_error("Native build root descriptor has no platform"))?;
    let platform = Platform {
        os: platform.os().to_string(),
        architecture: platform.architecture().to_string(),
        variant: platform.variant().clone(),
    };
    if platform.os != "linux" || platform.architecture.trim().is_empty() {
        return Err(output_error(
            "Native build output platform is outside the closed Linux contract",
        ));
    }

    validate_native_layout_entries(&layout_directory)?;
    let (blob_count, blob_bytes, blob_inventory_digest) = validate_blob_inventory(
        &layout_directory,
        image.content_descriptors(),
        "Native OCI output",
    )?;
    let metadata_bytes = metadata_bytes(&layout_directory, "Native OCI output")?;
    let content_bytes = metadata_bytes
        .checked_add(blob_bytes)
        .ok_or_else(|| output_error("Native build output byte count overflowed"))?;
    if stored.size_bytes != content_bytes {
        return Err(output_error(format!(
            "ImageStore reports {} bytes but the validated OCI output contains {content_bytes}",
            stored.size_bytes
        )));
    }

    Ok(BuildResult {
        reference: reference.to_string(),
        digest: descriptor.digest.clone(),
        size: content_bytes,
        layer_count: image.layer_paths().len(),
        descriptor,
        platform,
        layout_directory,
        blob_count,
        blob_inventory_digest,
    })
}

fn validate_native_layout_entries(layout: &Path) -> Result<()> {
    validate_exact_entries(
        layout,
        &[
            ("blobs", EntryKind::Directory),
            ("index.json", EntryKind::File),
            ("oci-layout", EntryKind::File),
        ],
        "native OCI layout",
    )?;
    validate_exact_entries(
        &layout.join("blobs"),
        &[("sha256", EntryKind::Directory)],
        "native OCI blob root",
    )
}

fn output_error(message: impl Into<String>) -> BoxError {
    BoxError::BuildError(message.into())
}
