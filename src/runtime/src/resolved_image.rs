//! Durable resolved OCI image defaults used by filesystem snapshots.

use std::io::Write;
use std::path::Path;

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::{
    SnapshotImageConfig, SnapshotImageHealthCheck, SnapshotMetadata,
};
use serde::de::DeserializeOwned;

use crate::oci::{OciHealthCheck, OciImageConfig};

/// Box-local artifact containing the resolved defaults from the source image.
pub const RESOLVED_IMAGE_CONFIG_FILE: &str = ".oci-image-config.json";

const MAX_IMAGE_CONFIG_BYTES: u64 = 1024 * 1024;

/// Load the resolved image defaults persisted for a box.
///
/// The artifact is independent of the control-plane process so a filesystem
/// snapshot created after a service restart retains the source image's OCI
/// entrypoint, command, environment, working directory, and user.
pub fn load_resolved_image_config(box_dir: &Path) -> Result<Option<SnapshotImageConfig>> {
    let path = box_dir.join(RESOLVED_IMAGE_CONFIG_FILE);
    match std::fs::symlink_metadata(&path) {
        Ok(_) => read_regular_json(&path, "resolved image configuration").map(Some),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(BoxError::ConfigError(format!(
            "Failed to inspect resolved image configuration {}: {error}",
            path.display()
        ))),
    }
}

pub(crate) fn persist_resolved_image_config(
    box_dir: &Path,
    config: &OciImageConfig,
) -> Result<()> {
    let config = SnapshotImageConfig::from(config);
    let mut encoded = serde_json::to_vec_pretty(&config).map_err(|error| {
        BoxError::SerializationError(format!(
            "Failed to encode resolved image configuration: {error}"
        ))
    })?;
    encoded.push(b'\n');

    let destination = box_dir.join(RESOLVED_IMAGE_CONFIG_FILE);
    let mut temporary = tempfile::NamedTempFile::new_in(box_dir).map_err(|error| {
        BoxError::ConfigError(format!(
            "Failed to create resolved image configuration beside {}: {error}",
            destination.display()
        ))
    })?;
    temporary.write_all(&encoded).map_err(|error| {
        BoxError::ConfigError(format!(
            "Failed to write resolved image configuration {}: {error}",
            destination.display()
        ))
    })?;
    temporary.as_file().sync_all().map_err(|error| {
        BoxError::ConfigError(format!(
            "Failed to sync resolved image configuration {}: {error}",
            destination.display()
        ))
    })?;
    temporary.persist(&destination).map_err(|error| {
        BoxError::ConfigError(format!(
            "Failed to publish resolved image configuration {}: {}",
            destination.display(),
            error.error
        ))
    })?;
    if let Ok(directory) = std::fs::File::open(box_dir) {
        let _ = directory.sync_all();
    }
    Ok(())
}

pub(crate) fn load_snapshot_oci_config(
    rootfs: &Path,
    expected_image: &str,
) -> Result<Option<OciImageConfig>> {
    if rootfs.file_name().is_none_or(|name| name != "rootfs") {
        return Err(BoxError::ConfigError(format!(
            "Snapshot lower must end in rootfs: {}",
            rootfs.display()
        )));
    }
    let snapshot_dir = rootfs.parent().ok_or_else(|| {
        BoxError::ConfigError(format!(
            "Snapshot lower has no snapshot directory: {}",
            rootfs.display()
        ))
    })?;
    let metadata_path = snapshot_dir.join("metadata.json");
    let metadata: SnapshotMetadata =
        read_regular_json(&metadata_path, "filesystem snapshot metadata")?;
    let directory_id = snapshot_dir.file_name().and_then(|value| value.to_str());
    if directory_id != Some(metadata.id.as_str()) {
        return Err(BoxError::ConfigError(format!(
            "Snapshot metadata identity does not match {}",
            snapshot_dir.display()
        )));
    }
    if metadata.image != expected_image {
        return Err(BoxError::ConfigError(format!(
            "Snapshot image {} does not match requested image {expected_image}",
            metadata.image
        )));
    }
    Ok(metadata.image_config.map(OciImageConfig::from))
}

