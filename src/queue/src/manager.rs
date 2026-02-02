//! Queue manager provides high-level queue management

use super::QueueStats;
use a3s_box_core::error::Result as BoxResult;
use a3s_box_core::queue::{Command, CommandQueue};
use anyhow::Result;
use std::sync::Arc;
use tracing::info;

/// Queue manager
#[allow(dead_code)]
pub struct QueueManager {
    queue: Arc<CommandQueue>,
    scheduler_handle: tokio::sync::Mutex<Option<()>>,
}

impl QueueManager {
    /// Create a new queue manager
    pub(crate) fn new(queue: Arc<CommandQueue>) -> Self {
        Self {
            queue,
            scheduler_handle: tokio::sync::Mutex::new(None),
        }
    }

    /// Start the queue scheduler
    pub async fn start(&self) -> Result<()> {
        info!("Starting queue scheduler");
        let queue = Arc::clone(&self.queue);
        queue.start_scheduler().await;
        Ok(())
    }

    /// Submit a command to a lane
    pub async fn submit(
        &self,
        lane_id: &str,
        command: Box<dyn Command>,
    ) -> BoxResult<tokio::sync::oneshot::Receiver<BoxResult<serde_json::Value>>> {
        self.queue.submit(lane_id, command).await
    }

    /// Get queue statistics
    pub async fn stats(&self) -> Result<QueueStats> {
        let lane_status = self.queue.status().await;

        let mut total_pending = 0;
        let mut total_active = 0;

        for status in lane_status.values() {
            total_pending += status.pending;
            total_active += status.active;
        }

        Ok(QueueStats {
            total_pending,
            total_active,
            lanes: lane_status,
        })
    }

    /// Get the underlying command queue
    pub fn queue(&self) -> Arc<CommandQueue> {
        Arc::clone(&self.queue)
    }
}
