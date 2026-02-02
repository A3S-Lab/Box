//! Lane-based command queue utilities and monitoring
//!
//! This module provides utilities and monitoring capabilities for the core command queue.

use a3s_box_core::queue::{lane_ids, priorities, CommandQueue, Lane, LaneStatus};
use a3s_box_core::{config::LaneConfig, event::EventEmitter};
use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;

pub mod manager;
pub mod monitor;

pub use manager::QueueManager;
pub use monitor::QueueMonitor;

/// Queue statistics
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct QueueStats {
    pub total_pending: usize,
    pub total_active: usize,
    pub lanes: HashMap<String, LaneStatus>,
}

/// Queue manager provides a high-level API for managing the command queue
pub struct QueueManagerBuilder {
    event_emitter: EventEmitter,
    lane_configs: HashMap<String, (LaneConfig, u8)>,
}

impl QueueManagerBuilder {
    /// Create a new queue manager builder
    pub fn new(event_emitter: EventEmitter) -> Self {
        Self {
            event_emitter,
            lane_configs: HashMap::new(),
        }
    }

    /// Add a lane configuration
    pub fn with_lane(
        mut self,
        id: impl Into<String>,
        config: LaneConfig,
        priority: u8,
    ) -> Self {
        self.lane_configs.insert(id.into(), (config, priority));
        self
    }

    /// Add default lanes (system, control, query, session, skill, prompt)
    pub fn with_default_lanes(mut self) -> Self {
        self.lane_configs.insert(
            lane_ids::SYSTEM.to_string(),
            (
                LaneConfig {
                    min_concurrency: 1,
                    max_concurrency: 5,
                },
                priorities::SYSTEM,
            ),
        );
        self.lane_configs.insert(
            lane_ids::CONTROL.to_string(),
            (
                LaneConfig {
                    min_concurrency: 1,
                    max_concurrency: 3,
                },
                priorities::CONTROL,
            ),
        );
        self.lane_configs.insert(
            lane_ids::QUERY.to_string(),
            (
                LaneConfig {
                    min_concurrency: 1,
                    max_concurrency: 10,
                },
                priorities::QUERY,
            ),
        );
        self.lane_configs.insert(
            lane_ids::SESSION.to_string(),
            (
                LaneConfig {
                    min_concurrency: 1,
                    max_concurrency: 5,
                },
                priorities::SESSION,
            ),
        );
        self.lane_configs.insert(
            lane_ids::SKILL.to_string(),
            (
                LaneConfig {
                    min_concurrency: 1,
                    max_concurrency: 3,
                },
                priorities::SKILL,
            ),
        );
        self.lane_configs.insert(
            lane_ids::PROMPT.to_string(),
            (
                LaneConfig {
                    min_concurrency: 1,
                    max_concurrency: 2,
                },
                priorities::PROMPT,
            ),
        );
        self
    }

    /// Build the queue manager
    pub async fn build(self) -> Result<QueueManager> {
        let queue = Arc::new(CommandQueue::new(self.event_emitter));

        // Register all lanes
        for (id, (config, priority)) in self.lane_configs {
            let lane = Arc::new(Lane::new(id, config, priority));
            queue.register_lane(lane).await;
        }

        Ok(QueueManager::new(queue))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_queue_manager_builder() {
        let emitter = EventEmitter::new(100);
        let manager = QueueManagerBuilder::new(emitter)
            .with_default_lanes()
            .build()
            .await
            .unwrap();

        let stats = manager.stats().await.unwrap();
        assert_eq!(stats.lanes.len(), 6);
    }
}
