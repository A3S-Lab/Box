//! Host resource preparation for managed local execution startup.

use std::path::{Path, PathBuf};

use a3s_box_core::{BoxError, ExecutionManagerError, ExecutionManagerResult, NetworkMode};

use crate::{BoxRecord, LocalExecutionManager, NetworkStore, VolumeStore};

/// Rolls back only the resource ownership acquired by one start attempt.
pub(super) struct ExecutionResourceGuard {
    home_dir: PathBuf,
    execution_id: String,
    attached_volumes: Vec<String>,
    connected_network: Option<String>,
    snapshot_marker_created: bool,
    armed: bool,
}

impl ExecutionResourceGuard {
    pub(super) fn prepare(home_dir: &Path, record: &BoxRecord) -> ExecutionManagerResult<Self> {
        let mut guard = Self {
            home_dir: home_dir.to_path_buf(),
            execution_id: record.id.clone(),
            attached_volumes: Vec::new(),
            connected_network: None,
            snapshot_marker_created: false,
            armed: true,
        };

        guard.prepare_snapshot_lower(record)?;

        let volume_store =
            VolumeStore::new(home_dir.join("volumes.json"), home_dir.join("volumes"));
        for volume_name in &record.volume_names {
            let volume = volume_store
                .get(volume_name)
                .map_err(|error| resource_error(record, "load named volume", error))?
                .ok_or_else(|| {
                    ExecutionManagerError::Unavailable(format!(
                        "named volume '{volume_name}' required by execution {} was not found",
                        record.id
                    ))
                })?;
            if volume.in_use_by.iter().any(|id| id == &record.id) {
                continue;
            }
            let attached = volume_store
                .modify(volume_name, |volume| volume.attach(&record.id))
                .map_err(|error| resource_error(record, "attach named volume", error))?;
            if !attached {
                return Err(ExecutionManagerError::Unavailable(format!(
                    "named volume '{volume_name}' required by execution {} disappeared during startup",
                    record.id
                )));
            }
            guard.attached_volumes.push(volume_name.clone());
        }

        if let Some(network_name) = network_name(record) {
            let network_store = NetworkStore::new(home_dir.join("networks.json"));
            let connected = network_store
                .with_write_lock(|networks| -> Result<bool, BoxError> {
                    let network = networks.get_mut(network_name).ok_or_else(|| {
                        BoxError::NetworkError(format!("network '{network_name}' not found"))
                    })?;
                    network.validate_runtime().map_err(BoxError::NetworkError)?;
                    if network.endpoints.contains_key(&record.id) {
                        return Ok(false);
                    }
                    network
                        .connect(&record.id, &record.name)
                        .map_err(BoxError::NetworkError)?;
                    Ok(true)
                })
                .map_err(|error| resource_error(record, "connect network", error))?;
            if connected {
                guard.connected_network = Some(network_name.to_string());
            }
        }

        Ok(guard)
    }

    pub(super) fn disarm(mut self) {
        self.armed = false;
    }

    pub(super) fn rollback(mut self) {
        self.rollback_inner();
    }

    fn rollback_inner(&mut self) {
        if !self.armed {
            return;
        }

        let volume_store = VolumeStore::new(
            self.home_dir.join("volumes.json"),
            self.home_dir.join("volumes"),
        );
        for volume_name in &self.attached_volumes {
            if let Err(error) = volume_store.modify(volume_name, |volume| {
                volume.detach(&self.execution_id);
            }) {
                tracing::warn!(
                    execution_id = %self.execution_id,
                    volume = %volume_name,
                    %error,
                    "Failed to roll back managed volume attachment"
                );
            }
        }

        if let Some(network_name) = self.connected_network.as_deref() {
            let network_store = NetworkStore::new(self.home_dir.join("networks.json"));
            if let Err(error) = network_store.with_write_lock(|networks| -> Result<(), BoxError> {
                if let Some(network) = networks.get_mut(network_name) {
                    let _ = network.disconnect(&self.execution_id);
                }
                Ok(())
            }) {
                tracing::warn!(
                    execution_id = %self.execution_id,
                    network = %network_name,
                    %error,
                    "Failed to roll back managed network attachment"
                );
            }
        }

        if self.snapshot_marker_created {
            let marker = self
                .home_dir
                .join("boxes")
                .join(&self.execution_id)
                .join(".snapshot-lower");
            if let Err(error) = std::fs::remove_file(&marker) {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(
                        execution_id = %self.execution_id,
                        path = %marker.display(),
                        %error,
                        "Failed to roll back managed snapshot marker"
                    );
                }
            }
        }

