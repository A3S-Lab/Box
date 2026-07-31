//! Single validation and reconstruction path for native OCI build outputs.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::platform::Platform;
use a3s_box_core::StoredImage;
use oci_spec::image::Descriptor;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use crate::oci::image::{
    canonical_sha256_digest_hex, read_regular_file_bounded, validate_plain_directory,
    MAX_OCI_INDEX_BYTES, MAX_OCI_LAYOUT_BYTES,
};
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
    let (blob_count, blob_bytes, blob_inventory_digest) =
        validate_blob_inventory(&layout_directory, image.content_descriptors())?;
    let metadata_bytes = metadata_bytes(&layout_directory)?;
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

fn validate_blob_inventory(
    layout: &Path,
    descriptors: &[Descriptor],
) -> Result<(usize, u64, String)> {
    let mut expected = BTreeMap::<String, u64>::new();
    for descriptor in descriptors {
        let digest = descriptor.digest().to_string();
        canonical_sha256_digest_hex(&digest)?;
        let size = descriptor.size();
        if size == 0 {
            return Err(output_error(format!(
                "Native OCI blob {digest} has an empty descriptor"
            )));
        }
        if let Some(existing) = expected.insert(digest.clone(), size) {
            if existing != size {
                return Err(output_error(format!(
                    "Native OCI blob {digest} has conflicting descriptor sizes"
                )));
            }
        }
    }

    let blob_root = layout.join("blobs").join("sha256");
    validate_plain_directory(&blob_root, "native OCI sha256 blobs")?;
    let mut actual = BTreeSet::new();
    for entry in std::fs::read_dir(&blob_root).map_err(|error| {
        output_error(format!(
            "Failed to inspect native OCI blobs {}: {error}",
            blob_root.display()
        ))
    })? {
        let entry = entry.map_err(|error| {
            output_error(format!(
                "Failed to inspect an entry in {}: {error}",
                blob_root.display()
            ))
        })?;
        let file_type = entry.file_type().map_err(|error| {
            output_error(format!(
                "Failed to inspect native OCI blob {}: {error}",
                entry.path().display()
            ))
        })?;
        if !file_type.is_file() || file_type.is_symlink() {
            return Err(output_error(format!(
                "Native OCI blob {} is not a regular file",
                entry.path().display()
            )));
        }
        let name = entry.file_name().into_string().map_err(|_| {
            output_error(format!(
                "Native OCI blob name in {} is not UTF-8",
                blob_root.display()
            ))
        })?;
        let digest = format!("sha256:{name}");
        canonical_sha256_digest_hex(&digest)?;
        let expected_size = expected.get(&digest).ok_or_else(|| {
            output_error(format!(
                "Native OCI output contains unreferenced blob {digest}"
            ))
        })?;
        let actual_size = entry
            .metadata()
            .map_err(|error| {
                output_error(format!(
                    "Failed to inspect native OCI blob {}: {error}",
                    entry.path().display()
                ))
            })?
            .len();
        if actual_size != *expected_size {
            return Err(output_error(format!(
                "Native OCI blob {digest} has {actual_size} bytes, expected {expected_size}"
            )));
        }
        actual.insert(digest);
    }
    let expected_names = expected.keys().cloned().collect::<BTreeSet<_>>();
    if actual != expected_names {
        let missing = expected_names
            .difference(&actual)
            .next()
            .cloned()
            .unwrap_or_else(|| "<unknown>".to_string());
        return Err(output_error(format!(
            "Native OCI output is missing referenced blob {missing}"
        )));
    }

    let mut inventory_hasher = Sha256::new();
    let mut blob_bytes = 0_u64;
    for (digest, size) in &expected {
        inventory_hasher.update(digest.as_bytes());
        inventory_hasher.update([0]);
        inventory_hasher.update(size.to_be_bytes());
        blob_bytes = blob_bytes
            .checked_add(*size)
            .ok_or_else(|| output_error("Native OCI blob byte count overflowed"))?;
    }
    Ok((
        expected.len(),
        blob_bytes,
        format!("sha256:{:x}", inventory_hasher.finalize()),
    ))
}

fn metadata_bytes(layout: &Path) -> Result<u64> {
    let layout_bytes = read_regular_file_bounded(
        &layout.join("oci-layout"),
        MAX_OCI_LAYOUT_BYTES,
        "oci-layout",
    )?;
    let index_bytes = read_regular_file_bounded(
        &layout.join("index.json"),
        MAX_OCI_INDEX_BYTES,
        "index.json",
    )?;
    u64::try_from(layout_bytes.len())
        .ok()
        .and_then(|left| {
            u64::try_from(index_bytes.len())
                .ok()
                .and_then(|right| left.checked_add(right))
        })
        .ok_or_else(|| output_error("Native OCI metadata byte count overflowed"))
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum EntryKind {
    File,
    Directory,
}

fn validate_exact_entries(
    directory: &Path,
    expected: &[(&str, EntryKind)],
    label: &str,
) -> Result<()> {
    validate_plain_directory(directory, label)?;
    let mut actual = BTreeMap::new();
    for entry in std::fs::read_dir(directory).map_err(|error| {
        output_error(format!(
            "Failed to inspect {label} {}: {error}",
            directory.display()
        ))
    })? {
        let entry = entry.map_err(|error| {
            output_error(format!(
                "Failed to inspect an entry in {label} {}: {error}",
                directory.display()
            ))
        })?;
        let name = entry.file_name().into_string().map_err(|_| {
            output_error(format!(
                "{label} {} contains a non-UTF-8 entry",
                directory.display()
            ))
        })?;
        let file_type = entry.file_type().map_err(|error| {
            output_error(format!(
                "Failed to inspect {label} entry {}: {error}",
                entry.path().display()
            ))
        })?;
        let kind = if file_type.is_file() && !file_type.is_symlink() {
            EntryKind::File
        } else if file_type.is_dir() && !file_type.is_symlink() {
            EntryKind::Directory
        } else {
            return Err(output_error(format!(
                "{label} entry {} is not a plain file or directory",
                entry.path().display()
            )));
        };
        actual.insert(name, kind);
    }
    let expected = expected
        .iter()
        .map(|(name, kind)| ((*name).to_string(), *kind))
        .collect::<BTreeMap<_, _>>();
    if actual != expected {
        return Err(output_error(format!(
            "{label} {} does not contain the exact native output entries",
            directory.display()
        )));
    }
    Ok(())
}

fn output_error(message: impl Into<String>) -> BoxError {
    BoxError::BuildError(message.into())
}
