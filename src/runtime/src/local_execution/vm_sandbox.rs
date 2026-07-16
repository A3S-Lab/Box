//! Durable `crun` recovery for the production local execution backend.

use super::*;

impl VmLocalExecutionBackend {
    pub(super) async fn inspect_sandbox(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionObservation> {
        self.metadata(record)?;
        let home_dir = self.home_dir.clone();
        let box_dir = record.box_dir.clone();
        let box_id = record.id.clone();
        let execution_id = execution_id(record)?;
        let state = tokio::task::spawn_blocking(move || {
            inspect_recorded_sandbox(&home_dir, &box_dir, &box_id)
        })
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "Sandbox inspection task failed for {}: {error}",
                record.id
            ))
        })?
        .map_err(|error| runtime_error("inspect", record, error))?
        .ok_or(ExecutionManagerError::NotFound(execution_id))?;

        match state.status.as_str() {
            "created" | "running" => {
                if state.pid == 0 {
                    return Err(ExecutionManagerError::Internal(format!(
                        "Sandbox runtime returned PID zero for {}",
                        record.id
                    )));
                }
                let manager = self.attach_sandbox(record, state).await?;
                self.inspect_registered(record, manager).await
            }
            "paused" => {
                if state.pid == 0 {
                    return Err(ExecutionManagerError::Internal(format!(
                        "Sandbox runtime returned PID zero for {}",
                        record.id
                    )));
                }
                let manager = self.attach_sandbox(record, state).await?;
                let manager = manager.lock().await;
                let handle = self.handle_from_manager(record, &manager).await?;
                Ok(LocalExecutionObservation {
                    state: ExecutionState::Paused,
                    handle: Some(handle),
                    exit_code: None,
                })
            }
            "stopped" => {
                self.cleanup_detached_sandbox(record).await?;
                Ok(LocalExecutionObservation {
                    state: ExecutionState::Stopped,
                    handle: None,
                    exit_code: None,
                })
            }
            status => Err(ExecutionManagerError::Internal(format!(
                "Sandbox runtime returned unknown state {status} for {}",
                record.id
            ))),
        }
    }

    #[cfg(target_os = "linux")]
    pub(super) async fn pause_sandbox(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        self.transition_sandbox(record, true).await
    }

    #[cfg(target_os = "linux")]
    pub(super) async fn resume_sandbox(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        self.transition_sandbox(record, false).await
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) async fn pause_sandbox(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        Err(unsupported(record, "pause", "the Sandbox backend on this host"))
    }

    #[cfg(not(target_os = "linux"))]
    pub(super) async fn resume_sandbox(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        Err(unsupported(record, "resume", "the Sandbox backend on this host"))
    }

    #[cfg(target_os = "linux")]
    async fn transition_sandbox(
        &self,
        record: &BoxRecord,
        pause: bool,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        self.metadata(record)?;
        let home_dir = self.home_dir.clone();
        let box_dir = record.box_dir.clone();
        let box_id = record.id.clone();
        let operation = if pause { "pause" } else { "resume" };
        let inspection = tokio::task::spawn_blocking(move || {
            let inspection = inspect_recorded_sandbox(&home_dir, &box_dir, &box_id)?
                .ok_or_else(|| {
                    a3s_box_core::BoxError::StateError(format!(
                        "Sandbox runtime record is missing for {box_id}"
                    ))
                })?;
            if pause {
                crate::sandbox::handler::CrunHandler::pause_at(
                    &inspection.runtime.runtime_path,
                    &inspection.runtime.runtime_root,
                    &box_id,
                )?;
            } else {
                crate::sandbox::handler::CrunHandler::resume_at(
                    &inspection.runtime.runtime_path,
                    &inspection.runtime.runtime_root,
                    &box_id,
                )?;
            }
            inspect_recorded_sandbox(&home_dir, &box_dir, &box_id)?.ok_or_else(|| {
                a3s_box_core::BoxError::StateError(format!(
                    "Sandbox runtime record disappeared after {operation} for {box_id}"
                ))
            })
        })
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "Sandbox {operation} task failed for {}: {error}",
                record.id
            ))
        })?
        .map_err(|error| runtime_error(operation, record, error))?;

        let expected = if pause { "paused" } else { "running" };
        if inspection.status != expected {
            return Err(ExecutionManagerError::Internal(format!(
                "Sandbox runtime returned state {} after {operation} for {}",
                inspection.status, record.id
            )));
        }
        let manager = self.attach_sandbox(record, inspection).await?;
        let manager = manager.lock().await;
        self.handle_from_manager(record, &manager).await
    }

    #[cfg(target_os = "linux")]
    async fn attach_sandbox(
        &self,
        record: &BoxRecord,
        inspection: SandboxInspection,
    ) -> ExecutionManagerResult<SharedVm> {
        let mut manager = self.new_manager(record)?;
        let socket_dir = crate::vm::runtime_socket_dir(&self.home_dir, &record.id);
        manager.exec_socket_path = Some(socket_dir.join("exec.sock"));
        manager.pty_socket_path = Some(socket_dir.join("pty.sock"));
        manager.port_forward_socket_path = Some(socket_dir.join("portfwd.sock"));
        *manager.handler.write().await = Some(Box::new(
            crate::sandbox::handler::CrunHandler::from_recorded_runtime(
                crate::sandbox::handler::CrunHandlerSpec::new(
                    inspection.runtime.runtime_path,
                    inspection.runtime.runtime_root,
                    record.id.clone(),
                    inspection.pid,
                    inspection.runtime.bundle_dir,
                    record.box_dir.join("sandbox/runtime.json"),
                ),
                inspection.runtime.log_worker_pid,
                inspection.runtime.log_worker_pid_start_time,
            ),
        ));
        if !matches!(
            managed_state(record)?,
            ManagedExecutionState::Starting | ManagedExecutionState::RestartStarting
        ) {
            *manager.state.write().await = crate::BoxState::Ready;
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

    #[cfg(not(target_os = "linux"))]
    async fn attach_sandbox(
        &self,
        record: &BoxRecord,
        _inspection: SandboxInspection,
    ) -> ExecutionManagerResult<SharedVm> {
        Err(unsupported(
            record,
            "recovery",
            "the Sandbox backend on this host",
        ))
    }

    pub(super) async fn destroy_detached_sandbox(
        &self,
        record: &BoxRecord,
        remove_anonymous_volumes: bool,
        timeout_secs: Option<u64>,
    ) -> ExecutionManagerResult<KillOutcome> {
        self.inspect_sandbox(record).await?;
        if let Some(manager) = self.manager(&record.id) {
            return self
                .destroy_registered(record, manager, remove_anonymous_volumes, timeout_secs)
                .await;
        }
        if remove_anonymous_volumes {
            let anonymous_volumes = self.anonymous_volumes_for_record(record).await;
            self.cleanup_anonymous_volumes(anonymous_volumes).await;
        }
        Ok(KillOutcome::Killed)
    }

    async fn cleanup_detached_sandbox(&self, record: &BoxRecord) -> ExecutionManagerResult<()> {
        let home_dir = self.home_dir.clone();
        let box_dir = record.box_dir.clone();
        let box_id = record.id.clone();
        tokio::task::spawn_blocking(move || {
            crate::vm::reap::cleanup_recorded_sandbox_runtime_in(&home_dir, &box_dir, &box_id)
        })
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "Sandbox cleanup task failed for {}: {error}",
                record.id
            ))
        })?
        .map_err(|error| runtime_error("kill", record, error))?;

        let mut manager = self.new_manager(record)?;
        manager
            .destroy()
            .await
            .map_err(|error| runtime_error("clean up", record, error))?;
        Ok(())
    }
}