        self.armed = false;
    }
}

impl ExecutionResourceGuard {
    fn prepare_snapshot_lower(&mut self, record: &BoxRecord) -> ExecutionManagerResult<()> {
        let Some(snapshot_id) = record
            .managed_execution
            .as_ref()
            .and_then(|metadata| metadata.request.rootfs_snapshot_id.as_ref())
        else {
            return Ok(());
        };
        let snapshots_root = self.home_dir.join("snapshots");
        let canonical_root = snapshots_root
            .canonicalize()
            .map_err(|error| resource_error(record, "canonicalize managed snapshot root", error))?;
        let snapshot_dir = snapshots_root.join(snapshot_id.as_str());
        let canonical_snapshot = snapshot_dir
            .canonicalize()
            .map_err(|error| resource_error(record, "resolve managed snapshot", error))?;
        if canonical_snapshot.parent() != Some(canonical_root.as_path()) {
            return Err(ExecutionManagerError::Unavailable(format!(
                "filesystem snapshot '{snapshot_id}' is not a published managed snapshot"
            )));
        }
        let metadata_path = canonical_snapshot.join("metadata.json");
        if std::fs::symlink_metadata(&metadata_path)
            .map_err(|error| resource_error(record, "inspect managed snapshot metadata", error))?
            .file_type()
            .is_symlink()
        {
            return Err(ExecutionManagerError::Unavailable(format!(
                "filesystem snapshot '{snapshot_id}' has unsafe metadata"
            )));
        }
        let metadata_file = metadata_path
            .canonicalize()
            .map_err(|error| resource_error(record, "resolve managed snapshot metadata", error))?;
        let metadata: a3s_box_core::snapshot::SnapshotMetadata =
            serde_json::from_slice(&std::fs::read(&metadata_file).map_err(|error| {
                resource_error(record, "read managed snapshot metadata", error)
            })?)
            .map_err(|error| {
                ExecutionManagerError::Unavailable(format!(
                    "filesystem snapshot '{snapshot_id}' has invalid metadata: {error}"
                ))
            })?;
        if metadata_file.parent() != Some(canonical_snapshot.as_path())
            || metadata.id != snapshot_id.as_str()
        {
            return Err(ExecutionManagerError::Unavailable(format!(
                "filesystem snapshot '{snapshot_id}' has inconsistent metadata"
            )));
        }
        let rootfs_path = canonical_snapshot.join("rootfs");
        if std::fs::symlink_metadata(&rootfs_path)
            .map_err(|error| resource_error(record, "inspect managed snapshot rootfs", error))?
            .file_type()
            .is_symlink()
        {
            return Err(ExecutionManagerError::Unavailable(format!(
                "filesystem snapshot '{snapshot_id}' has an unsafe rootfs"
            )));
        }
        let rootfs = rootfs_path
            .canonicalize()
            .map_err(|error| resource_error(record, "resolve managed snapshot rootfs", error))?;
        if rootfs.parent() != Some(canonical_snapshot.as_path()) || !rootfs.is_dir() {
            return Err(ExecutionManagerError::Unavailable(format!(
                "filesystem snapshot '{snapshot_id}' has no rootfs"
            )));
        }

        let boxes_root = self.home_dir.join("boxes");
        std::fs::create_dir_all(&boxes_root)
            .map_err(|error| resource_error(record, "create managed boxes root", error))?;
        let canonical_boxes_root = boxes_root
            .canonicalize()
            .map_err(|error| resource_error(record, "resolve managed boxes root", error))?;
        let box_dir = boxes_root.join(&record.id);
        std::fs::create_dir_all(&box_dir)
            .map_err(|error| resource_error(record, "create managed box directory", error))?;
        let canonical_box_dir = box_dir
            .canonicalize()
            .map_err(|error| resource_error(record, "resolve managed box directory", error))?;
        if canonical_box_dir.parent() != Some(canonical_boxes_root.as_path()) {
            return Err(ExecutionManagerError::Unavailable(format!(
                "execution {} has an unsafe managed box directory",
                record.id
            )));
        }
        let marker = box_dir.join(".snapshot-lower");
        let expected = rootfs.to_string_lossy().into_owned();
        if marker.exists() {
            let marker_type = std::fs::symlink_metadata(&marker)
                .map_err(|error| resource_error(record, "inspect snapshot marker", error))?
                .file_type();
            if !marker_type.is_file() || marker_type.is_symlink() {
                return Err(ExecutionManagerError::Unavailable(format!(
                    "execution {} has an unsafe filesystem snapshot marker",
                    record.id
                )));
            }
            let current = std::fs::read_to_string(&marker)
                .map_err(|error| resource_error(record, "read snapshot marker", error))?;
            if current.trim() != expected {
                return Err(ExecutionManagerError::Unavailable(format!(
                    "execution {} has a conflicting filesystem snapshot marker",
                    record.id
                )));
            }
            return Ok(());
        }
        let temporary = box_dir.join(format!(
            ".snapshot-lower.{}.tmp",
            uuid::Uuid::new_v4().simple()
        ));
        a3s_box_core::fs_atomic::write_durable(&temporary, &marker, expected.as_bytes())
            .map_err(|error| resource_error(record, "write snapshot marker", error))?;
        self.snapshot_marker_created = true;
        Ok(())
    }
}

