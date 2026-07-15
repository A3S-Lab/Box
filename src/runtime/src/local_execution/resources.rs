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
    armed: bool,
}

impl ExecutionResourceGuard {
    pub(super) fn prepare(home_dir: &Path, record: &BoxRecord) -> ExecutionManagerResult<Self> {
        let mut guard = Self {
            home_dir: home_dir.to_path_buf(),
            execution_id: record.id.clone(),
            attached_volumes: Vec::new(),
            connected_network: None,
            armed: true,
        };

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

        self.armed = false;
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
    use a3s_box_core::{network::NetworkConfig, volume::VolumeConfig};

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
