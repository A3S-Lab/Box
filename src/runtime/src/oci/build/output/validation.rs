//! The one complete graph validator for native manifest and index outputs.

use std::collections::BTreeMap;
use std::path::Path;

use a3s_box_core::error::Result;
use a3s_box_core::platform::Platform;
use oci_spec::image::{Descriptor, ImageIndex, MediaType, SCHEMA_VERSION};
use serde::Deserialize;

use super::{
    output_error, BuildOutputDescriptor, ValidatedBuildLayout, MAX_MULTI_PLATFORM_OUTPUTS,
    OCI_IMAGE_INDEX_MEDIA_TYPE, OCI_IMAGE_MANIFEST_MEDIA_TYPE,
};
use crate::oci::build::layout::{
    metadata_bytes, validate_blob_inventory, validate_exact_entries, EntryKind,
};
use crate::oci::image::canonical_sha256_digest_hex;
use crate::oci::OciImage;

const OCI_IMAGE_CONFIG_MEDIA_TYPE: &str = "application/vnd.oci.image.config.v1+json";
const OCI_IMAGE_LAYER_GZIP_MEDIA_TYPE: &str = "application/vnd.oci.image.layer.v1.tar+gzip";

pub(super) fn inspect_build_output_layout(layout: &Path) -> Result<ValidatedBuildLayout> {
    validate_native_layout_entries(layout)?;
    validate_layout_marker(layout)?;
    let top = OciImage::load_index(layout)
        .map_err(|error| output_error(format!("OCI index validation failed: {error}")))?;
    validate_index_contract(&top, "native OCI layout index")?;
    if top.manifests().len() != 1 {
        return Err(output_error(
            "native OCI layout index must contain exactly one root descriptor",
        ));
    }
    let root = top
        .manifests()
        .first()
        .cloned()
        .ok_or_else(|| output_error("native OCI layout omitted its root descriptor"))?;
    validate_descriptor(&root, "native OCI root descriptor")?;

    let mut descriptors = Vec::new();
    let mut platforms = BTreeMap::<String, Platform>::new();
    let mut layer_counts = BTreeMap::new();
    match root.media_type().as_ref() {
        OCI_IMAGE_MANIFEST_MEDIA_TYPE => {
            let platform = require_manifest_descriptor(&root, "native OCI root manifest")?;
            append_manifest(
                layout,
                &root,
                platform,
                &mut descriptors,
                &mut platforms,
                &mut layer_counts,
            )?;
        }
        OCI_IMAGE_INDEX_MEDIA_TYPE => {
            require_index_descriptor(&root, "native OCI image-index root")?;
            descriptors.push(root.clone());
            let index = OciImage::load_index_blob(layout, &root).map_err(|error| {
                output_error(format!("OCI image-index validation failed: {error}"))
            })?;
            validate_index_contract(&index, "native OCI image-index blob")?;
            if !(2..=MAX_MULTI_PLATFORM_OUTPUTS).contains(&index.manifests().len()) {
                return Err(output_error(
                    "native OCI image index must contain between two and eight manifests",
                ));
            }
            let mut previous = None;
            for descriptor in index.manifests() {
                let platform =
                    require_manifest_descriptor(descriptor, "native OCI indexed manifest")?;
                let key = platform.to_string();
                if previous
                    .as_ref()
                    .is_some_and(|value: &String| value >= &key)
                {
                    return Err(output_error(
                        "native OCI image-index manifests must be uniquely sorted by platform",
                    ));
                }
                previous = Some(key);
                append_manifest(
                    layout,
                    descriptor,
                    platform,
                    &mut descriptors,
                    &mut platforms,
                    &mut layer_counts,
                )?;
            }
        }
        _ => {
            return Err(output_error(
                "native OCI root is not an image manifest or image index",
            ))
        }
    }

    validate_descriptor_set(&descriptors)?;
    let (blob_count, blob_bytes, blob_inventory_digest) =
        validate_blob_inventory(layout, &descriptors, "Native OCI output")?;
    let metadata_bytes = metadata_bytes(layout, "Native OCI output")?;
    let content_bytes = metadata_bytes
        .checked_add(blob_bytes)
        .ok_or_else(|| output_error("native build output byte count overflowed"))?;
    let descriptor = BuildOutputDescriptor {
        media_type: root.media_type().to_string(),
        digest: root.digest().to_string(),
        size: root.size(),
    };

    Ok(ValidatedBuildLayout {
        descriptor,
        platforms: platforms.into_values().collect(),
        layer_counts,
        content_bytes,
        blob_count,
        blob_inventory_digest,
    })
}

fn append_manifest(
    layout: &Path,
    descriptor: &Descriptor,
    platform: Platform,
    descriptors: &mut Vec<Descriptor>,
    platforms: &mut BTreeMap<String, Platform>,
    layer_counts: &mut BTreeMap<String, usize>,
) -> Result<()> {
    let image = OciImage::from_manifest_descriptor(layout, descriptor.clone())
        .map_err(|error| output_error(format!("OCI manifest graph validation failed: {error}")))?;
    if image.manifest_descriptor() != descriptor {
        return Err(output_error(
            "validated OCI manifest differs from its root descriptor",
        ));
    }
    let manifest = image.manifest();
    if manifest.schema_version() != SCHEMA_VERSION
        || manifest.media_type().as_ref() != Some(&MediaType::ImageManifest)
        || manifest.artifact_type().is_some()
        || manifest.subject().is_some()
        || manifest.annotations().is_some()
    {
        return Err(output_error(
            "native OCI image manifest is outside the closed output contract",
        ));
    }
    if image.platform() != &platform {
        return Err(output_error(format!(
            "OCI manifest platform {platform} differs from its image configuration {}",
            image.platform()
        )));
    }
    let content = image.content_descriptors();
    let Some(config) = content.get(1) else {
        return Err(output_error("native OCI manifest omitted its image config"));
    };
    require_plain_descriptor(
        config,
        OCI_IMAGE_CONFIG_MEDIA_TYPE,
        "native OCI image config",
    )?;
    for layer in content.iter().skip(2) {
        require_plain_descriptor(
            layer,
            OCI_IMAGE_LAYER_GZIP_MEDIA_TYPE,
            "native OCI image layer",
        )?;
    }

    let key = platform.to_string();
    if platforms.insert(key.clone(), platform).is_some() {
        return Err(output_error(
            "native OCI output contains a duplicate platform",
        ));
    }
    layer_counts.insert(key, image.layer_paths().len());
    descriptors.extend(content.iter().cloned());
    Ok(())
}

