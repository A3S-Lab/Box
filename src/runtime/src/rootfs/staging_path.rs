//! Host-safe physical names for byte-exact guest paths.
//!
//! APFS rejects non-UTF-8 directory entry names and normalizes some Unicode
//! spellings that ext4 keeps distinct. During macOS construction, affected
//! components therefore receive deterministic ASCII physical names. The OCI
//! metadata manifest remains the source of truth for the original guest bytes.

use std::collections::BTreeMap;
use std::ffi::OsStr;
#[cfg(target_os = "macos")]
use std::io::Read;
use std::path::{Component, Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
#[cfg(target_os = "macos")]
use a3s_box_core::rootfs_metadata::{RootfsMetadataManifest, IMAGE_ROOTFS_METADATA_PATH};
#[cfg(target_os = "macos")]
use base64::Engine as _;

#[cfg(target_os = "macos")]
const ESCAPE_PREFIX: &str = ".a3s-rp1-";
#[cfg(target_os = "macos")]
const MAX_METADATA_BYTES: u64 = 64 * 1024 * 1024;

/// Map one normalized guest-relative path to its host staging location.
///
/// Linux can represent every non-NUL pathname byte and keeps the logical path
/// unchanged. macOS encodes every non-ASCII component, as well as literal
/// names in the reserved codec namespace, so APFS normalization cannot merge
/// two ext4 names. Parent, root, and platform-prefix components fail closed.
pub(crate) fn host_staging_path(path: &Path) -> Result<PathBuf> {
    let normalized = validate_relative(path)?;

    #[cfg(target_os = "macos")]
    {
        encode_macos_path(&normalized)
    }

    #[cfg(not(target_os = "macos"))]
    {
        Ok(normalized)
    }
}

/// Build the reverse namespace map used when a staged tree becomes a guest
/// filesystem. Prefixes are included even when an OCI layer omitted explicit
/// directory entries, so descendants never become detached from a translated
/// parent. Any physical collision fails before publication.
pub(crate) fn staging_path_map<'a>(
    paths: impl IntoIterator<Item = &'a PathBuf>,
) -> Result<BTreeMap<PathBuf, PathBuf>> {
    let mut staging_to_logical = BTreeMap::from([(PathBuf::new(), PathBuf::new())]);
    for logical in paths {
        let mut logical_prefix = PathBuf::new();
        for component in logical.components() {
            let Component::Normal(name) = component else {
                return Err(BoxError::BuildError(format!(
                    "Guest path is not normalized: {}",
                    logical.display()
                )));
            };
            logical_prefix.push(name);
            let staging = host_staging_path(&logical_prefix)?;
            if let Some(existing) =
                staging_to_logical.insert(staging.clone(), logical_prefix.clone())
            {
                if existing != logical_prefix {
                    return Err(BoxError::BuildError(format!(
                        "Guest paths {} and {} collide at host staging path {}",
                        existing.display(),
                        logical_prefix.display(),
                        staging.display()
                    )));
                }
            }
        }
    }
    Ok(staging_to_logical)
}

/// Resolve one physical child back to its guest path and reject stale staging
/// trees that were created before the current codec. ASCII children that were
/// added after extraction remain valid under an already-translated parent.
pub(crate) fn logical_path_for_staged_child(
    staging_to_logical: &BTreeMap<PathBuf, PathBuf>,
    physical: &Path,
    logical_parent: &Path,
    physical_name: &OsStr,
) -> Result<PathBuf> {
    if let Some(logical) = staging_to_logical.get(physical) {
        if logical.parent() != Some(logical_parent) {
            return Err(BoxError::BuildError(format!(
                "Discontinuous host staging namespace at {}",
                physical.display()
            )));
        }
        return Ok(logical.clone());
    }

    let logical = logical_parent.join(physical_name);
    if host_staging_path(&logical)? != physical {
        return Err(BoxError::BuildError(format!(
            "Unmapped or legacy host staging path {}; rebuild the OCI staging tree",
            physical.display()
        )));
    }
    Ok(logical)
}

/// Reject a macOS directory transport when its staged names differ from the
/// guest namespace. Only a guest-native filesystem writer can reverse this
/// mapping; exposing the APFS directory would leak private codec names.
pub(crate) fn ensure_directory_transport_is_lossless(root: &Path) -> Result<()> {
    #[cfg(target_os = "macos")]
    {
        let manifest_path = root.join(IMAGE_ROOTFS_METADATA_PATH.trim_start_matches('/'));
        let file = match std::fs::File::open(&manifest_path) {
            Ok(file) => file,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => return Err(BoxError::IoError(error)),
        };
        let length = file.metadata().map_err(BoxError::IoError)?.len();
        if length > MAX_METADATA_BYTES {
            return Err(BoxError::BuildError(format!(
                "Rootfs metadata {} exceeds {} bytes",
                manifest_path.display(),
                MAX_METADATA_BYTES
            )));
        }
        let mut bytes = Vec::with_capacity(length as usize);
        file.take(MAX_METADATA_BYTES + 1)
            .read_to_end(&mut bytes)
            .map_err(BoxError::IoError)?;
        if bytes.len() as u64 > MAX_METADATA_BYTES {
            return Err(BoxError::BuildError(
                "Rootfs metadata grew beyond its byte limit while reading".to_string(),
            ));
        }
        let manifest: RootfsMetadataManifest = serde_json::from_slice(&bytes).map_err(|error| {
            BoxError::BuildError(format!(
                "Invalid rootfs metadata {}: {error}",
                manifest_path.display()
            ))
        })?;
        manifest.validate().map_err(BoxError::BuildError)?;
        for entry in manifest.entries {
            let raw = base64::engine::general_purpose::STANDARD
                .decode(&entry.path_base64)
                .map_err(|error| {
                    BoxError::BuildError(format!("Invalid rootfs metadata path: {error}"))
                })?;
            let logical = unsafe { std::ffi::OsString::from_encoded_bytes_unchecked(raw) };
            let logical = validate_relative(Path::new(&logical))?;
            if host_staging_path(&logical)? != logical {
                return Err(BoxError::BuildError(format!(
                    "Guest path {} cannot be exposed losslessly through the macOS directory compatibility transport; use a non-snapshot MicroVM with the default guest-native rootfs or choose representable image paths",
                    logical.display()
                )));
            }
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        let _ = root;
    }
    Ok(())
}

fn validate_relative(path: &Path) -> Result<PathBuf> {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::Normal(name) => normalized.push(name),
            Component::ParentDir | Component::RootDir | Component::Prefix(_) => {
                return Err(BoxError::BuildError(format!(
                    "Guest staging path must be relative and normalized: {}",
                    path.display()
                )))
            }
        }
    }
    Ok(normalized)
}

