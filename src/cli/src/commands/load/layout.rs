use std::collections::HashSet;
use std::fs::File;
use std::io::Read;
use std::path::{Path, PathBuf};

use a3s_box_core::platform::Platform;
use oci_spec::image::{Descriptor, ImageConfiguration, ImageIndex, ImageManifest, MediaType};
use sha2::{Digest, Sha256};

const IMAGE_REF_ANNOTATION: &str = "org.opencontainers.image.ref.name";
const OCI_IMAGE_INDEX: &str = "application/vnd.oci.image.index.v1+json";
const OCI_IMAGE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
const DOCKER_IMAGE_INDEX: &str = "application/vnd.docker.distribution.manifest.list.v2+json";
const DOCKER_IMAGE_MANIFEST: &str = "application/vnd.docker.distribution.manifest.v2+json";
const MAX_INDEX_DEPTH: usize = 8;

pub(super) struct PreparedLayout {
    pub(super) reference: String,
    pub(super) digest: String,
}

/// Resolve an extracted OCI layout to one runnable image manifest.
///
/// A normal OCI archive may point from its root `index.json` to another image
/// index. A3S image consumers intentionally operate on a normalized layout
/// whose root descriptor is an image manifest, so selection must happen before
/// the layout is published to the persistent store.
pub(super) fn prepare(
    root: &Path,
    platform: Option<&str>,
    tag: Option<&str>,
) -> Result<PreparedLayout, String> {
    let index_path = root.join("index.json");
    let index_bytes = std::fs::read(&index_path)
        .map_err(|error| format!("Failed to read index.json from archive: {error}"))?;
    let mut index_json: serde_json::Value = serde_json::from_slice(&index_bytes)
        .map_err(|error| format!("Invalid index.json: {error}"))?;
    let index: ImageIndex = serde_json::from_value(index_json.clone())
        .map_err(|error| format!("Invalid OCI image index: {error}"))?;
    validate_index_schema(&index, "index.json")?;

    let target = target_platform(platform)?;
    let mut visited = HashSet::new();
    let selected = resolve_index(root, &index, &target, &mut visited, 0)?;
    let digest = selected.digest().to_string();
    let reference = load_reference(&index_json, &selected, tag, &digest)?;

    let mut selected_json = serde_json::to_value(&selected)
        .map_err(|error| format!("Failed to encode selected image descriptor: {error}"))?;
    stamp_reference(&mut selected_json, &reference)?;
    let object = index_json
        .as_object_mut()
        .ok_or_else(|| "Invalid index.json: expected a JSON object".to_string())?;
    object.insert("schemaVersion".to_string(), serde_json::json!(2));
    object.insert("mediaType".to_string(), serde_json::json!(OCI_IMAGE_INDEX));
    object.insert(
        "manifests".to_string(),
        serde_json::Value::Array(vec![selected_json]),
    );

    let normalized = serde_json::to_vec_pretty(&index_json)
        .map_err(|error| format!("Failed to encode normalized index.json: {error}"))?;
    std::fs::write(&index_path, normalized)
        .map_err(|error| format!("Failed to normalize index.json: {error}"))?;

    let image = a3s_box_runtime::OciImage::from_path(root)
        .map_err(|error| format!("Selected image for {target} is not usable: {error}"))?;
    if image.manifest_digest() != digest {
        return Err(format!(
            "Selected manifest digest changed while normalizing the OCI layout: expected {digest}, got {}",
            image.manifest_digest()
        ));
    }

    Ok(PreparedLayout { reference, digest })
}

fn target_platform(requested: Option<&str>) -> Result<Platform, String> {
    let platform = match requested {
        Some(value) => {
            let value = value.trim();
            if value.is_empty() {
                return Err("Image platform cannot be empty".to_string());
            }
            Platform::parse(value).map_err(|error| format!("Invalid image platform: {error}"))?
        }
        None => Platform::new("linux", Platform::host().oci_arch()),
    };

    if platform.os != "linux" {
        return Err(format!(
            "Unsupported image platform {platform}: A3S Box executes Linux images"
        ));
    }
    Ok(platform)
}

fn resolve_index(
    root: &Path,
    index: &ImageIndex,
    target: &Platform,
    visited: &mut HashSet<String>,
    depth: usize,
) -> Result<Descriptor, String> {
    if depth >= MAX_INDEX_DEPTH {
        return Err(format!(
            "OCI image index nesting exceeds the supported depth of {MAX_INDEX_DEPTH}"
        ));
    }

    let descriptor = choose_descriptor(index.manifests(), target)?;
    let digest = descriptor.digest().to_string();
    if !visited.insert(digest.clone()) {
        return Err(format!("OCI image index contains a descriptor cycle at {digest}"));
    }

    let bytes = read_verified_blob(root, &descriptor)?;
    match payload_kind(descriptor.media_type(), &bytes)? {
        PayloadKind::ImageIndex => {
            let nested: ImageIndex = serde_json::from_slice(&bytes).map_err(|error| {
                format!("Failed to parse image index blob {digest}: {error}")
            })?;
            validate_index_schema(&nested, &format!("image index blob {digest}"))?;
            resolve_index(root, &nested, target, visited, depth + 1)
        }
        PayloadKind::ImageManifest => {
            let manifest: ImageManifest = serde_json::from_slice(&bytes).map_err(|error| {
                format!("Failed to parse image manifest blob {digest}: {error}")
            })?;
            if manifest.schema_version() != 2 {
                return Err(format!(
                    "Unsupported image manifest schema version {} in {digest}",
                    manifest.schema_version()
                ));
            }
            validate_manifest_content(root, &manifest, target)?;
            Ok(descriptor)
        }
    }
}

