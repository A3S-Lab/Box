//! Crash-recoverable filesystem snapshots for managed executions.

use std::path::{Path, PathBuf};

use a3s_box_core::snapshot::SnapshotMetadata;
use a3s_box_core::{
    ExecutionBackend, ExecutionGeneration, ExecutionId, ExecutionLease, ExecutionManagerError,
    ExecutionManagerResult, ExecutionSnapshot, ExecutionSnapshotId, ExecutionState,
};

use super::record::lease_from_record;
use super::support::{
    managed_state, paused_with_memory, require_generation, required_handle, state_conflict,
};
use super::{LocalExecutionHandle, LocalExecutionManager, ManagedExecutionState, RuntimeUpdate};
use crate::{BoxRecord, BoxStateStore, ManagedExecutionOperation, SnapshotStore};

impl LocalExecutionManager {
    pub(super) async fn create_snapshot(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
        snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<ExecutionSnapshot> {
        let record = self
            .get(execution_id)
            .await?
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        require_generation(&record, execution_id, expected_generation)?;
        let source_state = managed_state(&record)?;
        if source_state == ManagedExecutionState::Snapshotting {
            let (pending_snapshot_id, _) = snapshot_operation(&record)?;
            if &pending_snapshot_id != snapshot_id {
                return Err(state_conflict(&record, execution_id, "snapshot"));
            }
            return self.drive_snapshot(record).await;
        }
        if !matches!(
            source_state,
            ManagedExecutionState::Running | ManagedExecutionState::Paused
        ) {
            return Err(state_conflict(&record, execution_id, "snapshot"));
        }
        let metadata = record.managed_execution.as_ref().ok_or_else(|| {
            ExecutionManagerError::Internal(format!(
                "execution {execution_id} lost managed lifecycle metadata"
            ))
        })?;
        if metadata.plan.backend != ExecutionBackend::Crun {
            return Err(ExecutionManagerError::Conflict {
                execution_id: execution_id.clone(),
                message: "filesystem snapshots currently require the Sandbox backend".to_string(),
            });
        }
        if let Some(existing) = self.load_snapshot(snapshot_id).await? {
            if existing.source_box_id != record.id {
                return Err(ExecutionManagerError::Conflict {
                    execution_id: execution_id.clone(),
                    message: format!(
                        "filesystem snapshot {snapshot_id} belongs to another execution"
                    ),
                });
            }
            return Ok(ExecutionSnapshot {
                snapshot_id: snapshot_id.clone(),
                size_bytes: existing.size_bytes,
                state: execution_state(source_state)?,
                lease: lease_from_record(&record)?,
            });
        }

        let claimed = self
            .transition(
                &record,
                source_state,
                ManagedExecutionState::Snapshotting,
                RuntimeUpdate::SnapshotClaim {
                    snapshot_id: snapshot_id.clone(),
                    source_state,
                },
            )
            .await?;
        self.drive_snapshot(claimed).await
    }

    pub(super) async fn recover_snapshot(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<ExecutionLease> {
        self.drive_snapshot(record)
            .await
            .map(|snapshot| snapshot.lease)
    }

    pub(super) async fn stabilize_snapshot(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<BoxRecord> {
        if managed_state(&record)? != ManagedExecutionState::Snapshotting {
            return Ok(record);
        }
        let execution_id = ExecutionId::new(record.id.clone())?;
        self.drive_snapshot(record).await?;
        self.get(&execution_id)
            .await?
            .ok_or(ExecutionManagerError::NotFound(execution_id))
    }

    async fn drive_snapshot(&self, record: BoxRecord) -> ExecutionManagerResult<ExecutionSnapshot> {
        let (snapshot_id, source_state) = snapshot_operation(&record)?;
        let execution_id = ExecutionId::new(record.id.clone())?;
        if source_state == ManagedExecutionState::Paused
            && !paused_with_memory(&record, &execution_id)?
        {
            return self.drive_cold_paused_snapshot(record, snapshot_id).await;
        }
        let observation = self.backend.inspect(&record).await?;
        observation.validate(&execution_id)?;
        let mut handle = required_handle(&observation, &execution_id)?;

        match (source_state, observation.state) {
            (ManagedExecutionState::Running, ExecutionState::Running) => {
                handle = self.pause_for_snapshot(&record, &execution_id).await?;
            }
            (ManagedExecutionState::Running, ExecutionState::Paused)
            | (ManagedExecutionState::Paused, ExecutionState::Paused) => {}
            (ManagedExecutionState::Paused, ExecutionState::Running) => {
                return Err(ExecutionManagerError::Conflict {
                    execution_id,
                    message: "a paused execution resumed while its filesystem snapshot was pending"
                        .to_string(),
                })
            }
            (_, state) => {
                return Err(ExecutionManagerError::Conflict {
                    execution_id,
                    message: format!(
                        "execution entered {state:?} while its filesystem snapshot was pending"
                    ),
                })
            }
        }

        let capture = self
            .capture_snapshot_rootfs(record.clone(), snapshot_id.clone())
            .await;
        let size_bytes = match capture {
            Ok(size_bytes) => size_bytes,
            Err(capture_error) => {
                let restored = self
                    .restore_snapshot_source_state(&record, source_state, handle)
                    .await;
                return match restored {
                    Ok(_) => Err(capture_error),
                    Err(restore_error) => Err(ExecutionManagerError::Internal(format!(
                        "{capture_error}; failed to restore execution {} after snapshot failure: {restore_error}",
                        record.id
                    ))),
                };
            }
        };

        let completed = self
            .restore_snapshot_source_state(&record, source_state, handle)
            .await?;
        Ok(ExecutionSnapshot {
            snapshot_id,
            size_bytes,
            state: execution_state(source_state)?,
            lease: lease_from_record(&completed)?,
        })
    }

    async fn drive_cold_paused_snapshot(
        &self,
        record: BoxRecord,
        snapshot_id: ExecutionSnapshotId,
    ) -> ExecutionManagerResult<ExecutionSnapshot> {
        let prepared = self.backend.prepare_quiescent_rootfs(&record).await;
        let capture = match prepared {
            Ok(()) => {
                self.capture_snapshot_rootfs(record.clone(), snapshot_id.clone())
                    .await
            }
            Err(error) => Err(error),
        };
        let cleanup = self.backend.cleanup_quiescent_rootfs(&record).await;
        if let Err(cleanup_error) = cleanup {
            return match capture {
                Ok(_) => Err(cleanup_error),
                Err(capture_error) => Err(ExecutionManagerError::Internal(format!(
                    "{capture_error}; failed to clean up quiescent rootfs for {}: {cleanup_error}",
                    record.id
                ))),
            };
        }
        let restored = self
            .complete_transition(
                &record,
                ManagedExecutionState::Snapshotting,
                ManagedExecutionState::Paused,
                RuntimeUpdate::None,
            )
            .await;
        match (capture, restored) {
            (Ok(size_bytes), Ok(restored)) => Ok(ExecutionSnapshot {
                snapshot_id,
                size_bytes,
                state: ExecutionState::Paused,
                lease: lease_from_record(&restored)?,
            }),
            (Err(capture_error), Ok(_)) => Err(capture_error),
            (Ok(_), Err(restore_error)) => Err(restore_error),
            (Err(capture_error), Err(restore_error)) => {
                Err(ExecutionManagerError::Internal(format!(
                    "{capture_error}; failed to restore cold-paused execution {} after snapshot failure: {restore_error}",
                    record.id
                )))
            }
        }
    }

    async fn pause_for_snapshot(
        &self,
        record: &BoxRecord,
        execution_id: &ExecutionId,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        match self.backend.pause(record, true).await {
            Ok(handle) => {
                handle.validate(execution_id)?;
                Ok(handle)
            }
            Err(pause_error) => match self.backend.inspect(record).await {
                Ok(observation) if observation.state == ExecutionState::Paused => {
                    observation.validate(execution_id)?;
                    required_handle(&observation, execution_id)
                }
                _ => Err(pause_error),
            },
        }
    }

    async fn restore_snapshot_source_state(
        &self,
        record: &BoxRecord,
        source_state: ManagedExecutionState,
        mut handle: LocalExecutionHandle,
    ) -> ExecutionManagerResult<BoxRecord> {
        if source_state == ManagedExecutionState::Running {
            let execution_id = ExecutionId::new(record.id.clone())?;
            handle = match self.backend.resume(record).await {
                Ok(handle) => {
                    handle.validate(&execution_id)?;
                    handle
                }
                Err(resume_error) => match self.backend.inspect(record).await {
                    Ok(observation) if observation.state == ExecutionState::Running => {
                        observation.validate(&execution_id)?;
                        required_handle(&observation, &execution_id)?
                    }
                    _ => return Err(resume_error),
                },
            };
        }
        self.complete_with_handle(
            record,
            ManagedExecutionState::Snapshotting,
            source_state,
            handle,
        )
        .await
    }

    async fn capture_snapshot_rootfs(
        &self,
        record: BoxRecord,
        snapshot_id: ExecutionSnapshotId,
    ) -> ExecutionManagerResult<u64> {
        let home_dir = self.home_dir.clone();
        let execution_id = record.id.clone();
        tokio::task::spawn_blocking(move || {
            let store = SnapshotStore::new(&home_dir.join("snapshots"))
                .map_err(|error| snapshot_error(&record, "open snapshot store", error))?;
            if let Some(existing) = store
                .get(snapshot_id.as_str())
                .map_err(|error| snapshot_error(&record, "load snapshot", error))?
            {
                if existing.id != snapshot_id.as_str()
                    || existing.source_box_id != record.id
                    || !store.rootfs_path(snapshot_id.as_str()).is_dir()
                {
                    return Err(ExecutionManagerError::Unavailable(format!(
                        "filesystem snapshot {snapshot_id} has inconsistent persisted metadata"
                    )));
                }
                return Ok(existing.size_bytes);
            }
            let rootfs = resolve_managed_rootfs(&record.box_dir).ok_or_else(|| {
                ExecutionManagerError::Unavailable(format!(
                    "execution {} has no populated managed rootfs to snapshot",
                    record.id
                ))
            })?;
            let metadata = build_snapshot_metadata(&record, &snapshot_id)?;
            #[cfg(target_os = "linux")]
            let rootfs_metadata = capture_sandbox_rootfs_metadata(&record, &rootfs)?;
            #[cfg(target_os = "linux")]
            let saved = store.save_managed(metadata, &rootfs, &rootfs_metadata);
            #[cfg(not(target_os = "linux"))]
            let saved = store.save(metadata, &rootfs);
            match saved {
                Ok(saved) => Ok(saved.size_bytes),
                Err(save_error) => match store.get(snapshot_id.as_str()) {
                    Ok(Some(existing))
                        if existing.id == snapshot_id.as_str()
                            && existing.source_box_id == record.id
                            && store.rootfs_path(snapshot_id.as_str()).is_dir() =>
                    {
                        Ok(existing.size_bytes)
                    }
                    _ => Err(snapshot_error(&record, "capture rootfs", save_error)),
                },
            }
        })
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "filesystem snapshot task failed for {}: {error}",
                execution_id
            ))
        })?
    }

    pub(super) async fn snapshot_size(
        &self,
        snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<Option<u64>> {
        Ok(self
            .load_snapshot(snapshot_id)
            .await?
            .map(|snapshot| snapshot.size_bytes))
    }

    async fn load_snapshot(
        &self,
        snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<Option<SnapshotMetadata>> {
        let home_dir = self.home_dir.clone();
        let snapshot_id = snapshot_id.clone();
        tokio::task::spawn_blocking(move || {
            let store = SnapshotStore::new(&home_dir.join("snapshots")).map_err(|error| {
                ExecutionManagerError::Unavailable(format!(
                    "failed to open filesystem snapshot store: {error}"
                ))
            })?;
            let Some(metadata) = store.get(snapshot_id.as_str()).map_err(|error| {
                ExecutionManagerError::Unavailable(format!(
                    "failed to inspect filesystem snapshot {snapshot_id}: {error}"
                ))
            })?
            else {
                return Ok(None);
            };
            if metadata.id != snapshot_id.as_str()
                || !store.rootfs_path(snapshot_id.as_str()).is_dir()
            {
                return Err(ExecutionManagerError::Unavailable(format!(
                    "filesystem snapshot {snapshot_id} is not a valid published snapshot"
                )));
            }
            Ok(Some(metadata))
        })
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!("filesystem snapshot task failed: {error}"))
        })?
    }

    pub(super) async fn delete_snapshot(
        &self,
        snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<bool> {
        let home_dir = self.home_dir.clone();
        let state_path = self.store.path().to_path_buf();
        let snapshot_id = snapshot_id.clone();
        tokio::task::spawn_blocking(move || {
            let store = SnapshotStore::new(&home_dir.join("snapshots")).map_err(|error| {
                ExecutionManagerError::Unavailable(format!(
                    "failed to open filesystem snapshot store: {error}"
                ))
            })?;
            let _snapshot_lock = store.acquire_exclusive_lock().map_err(|error| {
                ExecutionManagerError::Unavailable(format!(
                    "failed to lock filesystem snapshot store: {error}"
                ))
            })?;
            let state = BoxStateStore::load_readonly(state_path).map_err(|error| {
                ExecutionManagerError::Unavailable(format!(
                    "failed to inspect snapshot users: {error}"
                ))
            })?;
            let rootfs = store.rootfs_path(snapshot_id.as_str());
            let in_use = state.records().iter().any(|record| {
                let requested = record
                    .managed_execution
                    .as_ref()
                    .and_then(|metadata| metadata.request.rootfs_snapshot_id.as_ref())
                    == Some(&snapshot_id);
                let request_is_live = requested
                    && !record
                        .managed_state()
                        .is_ok_and(|state| state.is_some_and(ManagedExecutionState::is_terminal));
                let marker_uses_snapshot =
                    std::fs::read_to_string(record.box_dir.join(".snapshot-lower"))
                        .is_ok_and(|value| Path::new(value.trim()) == rootfs.as_path());
                request_is_live || marker_uses_snapshot
            });
            if in_use {
                return Err(ExecutionManagerError::Conflict {
                    execution_id: ExecutionId::new(format!("snapshot-{snapshot_id}"))?,
                    message: "filesystem snapshot is in use by an active execution".to_string(),
                });
            }
            store.delete_locked(snapshot_id.as_str()).map_err(|error| {
                ExecutionManagerError::Unavailable(format!(
                    "failed to delete filesystem snapshot {snapshot_id}: {error}"
                ))
            })
        })
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!("filesystem snapshot task failed: {error}"))
        })?
    }
}