impl Drop for ExecutionResourceGuard {
    fn drop(&mut self) {
        self.rollback_inner();
    }
}

impl LocalExecutionManager {
    pub(super) async fn release_execution_resources(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<()> {
        let home_dir = self.home_dir.clone();
        let execution_id = record.id.clone();
        let record = record.clone();
        tokio::task::spawn_blocking(move || release_resources(&home_dir, &record))
            .await
            .map_err(|error| {
                ExecutionManagerError::Internal(format!(
                    "managed resource cleanup task failed for {}: {error}",
                    execution_id
                ))
            })?
    }
}

fn release_resources(home_dir: &Path, record: &BoxRecord) -> ExecutionManagerResult<()> {
    let volume_store = VolumeStore::new(home_dir.join("volumes.json"), home_dir.join("volumes"));
    for volume_name in &record.volume_names {
        volume_store
            .modify(volume_name, |volume| volume.detach(&record.id))
            .map_err(|error| resource_error(record, "detach named volume", error))?;
    }

    if let Some(network_name) = network_name(record) {
        let network_store = NetworkStore::new(home_dir.join("networks.json"));
        network_store
            .with_write_lock(|networks| -> Result<(), BoxError> {
                if let Some(network) = networks.get_mut(network_name) {
                    let _ = network.disconnect(&record.id);
                }
                Ok(())
            })
            .map_err(|error| resource_error(record, "disconnect network", error))?;
    }
    Ok(())
}

fn network_name(record: &BoxRecord) -> Option<&str> {
    record
        .network_name
        .as_deref()
        .or(match &record.network_mode {
            NetworkMode::Bridge { network } => Some(network.as_str()),
            NetworkMode::Tsi | NetworkMode::None => None,
        })
}

fn resource_error(
    record: &BoxRecord,
    operation: &str,
    error: impl std::fmt::Display,
) -> ExecutionManagerError {
    ExecutionManagerError::Unavailable(format!(
        "failed to {operation} for execution {}: {error}",
        record.id
    ))
}

#[cfg(test)]
mod tests {
    use a3s_box_core::{
        network::NetworkConfig, snapshot::SnapshotMetadata, volume::VolumeConfig,
        CreateExecutionRequest, ExecutionGeneration, ExecutionIsolation, ExecutionSnapshotId,
        OperationId,
    };

