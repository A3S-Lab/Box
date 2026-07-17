//! Durable removal and complete host-resource cleanup for managed executions.

use std::path::Path;
#[cfg(target_os = "linux")]
use std::path::PathBuf;

use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionManagerError, ExecutionManagerResult,
};

use super::record::execution_id;
use super::store::run_store;
use super::support::generation;
use super::{BoxRecord, LocalExecutionManager};

impl LocalExecutionManager {
    /// Load one managed record without reconciling provider state.
    pub(crate) async fn managed_record(
        &self,
        execution_id: &ExecutionId,
    ) -> ExecutionManagerResult<Option<BoxRecord>> {
        self.get(execution_id).await
    }

    /// Load the complete managed inventory without exposing legacy CLI boxes.
    pub(crate) async fn managed_records(&self) -> ExecutionManagerResult<Vec<BoxRecord>> {
        let store = self.store.clone();
        run_store(move || store.list()).await
    }

    /// Remove one terminal generation after durably claiming its teardown.
    ///
    /// A failed cleanup deliberately leaves the record in `removing`. Retrying
    /// the same generation resumes cleanup before the record is forgotten.
    pub(crate) async fn remove_execution(
        &self,
        execution_id: &ExecutionId,
        expected_generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<bool> {
        let store = self.store.clone();
        let claimed_id = execution_id.clone();
        let claimed = run_store(move || store.begin_remove(&claimed_id, expected_generation)).await?;
        let Some(record) = claimed else {
            return Ok(false);
        };
        self.finish_remove(record).await
    }

    pub(super) async fn finish_remove(
        &self,
        record: BoxRecord,
    ) -> ExecutionManagerResult<bool> {
        let execution_id = execution_id(&record)?;
        let expected_generation = generation(&record, &execution_id)?;

        // Detach shared stores before deleting execution-owned paths. Both the
        // detach and the path cleanup are idempotent, so a crash can replay.
        self.release_execution_resources(&record).await?;

        let home_dir = self.home_dir.clone();
        let cleanup_record = record.clone();
        tokio::task::spawn_blocking(move || cleanup_execution_paths(&home_dir, &cleanup_record))
            .await
            .map_err(|error| {
                ExecutionManagerError::Internal(format!(
                    "managed removal task failed for {execution_id}: {error}"
                ))
            })??;

        let store = self.store.clone();
        let removed_id = execution_id.clone();
        run_store(move || store.finish_remove(&removed_id, expected_generation)).await
    }
}

fn cleanup_execution_paths(home_dir: &Path, record: &BoxRecord) -> ExecutionManagerResult<()> {
    validate_owned_paths(home_dir, record)?;

    if record.isolation.is_sandbox() {
        crate::vm::reap::cleanup_recorded_sandbox_runtime_in(
            home_dir,
            &record.box_dir,
            &record.id,
        )
        .map_err(|error| cleanup_error(record, "delete the recorded crun runtime", error))?;
    }

    remove_anonymous_volumes(home_dir, record)?;

    let socket_dir = crate::vm::runtime_socket_dir(home_dir, &record.id);
    #[cfg(target_os = "linux")]
    crate::network::terminate_passt(&socket_dir);

    crate::rootfs::unmount_box_overlay(&record.box_dir.join("merged"));
    crate::rootfs::unmount_box_rootfs(&record.box_dir.join("rootfs"));

    remove_tree_if_present(&record.box_dir)
        .map_err(|error| cleanup_error(record, "remove the execution directory", error))?;
    remove_tree_if_present(&socket_dir)
        .map_err(|error| cleanup_error(record, "remove the runtime socket directory", error))?;

    let runtime_root = home_dir.join("run/crun").join(&record.id);
    remove_tree_if_present(&runtime_root)
        .map_err(|error| cleanup_error(record, "remove the crun state directory", error))?;

    let bind_mount_dir = std::env::temp_dir().join(format!("a3s-fs-mount-{}", record.id));
    remove_tree_if_present(&bind_mount_dir)
        .map_err(|error| cleanup_error(record, "remove temporary bind-mount staging", error))?;

    remove_host_cgroup(record)?;
    Ok(())
}

fn validate_owned_paths(home_dir: &Path, record: &BoxRecord) -> ExecutionManagerResult<()> {
    uuid::Uuid::parse_str(&record.id).map_err(|error| {
        ExecutionManagerError::Internal(format!(
            "managed execution has an invalid internal ID {}: {error}",
            record.id
        ))
    })?;
    let expected_box_dir = home_dir.join("boxes").join(&record.id);
    if record.box_dir != expected_box_dir {
        return Err(ExecutionManagerError::Internal(format!(
            "managed execution {} has an unexpected host directory {}",
            record.id,
            record.box_dir.display()
        )));
    }

    let internal_exec = expected_box_dir.join("sockets/exec.sock");
    let external_exec = crate::vm::runtime_socket_dir(home_dir, &record.id).join("exec.sock");
    if !record.exec_socket_path.as_os_str().is_empty()
        && record.exec_socket_path != internal_exec
        && record.exec_socket_path != external_exec
    {
        return Err(ExecutionManagerError::Internal(format!(
            "managed execution {} has an unexpected exec endpoint {}",
            record.id,
            record.exec_socket_path.display()
        )));
    }
    Ok(())
}

fn remove_anonymous_volumes(
    home_dir: &Path,
    record: &BoxRecord,
) -> ExecutionManagerResult<()> {
    if record.anonymous_volumes.is_empty() {
        return Ok(());
    }
    let store = crate::VolumeStore::new(home_dir.join("volumes.json"), home_dir.join("volumes"));
    for name in &record.anonymous_volumes {
        if store
            .get(name)
            .map_err(|error| cleanup_error(record, "inspect an anonymous volume", error))?
            .is_some()
        {
            store
                .remove(name, true)
                .map_err(|error| cleanup_error(record, "remove an anonymous volume", error))?;
        }
    }
    Ok(())
}

fn remove_tree_if_present(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_host_cgroup(record: &BoxRecord) -> ExecutionManagerResult<()> {
    #[cfg(target_os = "linux")]
    {
        let path = PathBuf::from("/sys/fs/cgroup/a3s-box").join(&record.id);
        for attempt in 0..50 {
            match std::fs::remove_dir(&path) {
                Ok(()) => return Ok(()),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
                Err(error) if attempt + 1 < 50 => {
                    let _ = error;
                    std::thread::sleep(std::time::Duration::from_millis(20));
                }
                Err(error) => {
                    return Err(cleanup_error(
                        record,
                        "remove the host cgroup",
                        error,
                    ));
                }
            }
        }
    }
    #[cfg(not(target_os = "linux"))]
    let _ = record;
    Ok(())
}

fn cleanup_error(
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
    use std::sync::Arc;

    use a3s_box_core::{
        BoxConfig, CreateExecutionRequest, ExecutionIsolation, ExecutionManager,
        ExecutionRecordPolicy, OperationId,
    };
    use async_trait::async_trait;

    use super::*;
    use crate::local_execution::{
        LocalExecutionBackend, LocalExecutionHandle, LocalExecutionObservation,
    };

    struct UnusedBackend;

    #[async_trait]
    impl LocalExecutionBackend for UnusedBackend {
        async fn start(&self, _record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
            unreachable!("removal test never starts a backend")
        }

        async fn inspect(
            &self,
            _record: &BoxRecord,
        ) -> ExecutionManagerResult<LocalExecutionObservation> {
            unreachable!("removal test never inspects a backend")
        }

        async fn pause(
            &self,
            _record: &BoxRecord,
            _keep_memory: bool,
        ) -> ExecutionManagerResult<LocalExecutionHandle> {
            unreachable!("removal test never pauses a backend")
        }

        async fn resume(
            &self,
            _record: &BoxRecord,
        ) -> ExecutionManagerResult<LocalExecutionHandle> {
            unreachable!("removal test never resumes a backend")
        }

        async fn kill(
            &self,
            _record: &BoxRecord,
        ) -> ExecutionManagerResult<a3s_box_core::KillOutcome> {
            unreachable!("removal test never kills a backend")
        }
    }

    #[tokio::test]
    async fn removal_claim_cleans_owned_paths_before_forgetting_the_record() {
        let temporary = tempfile::tempdir().unwrap();
        let home_dir = temporary.path().join("home");
        let manager = LocalExecutionManager::new(
            home_dir.join("boxes.json"),
            &home_dir,
            Arc::new(UnusedBackend),
        );
        let reservation = manager
            .create(
                CreateExecutionRequest {
                    external_sandbox_id: "runtime-unit-1".to_string(),
                    config: BoxConfig {
                        isolation: ExecutionIsolation::Sandbox,
                        persistent: true,
                        ..Default::default()
                    },
                    labels: Default::default(),
                    policy: ExecutionRecordPolicy::default(),
                    rootfs_snapshot_id: None,
                },
                &OperationId::new("runtime-create-1").unwrap(),
            )
            .await
            .unwrap();

        let box_dir = home_dir
            .join("boxes")
            .join(reservation.execution_id.as_str());
        std::fs::create_dir_all(box_dir.join("logs")).unwrap();
        std::fs::write(
            box_dir.join("logs/container.json"),
            b"retained until remove\n",
        )
        .unwrap();
        let socket_dir =
            crate::vm::runtime_socket_dir(&home_dir, reservation.execution_id.as_str());
        std::fs::create_dir_all(&socket_dir).unwrap();

        assert!(manager
            .remove_execution(&reservation.execution_id, reservation.generation)
            .await
            .unwrap());
        assert!(!box_dir.exists());
        assert!(!socket_dir.exists());
        assert!(manager
            .managed_record(&reservation.execution_id)
            .await
            .unwrap()
            .is_none());
        assert!(!manager
            .remove_execution(&reservation.execution_id, reservation.generation)
            .await
            .unwrap());
    }
}