fn snapshot_operation(
    record: &BoxRecord,
) -> ExecutionManagerResult<(ExecutionSnapshotId, ManagedExecutionState)> {
    match record
        .managed_execution
        .as_ref()
        .and_then(|metadata| metadata.pending_operation.as_ref())
    {
        Some(ManagedExecutionOperation::Snapshot {
            snapshot_id,
            source_state,
        }) if matches!(
            source_state,
            ManagedExecutionState::Running | ManagedExecutionState::Paused
        ) =>
        {
            Ok((snapshot_id.clone(), *source_state))
        }
        _ => Err(ExecutionManagerError::Internal(format!(
            "execution {} has invalid snapshot recovery metadata",
            record.id
        ))),
    }
}

fn execution_state(state: ManagedExecutionState) -> ExecutionManagerResult<ExecutionState> {
    match state {
        ManagedExecutionState::Running => Ok(ExecutionState::Running),
        ManagedExecutionState::Paused => Ok(ExecutionState::Paused),
        _ => Err(ExecutionManagerError::Internal(format!(
            "invalid stable snapshot state {state}"
        ))),
    }
}

fn resolve_managed_rootfs(box_dir: &Path) -> Option<PathBuf> {
    let populated = |path: &Path| {
        path.is_dir()
            && std::fs::read_dir(path)
                .map(|mut entries| entries.next().is_some())
                .unwrap_or(false)
    };
    let merged = box_dir.join("merged");
    if populated(&merged) {
        return Some(merged);
    }
    let rootfs = box_dir.join("rootfs");
    let apfs_data = rootfs.join(".a3s-rootfs");
    if populated(&apfs_data) {
        return Some(apfs_data);
    }
    populated(&rootfs).then_some(rootfs)
}