    use super::*;

    fn record(home_dir: &Path) -> BoxRecord {
        let id = "11111111-1111-4111-8111-111111111111";
        let mut record: BoxRecord = serde_json::from_value(serde_json::json!({
            "id": id,
            "short_id": "11111111",
            "name": "managed-resources",
            "image": "alpine:latest",
            "status": "created",
            "pid": null,
            "cpus": 1,
            "memory_mb": 128,
            "volumes": [],
            "env": {},
            "cmd": ["sleep", "60"],
            "box_dir": home_dir.join("boxes").join(id),
            "console_log": home_dir.join("boxes").join(id).join("logs/console.log"),
            "created_at": "2026-07-15T00:00:00Z",
            "started_at": null,
            "auto_remove": false
        }))
        .unwrap();
        record.volume_names = vec!["workspace".to_string()];
        record.network_mode = NetworkMode::Bridge {
            network: "dev".to_string(),
        };
        record.network_name = Some("dev".to_string());
        record
    }

    fn stores(home_dir: &Path) -> (VolumeStore, NetworkStore) {
        let volumes = VolumeStore::new(home_dir.join("volumes.json"), home_dir.join("volumes"));
        volumes.create(VolumeConfig::new("workspace", "")).unwrap();
        let networks = NetworkStore::new(home_dir.join("networks.json"));
        networks
            .create(NetworkConfig::new("dev", "10.88.0.0/24").unwrap())
            .unwrap();
        (volumes, networks)
    }

    fn snapshot_record(home_dir: &Path, snapshot_id: &str) -> BoxRecord {
        let mut record = record(home_dir);
        record.volume_names.clear();
        record.network_mode = NetworkMode::None;
        record.network_name = None;
        let config = a3s_box_core::BoxConfig {
            image: record.image.clone(),
            isolation: ExecutionIsolation::Sandbox,
            ..Default::default()
        };
        record.isolation = ExecutionIsolation::Sandbox;
        record.managed_execution = Some(
            crate::ManagedExecutionMetadata::new(
                OperationId::new("snapshot-restore-operation").unwrap(),
                ExecutionGeneration::INITIAL,
                CreateExecutionRequest {
                    external_sandbox_id: "snapshot-restore".to_string(),
                    config,
                    labels: Default::default(),
                    policy: Default::default(),
                    rootfs_snapshot_id: Some(ExecutionSnapshotId::new(snapshot_id).unwrap()),
                },
            )
            .unwrap(),
        );
        record
    }

    fn create_snapshot(home_dir: &Path, snapshot_id: &str) -> PathBuf {
        let source = home_dir.join("snapshot-source");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join("state.txt"), "safe-state").unwrap();
        let metadata = SnapshotMetadata::new(
            snapshot_id.to_string(),
            snapshot_id.to_string(),
            "source-execution".to_string(),
            "alpine:latest".to_string(),
        );
        crate::SnapshotStore::new(&home_dir.join("snapshots"))
            .unwrap()
            .save(metadata, &source)
            .unwrap();
        home_dir.join("snapshots").join(snapshot_id).join("rootfs")
    }

