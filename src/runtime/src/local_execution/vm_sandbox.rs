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
                inspection.runtime.runtime_path,
                inspection.runtime.runtime_root,
                record.id.clone(),
                inspection.pid,
                inspection.runtime.log_worker_pid,
                inspection.runtime.log_worker_pid_start_time,
                inspection.runtime.bundle_dir,
                record.box_dir.join("sandbox/runtime.json"),
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
    if matches!(status.as_str(), "created" | "running") && pid != runtime.init_pid {
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