fn choose_descriptor(manifests: &[Descriptor], target: &Platform) -> Result<Descriptor, String> {
    if manifests.is_empty() {
        return Err("No image descriptors in OCI image index".to_string());
    }

    let mut matching: Vec<&Descriptor> = manifests
        .iter()
        .filter(|descriptor| {
            descriptor
                .platform()
                .as_ref()
                .is_some_and(|platform| platform_matches(platform, target))
        })
        .collect();

    if target.variant.is_none() {
        matching.sort_by_key(|descriptor| {
            descriptor
                .platform()
                .as_ref()
                .and_then(|platform| platform.variant().as_ref())
                .is_some()
        });
    }
    if let Some(descriptor) = matching.first() {
        return Ok((*descriptor).clone());
    }

    // Root index.json commonly contains one unplatformed descriptor that
    // points to the actual multi-platform index. Prefer recursing through that
    // descriptor before treating the target as unavailable.
    if let Some(descriptor) = manifests.iter().find(|descriptor| {
        descriptor.platform().is_none() && is_index_media_type(descriptor.media_type())
    }) {
        return Ok(descriptor.clone());
    }

    let has_platform_descriptors = manifests
        .iter()
        .any(|descriptor| descriptor.platform().is_some());
    if !has_platform_descriptors {
        if let Some(descriptor) = manifests.iter().find(|descriptor| {
            descriptor.platform().is_none() && is_image_media_type(descriptor.media_type())
        }) {
            return Ok(descriptor.clone());
        }
        if manifests.len() == 1 && manifests[0].platform().is_none() {
            // Permit a single non-standard media type and let payload sniffing
            // produce the authoritative image/index error.
            return Ok(manifests[0].clone());
        }
    }

    let available = manifests
        .iter()
        .filter_map(|descriptor| descriptor.platform().as_ref())
        .map(|platform| {
            let mut value = format!("{}/{}", platform.os(), platform.architecture());
            if let Some(variant) = platform.variant() {
                value.push('/');
                value.push_str(variant);
            }
            value
        })
        .collect::<Vec<_>>();
    Err(format!(
        "No image manifest for {target} in OCI archive{}",
        if available.is_empty() {
            String::new()
        } else {
            format!("; available platforms: {}", available.join(", "))
        }
    ))
}

fn platform_matches(platform: &oci_spec::image::Platform, target: &Platform) -> bool {
    if platform.os().to_string() != target.os
        || platform.architecture().to_string() != target.architecture
    {
        return false;
    }
    match target.variant.as_deref() {
        Some(variant) => platform.variant().as_deref() == Some(variant),
        None => true,
    }
}

#[derive(Clone, Copy)]
enum PayloadKind {
    ImageIndex,
    ImageManifest,
}

fn payload_kind(media_type: &MediaType, bytes: &[u8]) -> Result<PayloadKind, String> {
    if is_index_media_type(media_type) {
        return Ok(PayloadKind::ImageIndex);
    }
    if is_manifest_media_type(media_type) {
        return Ok(PayloadKind::ImageManifest);
    }

    let value: serde_json::Value = serde_json::from_slice(bytes)
        .map_err(|error| format!("Unsupported OCI descriptor media type {media_type}: {error}"))?;
    if value.get("manifests").is_some() {
        Ok(PayloadKind::ImageIndex)
    } else if value.get("config").is_some() && value.get("layers").is_some() {
        Ok(PayloadKind::ImageManifest)
    } else {
        Err(format!(
            "Unsupported OCI descriptor media type {media_type}: payload is neither an image index nor an image manifest"
        ))
    }
}

fn is_index_media_type(media_type: &MediaType) -> bool {
    matches!(media_type, MediaType::ImageIndex)
        || media_type.to_string() == DOCKER_IMAGE_INDEX
}

fn is_manifest_media_type(media_type: &MediaType) -> bool {
    matches!(media_type, MediaType::ImageManifest)
        || media_type.to_string() == DOCKER_IMAGE_MANIFEST
}

fn is_image_media_type(media_type: &MediaType) -> bool {
    is_index_media_type(media_type) || is_manifest_media_type(media_type)
}

fn validate_index_schema(index: &ImageIndex, source: &str) -> Result<(), String> {
    if index.schema_version() != 2 {
        return Err(format!(
            "Unsupported OCI image index schema version {} in {source}",
            index.schema_version()
        ));
    }
    Ok(())
}

