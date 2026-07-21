//! Production local execution backend backed by [`crate::VmManager`].

#[path = "vm_sandbox.rs"]
mod sandbox;

use std::path::{Path, PathBuf};
use std::sync::Arc;
#[cfg(unix)]
use std::time::Duration;

use a3s_box_core::{
    EventEmitter, ExecutionBackend, ExecutionId, ExecutionManagerError, ExecutionManagerResult,
    ExecutionState, KillOutcome, DEFAULT_SHUTDOWN_TIMEOUT_MS,
};
use async_trait::async_trait;
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;
use tokio::sync::Mutex;

use super::resources::ExecutionResourceGuard;
use super::vm_process::{locate_microvm_process, LocatedProcess};
use super::{LocalExecutionBackend, LocalExecutionHandle, LocalExecutionObservation};
use crate::{
    BoxRecord, ManagedExecutionMetadata, ManagedExecutionOperation, ManagedExecutionState,
    VmManager,
};

type SharedVm = Arc<Mutex<VmManager>>;

/// Runtime adapter that owns live [`VmManager`] handles and reconstructs them
/// from durable runtime evidence after a control-plane restart.
#[derive(Clone)]
pub struct VmLocalExecutionBackend {
    home_dir: PathBuf,
    managers: Arc<DashMap<String, SharedVm>>,
    pull_progress_fn: Option<crate::PullProgressFn>,
}

impl VmLocalExecutionBackend {
    pub fn new(home_dir: impl Into<PathBuf>) -> Self {
        Self {
            home_dir: home_dir.into(),
            managers: Arc::new(DashMap::new()),
            pull_progress_fn: None,
        }
    }

    pub fn with_pull_progress_fn(mut self, pull_progress_fn: crate::PullProgressFn) -> Self {
        self.pull_progress_fn = Some(pull_progress_fn);
        self
    }

    pub fn home_dir(&self) -> &Path {
        &self.home_dir
    }

