//! Transient native-engine control derived from the durable operation journal.
//!
//! This module deliberately owns no state store, queue, or scheduler. The
//! observer reads and updates the existing receipt journal; this cloneable
//! wrapper only lets the native engine reach that single authority.

use std::sync::Arc;
use std::time::Duration;

use a3s_box_core::error::{BoxError, Result};
use async_trait::async_trait;

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
const CANCELLATION_POLL_INTERVAL: Duration = Duration::from_millis(25);

#[async_trait]
#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
pub(in crate::oci::build) trait BuildExecutionObserver: Send + Sync {
    async fn cancellation_requested(&self) -> Result<bool>;

    async fn run_process_started(&self, pid: u32, start_time: Option<u64>) -> Result<()>;

    async fn run_process_finished(&self, pid: u32, start_time: Option<u64>) -> Result<()>;
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
