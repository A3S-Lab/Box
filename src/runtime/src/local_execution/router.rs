//! Durable dispatch between the retained Box backend and the A3S OCI SDK.

use std::sync::Arc;

use a3s_box_core::{
    pty::PtyRequest, ExecOutput, ExecRequest, ExecutionEventBatch, ExecutionEventsRequest,
    ExecutionIsolation, ExecutionManagerError, ExecutionManagerResult, ExecutionProcess,
    ExecutionProcessInventory, ExecutionResourceUpdate, ExecutionStats, FileRequest, FileResponse,
    FilesystemRequest, FilesystemResponse, KillOutcome, OperationId,
};
use async_trait::async_trait;

use super::{
    LocalExecutionBackend, LocalExecutionHandle, LocalExecutionObservation,
    LocalExecutionTermination,
};
use crate::{BoxRecord, ManagedRuntimeRoute};

/// Explicit cutover policy applied only while creating a new Box record.
///
/// Once selected, the exact route is persisted in the record and this policy
/// is no longer consulted for lifecycle, recovery, or cleanup operations.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum OciMigrationPolicy {
    /// Keep both isolation choices on their current Box-owned implementation.
    #[default]
    LegacyOnly,
    /// Route Sandbox to OCI while retaining the current MicroVM implementation.
    SandboxViaOci,
    /// Route both Sandbox and MicroVM through the public OCI SDK.
    AllViaOci,
}

impl OciMigrationPolicy {
    const fn route(self, isolation: ExecutionIsolation) -> ManagedRuntimeRoute {
        match (self, isolation) {
            (Self::LegacyOnly, _) | (Self::SandboxViaOci, ExecutionIsolation::Microvm) => {
                ManagedRuntimeRoute::BoxVm
            }
            (Self::SandboxViaOci, ExecutionIsolation::Sandbox) | (Self::AllViaOci, _) => {
                ManagedRuntimeRoute::OciSdk
            }
        }
    }
}

/// One fail-closed backend router that supports mixed legacy and OCI records.
///
/// The router never attempts the alternate backend after an error. A policy
/// selects only new records; explicit record metadata owns every later call.
#[derive(Clone)]
pub struct LocalExecutionBackendRouter {
    legacy: Arc<dyn LocalExecutionBackend>,
    oci: Arc<dyn LocalExecutionBackend>,
    policy: OciMigrationPolicy,
}

impl LocalExecutionBackendRouter {
    /// Compose the two implementations behind one immutable creation policy.
    pub fn new(
        legacy: Arc<dyn LocalExecutionBackend>,
        oci: Arc<dyn LocalExecutionBackend>,
        policy: OciMigrationPolicy,
    ) -> Self {
        Self {
            legacy,
            oci,
            policy,
        }
    }

    /// Policy used only for records created by this router.
    #[must_use]
    pub const fn policy(&self) -> OciMigrationPolicy {
        self.policy
    }

    fn backend_for_isolation(
        &self,
        isolation: ExecutionIsolation,
    ) -> &Arc<dyn LocalExecutionBackend> {
        match self.policy.route(isolation) {
            ManagedRuntimeRoute::BoxVm => &self.legacy,
            ManagedRuntimeRoute::OciSdk => &self.oci,
            ManagedRuntimeRoute::Unspecified => {
                unreachable!("creation policy always selects a route")
            }
        }
    }

    fn route_for_record(&self, record: &BoxRecord) -> ExecutionManagerResult<ManagedRuntimeRoute> {
        resolved_runtime_route(record)
    }

    fn backend_for_record(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<&Arc<dyn LocalExecutionBackend>> {
        match self.route_for_record(record)? {
            ManagedRuntimeRoute::BoxVm => Ok(&self.legacy),
            ManagedRuntimeRoute::OciSdk => Ok(&self.oci),
            ManagedRuntimeRoute::Unspecified => unreachable!("route inference is exhaustive"),
        }
    }
}

/// Resolve records written before the route field was introduced using the
/// same durable evidence for both router dispatch and concrete backend checks.
pub(super) fn resolved_runtime_route(
    record: &BoxRecord,
) -> ExecutionManagerResult<ManagedRuntimeRoute> {
    let metadata = record.managed_execution.as_ref().ok_or_else(|| {
        ExecutionManagerError::Internal(format!(
            "execution {} has no managed lifecycle metadata for backend routing",
            record.id
        ))
    })?;
    metadata
        .validate()
        .map_err(|error| ExecutionManagerError::Internal(error.to_string()))?;
    match metadata.runtime_route {
        ManagedRuntimeRoute::BoxVm => Ok(ManagedRuntimeRoute::BoxVm),
        ManagedRuntimeRoute::OciSdk => Ok(ManagedRuntimeRoute::OciSdk),
        ManagedRuntimeRoute::Unspecified if metadata.oci_runtime.is_some() => {
            Ok(ManagedRuntimeRoute::OciSdk)
        }
        // OCI handles deliberately store no Box-owned exec socket. This
        // preserves stopped pre-routing OCI records after their live binding
        // has been cleared during teardown.
        ManagedRuntimeRoute::Unspecified if record.exec_socket_path.as_os_str().is_empty() => {
            Ok(ManagedRuntimeRoute::OciSdk)
        }
        ManagedRuntimeRoute::Unspecified => Ok(ManagedRuntimeRoute::BoxVm),
    }
}

#[async_trait]
impl LocalExecutionBackend for LocalExecutionBackendRouter {
    async fn preflight_isolation(
        &self,
        isolation: ExecutionIsolation,
    ) -> ExecutionManagerResult<()> {
        self.backend_for_isolation(isolation)
            .preflight_isolation(isolation)
            .await
    }

