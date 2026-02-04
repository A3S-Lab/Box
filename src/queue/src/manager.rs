//! Queue manager provides high-level queue management

use super::QueueStats;
use a3s_box_core::error::Result as BoxResult;
use a3s_box_core::queue::{Command, CommandQueue};
use anyhow::Result;
use std::sync::Arc;

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
        tracing::info!("Starting queue scheduler");
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

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::config::LaneConfig;
    use a3s_box_core::event::EventEmitter;
    use a3s_box_core::queue::{lane_ids, priorities, Lane};
    use async_trait::async_trait;

    struct TestCommand {
        result: serde_json::Value,
    }

    #[async_trait]
    impl Command for TestCommand {
        async fn execute(&self) -> BoxResult<serde_json::Value> {
            Ok(self.result.clone())
        }
        fn command_type(&self) -> &str {
            "test"
        }
    }

    struct FailingCommand {
        message: String,
    }

    #[async_trait]
    impl Command for FailingCommand {
        async fn execute(&self) -> BoxResult<serde_json::Value> {
            Err(a3s_box_core::error::BoxError::Other(self.message.clone()))
        }
        fn command_type(&self) -> &str {
            "failing"
        }
    }

    /// Helper: build a QueueManager with standard lanes
    async fn make_manager() -> QueueManager {
        let emitter = EventEmitter::new(100);
        let queue = Arc::new(CommandQueue::new(emitter));

        let lanes = [
            (lane_ids::SYSTEM, priorities::SYSTEM, 5),
            (lane_ids::CONTROL, priorities::CONTROL, 3),
            (lane_ids::QUERY, priorities::QUERY, 10),
            (lane_ids::PROMPT, priorities::PROMPT, 2),
        ];

        for (id, priority, max) in lanes {
            let config = LaneConfig {
                min_concurrency: 1,
                max_concurrency: max,
            };
            queue
                .register_lane(Arc::new(Lane::new(id, config, priority)))
                .await;
        }

        QueueManager::new(queue)
    }

    // ========================================================================
    // Construction Tests
    // ========================================================================

    #[tokio::test]
    async fn test_manager_new() {
        let manager = make_manager().await;

        let stats = manager.stats().await.unwrap();
        assert_eq!(stats.total_pending, 0);
        assert_eq!(stats.total_active, 0);
        assert_eq!(stats.lanes.len(), 4);
    }

    #[tokio::test]
    async fn test_manager_queue_accessor() {
        let manager = make_manager().await;

        let queue = manager.queue();
        let status = queue.status().await;
        assert_eq!(status.len(), 4);
    }

    // ========================================================================
    // stats() Tests
    // ========================================================================

    #[tokio::test]
    async fn test_manager_stats_empty() {
        let manager = make_manager().await;

        let stats = manager.stats().await.unwrap();
        assert_eq!(stats.total_pending, 0);
        assert_eq!(stats.total_active, 0);

        // Check each lane
        assert_eq!(stats.lanes[lane_ids::SYSTEM].pending, 0);
        assert_eq!(stats.lanes[lane_ids::SYSTEM].max, 5);
        assert_eq!(stats.lanes[lane_ids::CONTROL].max, 3);
        assert_eq!(stats.lanes[lane_ids::QUERY].max, 10);
        assert_eq!(stats.lanes[lane_ids::PROMPT].max, 2);
    }

    #[tokio::test]
    async fn test_manager_stats_with_pending() {
        let manager = make_manager().await;

        // Submit commands without starting scheduler
        for _ in 0..4 {
            let cmd = Box::new(TestCommand {
                result: serde_json::json!({}),
            });
            let _ = manager.submit(lane_ids::QUERY, cmd).await;
        }

        let stats = manager.stats().await.unwrap();
        assert_eq!(stats.total_pending, 4);
        assert_eq!(stats.total_active, 0);
        assert_eq!(stats.lanes[lane_ids::QUERY].pending, 4);
    }

    #[tokio::test]
    async fn test_manager_stats_multiple_lanes() {
        let manager = make_manager().await;

        let _ = manager
            .submit(
                lane_ids::SYSTEM,
                Box::new(TestCommand {
                    result: serde_json::json!({}),
                }),
            )
            .await;
        let _ = manager
            .submit(
                lane_ids::QUERY,
                Box::new(TestCommand {
                    result: serde_json::json!({}),
                }),
            )
            .await;
        let _ = manager
            .submit(
                lane_ids::QUERY,
                Box::new(TestCommand {
                    result: serde_json::json!({}),
                }),
            )
            .await;
        let _ = manager
            .submit(
                lane_ids::PROMPT,
                Box::new(TestCommand {
                    result: serde_json::json!({}),
                }),
            )
            .await;

        let stats = manager.stats().await.unwrap();
        assert_eq!(stats.total_pending, 4);
        assert_eq!(stats.lanes[lane_ids::SYSTEM].pending, 1);
        assert_eq!(stats.lanes[lane_ids::QUERY].pending, 2);
        assert_eq!(stats.lanes[lane_ids::PROMPT].pending, 1);
        assert_eq!(stats.lanes[lane_ids::CONTROL].pending, 0);
    }

    // ========================================================================
    // submit() Tests
    // ========================================================================

    #[tokio::test]
    async fn test_manager_submit_valid_lane() {
        let manager = make_manager().await;

        let cmd = Box::new(TestCommand {
            result: serde_json::json!({"data": "ok"}),
        });
        let result = manager.submit(lane_ids::QUERY, cmd).await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_manager_submit_unknown_lane() {
        let manager = make_manager().await;

        let cmd = Box::new(TestCommand {
            result: serde_json::json!({}),
        });
        let result = manager.submit("nonexistent-lane", cmd).await;

        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_manager_submit_and_execute() {
        let manager = make_manager().await;
        manager.start().await.unwrap();

        let cmd = Box::new(TestCommand {
            result: serde_json::json!({"key": "value"}),
        });
        let rx = manager.submit(lane_ids::QUERY, cmd).await.unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("Timeout")
            .expect("Channel closed");
        assert!(result.is_ok());
        assert_eq!(result.unwrap(), serde_json::json!({"key": "value"}));
    }

    #[tokio::test]
    async fn test_manager_submit_failing_command() {
        let manager = make_manager().await;
        manager.start().await.unwrap();

        let cmd = Box::new(FailingCommand {
            message: "manager test failure".to_string(),
        });
        let rx = manager.submit(lane_ids::QUERY, cmd).await.unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("Timeout")
            .expect("Channel closed");
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_manager_submit_multiple_commands() {
        let manager = make_manager().await;
        manager.start().await.unwrap();

        let mut receivers = Vec::new();
        for i in 0..5 {
            let cmd = Box::new(TestCommand {
                result: serde_json::json!({"index": i}),
            });
            let rx = manager.submit(lane_ids::QUERY, cmd).await.unwrap();
            receivers.push(rx);
        }

        for (i, rx) in receivers.into_iter().enumerate() {
            let result = tokio::time::timeout(std::time::Duration::from_secs(2), rx)
                .await
                .expect("Timeout")
                .expect("Channel closed");
            assert!(result.is_ok());
            let val = result.unwrap();
            assert_eq!(val["index"], i);
        }
    }

    // ========================================================================
    // start() Tests
    // ========================================================================

    #[tokio::test]
    async fn test_manager_start() {
        let manager = make_manager().await;

        let result = manager.start().await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_manager_start_drains_pending() {
        let manager = make_manager().await;

        // Submit before start
        let cmd = Box::new(TestCommand {
            result: serde_json::json!({"queued": true}),
        });
        let rx = manager.submit(lane_ids::QUERY, cmd).await.unwrap();

        // Verify pending
        let stats = manager.stats().await.unwrap();
        assert_eq!(stats.total_pending, 1);

        // Start scheduler
        manager.start().await.unwrap();

        // Command should now execute
        let result = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("Timeout")
            .expect("Channel closed");
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["queued"], true);
    }

    // ========================================================================
    // queue() Accessor Tests
    // ========================================================================

    #[tokio::test]
    async fn test_manager_queue_returns_same_instance() {
        let manager = make_manager().await;

        let q1 = manager.queue();
        let q2 = manager.queue();

        // Both should point to the same underlying data
        assert!(Arc::ptr_eq(&q1, &q2));
    }

    #[tokio::test]
    async fn test_manager_queue_can_submit_directly() {
        let manager = make_manager().await;
        manager.start().await.unwrap();

        // Submit directly via underlying queue
        let queue = manager.queue();
        let cmd = Box::new(TestCommand {
            result: serde_json::json!({"direct": true}),
        });
        let rx = queue.submit(lane_ids::SYSTEM, cmd).await.unwrap();

        let result = tokio::time::timeout(std::time::Duration::from_secs(1), rx)
            .await
            .expect("Timeout")
            .expect("Channel closed");
        assert!(result.is_ok());
        assert_eq!(result.unwrap()["direct"], true);
    }
}