    fn metadata<'a>(
        &self,
        record: &'a BoxRecord,
    ) -> ExecutionManagerResult<&'a ManagedExecutionMetadata> {
        uuid::Uuid::parse_str(&record.id).map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "managed execution has an invalid internal ID {}: {error}",
                record.id
            ))
        })?;
        let expected_box_dir = self.home_dir.join("boxes").join(&record.id);
        if record.box_dir != expected_box_dir {
            return Err(ExecutionManagerError::Internal(format!(
                "managed execution {} has an unexpected host directory {}",
                record.id,
                record.box_dir.display()
            )));
        }
        let metadata = record.managed_execution.as_ref().ok_or_else(|| {
            ExecutionManagerError::Internal(format!(
                "execution {} lost managed lifecycle metadata",
                record.id
            ))
        })?;
        metadata
            .validate()
            .map_err(|error| ExecutionManagerError::Internal(error.to_string()))?;
        if record.isolation != metadata.request.config.isolation {
            return Err(ExecutionManagerError::Internal(format!(
                "managed execution {} has inconsistent isolation metadata",
                record.id
            )));
        }
        Ok(metadata)
    }

    fn new_manager(&self, record: &BoxRecord) -> ExecutionManagerResult<VmManager> {
        let metadata = self.metadata(record)?;
        let mut config = metadata.request.config.clone();
        if let Some(shm_size) = metadata.request.policy.shm_size {
            let has_shared_memory_mount = config
                .tmpfs
                .iter()
                .any(|entry| entry.split(':').next() == Some("/dev/shm"));
            if !has_shared_memory_mount {
                config.tmpfs.push(format!("/dev/shm:size={shm_size}"));
            }
        }
        let mut manager = VmManager::with_box_id(config, EventEmitter::new(256), record.id.clone());
        manager.home_dir = self.home_dir.clone();
        manager.set_healthcheck_disabled(metadata.request.policy.healthcheck_disabled);
        if let Some(pull_progress_fn) = self.pull_progress_fn.clone() {
            manager.set_pull_progress_fn(pull_progress_fn);
        }
        manager.anonymous_volumes = record.anonymous_volumes.clone();
        manager.set_log_config(record.log_config.clone());
        manager.resolved_execution_plan = Some(metadata.plan.clone());
        Ok(manager)
    }

    fn manager(&self, execution_id: &str) -> Option<SharedVm> {
        self.managers
            .get(execution_id)
            .map(|entry| Arc::clone(entry.value()))
    }

    fn remove_manager(&self, execution_id: &str, expected: &SharedVm) {
        if let Entry::Occupied(entry) = self.managers.entry(execution_id.to_string()) {
            if Arc::ptr_eq(entry.get(), expected) {
                entry.remove();
            }
        }
    }

    async fn handle_from_manager(
        &self,
        record: &BoxRecord,
        manager: &VmManager,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        let execution_id = execution_id(record)?;
        let pid = manager.pid().await.ok_or_else(|| {
            ExecutionManagerError::Internal(format!(
                "runtime returned no host PID for {execution_id}"
            ))
        })?;
        let pid_start_time = crate::process::pid_start_time(pid);
        #[cfg(target_os = "linux")]
        if pid_start_time.is_none() {
            return Err(ExecutionManagerError::NotFound(execution_id));
        }
        if !crate::process::is_process_alive_with_identity(pid, pid_start_time) {
            return Err(ExecutionManagerError::NotFound(execution_id));
        }
        let exec_socket_path = manager
            .exec_socket_path()
            .map(Path::to_path_buf)
            .ok_or_else(|| {
                ExecutionManagerError::Internal(format!(
                    "runtime returned no exec socket for {}",
                    record.id
                ))
            })?;
        let anonymous_volumes = if manager.anonymous_volumes().is_empty() {
            self.anonymous_volumes_for_record(record).await
        } else {
            manager.anonymous_volumes().to_vec()
        };
        Ok(LocalExecutionHandle {
            started_at: record.started_at.unwrap_or_else(chrono::Utc::now),
            pid: Some(pid),
            pid_start_time,
            exec_socket_path,
            console_log: record.box_dir.join("logs/console.log"),
            anonymous_volumes,
        })
    }

    async fn inspect_registered(
        &self,
        record: &BoxRecord,
        shared: SharedVm,
    ) -> ExecutionManagerResult<LocalExecutionObservation> {
        let mut manager = shared.lock().await;
        let exit_code = manager
            .try_wait_exit()
            .await
            .map_err(|error| runtime_error("inspect", record, error))?;
        let mut state = manager.state().await;
        let terminal = exit_code.is_some() || state == crate::BoxState::Stopped;
        if terminal {
            let cleanup = manager.destroy().await;
            let exit_code = manager.exit_code().or(exit_code);
            drop(manager);
            self.remove_manager(&record.id, &shared);
            cleanup.map_err(|error| runtime_error("clean up", record, error))?;
            return Ok(LocalExecutionObservation {
                state: ExecutionState::Stopped,
                handle: None,
                exit_code,
            });
        }

        if state == crate::BoxState::Created {
            if manager.has_exited().await {
                let cleanup = manager.destroy().await;
                let exit_code = manager.exit_code();
                drop(manager);
                self.remove_manager(&record.id, &shared);
                cleanup.map_err(|error| runtime_error("clean up", record, error))?;
                return Ok(LocalExecutionObservation {
                    state: ExecutionState::Stopped,
                    handle: None,
                    exit_code,
                });
            }
            if !self.promote_if_ready(record, &mut manager).await {
                return Ok(LocalExecutionObservation {
                    state: ExecutionState::Creating,
                    handle: None,
                    exit_code: None,
                });
            }
            state = manager.state().await;
        }

        if !manager
            .health_check()
            .await
            .map_err(|error| runtime_error("inspect", record, error))?
        {
            let cleanup = manager.destroy().await;
            let exit_code = manager.exit_code();
            drop(manager);
            self.remove_manager(&record.id, &shared);
            cleanup.map_err(|error| runtime_error("clean up", record, error))?;
            return Ok(LocalExecutionObservation {
                state: ExecutionState::Stopped,
                handle: None,
                exit_code,
            });
        }

        if state != crate::BoxState::Ready
            && state != crate::BoxState::Busy
            && state != crate::BoxState::Compacting
        {
            return Err(ExecutionManagerError::Internal(format!(
                "runtime manager for {} is in unexpected state {state:?}",
                record.id
            )));
        }
        if matches!(
            managed_state(record)?,
            ManagedExecutionState::Starting | ManagedExecutionState::RestartStarting
        ) && !exec_endpoint_ready(manager.exec_socket_path()).await
        {
            return Ok(LocalExecutionObservation {
                state: ExecutionState::Creating,
                handle: None,
                exit_code: None,
            });
        }
        let visible_state = visible_active_state(record)?;
        let handle = self.handle_from_manager(record, &manager).await?;
        Ok(LocalExecutionObservation {
            state: visible_state,
            handle: Some(handle),
            exit_code: None,
        })
    }

    async fn promote_if_ready(&self, record: &BoxRecord, manager: &mut VmManager) -> bool {
        let socket_dir = crate::vm::runtime_socket_dir(&self.home_dir, &record.id);
        let exec_socket = socket_dir.join("exec.sock");
        if !exec_endpoint_ready(Some(&exec_socket)).await {
            return false;
        }
        manager.exec_socket_path = Some(exec_socket);
        manager.pty_socket_path = Some(socket_dir.join("pty.sock"));
        manager.port_forward_socket_path = Some(socket_dir.join("portfwd.sock"));
        *manager.state.write().await = crate::BoxState::Ready;
        true
    }

    async fn recover_microvm(&self, record: &BoxRecord) -> ExecutionManagerResult<SharedVm> {
        self.metadata(record)?;
        let execution_id = execution_id(record)?;
        let execution_id_label = record.id.clone();
        let recorded = record.pid.map(|pid| (pid, record.pid_start_time));
        let located = tokio::task::spawn_blocking(move || {
            locate_microvm_process(&execution_id_label, recorded)
        })
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "MicroVM process discovery task failed for {}: {error}",
                record.id
            ))
        })?
        .map_err(ExecutionManagerError::Internal)?
        .ok_or(ExecutionManagerError::NotFound(execution_id))?;
        self.attach_microvm(record, located).await
    }

    async fn attach_microvm(
        &self,
        record: &BoxRecord,
        located: LocatedProcess,
    ) -> ExecutionManagerResult<SharedVm> {
        let mut manager = self.new_manager(record)?;
        let socket_dir = crate::vm::runtime_socket_dir(&self.home_dir, &record.id);
        manager
            .attach_running_process(
                located.pid,
                socket_dir.join("exec.sock"),
                Some(socket_dir.join("pty.sock")),
            )
            .await
            .map_err(|error| runtime_error("recover", record, error))?;
        if located.start_time.is_some()
            && crate::process::pid_start_time(located.pid) != located.start_time
        {
            return Err(ExecutionManagerError::NotFound(execution_id(record)?));
        }
        let recovered = Arc::new(Mutex::new(manager));
        match self.managers.entry(record.id.clone()) {
            Entry::Occupied(entry) => Ok(Arc::clone(entry.get())),
            Entry::Vacant(entry) => {
                entry.insert(Arc::clone(&recovered));
                Ok(recovered)
            }
        }
    }

    async fn require_microvm(&self, record: &BoxRecord) -> ExecutionManagerResult<SharedVm> {
        match self.manager(&record.id) {
            Some(manager) => Ok(manager),
            None => self.recover_microvm(record).await,
        }
    }

    async fn destroy_registered(
        &self,
        record: &BoxRecord,
        shared: SharedVm,
        remove_anonymous_volumes: bool,
        timeout_secs: Option<u64>,
    ) -> ExecutionManagerResult<KillOutcome> {
        let mut manager = shared.lock().await;
        let mut anonymous_volumes = if manager.anonymous_volumes().is_empty() {
            record.anonymous_volumes.clone()
        } else {
            manager.anonymous_volumes().to_vec()
        };
        let result = match graceful_stop_options(record, timeout_secs)? {
            Some((signal, timeout_ms)) => manager.destroy_with_options(signal, timeout_ms).await,
            None => manager.destroy().await,
        };
        drop(manager);
        self.remove_manager(&record.id, &shared);
        result.map_err(|error| runtime_error("kill", record, error))?;
        if remove_anonymous_volumes {
            if anonymous_volumes.is_empty() {
                anonymous_volumes = self.anonymous_volumes_for_record(record).await;
            }
            self.cleanup_anonymous_volumes(anonymous_volumes).await;
        }
        Ok(KillOutcome::Killed)
    }

    async fn anonymous_volumes_for_record(&self, record: &BoxRecord) -> Vec<String> {
        if !record.anonymous_volumes.is_empty() {
            return record.anonymous_volumes.clone();
        }
        let home_dir = self.home_dir.clone();
        let execution_id = record.id.clone();
        let short_id = record.id.chars().take(8).collect::<String>();
        let result = tokio::task::spawn_blocking(move || -> a3s_box_core::Result<Vec<String>> {
            let store =
                crate::VolumeStore::new(home_dir.join("volumes.json"), home_dir.join("volumes"));
            let prefix = format!("anon_{short_id}_");
            let mut names = store
                .load()?
                .into_values()
                .filter(|volume| {
                    volume
                        .labels
                        .get("anonymous")
                        .is_some_and(|value| value == "true")
                        && (volume.in_use_by.iter().any(|id| id == &execution_id)
                            || volume.name.starts_with(&prefix))
                })
                .map(|volume| volume.name)
                .collect::<Vec<_>>();
            names.sort();
            Ok(names)
        })
        .await;
        match result {
            Ok(Ok(names)) => names,
            Ok(Err(error)) => {
                tracing::warn!(
                    execution_id = %record.id,
                    %error,
                    "Failed to load anonymous volumes during managed cleanup"
                );
                Vec::new()
            }
            Err(error) => {
                tracing::warn!(
                    execution_id = %record.id,
                    %error,
                    "Anonymous volume recovery task failed"
                );
                Vec::new()
            }
        }
    }

    async fn cleanup_anonymous_volumes(&self, names: Vec<String>) {
        if names.is_empty() {
            return;
        }
        let home_dir = self.home_dir.clone();
        let task = tokio::task::spawn_blocking(move || {
            let store = crate::VolumeStore::new(
                home_dir.join("volumes.json"),
                home_dir.join("volumes"),
            );
            for name in names {
                if let Err(error) = store.remove(&name, true) {
                    tracing::warn!(volume = %name, %error, "Failed to remove managed anonymous volume");
                }
            }
        })
        .await;
        if let Err(error) = task {
            tracing::warn!(%error, "Anonymous volume cleanup task failed");
        }
    }
}

