//! Queue monitor for tracking queue metrics and health

use super::QueueStats;
use a3s_box_core::queue::CommandQueue;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::interval;
use tracing::{debug, warn};

/// Queue monitor configuration
#[derive(Debug, Clone)]
pub struct MonitorConfig {
    /// Monitoring interval
    pub interval: Duration,
    /// Warning threshold for pending commands
    pub pending_warning_threshold: usize,
    /// Warning threshold for active commands
    pub active_warning_threshold: usize,
}

impl Default for MonitorConfig {
    fn default() -> Self {
        Self {
            interval: Duration::from_secs(10),
            pending_warning_threshold: 100,
            active_warning_threshold: 50,
        }
    }
}

/// Queue monitor
pub struct QueueMonitor {
    queue: Arc<CommandQueue>,
    config: MonitorConfig,
}

impl QueueMonitor {
    /// Create a new queue monitor
    pub fn new(queue: Arc<CommandQueue>) -> Self {
        Self::with_config(queue, MonitorConfig::default())
    }

    /// Create a new queue monitor with custom configuration
    pub fn with_config(queue: Arc<CommandQueue>, config: MonitorConfig) -> Self {
        Self { queue, config }
    }

    /// Start monitoring
    pub async fn start(self: Arc<Self>) {
        let mut ticker = interval(self.config.interval);

        tokio::spawn(async move {
            loop {
                ticker.tick().await;
                self.check_health().await;
            }
        });
    }

    /// Check queue health
    async fn check_health(&self) {
        let status = self.queue.status().await;

        let mut total_pending = 0;
        let mut total_active = 0;

        for (lane_id, lane_status) in status.iter() {
            total_pending += lane_status.pending;
            total_active += lane_status.active;

            debug!(
                "Lane {}: pending={}, active={}, max={}",
                lane_id, lane_status.pending, lane_status.active, lane_status.max
            );

            // Check if lane is at capacity
            if lane_status.active >= lane_status.max {
                warn!("Lane {} is at maximum capacity", lane_id);
            }
        }

        // Check global thresholds
        if total_pending > self.config.pending_warning_threshold {
            warn!(
                "High number of pending commands: {} (threshold: {})",
                total_pending, self.config.pending_warning_threshold
            );
        }

        if total_active > self.config.active_warning_threshold {
            warn!(
                "High number of active commands: {} (threshold: {})",
                total_active, self.config.active_warning_threshold
            );
        }
    }

    /// Get current statistics
    pub async fn stats(&self) -> QueueStats {
        let lane_status = self.queue.status().await;

        let mut total_pending = 0;
        let mut total_active = 0;

        for status in lane_status.values() {
            total_pending += status.pending;
            total_active += status.active;
        }

        QueueStats {
            total_pending,
            total_active,
            lanes: lane_status,
        }
    }
}
