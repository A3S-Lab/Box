//! Adapter from native-engine control to the sole durable operation journal.

#[cfg(target_os = "linux")]
use std::time::Duration;

use a3s_box_core::error::{BoxError, Result as BoxResult};
use a3s_box_core::OperationId;
use async_trait::async_trait;

use crate::oci::build::cache::RecordedBuildCache;
use crate::oci::build::engine::{BuildExecutionObserver, BuildImageCommitPermit};
use crate::oci::build::receipt::{
    BuildOperationJournal, BuildProcessIdentity, PersistedBuildOperation, PersistedBuildPhase,
};
use crate::oci::build::{BuildCachePolicy, BuildOperationIdentity, BuildReceiptError};

#[cfg(target_os = "linux")]
const RUN_PROCESS_STOP_TIMEOUT: Duration = Duration::from_secs(5);

pub(super) struct JournalBuildObserver {
    pub(super) journal: BuildOperationJournal,
    pub(super) identity: BuildOperationIdentity,
    pub(super) plan_digest: String,
    pub(super) cache_policy: BuildCachePolicy,
}

#[async_trait]
impl BuildExecutionObserver for JournalBuildObserver {
    async fn cancellation_requested(&self) -> BoxResult<bool> {
        let locked = self
            .journal
            .lock(self.identity.operation_id())
            .await
            .map_err(observer_error)?;
        let record = locked.read().await.map_err(observer_error)?;
        match record {
            Some(PersistedBuildOperation::Supervised(operation)) => {
                operation
                    .require_identity(&self.identity, &self.plan_digest, self.cache_policy)
                    .map_err(observer_error)?;
                Ok(operation.phase != PersistedBuildPhase::Running)
            }
            Some(PersistedBuildOperation::Succeeded(_)) => Ok(true),
            Some(PersistedBuildOperation::Pending(_)) | None => Err(BoxError::BuildError(
                "supervised build lost its authoritative operation state".to_string(),
            )),
        }
    }

    async fn acquire_image_commit_permit(&self) -> BoxResult<BuildImageCommitPermit> {
        let locked = self
            .journal
            .lock(self.identity.operation_id())
            .await
            .map_err(observer_error)?;
        let Some(PersistedBuildOperation::Supervised(operation)) =
            locked.read().await.map_err(observer_error)?
        else {
            return Err(BoxError::BuildError(
                "supervised build lost its authoritative operation state before image commit"
                    .to_string(),
            ));
        };
        operation
            .require_identity(&self.identity, &self.plan_digest, self.cache_policy)
            .map_err(observer_error)?;
        if operation.phase != PersistedBuildPhase::Running {
            return Err(BoxError::BuildError(
                "recorded build operation was cancelled before image commit".to_string(),
            ));
        }
        Ok(BuildImageCommitPermit::new(locked))
    }

    async fn publish_cache_export(
        &self,
        staged: RecordedBuildCache,
    ) -> BoxResult<RecordedBuildCache> {
        self.journal
            .publish_cache_export(self.identity.operation_id(), staged)
            .await
            .map_err(observer_error)
    }

    async fn run_process_started(&self, pid: u32, start_time: Option<u64>) -> BoxResult<()> {
        let locked = self
            .journal
            .lock(self.identity.operation_id())
            .await
            .map_err(observer_error)?;
        let Some(PersistedBuildOperation::Supervised(mut operation)) =
            locked.read().await.map_err(observer_error)?
        else {
            return Err(BoxError::BuildError(
                "supervised RUN has no active operation record".to_string(),
            ));
        };
        operation
            .require_identity(&self.identity, &self.plan_digest, self.cache_policy)
            .map_err(observer_error)?;
        operation
            .set_run_process(Some(BuildProcessIdentity { pid, start_time }))
            .map_err(observer_error)?;
        let cancelling = operation.phase == PersistedBuildPhase::Cancelling;
        locked
            .write_supervised(operation)
            .await
            .map_err(observer_error)?;
        if cancelling {
            return Err(BoxError::BuildError(
                "recorded build cancellation raced Dockerfile RUN startup".to_string(),
            ));
        }
        Ok(())
    }

    async fn run_process_finished(&self, pid: u32, start_time: Option<u64>) -> BoxResult<()> {
        let locked = self
            .journal
            .lock(self.identity.operation_id())
            .await
            .map_err(observer_error)?;
        let Some(PersistedBuildOperation::Supervised(mut operation)) =
            locked.read().await.map_err(observer_error)?
        else {
            return Ok(());
        };
        operation
            .require_identity(&self.identity, &self.plan_digest, self.cache_policy)
            .map_err(observer_error)?;
        if operation.run_process == Some(BuildProcessIdentity { pid, start_time }) {
            operation.set_run_process(None).map_err(observer_error)?;
            locked
                .write_supervised(operation)
                .await
                .map_err(observer_error)?;
        }
        Ok(())
    }
}

fn observer_error(error: BuildReceiptError) -> BoxError {
    BoxError::BuildError(format!(
        "build operation journal rejected native execution: {error}"
    ))
}

pub(super) async fn fence_run_process(
    process: Option<BuildProcessIdentity>,
    operation_id: &OperationId,
) -> Result<(), BuildReceiptError> {
    let Some(process) = process else {
        return Ok(());
    };
    let operation = operation_id.to_string();
    tokio::task::spawn_blocking(move || fence_run_process_blocking(process, &operation))
        .await
        .map_err(|error| BuildReceiptError::Task {
            operation_id: operation_id.to_string(),
            message: format!("RUN process fencing task failed: {error}"),
        })?
}

#[cfg(target_os = "linux")]
fn fence_run_process_blocking(
    process: BuildProcessIdentity,
    operation_id: &str,
) -> Result<(), BuildReceiptError> {
    if !crate::process::is_process_alive_with_identity(process.pid, process.start_time) {
        return Ok(());
    }
    let pid = i32::try_from(process.pid).map_err(|_| BuildReceiptError::InvalidReceipt {
        operation_id: operation_id.to_string(),
        message: "RUN process PID exceeds the host signal range".to_string(),
    })?;
    if unsafe { libc::kill(pid, libc::SIGKILL) } != 0 {
        let source = std::io::Error::last_os_error();
        if source.raw_os_error() != Some(libc::ESRCH) {
            return Err(BuildReceiptError::StoreIo {
                message: format!("failed to kill RUN process for operation {operation_id}"),
                source,
            });
        }
    }
    let deadline = std::time::Instant::now() + RUN_PROCESS_STOP_TIMEOUT;
    while crate::process::is_process_running_with_identity(process.pid, process.start_time) {
        if std::time::Instant::now() >= deadline {
            return Err(BuildReceiptError::Task {
                operation_id: operation_id.to_string(),
                message: format!(
                    "RUN process {} did not stop within {:?}",
                    process.pid, RUN_PROCESS_STOP_TIMEOUT
                ),
            });
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    Ok(())
}

#[cfg(not(target_os = "linux"))]
fn fence_run_process_blocking(
    _process: BuildProcessIdentity,
    operation_id: &str,
) -> Result<(), BuildReceiptError> {
    Err(BuildReceiptError::Task {
        operation_id: operation_id.to_string(),
        message: "a recorded Linux RUN process cannot be fenced on this host".to_string(),
    })
}