#[async_trait]
impl LocalExecutionBackend for VmLocalExecutionBackend {
    async fn start(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        super::record::validate_record_health(record)?;
        self.metadata(record)?;
        let box_dir = record.box_dir.clone();
        let execution_id = record.id.clone();
        tokio::task::spawn_blocking(move || {
            crate::rootfs::stage_box_terminal_rootfs_metadata(&box_dir)
        })
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "rootfs metadata staging task failed for {execution_id}: {error}"
            ))
        })?
        .map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "failed to stage rootfs metadata for {execution_id}: {error}"
            ))
        })?;
        let manager = Arc::new(Mutex::new(self.new_manager(record)?));
        match self.managers.entry(record.id.clone()) {
            Entry::Occupied(_) => {
                return Err(ExecutionManagerError::Unavailable(format!(
                    "execution {} already has an in-process runtime owner",
                    record.id
                )))
            }
            Entry::Vacant(entry) => {
                entry.insert(Arc::clone(&manager));
            }
        }

        let mut guard = manager.lock().await;
        let resource_home = self.home_dir.clone();
        let resource_record = record.clone();
        let resources = match tokio::task::spawn_blocking(move || {
            ExecutionResourceGuard::prepare(&resource_home, &resource_record)
        })
        .await
        {
            Ok(Ok(resources)) => resources,
            Ok(Err(error)) => {
                drop(guard);
                self.remove_manager(&record.id, &manager);
                return Err(error);
            }
            Err(error) => {
                drop(guard);
                self.remove_manager(&record.id, &manager);
                return Err(ExecutionManagerError::Internal(format!(
                    "managed resource preparation task failed for {}: {error}",
                    record.id
                )));
            }
        };
        if let Err(error) = guard.boot().await {
            drop(guard);
            self.remove_manager(&record.id, &manager);
            let rollback = tokio::task::spawn_blocking(move || resources.rollback()).await;
            if let Err(rollback_error) = rollback {
                tracing::warn!(
                    execution_id = %record.id,
                    %rollback_error,
                    "Managed resource rollback task failed"
                );
            }
            return Err(runtime_error("start", record, error));
        }
        resources.disarm();
        self.handle_from_manager(record, &guard).await
    }

    async fn inspect(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionObservation> {
        let metadata = self.metadata(record)?;
        if metadata.plan.backend == ExecutionBackend::Crun {
            return self.inspect_sandbox(record).await;
        }
        if let Some(manager) = self.manager(&record.id) {
            return self.inspect_registered(record, manager).await;
        }
        let manager = self.recover_microvm(record).await?;
        self.inspect_registered(record, manager).await
    }

    async fn pause(
        &self,
        record: &BoxRecord,
        keep_memory: bool,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        let metadata = self.metadata(record)?;
        if metadata.plan.backend == ExecutionBackend::Crun {
            if !keep_memory {
                return Err(unsupported(
                    record,
                    "pause without memory retention",
                    "the Sandbox backend",
                ));
            }
            return self.pause_sandbox(record).await;
        }
        if !keep_memory {
            return Err(unsupported(
                record,
                "pause without memory retention",
                "the local MicroVM backend",
            ));
        }
        let shared = self.require_microvm(record).await?;
        let manager = shared.lock().await;
        require_recorded_pid(record, &manager).await?;
        manager
            .pause()
            .await
            .map_err(|error| runtime_error("pause", record, error))?;
        self.handle_from_manager(record, &manager).await
    }

    async fn resume(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        let metadata = self.metadata(record)?;
        if metadata.plan.backend == ExecutionBackend::Crun {
            return self.resume_sandbox(record).await;
        }
        let shared = self.require_microvm(record).await?;
        let manager = shared.lock().await;
        require_recorded_pid(record, &manager).await?;
        manager
            .resume()
            .await
            .map_err(|error| runtime_error("resume", record, error))?;
        self.handle_from_manager(record, &manager).await
    }

    async fn kill(&self, record: &BoxRecord) -> ExecutionManagerResult<KillOutcome> {
        let metadata = self.metadata(record)?;
        let remove_anonymous_volumes = record.auto_remove;
        let timeout_secs = record.stop_timeout;
        if let Some(manager) = self.manager(&record.id) {
            return self
                .destroy_registered(record, manager, remove_anonymous_volumes, timeout_secs)
                .await;
        }
        match metadata.plan.backend {
            ExecutionBackend::Crun => {
                self.destroy_detached_sandbox(record, remove_anonymous_volumes, timeout_secs)
                    .await
            }
            ExecutionBackend::Krun => {
                let manager = self.recover_microvm(record).await?;
                self.destroy_registered(record, manager, remove_anonymous_volumes, timeout_secs)
                    .await
            }
        }
    }

    async fn stop_for_restart(
        &self,
        record: &BoxRecord,
        timeout_secs: Option<u64>,
    ) -> ExecutionManagerResult<KillOutcome> {
        let metadata = self.metadata(record)?;
        let timeout_secs = timeout_secs.or(record.stop_timeout);
        if let Some(manager) = self.manager(&record.id) {
            return self
                .destroy_registered(record, manager, false, timeout_secs)
                .await;
        }
        match metadata.plan.backend {
            ExecutionBackend::Crun => {
                self.destroy_detached_sandbox(record, false, timeout_secs)
                    .await
            }
            ExecutionBackend::Krun => {
                let manager = self.recover_microvm(record).await?;
                self.destroy_registered(record, manager, false, timeout_secs)
                    .await
            }
        }
    }
}