#[cfg(target_os = "linux")]
fn capture_sandbox_rootfs_metadata(
    record: &BoxRecord,
    rootfs: &Path,
) -> ExecutionManagerResult<a3s_box_core::rootfs_metadata::RootfsMetadataManifest> {
    let plan = if sandbox_bundle_config_present(record)? {
        let plan = sandbox_id_mapping_plan_from_bundle(record, Some(rootfs))?;
        crate::sandbox::rootfs::persist_snapshot_id_mappings(&record.box_dir, &plan).map_err(
            |error| snapshot_error(record, "persist Sandbox snapshot ID mappings", error),
        )?;
        plan
    } else {
        let plan = crate::sandbox::rootfs::load_snapshot_id_mappings(&record.box_dir)
            .map_err(|error| snapshot_error(record, "load Sandbox snapshot ID mappings", error))?
            .ok_or_else(|| {
                snapshot_error(
                    record,
                    "load Sandbox snapshot ID mappings",
                    "neither a live OCI bundle nor a retained mapping artifact exists",
                )
            })?;
        validate_id_mapping_plan(record, &plan)?;
        plan
    };
    crate::sandbox::rootfs::capture_snapshot_rootfs_metadata(rootfs, &plan)
        .map_err(|error| snapshot_error(record, "capture Sandbox rootfs metadata", error))
}