    fn route_for_create(&self, record: &BoxRecord) -> ExecutionManagerResult<ManagedRuntimeRoute> {
        if record.managed_execution.is_none() {
            return Err(ExecutionManagerError::Internal(format!(
                "new execution {} has no managed lifecycle metadata for backend routing",
                record.id
            )));
        }
        Ok(self.policy.route(record.isolation))
    }

    async fn preflight(&self, record: &BoxRecord) -> ExecutionManagerResult<()> {
        self.backend_for_record(record)?.preflight(record).await
    }

    async fn start(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        self.backend_for_record(record)?.start(record).await
    }

    async fn inspect(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionObservation> {
        self.backend_for_record(record)?.inspect(record).await
    }

    async fn pause(
        &self,
        record: &BoxRecord,
        keep_memory: bool,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        self.backend_for_record(record)?
            .pause(record, keep_memory)
            .await
    }

    async fn resume(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        self.backend_for_record(record)?.resume(record).await
    }

    async fn preflight_resource_update(
        &self,
        record: &BoxRecord,
        update: &ExecutionResourceUpdate,
    ) -> ExecutionManagerResult<()> {
        self.backend_for_record(record)?
            .preflight_resource_update(record, update)
            .await
    }

    async fn update_resources(
        &self,
        record: &BoxRecord,
        operation_id: &OperationId,
        update: &ExecutionResourceUpdate,
    ) -> ExecutionManagerResult<()> {
        self.backend_for_record(record)?
            .update_resources(record, operation_id, update)
            .await
    }

    async fn list_processes(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<ExecutionProcessInventory> {
        self.backend_for_record(record)?
            .list_processes(record)
            .await
    }

    async fn stats(&self, record: &BoxRecord) -> ExecutionManagerResult<ExecutionStats> {
        self.backend_for_record(record)?.stats(record).await
    }

    async fn events(
        &self,
        record: &BoxRecord,
        request: ExecutionEventsRequest,
    ) -> ExecutionManagerResult<ExecutionEventBatch> {
        self.backend_for_record(record)?
            .events(record, request)
            .await
    }

    async fn execute(
        &self,
        record: &BoxRecord,
        request: ExecRequest,
    ) -> ExecutionManagerResult<ExecOutput> {
        self.backend_for_record(record)?
            .execute(record, request)
            .await
    }

    async fn start_process(
        &self,
        record: &BoxRecord,
        request: ExecRequest,
    ) -> ExecutionManagerResult<ExecutionProcess> {
        self.backend_for_record(record)?
            .start_process(record, request)
            .await
    }

    async fn start_pty(
        &self,
        record: &BoxRecord,
        request: PtyRequest,
    ) -> ExecutionManagerResult<ExecutionProcess> {
        self.backend_for_record(record)?
            .start_pty(record, request)
            .await
    }

    async fn transfer_file(
        &self,
        record: &BoxRecord,
        request: FileRequest,
    ) -> ExecutionManagerResult<FileResponse> {
        self.backend_for_record(record)?
            .transfer_file(record, request)
            .await
    }

    async fn filesystem(
        &self,
        record: &BoxRecord,
        request: FilesystemRequest,
    ) -> ExecutionManagerResult<FilesystemResponse> {
        self.backend_for_record(record)?
            .filesystem(record, request)
            .await
    }

    async fn prepare_quiescent_rootfs(&self, record: &BoxRecord) -> ExecutionManagerResult<()> {
        self.backend_for_record(record)?
            .prepare_quiescent_rootfs(record)
            .await
    }

    async fn cleanup_quiescent_rootfs(&self, record: &BoxRecord) -> ExecutionManagerResult<()> {
        self.backend_for_record(record)?
            .cleanup_quiescent_rootfs(record)
            .await
    }

    async fn stop_for_restart(
        &self,
        record: &BoxRecord,
        timeout_secs: Option<u64>,
    ) -> ExecutionManagerResult<KillOutcome> {
        self.backend_for_record(record)?
            .stop_for_restart(record, timeout_secs)
            .await
    }

    async fn kill(&self, record: &BoxRecord) -> ExecutionManagerResult<KillOutcome> {
        self.backend_for_record(record)?.kill(record).await
    }

    async fn kill_with_status(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionTermination> {
        self.backend_for_record(record)?
            .kill_with_status(record)
            .await
    }
}