fn graceful_stop_options(
    record: &BoxRecord,
    timeout_secs: Option<u64>,
) -> ExecutionManagerResult<Option<(i32, u64)>> {
    if timeout_secs.is_none() && record.stop_signal.is_none() {
        return Ok(None);
    }
    let timeout_ms = timeout_secs
        .unwrap_or(DEFAULT_SHUTDOWN_TIMEOUT_MS / 1_000)
        .checked_mul(1_000)
        .ok_or_else(|| {
            ExecutionManagerError::InvalidRequest(format!(
                "stop timeout is too large for execution {}",
                record.id
            ))
        })?;
    let signal = record
        .stop_signal
        .as_deref()
        .map(a3s_box_core::vmm::parse_signal_name)
        .unwrap_or(libc::SIGTERM);
    Ok(Some((signal, timeout_ms)))
}

async fn require_recorded_pid(
    record: &BoxRecord,
    manager: &VmManager,
) -> ExecutionManagerResult<()> {
    let execution_id = execution_id(record)?;
    let pid = manager
        .pid()
        .await
        .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
    if record.pid != Some(pid)
        || !crate::process::is_process_alive_with_identity(pid, record.pid_start_time)
    {
        return Err(ExecutionManagerError::NotFound(execution_id));
    }
    Ok(())
}