fn validate_manifest_content(
    root: &Path,
    manifest: &ImageManifest,
    target: &Platform,
) -> Result<(), String> {
    let config = read_verified_blob(root, manifest.config())?;
    let config: ImageConfiguration = serde_json::from_slice(&config)
        .map_err(|error| format!("Failed to parse selected image config: {error}"))?;
    let mut config_platform = Platform::parse(&format!(
        "{}/{}",
        config.os(),
        config.architecture()
    ))
    .map_err(|error| format!("Invalid selected image config platform: {error}"))?;
    config_platform.variant = config.variant().clone();
    if config_platform.os != target.os
        || config_platform.architecture != target.architecture
        || matches!(
            (target.variant.as_deref(), config_platform.variant.as_deref()),
            (Some(expected), Some(actual)) if expected != actual
        )
    {
        return Err(format!(
            "Selected image config platform {config_platform} does not match requested platform {target}"
        ));
    }

    for layer in manifest.layers() {
        verify_blob(root, layer)?;
    }
    Ok(())
}

fn read_verified_blob(root: &Path, descriptor: &Descriptor) -> Result<Vec<u8>, String> {
    let path = verify_blob(root, descriptor)?;
    std::fs::read(&path)
        .map_err(|error| format!("Failed to read OCI blob {}: {error}", path.display()))
}

fn verify_blob(root: &Path, descriptor: &Descriptor) -> Result<PathBuf, String> {
    let path = descriptor_blob_path(root, descriptor.digest())?;
    let metadata = std::fs::symlink_metadata(&path)
        .map_err(|error| format!("Missing OCI blob {}: {error}", path.display()))?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_file() {
        return Err(format!("OCI blob is not a regular file: {}", path.display()));
    }
    if descriptor.size() < 0 || metadata.len() != descriptor.size() as u64 {
        return Err(format!(
            "OCI blob size mismatch for {}: descriptor says {}, file has {} bytes",
            descriptor.digest(),
            descriptor.size(),
            metadata.len()
        ));
    }

    let expected = descriptor
        .digest()
        .strip_prefix("sha256:")
        .ok_or_else(|| {
            format!(
                "Unsupported OCI blob digest {}: expected sha256",
                descriptor.digest()
            )
        })?;
    let mut file = File::open(&path)
        .map_err(|error| format!("Failed to open OCI blob {}: {error}", path.display()))?;
    let mut hasher = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file
            .read(&mut buffer)
            .map_err(|error| format!("Failed to verify OCI blob {}: {error}", path.display()))?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    let actual = format!("{:x}", hasher.finalize());
    if !actual.eq_ignore_ascii_case(expected) {
        return Err(format!(
            "OCI blob digest mismatch for {}: computed sha256:{actual}",
            descriptor.digest()
        ));
    }
    Ok(path)
}

fn descriptor_blob_path(root: &Path, digest: &str) -> Result<PathBuf, String> {
    let Some(hex) = digest.strip_prefix("sha256:") else {
        return Err(format!("Unsupported OCI blob digest {digest}: expected sha256"));
    };
    if hex.len() != 64 || !hex.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(format!("Invalid OCI sha256 digest: {digest}"));
    }
    Ok(root.join("blobs").join("sha256").join(hex))
}

fn load_reference(
    index: &serde_json::Value,
    selected: &Descriptor,
    tag: Option<&str>,
    digest: &str,
) -> Result<String, String> {
    if let Some(tag) = tag.map(str::trim).filter(|tag| !tag.is_empty()) {
        return Ok(tag.to_string());
    }
    if tag.is_some() {
        return Err("Image tag cannot be empty".to_string());
    }

    Ok(selected
        .annotations()
        .as_ref()
        .and_then(|annotations| annotations.get(IMAGE_REF_ANNOTATION))
        .map(String::as_str)
        .or_else(|| {
            index["manifests"][0]["annotations"][IMAGE_REF_ANNOTATION].as_str()
        })
        .map(str::trim)
        .filter(|reference| !reference.is_empty())
        .map(str::to_string)
        .unwrap_or_else(|| digest.to_string()))
}

fn stamp_reference(descriptor: &mut serde_json::Value, reference: &str) -> Result<(), String> {
    let descriptor = descriptor
        .as_object_mut()
        .ok_or_else(|| "Selected OCI descriptor is not a JSON object".to_string())?;
    let annotations = descriptor
        .entry("annotations".to_string())
        .or_insert_with(|| serde_json::json!({}));
    if !annotations.is_object() {
        *annotations = serde_json::json!({});
    }
    let annotations = annotations
        .as_object_mut()
        .ok_or_else(|| "Selected OCI descriptor annotations are not a JSON object".to_string())?;
    annotations.insert(
        IMAGE_REF_ANNOTATION.to_string(),
        serde_json::Value::String(reference.to_string()),
    );
    Ok(())
}

#[cfg(test)]
mod tests;
