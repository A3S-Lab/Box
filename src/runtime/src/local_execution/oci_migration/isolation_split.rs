//! Dispatch one OCI router slot by durable isolation.
//!
//! Linux KVM qualification keeps production Sandbox on the Native Linux owner
//! and sends only MicroVM records to the DedicatedVm qualification provider.
//! Neither side is retried on the other after an error.

use std::sync::Arc;

use a3s_box_core::pty::PtyRequest;
use a3s_box_core::{
    ExecOutput, ExecRequest, ExecutionEventBatch, ExecutionEventsRequest, ExecutionIsolation,
    ExecutionManagerResult, ExecutionProcess, ExecutionProcessInventory, ExecutionResourceUpdate,
    ExecutionStats, FileRequest, FileResponse, FilesystemRequest, FilesystemResponse, KillOutcome,
    OperationId,
};
use async_trait::async_trait;

use super::super::{
    LocalExecutionBackend, LocalExecutionHandle, LocalExecutionObservation,
    LocalExecutionResourcePlan, LocalExecutionTermination,
};
use crate::{BoxRecord, ManagedRuntimeRoute};

pub(super) struct IsolationSplitBackend {
    sandbox: Arc<dyn LocalExecutionBackend>,
    microvm: Arc<dyn LocalExecutionBackend>,
}

impl IsolationSplitBackend {
    pub(super) fn new(
        sandbox: Arc<dyn LocalExecutionBackend>,
        microvm: Arc<dyn LocalExecutionBackend>,
    ) -> Self {
        Self { sandbox, microvm }
    }

    fn pick(&self, isolation: ExecutionIsolation) -> &Arc<dyn LocalExecutionBackend> {
        if isolation.is_sandbox() {
            &self.sandbox
        } else {
            &self.microvm
        }
    }
}

#[async_trait]
impl LocalExecutionBackend for IsolationSplitBackend {
    async fn preflight_isolation(
        &self,
        isolation: ExecutionIsolation,
    ) -> ExecutionManagerResult<()> {
        self.pick(isolation).preflight_isolation(isolation).await
    }

    fn route_for_create(&self, record: &BoxRecord) -> ExecutionManagerResult<ManagedRuntimeRoute> {
        self.pick(record.isolation).route_for_create(record)
    }

    async fn preflight(&self, record: &BoxRecord) -> ExecutionManagerResult<()> {
        self.pick(record.isolation).preflight(record).await
    }

    async fn plan_create_resources(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionResourcePlan> {
        self.pick(record.isolation)
            .plan_create_resources(record)
            .await
    }

    async fn start(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        self.pick(record.isolation).start(record).await
    }

    async fn inspect(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionObservation> {
        self.pick(record.isolation).inspect(record).await
    }

    async fn pause(
        &self,
        record: &BoxRecord,
        keep_memory: bool,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        self.pick(record.isolation).pause(record, keep_memory).await
    }

    async fn resume(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        self.pick(record.isolation).resume(record).await
    }

    async fn preflight_resource_update(
        &self,
        record: &BoxRecord,
        update: &ExecutionResourceUpdate,
    ) -> ExecutionManagerResult<()> {
        self.pick(record.isolation)
            .preflight_resource_update(record, update)
            .await
    }

    async fn update_resources(
        &self,
        record: &BoxRecord,
        operation_id: &OperationId,
        update: &ExecutionResourceUpdate,
    ) -> ExecutionManagerResult<()> {
        self.pick(record.isolation)
            .update_resources(record, operation_id, update)
            .await
    }

    async fn list_processes(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<ExecutionProcessInventory> {
        self.pick(record.isolation).list_processes(record).await
    }

    async fn stats(&self, record: &BoxRecord) -> ExecutionManagerResult<ExecutionStats> {
        self.pick(record.isolation).stats(record).await
    }

    async fn events(
        &self,
        record: &BoxRecord,
        request: ExecutionEventsRequest,
    ) -> ExecutionManagerResult<ExecutionEventBatch> {
        self.pick(record.isolation).events(record, request).await
    }

    async fn execute(
        &self,
        record: &BoxRecord,
        request: ExecRequest,
    ) -> ExecutionManagerResult<ExecOutput> {
        self.pick(record.isolation).execute(record, request).await
    }

    async fn start_process(
        &self,
        record: &BoxRecord,
        request: ExecRequest,
    ) -> ExecutionManagerResult<ExecutionProcess> {
        self.pick(record.isolation)
            .start_process(record, request)
            .await
    }

    async fn start_pty(
        &self,
        record: &BoxRecord,
        request: PtyRequest,
    ) -> ExecutionManagerResult<ExecutionProcess> {
        self.pick(record.isolation).start_pty(record, request).await
    }

    async fn transfer_file(
        &self,
        record: &BoxRecord,
        request: FileRequest,
    ) -> ExecutionManagerResult<FileResponse> {
        self.pick(record.isolation)
            .transfer_file(record, request)
            .await
    }

    async fn filesystem(
        &self,
        record: &BoxRecord,
        request: FilesystemRequest,
    ) -> ExecutionManagerResult<FilesystemResponse> {
        self.pick(record.isolation)
            .filesystem(record, request)
            .await
    }

    async fn prepare_quiescent_rootfs(&self, record: &BoxRecord) -> ExecutionManagerResult<()> {
        self.pick(record.isolation)
            .prepare_quiescent_rootfs(record)
            .await
    }

    async fn cleanup_quiescent_rootfs(&self, record: &BoxRecord) -> ExecutionManagerResult<()> {
        self.pick(record.isolation)
            .cleanup_quiescent_rootfs(record)
            .await
    }

    async fn stop_for_restart(
        &self,
        record: &BoxRecord,
        timeout_secs: Option<u64>,
    ) -> ExecutionManagerResult<KillOutcome> {
        self.pick(record.isolation)
            .stop_for_restart(record, timeout_secs)
            .await
    }

    async fn kill(&self, record: &BoxRecord) -> ExecutionManagerResult<KillOutcome> {
        self.pick(record.isolation).kill(record).await
    }

    async fn kill_with_status(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionTermination> {
        self.pick(record.isolation).kill_with_status(record).await
    }
}
