//! Durable implementation of the backend-neutral local execution lifecycle.

mod api;
mod backend;
mod create;
mod logs;
mod operations;
mod port;
mod record;
mod recovery;
mod remove;
mod resources;
mod restart;
#[cfg(unix)]
mod session;
mod snapshot;
mod store;
mod support;
#[cfg(feature = "vm")]
mod vm_backend;
#[cfg(feature = "vm")]
mod vm_process;

use std::path::PathBuf;
use std::sync::Arc;

use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionManagerError, ExecutionManagerResult,
};

pub use backend::{LocalExecutionBackend, LocalExecutionHandle, LocalExecutionObservation};
use record::{build_managed_record, status_from_record};
use store::RuntimeUpdate;
#[cfg(feature = "vm")]
pub use vm_backend::VmLocalExecutionBackend;

use crate::{BoxRecord, ManagedExecutionOperation, ManagedExecutionState, ManagedExecutionStore};

/// Local lifecycle facade shared by service, CLI, and SDK adapters.
#[derive(Clone)]
pub struct LocalExecutionManager {
    store: ManagedExecutionStore,
    home_dir: PathBuf,
    backend: Arc<dyn LocalExecutionBackend>,
}

impl LocalExecutionManager {
    pub fn new(
        state_path: impl Into<PathBuf>,
        home_dir: impl Into<PathBuf>,
        backend: Arc<dyn LocalExecutionBackend>,
    ) -> Self {
        Self {
            store: ManagedExecutionStore::new(state_path),
            home_dir: home_dir.into(),
            backend,
        }
    }

    pub fn state_path(&self) -> &std::path::Path {
        self.store.path()
    }

    pub(super) async fn require_running_record(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<BoxRecord> {
        let record = self
            .get(execution_id)
            .await?
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        support::require_generation(&record, execution_id, generation)?;
        if support::managed_state(&record)? != ManagedExecutionState::Running {
            return Err(ExecutionManagerError::Conflict {
                execution_id: execution_id.clone(),
                message: "execution is not running".to_string(),
            });
        }
        if record.exec_socket_path.as_os_str().is_empty() {
            return Err(ExecutionManagerError::Internal(format!(
                "execution {execution_id} has no exec endpoint"
            )));
        }
        #[cfg(target_os = "linux")]
        {
            let pid = record
                .pid
                .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
            if !crate::process::is_process_alive_with_identity(pid, record.pid_start_time) {
                return Err(ExecutionManagerError::NotFound(execution_id.clone()));
            }
        }
        Ok(record)
    }

    #[cfg(feature = "vm")]
    pub fn with_vm_backend(state_path: impl Into<PathBuf>, home_dir: impl Into<PathBuf>) -> Self {
        let home_dir = home_dir.into();
        Self::new(
            state_path,
            home_dir.clone(),
            Arc::new(VmLocalExecutionBackend::new(home_dir)),
        )
    }
}

#[cfg(test)]
mod tests;