#[cfg(target_os = "linux")]
struct SandboxInspection {
    status: String,
    pid: u32,
    runtime: crate::vm::reap::RecordedSandboxRuntime,
}

#[cfg(target_os = "linux")]
fn inspect_recorded_sandbox(
    home_dir: &Path,
    box_dir: &Path,
    box_id: &str,
) -> a3s_box_core::Result<Option<SandboxInspection>> {
    let Some(runtime) = crate::vm::reap::load_recorded_sandbox_runtime(home_dir, box_dir, box_id)?
    else {
        return Ok(None);
    };
    let state = crate::sandbox::handler::CrunHandler::query_state_at(
        &runtime.runtime_path,
        &runtime.runtime_root,
        box_id,
    )?;
    let (status, pid) = match state {
        Some(state) => (state.status, state.pid),
        None => ("stopped".to_string(), 0),
    };
    if matches!(status.as_str(), "created" | "running" | "paused")
        && pid != runtime.init_pid
    {
        return Err(a3s_box_core::BoxError::StateError(format!(
            "Sandbox runtime PID disagrees with its durable record for {box_id}"
        )));
    }
    Ok(Some(SandboxInspection {
        status,
        pid,
        runtime,
    }))
}

#[cfg(not(target_os = "linux"))]
struct SandboxInspection {
    status: String,
    pid: u32,
}

#[cfg(not(target_os = "linux"))]
fn inspect_recorded_sandbox(
    _home_dir: &Path,
    _box_dir: &Path,
    _box_id: &str,
) -> a3s_box_core::Result<Option<SandboxInspection>> {
    Err(a3s_box_core::BoxError::StateError(
        "Sandbox execution requires Linux".to_string(),
    ))
}