fn read_regular_json<T: DeserializeOwned>(path: &Path, description: &str) -> Result<T> {
    let file = std::fs::symlink_metadata(path).map_err(|error| {
        BoxError::ConfigError(format!(
            "Failed to inspect {description} {}: {error}",
            path.display()
        ))
    })?;
    if !file.file_type().is_file() || file.file_type().is_symlink() {
        return Err(BoxError::ConfigError(format!(
            "{description} is not a regular file: {}",
            path.display()
        )));
    }
    if file.len() > MAX_IMAGE_CONFIG_BYTES {
        return Err(BoxError::ConfigError(format!(
            "{description} exceeds {MAX_IMAGE_CONFIG_BYTES} bytes: {}",
            path.display()
        )));
    }
    let encoded = std::fs::read(path).map_err(|error| {
        BoxError::ConfigError(format!(
            "Failed to read {description} {}: {error}",
            path.display()
        ))
    })?;
    serde_json::from_slice(&encoded).map_err(|error| {
        BoxError::SerializationError(format!(
            "Failed to parse {description} {}: {error}",
            path.display()
        ))
    })
}

impl From<&OciImageConfig> for SnapshotImageConfig {
    fn from(config: &OciImageConfig) -> Self {
        Self {
            entrypoint: config.entrypoint.clone(),
            cmd: config.cmd.clone(),
            env: config.env.clone(),
            working_dir: config.working_dir.clone(),
            user: config.user.clone(),
            exposed_ports: config.exposed_ports.clone(),
            labels: config.labels.clone(),
            volumes: config.volumes.clone(),
            stop_signal: config.stop_signal.clone(),
            health_check: config
                .health_check
                .as_ref()
                .map(SnapshotImageHealthCheck::from),
            onbuild: config.onbuild.clone(),
        }
    }
}

impl From<&OciHealthCheck> for SnapshotImageHealthCheck {
    fn from(health_check: &OciHealthCheck) -> Self {
        Self {
            test: health_check.test.clone(),
            interval: health_check.interval,
            timeout: health_check.timeout,
            retries: health_check.retries,
            start_period: health_check.start_period,
        }
    }
}

impl From<SnapshotImageConfig> for OciImageConfig {
    fn from(config: SnapshotImageConfig) -> Self {
        Self {
            entrypoint: config.entrypoint,
            cmd: config.cmd,
            env: config.env,
            working_dir: config.working_dir,
            user: config.user,
            exposed_ports: config.exposed_ports,
            labels: config.labels,
            volumes: config.volumes,
            stop_signal: config.stop_signal,
            health_check: config.health_check.map(OciHealthCheck::from),
            onbuild: config.onbuild,
        }
    }
}

impl From<SnapshotImageHealthCheck> for OciHealthCheck {
    fn from(health_check: SnapshotImageHealthCheck) -> Self {
        Self {
            test: health_check.test,
            interval: health_check.interval,
            timeout: health_check.timeout,
            retries: health_check.retries,
            start_period: health_check.start_period,
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    fn image_config() -> OciImageConfig {
        OciImageConfig {
            entrypoint: Some(vec!["/usr/local/bin/envd".to_string()]),
            cmd: Some(vec!["--port".to_string(), "49983".to_string()]),
            env: vec![("PATH".to_string(), "/usr/local/bin:/usr/bin".to_string())],
            working_dir: Some("/home/user".to_string()),
            user: Some("1000:1000".to_string()),
            exposed_ports: vec!["49983/tcp".to_string()],
            labels: HashMap::from([("runtime".to_string(), "envd".to_string())]),
            volumes: vec!["/home/user".to_string()],
            stop_signal: Some("SIGTERM".to_string()),
            health_check: Some(OciHealthCheck {
                test: vec!["CMD".to_string(), "envd-health".to_string()],
                interval: Some(10),
                timeout: Some(2),
                retries: Some(3),
                start_period: Some(5),
            }),
            onbuild: vec!["RUN prepare-runtime".to_string()],
        }
    }

    #[test]
    fn resolved_image_config_round_trips_through_the_snapshot_schema() {
        let original = image_config();
        let restored = OciImageConfig::from(SnapshotImageConfig::from(&original));

        assert_eq!(restored.entrypoint, original.entrypoint);
        assert_eq!(restored.cmd, original.cmd);
        assert_eq!(restored.env, original.env);
        assert_eq!(restored.working_dir, original.working_dir);
        assert_eq!(restored.user, original.user);
        assert_eq!(restored.exposed_ports, original.exposed_ports);
        assert_eq!(restored.labels, original.labels);
        assert_eq!(restored.volumes, original.volumes);
        assert_eq!(restored.stop_signal, original.stop_signal);
        assert_eq!(restored.health_check, original.health_check);
        assert_eq!(restored.onbuild, original.onbuild);
    }

    #[test]
    fn box_image_config_artifact_survives_process_local_state() {
        let directory = tempfile::tempdir().unwrap();
        let original = image_config();

        persist_resolved_image_config(directory.path(), &original).unwrap();
        let loaded = load_resolved_image_config(directory.path())
            .unwrap()
            .unwrap();

        assert_eq!(loaded, SnapshotImageConfig::from(&original));
    }
}