fn validate_layout_marker(layout: &Path) -> Result<()> {
    #[derive(Deserialize)]
    #[serde(rename_all = "camelCase", deny_unknown_fields)]
    struct LayoutMarker {
        image_layout_version: String,
    }

    let bytes = crate::oci::image::read_regular_file_bounded(
        &layout.join("oci-layout"),
        crate::oci::image::MAX_OCI_LAYOUT_BYTES,
        "oci-layout",
    )?;
    let marker: LayoutMarker = serde_json::from_slice(&bytes)
        .map_err(|error| output_error(format!("invalid OCI layout marker: {error}")))?;
    if marker.image_layout_version != "1.0.0" {
        return Err(output_error("unsupported OCI image-layout version"));
    }
    Ok(())
}

fn validate_index_contract(index: &ImageIndex, label: &str) -> Result<()> {
    if index.schema_version() != SCHEMA_VERSION
        || index.media_type().as_ref() != Some(&MediaType::ImageIndex)
        || index.artifact_type().is_some()
        || index.subject().is_some()
        || index.annotations().is_some()
    {
        return Err(output_error(format!(
            "{label} is outside the closed native output contract"
        )));
    }
    Ok(())
}

fn require_manifest_descriptor(descriptor: &Descriptor, label: &str) -> Result<Platform> {
    if descriptor.media_type().as_ref() != OCI_IMAGE_MANIFEST_MEDIA_TYPE {
        return Err(output_error(format!(
            "{label} has a non-manifest media type"
        )));
    }
    validate_descriptor(descriptor, label)?;
    if has_optional_descriptor_data(descriptor) {
        return Err(output_error(format!(
            "{label} contains unsupported descriptor metadata"
        )));
    }
    let platform = descriptor
        .platform()
        .as_ref()
        .ok_or_else(|| output_error(format!("{label} has no platform")))?;
    if platform.os_version().is_some()
        || platform.os_features().is_some()
        || platform.features().is_some()
    {
        return Err(output_error(format!(
            "{label} contains unsupported platform metadata"
        )));
    }
    let platform = Platform {
        os: platform.os().to_string(),
        architecture: platform.architecture().to_string(),
        variant: platform.variant().clone(),
    };
    validate_platform(&platform)?;
    Ok(platform)
}

fn require_index_descriptor(descriptor: &Descriptor, label: &str) -> Result<()> {
    if descriptor.platform().is_some()
        || has_optional_descriptor_data(descriptor)
        || descriptor.media_type().as_ref() != OCI_IMAGE_INDEX_MEDIA_TYPE
    {
        return Err(output_error(format!(
            "{label} contains unsupported descriptor metadata"
        )));
    }
    Ok(())
}

fn require_plain_descriptor(descriptor: &Descriptor, media_type: &str, label: &str) -> Result<()> {
    validate_descriptor(descriptor, label)?;
    if descriptor.media_type().as_ref() != media_type
        || descriptor.platform().is_some()
        || has_optional_descriptor_data(descriptor)
    {
        return Err(output_error(format!(
            "{label} is outside the closed native output contract"
        )));
    }
    Ok(())
}

fn validate_descriptor(descriptor: &Descriptor, label: &str) -> Result<()> {
    canonical_sha256_digest_hex(descriptor.digest().as_ref())
        .map_err(|_| output_error(format!("{label} digest is not canonical SHA-256")))?;
    if descriptor.size() == 0 {
        return Err(output_error(format!("{label} has an empty payload")));
    }
    Ok(())
}

fn has_optional_descriptor_data(descriptor: &Descriptor) -> bool {
    descriptor.urls().is_some()
        || descriptor.annotations().is_some()
        || descriptor.artifact_type().is_some()
        || descriptor.data().is_some()
}

fn validate_platform(platform: &Platform) -> Result<()> {
    if platform.os != "linux"
        || platform.architecture.trim().is_empty()
        || platform
            .variant
            .as_ref()
            .is_some_and(|variant| variant.trim().is_empty())
    {
        return Err(output_error(
            "native build output platform is outside the closed Linux contract",
        ));
    }
    Ok(())
}

fn validate_descriptor_set(descriptors: &[Descriptor]) -> Result<()> {
    let mut seen = BTreeMap::<String, (String, u64)>::new();
    for descriptor in descriptors {
        let digest = descriptor.digest().to_string();
        let metadata = (descriptor.media_type().to_string(), descriptor.size());
        if seen
            .insert(digest.clone(), metadata.clone())
            .is_some_and(|existing| existing != metadata)
        {
            return Err(output_error(format!(
                "native OCI digest {digest} has conflicting descriptor metadata"
            )));
        }
    }
    Ok(())
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