fn visible_active_state(record: &BoxRecord) -> ExecutionManagerResult<ExecutionState> {
    match managed_state(record)? {
        ManagedExecutionState::Paused | ManagedExecutionState::Resuming => {
            Ok(ExecutionState::Paused)
        }
        ManagedExecutionState::Starting
        | ManagedExecutionState::RestartStarting
        | ManagedExecutionState::Running
        | ManagedExecutionState::Pausing
        | ManagedExecutionState::Killing => Ok(ExecutionState::Running),
        ManagedExecutionState::Snapshotting => match record
            .managed_execution
            .as_ref()
            .and_then(|metadata| metadata.pending_operation.as_ref())
        {
            Some(ManagedExecutionOperation::Snapshot {
                source_state: ManagedExecutionState::Running,
                ..
            }) => Ok(ExecutionState::Running),
            Some(ManagedExecutionOperation::Snapshot {
                source_state: ManagedExecutionState::Paused,
                ..
            }) => Ok(ExecutionState::Paused),
            _ => Err(ExecutionManagerError::Internal(format!(
                "execution {} has invalid snapshot metadata",
                record.id
            ))),
        },
        ManagedExecutionState::RestartStopping => match record
            .managed_execution
            .as_ref()
            .and_then(|metadata| metadata.pending_operation.as_ref())
        {
            Some(ManagedExecutionOperation::Restart {
                source_state: ManagedExecutionState::Paused,
                ..
            }) => Ok(ExecutionState::Paused),
            Some(ManagedExecutionOperation::Restart {
                source_state: ManagedExecutionState::Running,
                ..
            }) => Ok(ExecutionState::Running),
            _ => Err(ExecutionManagerError::Internal(format!(
                "execution {} has invalid restart teardown metadata",
                record.id
            ))),
        },
        state => Err(ExecutionManagerError::Internal(format!(
            "execution {} has no active runtime in managed state {state}",
            record.id
        ))),
    }
}