    #[test]
    fn failed_start_rolls_back_only_resources_acquired_by_that_attempt() {
        let temporary = tempfile::tempdir().unwrap();
        let record = record(temporary.path());
        let (volumes, networks) = stores(temporary.path());

        let guard = ExecutionResourceGuard::prepare(temporary.path(), &record).unwrap();
        assert_eq!(
            volumes.get("workspace").unwrap().unwrap().in_use_by,
            vec![record.id.clone()]
        );
        assert!(networks
            .get("dev")
            .unwrap()
            .unwrap()
            .endpoints
            .contains_key(&record.id));

        drop(guard);

        assert!(volumes
            .get("workspace")
            .unwrap()
            .unwrap()
            .in_use_by
            .is_empty());
        assert!(!networks
            .get("dev")
            .unwrap()
            .unwrap()
            .endpoints
            .contains_key(&record.id));
    }

    #[test]
    fn preexisting_resource_ownership_survives_start_rollback() {
        let temporary = tempfile::tempdir().unwrap();
        let record = record(temporary.path());
        let (volumes, networks) = stores(temporary.path());
        volumes
            .modify("workspace", |volume| volume.attach(&record.id))
            .unwrap();
        networks
            .with_write_lock(|entries| -> Result<(), BoxError> {
                entries
                    .get_mut("dev")
                    .unwrap()
                    .connect(&record.id, &record.name)
                    .map_err(BoxError::NetworkError)?;
                Ok(())
            })
            .unwrap();

        drop(ExecutionResourceGuard::prepare(temporary.path(), &record).unwrap());

        assert_eq!(
            volumes.get("workspace").unwrap().unwrap().in_use_by,
            vec![record.id.clone()]
        );
        assert!(networks
            .get("dev")
            .unwrap()
            .unwrap()
            .endpoints
            .contains_key(&record.id));
    }

    #[test]
    fn successful_start_keeps_prepared_resources() {
        let temporary = tempfile::tempdir().unwrap();
        let record = record(temporary.path());
        let (volumes, networks) = stores(temporary.path());

        ExecutionResourceGuard::prepare(temporary.path(), &record)
            .unwrap()
            .disarm();

        assert_eq!(
            volumes.get("workspace").unwrap().unwrap().in_use_by,
            vec![record.id.clone()]
        );
        assert!(networks
            .get("dev")
            .unwrap()
            .unwrap()
            .endpoints
            .contains_key(&record.id));
    }

    #[test]
    fn snapshot_marker_is_canonical_atomic_and_rolled_back_with_the_start_attempt() {
        let temporary = tempfile::tempdir().unwrap();
        let snapshot_id = "managed-snapshot";
        let expected = create_snapshot(temporary.path(), snapshot_id)
            .canonicalize()
            .unwrap();
        let record = snapshot_record(temporary.path(), snapshot_id);
        let marker = record.box_dir.join(".snapshot-lower");

        let guard = ExecutionResourceGuard::prepare(temporary.path(), &record).unwrap();
        assert_eq!(
            PathBuf::from(std::fs::read_to_string(&marker).unwrap()),
            expected
        );
        assert!(std::fs::symlink_metadata(&marker)
            .unwrap()
            .file_type()
            .is_file());
        drop(guard);
        assert!(!marker.exists());

        ExecutionResourceGuard::prepare(temporary.path(), &record)
            .unwrap()
            .disarm();
        assert_eq!(
            PathBuf::from(std::fs::read_to_string(marker).unwrap()),
            expected
        );
    }

    #[test]
    fn snapshot_restore_rejects_a_conflicting_existing_marker() {
        let temporary = tempfile::tempdir().unwrap();
        let snapshot_id = "managed-snapshot";
        create_snapshot(temporary.path(), snapshot_id);
        let record = snapshot_record(temporary.path(), snapshot_id);
        std::fs::create_dir_all(&record.box_dir).unwrap();
        std::fs::write(record.box_dir.join(".snapshot-lower"), "/tmp/untrusted").unwrap();

        assert!(matches!(
            ExecutionResourceGuard::prepare(temporary.path(), &record),
            Err(ExecutionManagerError::Unavailable(_))
        ));
    }

