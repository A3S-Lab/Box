//! Runtime-owned bindings over Box's existing named-Volume store.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use a3s_box_core::volume::VolumeConfig;
use a3s_runtime::contract::{RuntimeOutputSpec, RuntimeUnitSpec};
use a3s_runtime::{RuntimeError, RuntimeResult};
use sha2::{Digest, Sha256};

use crate::VolumeStore;

const VOLUME_ROLE_LABEL: &str = "a3s.runtime.volume-role";
const VOLUME_ID_LABEL: &str = "a3s.runtime.volume-id";
const SPEC_DIGEST_LABEL: &str = "a3s.runtime.spec-digest";
const OUTPUT_NAME_LABEL: &str = "a3s.runtime.output-name";
const PERSISTENT_VOLUME_ROLE: &str = "persistent";
const OUTPUT_VOLUME_ROLE: &str = "task-output";

#[derive(Debug)]
pub(super) struct ResolvedVolume {
    pub(super) name: String,
    pub(super) path: PathBuf,
}

pub(super) fn resolve_persistent_volume(
    home_dir: &Path,
    volume_id: &str,
    create: bool,
) -> RuntimeResult<ResolvedVolume> {
    let name = format!(
        "a3s-runtime-volume-{:x}",
        Sha256::digest(volume_id.as_bytes())
    );
    let labels = BTreeMap::from([
        (
            VOLUME_ROLE_LABEL.to_owned(),
            PERSISTENT_VOLUME_ROLE.to_owned(),
        ),
        (VOLUME_ID_LABEL.to_owned(), volume_id.to_owned()),
    ]);
    resolve_volume(home_dir, name, labels, create)
}

pub(super) fn resolve_output_volume(
    home_dir: &Path,
    spec: &RuntimeUnitSpec,
    output: &RuntimeOutputSpec,
    create: bool,
) -> RuntimeResult<ResolvedVolume> {
    let digest = spec.digest().map_err(RuntimeError::InvalidRequest)?;
    let hex = digest.strip_prefix("sha256:").ok_or_else(|| {
        RuntimeError::Protocol("Box Task output requires a SHA-256 spec digest".into())
    })?;
    let name = format!(
        "a3s-runtime-output-{hex}-{:x}",
        Sha256::digest(output.name.as_bytes())
    );
    let labels = BTreeMap::from([
        (VOLUME_ROLE_LABEL.to_owned(), OUTPUT_VOLUME_ROLE.to_owned()),
        (SPEC_DIGEST_LABEL.to_owned(), digest),
        (OUTPUT_NAME_LABEL.to_owned(), output.name.clone()),
    ]);
    resolve_volume(home_dir, name, labels, create)
}

pub(super) fn require_output_volume(
    home_dir: &Path,
    spec: &RuntimeUnitSpec,
    output: &RuntimeOutputSpec,
) -> RuntimeResult<PathBuf> {
    let resolved = resolve_output_volume(home_dir, spec, output, false)?;
    let volume = volume_store(home_dir)
        .get(&resolved.name)
        .map_err(volume_store_error)?
        .ok_or_else(|| {
            RuntimeError::ProviderUnavailable("Task output Volume disappeared".into())
        })?;
    if !volume.in_use_by.is_empty() {
        return Err(RuntimeError::ProviderUnavailable(
            "Box Task output is still attached to a live execution".into(),
        ));
    }
    Ok(resolved.path)
}

pub(super) fn reset_output_volumes(home_dir: &Path, spec: &RuntimeUnitSpec) -> RuntimeResult<()> {
    for output in &spec.outputs {
        let path = require_output_volume(home_dir, spec, output)?;
        for entry in std::fs::read_dir(&path).map_err(volume_io_error)? {
            let entry = entry.map_err(volume_io_error)?;
            let metadata = std::fs::symlink_metadata(entry.path()).map_err(volume_io_error)?;
            if metadata.file_type().is_dir() && !metadata.file_type().is_symlink() {
                std::fs::remove_dir_all(entry.path()).map_err(volume_io_error)?;
            } else {
                std::fs::remove_file(entry.path()).map_err(volume_io_error)?;
            }
        }
    }
    Ok(())
}

pub(super) fn cleanup_output_volumes(home_dir: &Path, digest: &str) -> RuntimeResult<()> {
    let store = volume_store(home_dir);
    let outputs = store
        .list()
        .map_err(volume_store_error)?
        .into_iter()
        .filter(|volume| {
            volume.labels.get(VOLUME_ROLE_LABEL).map(String::as_str) == Some(OUTPUT_VOLUME_ROLE)
                && volume.labels.get(SPEC_DIGEST_LABEL).map(String::as_str) == Some(digest)
        })
        .collect::<Vec<_>>();
    for volume in outputs {
        if !volume.in_use_by.is_empty() {
            return Err(RuntimeError::ProviderUnavailable(format!(
                "Box Task-output Volume {:?} is still attached",
                volume.name
            )));
        }
        store
            .remove(&volume.name, false)
            .map_err(volume_store_error)?;
    }
    Ok(())
}

fn resolve_volume(
    home_dir: &Path,
    name: String,
    labels: BTreeMap<String, String>,
    create: bool,
) -> RuntimeResult<ResolvedVolume> {
    let store = volume_store(home_dir);
    let mut expected = VolumeConfig::new(&name, "");
    expected.labels.extend(labels.clone());
    let volume = match store.get(&name).map_err(volume_store_error)? {
        Some(volume) => volume,
        None if create => store.get_or_create(expected).map_err(volume_store_error)?,
        None => {
            return Err(RuntimeError::ProviderUnavailable(format!(
                "Box Runtime Volume {name:?} is missing"
            )))
        }
    };
    let expected_path = home_dir.join("volumes").join(&name);
    if volume.name != name
        || volume.driver != "local"
        || volume.size_limit != 0
        || volume.labels.len() != labels.len()
        || labels
            .iter()
            .any(|(key, value)| volume.labels.get(key) != Some(value))
        || Path::new(&volume.mount_point) != expected_path
    {
        return Err(RuntimeError::ProviderUnavailable(format!(
            "Box Runtime Volume {name:?} has conflicting persisted metadata"
        )));
    }
    validate_volume_path(&expected_path)?;
    Ok(ResolvedVolume {
        name,
        path: expected_path,
    })
}

fn volume_store(home_dir: &Path) -> VolumeStore {
    VolumeStore::new(home_dir.join("volumes.json"), home_dir.join("volumes"))
}

fn validate_volume_path(path: &Path) -> RuntimeResult<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(volume_io_error)?;
    let canonical = path.canonicalize().map_err(volume_io_error)?;
    if !path.is_absolute()
        || metadata.file_type().is_symlink()
        || !metadata.file_type().is_dir()
        || canonical != path
    {
        return Err(RuntimeError::ProviderUnavailable(format!(
            "Box Runtime Volume path is not a canonical plain directory: {}",
            path.display()
        )));
    }
    Ok(())
}

fn volume_store_error(error: impl std::fmt::Display) -> RuntimeError {
    RuntimeError::ProviderUnavailable(format!("Box VolumeStore operation failed: {error}"))
}

fn volume_io_error(error: std::io::Error) -> RuntimeError {
    RuntimeError::ProviderUnavailable(format!("Box Runtime Volume I/O failed: {error}"))
}