fn managed_state(record: &BoxRecord) -> ExecutionManagerResult<ManagedExecutionState> {
    record
        .managed_state()
        .map_err(|error| ExecutionManagerError::Internal(error.to_string()))?
        .ok_or_else(|| {
            ExecutionManagerError::Internal(format!("execution {} is not managed", record.id))
        })
}

fn execution_id(record: &BoxRecord) -> ExecutionManagerResult<ExecutionId> {
    ExecutionId::new(record.id.clone())
        .map_err(|error| ExecutionManagerError::Internal(error.to_string()))
}

fn runtime_error(
    action: &str,
    record: &BoxRecord,
    error: impl std::fmt::Display,
) -> ExecutionManagerError {
    ExecutionManagerError::Internal(format!(
        "failed to {action} execution {}: {error}",
        record.id
    ))
}

fn unsupported(record: &BoxRecord, operation: &str, backend: &str) -> ExecutionManagerError {
    match execution_id(record) {
        Ok(execution_id) => ExecutionManagerError::Conflict {
            execution_id,
            message: format!("{operation} is not supported by {backend}"),
        },
        Err(error) => error,
    }
}

#[cfg(unix)]
async fn exec_endpoint_ready(path: Option<&Path>) -> bool {
    let Some(path) = path else {
        return false;
    };
    let attempt = async {
        let client = crate::ExecClient::connect(path).await.ok()?;
        client.heartbeat().await.ok().filter(|ready| *ready)
    };
    tokio::time::timeout(Duration::from_millis(500), attempt)
        .await
        .ok()
        .flatten()
        .is_some()
}

#[cfg(not(unix))]
async fn exec_endpoint_ready(path: Option<&Path>) -> bool {
    path.is_some()
}

#[cfg(test)]
#[path = "vm_backend_tests.rs"]
mod tests;