#[cfg(target_os = "macos")]
fn encode_macos_path(path: &Path) -> Result<PathBuf> {
    use sha2::{Digest, Sha256};
    use std::os::unix::ffi::OsStrExt;

    let mut encoded = PathBuf::new();
    for component in path.components() {
        let Component::Normal(name) = component else {
            return Err(BoxError::BuildError(format!(
                "Guest staging path was not normalized: {}",
                path.display()
            )));
        };
        let bytes = name.as_bytes();
        if bytes.iter().all(u8::is_ascii) && !bytes.starts_with(ESCAPE_PREFIX.as_bytes()) {
            encoded.push(name);
            continue;
        }
        let digest = Sha256::digest(bytes);
        encoded.push(format!("{ESCAPE_PREFIX}{}", hex::encode(digest)));
    }
    Ok(encoded)
}

#[cfg(all(test, target_os = "macos"))]
mod tests {
    use super::*;
    use std::ffi::OsString;
    use std::os::unix::ffi::OsStringExt;

    #[test]
    fn ascii_paths_remain_readable_on_the_host() {
        assert_eq!(
            host_staging_path(Path::new("usr/bin/tool")).unwrap(),
            PathBuf::from("usr/bin/tool")
        );
    }

    #[test]
    fn raw_and_unicode_components_receive_stable_bounded_names() {
        let raw = PathBuf::from(OsString::from_vec(vec![b'n', b'a', b'm', b'e', b'-', 0xff]));
        let first = host_staging_path(&raw).unwrap();
        let second = host_staging_path(&raw).unwrap();
        assert_eq!(first, second);
        let name = first.file_name().unwrap().to_str().unwrap();
        assert!(name.starts_with(ESCAPE_PREFIX));
        assert!(name.len() <= 255);

        let unicode = host_staging_path(Path::new("café")).unwrap();
        assert_ne!(unicode, PathBuf::from("café"));
        assert_ne!(unicode, first);
    }

    #[test]
    fn literal_codec_namespace_names_cannot_alias_encoded_names() {
        let literal = PathBuf::from(format!("{ESCAPE_PREFIX}literal"));
        let encoded_literal = host_staging_path(&literal).unwrap();
        assert_ne!(encoded_literal, literal);
        assert!(encoded_literal
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .starts_with(ESCAPE_PREFIX));
    }

    #[test]
    fn descendants_share_the_encoded_physical_parent() {
        let raw_parent = OsString::from_vec(vec![b'd', b'i', b'r', 0xfe]);
        let parent = PathBuf::from(&raw_parent);
        let child = parent.join("child");
        let encoded_parent = host_staging_path(&parent).unwrap();
        let encoded_child = host_staging_path(&child).unwrap();
        assert_eq!(encoded_child.parent(), Some(encoded_parent.as_path()));
    }

    #[test]
    fn reverse_map_includes_implicit_translated_parents() {
        let raw_parent = OsString::from_vec(vec![b'd', b'i', b'r', 0xfe]);
        let logical_child = PathBuf::from(&raw_parent).join("child");
        let map = staging_path_map([&logical_child]).unwrap();
        let logical_parent = PathBuf::from(raw_parent);
        assert_eq!(
            map.get(&host_staging_path(&logical_parent).unwrap()),
            Some(&logical_parent)
        );
        assert_eq!(
            map.get(&host_staging_path(&logical_child).unwrap()),
            Some(&logical_child)
        );
    }

    #[test]
    fn stale_unencoded_unicode_component_fails_closed() {
        let logical = PathBuf::from("caf\u{e9}");
        let error = logical_path_for_staged_child(
            &BTreeMap::new(),
            &logical,
            Path::new(""),
            logical.file_name().unwrap(),
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("rebuild the OCI staging tree"));
    }

    #[test]
    fn ascii_runtime_child_is_valid_below_translated_parent() {
        let logical_parent = PathBuf::from(OsString::from_vec(vec![b'd', b'i', b'r', 0xfe]));
        let staging_parent = host_staging_path(&logical_parent).unwrap();
        let physical_child = staging_parent.join("runtime-child");
        let logical_child = logical_path_for_staged_child(
            &staging_path_map([&logical_parent]).unwrap(),
            &physical_child,
            &logical_parent,
            OsStr::new("runtime-child"),
        )
        .unwrap();
        assert_eq!(logical_child, logical_parent.join("runtime-child"));
    }

    #[test]
    fn traversal_never_enters_the_staging_codec() {
        assert!(host_staging_path(Path::new("../escape")).is_err());
        assert!(host_staging_path(Path::new("/absolute")).is_err());
    }
}