#[cfg(target_os = "linux")]
pub(super) fn persist_sandbox_snapshot_mappings(record: &BoxRecord) -> ExecutionManagerResult<()> {
    if sandbox_bundle_config_present(record)? {
        let plan = sandbox_id_mapping_plan_from_bundle(record, None)?;
        return crate::sandbox::rootfs::persist_snapshot_id_mappings(&record.box_dir, &plan)
            .map_err(|error| {
                snapshot_error(record, "persist Sandbox snapshot ID mappings", error)
            });
    }
    if let Some(plan) = crate::sandbox::rootfs::load_snapshot_id_mappings(&record.box_dir)
        .map_err(|error| snapshot_error(record, "load Sandbox snapshot ID mappings", error))?
    {
        return validate_id_mapping_plan(record, &plan);
    }
    Err(snapshot_error(
        record,
        "persist Sandbox snapshot ID mappings",
        "neither a live OCI bundle nor a retained mapping artifact exists",
    ))
}

#[cfg(target_os = "linux")]
fn sandbox_bundle_config_present(record: &BoxRecord) -> ExecutionManagerResult<bool> {
    let config_path = record.box_dir.join("sandbox/bundle/config.json");
    match std::fs::symlink_metadata(&config_path) {
        Ok(metadata) if metadata.file_type().is_file() => Ok(true),
        Ok(_) => Err(snapshot_error(
            record,
            "validate Sandbox OCI mappings",
            format!(
                "OCI configuration is not a regular file: {}",
                config_path.display()
            ),
        )),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(false),
        Err(error) => Err(snapshot_error(
            record,
            "inspect Sandbox OCI mappings",
            format!("{}: {error}", config_path.display()),
        )),
    }
}

