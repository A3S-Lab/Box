//! Transient native-engine control derived from the durable operation journal.
//!
//! This module deliberately owns no state store, queue, or scheduler. The
//! observer reads and updates the existing receipt journal; this cloneable
//! wrapper only lets the native engine reach that single authority.

use std::sync::Arc;
use std::time::Duration;

use a3s_box_core::error::{BoxError, Result};
use async_trait::async_trait;

use crate::oci::build::cache::RecordedBuildCache;

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(25);

#[async_trait]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(in crate::oci::build) trait BuildExecutionObserver: Send + Sync {
    async fn cancellation_requested(&self) -> Result<bool>;

    async fn acquire_image_commit_permit(&self) -> Result<BuildImageCommitPermit>;

    async fn publish_cache_export(
        &self,
        _staged: RecordedBuildCache,
    ) -> Result<RecordedBuildCache> {
        Err(BoxError::BuildError(
            "recorded build cache export has no journal publisher".to_string(),
        ))
    }

    async fn run_process_started(&self, pid: u32, start_time: Option<u64>) -> Result<()>;

    async fn run_process_finished(&self, pid: u32, start_time: Option<u64>) -> Result<()>;
}

trait SendGuard: Send {}

impl<T: Send> SendGuard for T {}

/// RAII permit that serializes ImageStore publication with cancellation.
///
/// The concrete guard is supplied by the existing operation journal. This
/// wrapper owns no lock implementation or state of its own.
pub(in crate::oci::build) struct BuildImageCommitPermit {
    _guard: Box<dyn SendGuard>,
}

impl BuildImageCommitPermit {
    pub(in crate::oci::build) fn new(guard: impl Send + 'static) -> Self {
        Self {
            _guard: Box::new(guard),
        }
    }
}

/// Native-engine view of the one durable operation journal.
#[derive(Clone)]
pub(in crate::oci::build) struct BuildExecutionControl {
    observer: Arc<dyn BuildExecutionObserver>,
}

impl BuildExecutionControl {
    pub(in crate::oci::build) fn new(observer: Arc<dyn BuildExecutionObserver>) -> Self {
        Self { observer }
    }

    pub(in crate::oci::build) async fn ensure_active(&self) -> Result<()> {
        if self.observer.cancellation_requested().await? {
            return Err(cancelled_error());
        }
        Ok(())
    }

    pub(super) async fn acquire_image_commit_permit(&self) -> Result<BuildImageCommitPermit> {
        self.observer.acquire_image_commit_permit().await
    }

    pub(super) async fn publish_cache_export(
        &self,
        staged: RecordedBuildCache,
    ) -> Result<RecordedBuildCache> {
        self.observer.publish_cache_export(staged).await
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(super) async fn wait_for_cancellation(&self) -> Result<()> {
        loop {
            if self.observer.cancellation_requested().await? {
                return Ok(());
            }
            tokio::time::sleep(CANCELLATION_POLL_INTERVAL).await;
        }
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(super) async fn run_process_started(
        &self,
        pid: u32,
        start_time: Option<u64>,
    ) -> Result<()> {
        self.observer.run_process_started(pid, start_time).await
    }

    #[cfg_attr(not(target_os = "linux"), allow(dead_code))]
    pub(super) async fn run_process_finished(
        &self,
        pid: u32,
        start_time: Option<u64>,
    ) -> Result<()> {
        self.observer.run_process_finished(pid, start_time).await
    }
}

pub(super) fn cancelled_error() -> BoxError {
    BoxError::BuildError("recorded build operation was cancelled".to_string())
}