    #[cfg(unix)]
    #[test]
    fn snapshot_restore_rejects_a_symlinked_rootfs_escape() {
        use std::os::unix::fs::symlink;

        let temporary = tempfile::tempdir().unwrap();
        let snapshot_id = "managed-snapshot";
        let snapshot_dir = temporary.path().join("snapshots").join(snapshot_id);
        let outside = temporary.path().join("outside-rootfs");
        std::fs::create_dir_all(&snapshot_dir).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        let metadata = SnapshotMetadata::new(
            snapshot_id.to_string(),
            snapshot_id.to_string(),
            "source-execution".to_string(),
            "alpine:latest".to_string(),
        );
        std::fs::write(
            snapshot_dir.join("metadata.json"),
            serde_json::to_vec(&metadata).unwrap(),
        )
        .unwrap();
        symlink(&outside, snapshot_dir.join("rootfs")).unwrap();
        let record = snapshot_record(temporary.path(), snapshot_id);

        assert!(matches!(
            ExecutionResourceGuard::prepare(temporary.path(), &record),
            Err(ExecutionManagerError::Unavailable(_))
        ));
        assert!(!record.box_dir.join(".snapshot-lower").exists());
    }

    #[test]
    fn terminal_release_detaches_all_record_resources() {
        let temporary = tempfile::tempdir().unwrap();
        let record = record(temporary.path());
        let (volumes, networks) = stores(temporary.path());
        ExecutionResourceGuard::prepare(temporary.path(), &record)
            .unwrap()
            .disarm();

        release_resources(temporary.path(), &record).unwrap();

        assert!(volumes
            .get("workspace")
            .unwrap()
            .unwrap()
            .in_use_by
            .is_empty());
        assert!(!networks
            .get("dev")
            .unwrap()
            .unwrap()
            .endpoints
            .contains_key(&record.id));
    }

    #[test]
    fn restart_release_and_prepare_rebind_resources_exactly_once() {
        let temporary = tempfile::tempdir().unwrap();
        let record = record(temporary.path());
        let (volumes, networks) = stores(temporary.path());
        ExecutionResourceGuard::prepare(temporary.path(), &record)
            .unwrap()
            .disarm();

        release_resources(temporary.path(), &record).unwrap();
        ExecutionResourceGuard::prepare(temporary.path(), &record)
            .unwrap()
            .disarm();

        assert_eq!(
            volumes.get("workspace").unwrap().unwrap().in_use_by,
            vec![record.id.clone()]
        );
        let network = networks.get("dev").unwrap().unwrap();
        assert_eq!(
            network
                .endpoints
                .keys()
                .filter(|execution_id| execution_id.as_str() == record.id)
                .count(),
            1
        );
    }

    #[test]
    fn concurrent_preparation_allocates_distinct_network_endpoints() {
        use std::collections::HashSet;
        use std::sync::{Arc, Barrier};

        let temporary = tempfile::tempdir().unwrap();
        let home_dir = temporary.path().to_path_buf();
        let (_volumes, networks) = stores(&home_dir);
        let barrier = Arc::new(Barrier::new(16));
        let handles = (0..16)
            .map(|index| {
                let home_dir = home_dir.clone();
                let barrier = Arc::clone(&barrier);
                std::thread::spawn(move || {
                    let mut record = record(&home_dir);
                    record.id = format!("00000000-0000-4000-8000-{index:012}");
                    record.name = format!("worker-{index}");
                    record.volume_names.clear();
                    barrier.wait();
                    ExecutionResourceGuard::prepare(&home_dir, &record)
                        .unwrap()
                        .disarm();
                })
            })
            .collect::<Vec<_>>();

        for handle in handles {
            handle.join().unwrap();
        }

        let network = networks.get("dev").unwrap().unwrap();
        let addresses = network
            .endpoints
            .values()
            .map(|endpoint| endpoint.ip_address)
            .collect::<HashSet<_>>();
        assert_eq!(network.endpoints.len(), 16);
        assert_eq!(addresses.len(), 16);
    }
}