#[cfg(target_os = "linux")]
fn sandbox_id_mapping_plan_from_bundle(
    record: &BoxRecord,
    expected_rootfs: Option<&Path>,
) -> ExecutionManagerResult<crate::sandbox::SandboxIdMappingPlan> {
    let config_path = record.box_dir.join("sandbox/bundle/config.json");
    let spec = oci_spec::runtime::Spec::load(&config_path).map_err(|error| {
        snapshot_error(
            record,
            "load Sandbox OCI mappings",
            format!("{}: {error}", config_path.display()),
        )
    })?;
    if let Some(expected_rootfs) = expected_rootfs {
        let configured_rootfs = spec
            .root()
            .as_ref()
            .map(|root| root.path())
            .ok_or_else(|| {
                snapshot_error(
                    record,
                    "validate Sandbox OCI mappings",
                    "OCI specification has no rootfs",
                )
            })?;
        if configured_rootfs != expected_rootfs {
            return Err(snapshot_error(
                record,
                "validate Sandbox OCI mappings",
                format!(
                    "OCI rootfs {} does not match the active rootfs {}",
                    configured_rootfs.display(),
                    expected_rootfs.display()
                ),
            ));
        }
    }
    let linux = spec.linux().as_ref().ok_or_else(|| {
        snapshot_error(
            record,
            "validate Sandbox OCI mappings",
            "OCI specification has no Linux section",
        )
    })?;
    let uid_mappings = convert_id_mappings(record, "UID", linux.uid_mappings())?;
    let gid_mappings = convert_id_mappings(record, "GID", linux.gid_mappings())?;
    let maximum_container_uid = maximum_container_id(record, "UID", &uid_mappings)?;
    let maximum_container_gid = maximum_container_id(record, "GID", &gid_mappings)?;
    let plan = crate::sandbox::SandboxIdMappingPlan {
        uid_mappings,
        gid_mappings,
        maximum_container_uid,
        maximum_container_gid,
    };
    validate_id_mapping_plan(record, &plan)?;
    Ok(plan)
}

