//! Shared validation for complete, content-addressed OCI build layouts.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;

use a3s_box_core::error::{BoxError, Result};
use oci_spec::image::Descriptor;
use sha2::{Digest, Sha256};

use crate::oci::image::{
    canonical_sha256_digest_hex, read_regular_file_bounded, validate_plain_directory,
    MAX_OCI_INDEX_BYTES, MAX_OCI_LAYOUT_BYTES,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) enum EntryKind {
    File,
    Directory,
}

pub(super) fn validate_exact_entries(
    directory: &Path,
    expected: &[(&str, EntryKind)],
    label: &str,
) -> Result<()> {
    validate_plain_directory(directory, label)?;
    let mut actual = BTreeMap::new();
    for entry in std::fs::read_dir(directory).map_err(|error| {
        layout_error(format!(
            "Failed to inspect {label} {}: {error}",
            directory.display()
        ))
    })? {
        let entry = entry.map_err(|error| {
            layout_error(format!(
                "Failed to inspect an entry in {label} {}: {error}",
                directory.display()
            ))
        })?;
        let name = entry.file_name().into_string().map_err(|_| {
            layout_error(format!(
                "{label} {} contains a non-UTF-8 entry",
                directory.display()
            ))
        })?;
        let file_type = entry.file_type().map_err(|error| {
            layout_error(format!(
                "Failed to inspect {label} entry {}: {error}",
                entry.path().display()
            ))
        })?;
        let kind = if file_type.is_file() && !file_type.is_symlink() {
            EntryKind::File
        } else if file_type.is_dir() && !file_type.is_symlink() {
            EntryKind::Directory
        } else {
            return Err(layout_error(format!(
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
        return Err(layout_error(format!(
            "{label} {} does not contain the exact expected entries",
            directory.display()
        )));
    }
    Ok(())
}

pub(super) fn validate_blob_inventory(
    layout: &Path,
    descriptors: &[Descriptor],
    label: &str,
) -> Result<(usize, u64, String)> {
    let mut expected = BTreeMap::<String, u64>::new();
    for descriptor in descriptors {
        let digest = descriptor.digest().to_string();
        canonical_sha256_digest_hex(&digest)?;
        let size = descriptor.size();
        if size == 0 {
            return Err(layout_error(format!(
                "{label} blob {digest} has an empty descriptor"
            )));
        }
        if let Some(existing) = expected.insert(digest.clone(), size) {
            if existing != size {
                return Err(layout_error(format!(
                    "{label} blob {digest} has conflicting descriptor sizes"
                )));
            }
        }
    }

    let blob_root = layout.join("blobs").join("sha256");
    validate_plain_directory(&blob_root, &format!("{label} sha256 blobs"))?;
    let mut actual = BTreeSet::new();
    for entry in std::fs::read_dir(&blob_root).map_err(|error| {
        layout_error(format!(
            "Failed to inspect {label} blobs {}: {error}",
            blob_root.display()
        ))
    })? {
        let entry = entry.map_err(|error| {
            layout_error(format!(
                "Failed to inspect an entry in {}: {error}",
                blob_root.display()
            ))
        })?;
        let file_type = entry.file_type().map_err(|error| {
            layout_error(format!(
                "Failed to inspect {label} blob {}: {error}",
                entry.path().display()
            ))
        })?;
        if !file_type.is_file() || file_type.is_symlink() {
            return Err(layout_error(format!(
                "{label} blob {} is not a regular file",
                entry.path().display()
            )));
        }
        let name = entry.file_name().into_string().map_err(|_| {
            layout_error(format!(
                "{label} blob name in {} is not UTF-8",
                blob_root.display()
            ))
        })?;
        let digest = format!("sha256:{name}");
        canonical_sha256_digest_hex(&digest)?;
        let expected_size = expected
            .get(&digest)
            .ok_or_else(|| layout_error(format!("{label} contains unreferenced blob {digest}")))?;
        let actual_size = entry
            .metadata()
            .map_err(|error| {
                layout_error(format!(
                    "Failed to inspect {label} blob {}: {error}",
                    entry.path().display()
                ))
            })?
            .len();
        if actual_size != *expected_size {
            return Err(layout_error(format!(
                "{label} blob {digest} has {actual_size} bytes, expected {expected_size}"
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
        return Err(layout_error(format!(
            "{label} is missing referenced blob {missing}"
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
            .ok_or_else(|| layout_error(format!("{label} blob byte count overflowed")))?;
    }
    Ok((
        expected.len(),
        blob_bytes,
        format!("sha256:{:x}", inventory_hasher.finalize()),
    ))
}

pub(super) fn metadata_bytes(layout: &Path, label: &str) -> Result<u64> {
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
        .ok_or_else(|| layout_error(format!("{label} metadata byte count overflowed")))
}

fn layout_error(message: impl Into<String>) -> BoxError {
    BoxError::BuildError(message.into())
}
