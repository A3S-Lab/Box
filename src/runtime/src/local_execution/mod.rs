//! Durable implementation of the backend-neutral local execution lifecycle.

mod api;
mod backend;
mod create;
#[cfg(test)]
mod inspection_cancellation_tests;
mod lifecycle_lock;
pub use lifecycle_lock::{
    acquire_blocking as acquire_execution_lifecycle_lock, ExecutionLifecycleLock,
};
mod logs;
mod oci_backend;
#[cfg(feature = "vm")]
mod oci_migration;
#[cfg(all(feature = "vm", target_os = "linux"))]
mod oci_owner;
#[cfg(feature = "vm")]
mod oci_production;
mod oci_session;
mod operations;
mod port;
mod record;
mod recovery;
mod remove;
mod resources;
mod restart;
mod router;
#[cfg(test)]
mod router_tests;
#[cfg(unix)]
mod session;
mod session_support;
#[cfg(not(unix))]
mod session_unsupported;
mod snapshot;
mod store;
mod support;
#[cfg(all(feature = "vm", target_os = "linux"))]
mod transient_registry_auth;
#[cfg(feature = "vm")]
mod vm_backend;
#[cfg(feature = "vm")]
mod vm_process;

use std::path::PathBuf;
use std::sync::Arc;

use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionManagerError, ExecutionManagerResult,
};

pub use backend::{
    LocalExecutionBackend, LocalExecutionHandle, LocalExecutionObservation,
    LocalExecutionTermination,
};
pub use oci_backend::{
    oci_isolation_request, OciBundlePreparationContext, OciBundleProvider, OciLifecycleAdapter,
    OciLocalExecutionBackend, OciPreparedExecution, OciRuntimeBinding, OciRuntimeEndpoint,
    OciRuntimeLaunch, OCI_RUNTIME_BINDING_SCHEMA_VERSION,
};
#[cfg(feature = "vm")]
pub use oci_migration::NativeLinuxOciMigrationConfig;
#[cfg(feature = "vm")]
pub use oci_production::NativeLinuxOciBundleProvider;
use record::{build_managed_record, status_from_record};
pub use router::{LocalExecutionBackendRouter, OciMigrationPolicy};
use store::RuntimeUpdate;
#[cfg(all(feature = "vm", target_os = "linux"))]
pub(crate) use transient_registry_auth::{TransientRegistryAuthBroker, TransientRegistryAuthLease};
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
        if let Some(binding) = record
            .managed_execution
            .as_ref()
            .and_then(|metadata| metadata.oci_runtime.as_ref())
        {
            binding.validate_for(execution_id)?;
            return Ok(record);
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

    pub(super) async fn require_same_runtime(
        &self,
        bound: &BoxRecord,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<()> {
        let current = self
            .require_running_record(execution_id, generation)
            .await?;
        if current.pid != bound.pid
            || current.pid_start_time != bound.pid_start_time
            || current.exec_socket_path != bound.exec_socket_path
            || current
                .managed_execution
                .as_ref()
                .and_then(|metadata| metadata.oci_runtime.as_ref())
                != bound
                    .managed_execution
                    .as_ref()
                    .and_then(|metadata| metadata.oci_runtime.as_ref())
        {
            return Err(ExecutionManagerError::Conflict {
                execution_id: execution_id.clone(),
                message: "runtime generation changed while binding its execution session"
                    .to_string(),
            });
        }
        Ok(())
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

    /// Compose the retained Box backend with an explicitly supplied OCI SDK
    /// backend. The policy affects new reservations only; every selected route
    /// is persisted before preflight and remains authoritative after restart.
    #[cfg(feature = "vm")]
    pub fn with_oci_migration_backend(
        state_path: impl Into<PathBuf>,
        home_dir: impl Into<PathBuf>,
        oci_backend: Arc<dyn LocalExecutionBackend>,
        policy: OciMigrationPolicy,
    ) -> Self {
        Self::with_oci_migration_backend_and_pull_progress(
            state_path,
            home_dir,
            oci_backend,
            policy,
            None,
        )
    }

    #[cfg(feature = "vm")]
    fn with_oci_migration_backend_and_pull_progress(
        state_path: impl Into<PathBuf>,
        home_dir: impl Into<PathBuf>,
        oci_backend: Arc<dyn LocalExecutionBackend>,
        policy: OciMigrationPolicy,
        pull_progress_fn: Option<crate::PullProgressFn>,
    ) -> Self {
        let home_dir = home_dir.into();
        let mut legacy_backend = VmLocalExecutionBackend::new(home_dir.clone());
        if let Some(pull_progress_fn) = pull_progress_fn {
            legacy_backend = legacy_backend.with_pull_progress_fn(pull_progress_fn);
        }
        let legacy: Arc<dyn LocalExecutionBackend> = Arc::new(legacy_backend);
        let router = LocalExecutionBackendRouter::new(legacy, oci_backend, policy);
        Self::new(state_path, home_dir, Arc::new(router))
    }
}

#[cfg(test)]
mod tests;