#[cfg(target_os = "linux")]
fn validate_id_mapping_plan(
    record: &BoxRecord,
    plan: &crate::sandbox::SandboxIdMappingPlan,
) -> ExecutionManagerResult<()> {
    validate_id_mappings(record, "UID", &plan.uid_mappings)?;
    validate_id_mappings(record, "GID", &plan.gid_mappings)?;
    if plan.maximum_container_uid != maximum_container_id(record, "UID", &plan.uid_mappings)?
        || plan.maximum_container_gid != maximum_container_id(record, "GID", &plan.gid_mappings)?
    {
        return Err(snapshot_error(
            record,
            "validate Sandbox snapshot ID mappings",
            "maximum container IDs do not match the persisted mapping ranges",
        ));
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn convert_id_mappings(
    record: &BoxRecord,
    kind: &str,
    mappings: &Option<Vec<oci_spec::runtime::LinuxIdMapping>>,
) -> ExecutionManagerResult<Vec<crate::sandbox::IdMapping>> {
    let mappings = mappings.as_ref().ok_or_else(|| {
        snapshot_error(
            record,
            "validate Sandbox OCI mappings",
            format!("OCI specification has no {kind} mappings"),
        )
    })?;
    if mappings.is_empty() {
        return Err(snapshot_error(
            record,
            "validate Sandbox OCI mappings",
            format!("OCI specification has empty {kind} mappings"),
        ));
    }
    let converted: Vec<_> = mappings
        .iter()
        .map(|mapping| crate::sandbox::IdMapping {
            container_id: mapping.container_id(),
            host_id: mapping.host_id(),
            size: mapping.size(),
        })
        .collect();
    validate_id_mappings(record, kind, &converted)?;
    Ok(converted)
}

#[cfg(target_os = "linux")]
fn validate_id_mappings(
    record: &BoxRecord,
    kind: &str,
    mappings: &[crate::sandbox::IdMapping],
) -> ExecutionManagerResult<()> {
    for (index, mapping) in mappings.iter().enumerate() {
        let (Some(container_end), Some(host_end)) = (
            mapping.container_id.checked_add(mapping.size),
            mapping.host_id.checked_add(mapping.size),
        ) else {
            return Err(snapshot_error(
                record,
                "validate Sandbox OCI mappings",
                format!("OCI {kind} mapping {index} is empty or overflows"),
            ));
        };
        if mapping.size == 0 {
            return Err(snapshot_error(
                record,
                "validate Sandbox OCI mappings",
                format!("OCI {kind} mapping {index} is empty or overflows"),
            ));
        }
        for previous in &mappings[..index] {
            let previous_container_end = previous
                .container_id
                .checked_add(previous.size)
                .ok_or_else(|| {
                    snapshot_error(
                        record,
                        "validate Sandbox OCI mappings",
                        format!("OCI {kind} mappings overflow"),
                    )
                })?;
            let previous_host_end =
                previous.host_id.checked_add(previous.size).ok_or_else(|| {
                    snapshot_error(
                        record,
                        "validate Sandbox OCI mappings",
                        format!("OCI {kind} mappings overflow"),
                    )
                })?;
            if (mapping.container_id < previous_container_end
                && previous.container_id < container_end)
                || (mapping.host_id < previous_host_end && previous.host_id < host_end)
            {
                return Err(snapshot_error(
                    record,
                    "validate Sandbox OCI mappings",
                    format!("OCI {kind} mappings overlap"),
                ));
            }
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn maximum_container_id(
    record: &BoxRecord,
    kind: &str,
    mappings: &[crate::sandbox::IdMapping],
) -> ExecutionManagerResult<u32> {
    mappings
        .iter()
        .filter_map(|mapping| {
            mapping
                .container_id
                .checked_add(mapping.size)
                .and_then(|end| end.checked_sub(1))
        })
        .max()
        .ok_or_else(|| {
            snapshot_error(
                record,
                "validate Sandbox OCI mappings",
                format!("OCI {kind} mappings have no covered IDs"),
            )
        })
}

fn build_snapshot_metadata(
    record: &BoxRecord,
    snapshot_id: &ExecutionSnapshotId,
) -> ExecutionManagerResult<SnapshotMetadata> {
    let mut metadata = SnapshotMetadata::new(
        snapshot_id.to_string(),
        snapshot_id.to_string(),
        record.id.clone(),
        record.image.clone(),
    );
    metadata.vcpus = record.cpus;
    metadata.memory_mb = record.memory_mb;
    metadata.env = record.env.clone();
    metadata.cmd = record.cmd.clone();
    metadata.entrypoint = record.entrypoint.clone();
    metadata.workdir = record.workdir.clone();
    metadata.labels = record.labels.clone();
    metadata.health_check = record.health_check.clone();
    metadata.healthcheck_disabled = record.healthcheck_disabled;
    metadata.image_config = crate::load_resolved_image_config(&record.box_dir)
        .map_err(|error| snapshot_error(record, "load resolved image configuration", error))?;
    if metadata.image_config.is_none() {
        return Err(snapshot_error(
            record,
            "load resolved image configuration",
            format!(
                "{} is missing",
                record
                    .box_dir
                    .join(crate::RESOLVED_IMAGE_CONFIG_FILE)
                    .display()
            ),
        ));
    }
    Ok(metadata)
}

fn snapshot_error(
    record: &BoxRecord,
    operation: &str,
    error: impl std::fmt::Display,
) -> ExecutionManagerError {
    ExecutionManagerError::Unavailable(format!(
        "failed to {operation} for execution {}: {error}",
        record.id
    ))
}
