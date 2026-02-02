use crate::config::LaneConfig;
use crate::error::{BoxError, Result};
use crate::event::EventEmitter;
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};
use uuid::Uuid;

/// Lane identifier
pub type LaneId = String;

/// Command identifier
pub type CommandId = String;

/// Lane priority (lower number = higher priority)
pub type Priority = u8;

/// Lane priorities
pub mod priorities {
    use super::Priority;

    pub const SYSTEM: Priority = 0;
    pub const CONTROL: Priority = 1;
    pub const QUERY: Priority = 2;
    pub const SESSION: Priority = 3;
    pub const SKILL: Priority = 4;
    pub const PROMPT: Priority = 5;
}

/// Command to be executed
#[async_trait]
pub trait Command: Send + Sync {
    /// Execute the command
    async fn execute(&self) -> Result<serde_json::Value>;

    /// Get command type (for logging/debugging)
    fn command_type(&self) -> &str;
}

/// Command wrapper
#[allow(dead_code)]
struct CommandWrapper {
    id: CommandId,
    command: Box<dyn Command>,
    result_tx: tokio::sync::oneshot::Sender<Result<serde_json::Value>>,
}

/// Lane state
#[allow(dead_code)]
struct LaneState {
    /// Lane configuration
    config: LaneConfig,

    /// Priority
    priority: Priority,

    /// Pending commands (FIFO queue)
    pending: VecDeque<CommandWrapper>,

    /// Active command count
    active: usize,

    /// Semaphore for concurrency control
    semaphore: Arc<Semaphore>,
}

impl LaneState {
    fn new(config: LaneConfig, priority: Priority) -> Self {
        let semaphore = Arc::new(Semaphore::new(config.max_concurrency));
        Self {
            config,
            priority,
            pending: VecDeque::new(),
            active: 0,
            semaphore,
        }
    }

    fn has_capacity(&self) -> bool {
        self.active < self.config.max_concurrency
    }

    fn has_pending(&self) -> bool {
        !self.pending.is_empty()
    }
}

/// Lane
pub struct Lane {
    id: LaneId,
    state: Arc<Mutex<LaneState>>,
}

impl Lane {
    /// Create a new lane
    pub fn new(id: impl Into<String>, config: LaneConfig, priority: Priority) -> Self {
        Self {
            id: id.into(),
            state: Arc::new(Mutex::new(LaneState::new(config, priority))),
        }
    }

    /// Get lane ID
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Get lane priority
    pub async fn priority(&self) -> Priority {
        self.state.lock().await.priority
    }

    /// Enqueue a command
    pub async fn enqueue(
        &self,
        command: Box<dyn Command>,
    ) -> tokio::sync::oneshot::Receiver<Result<serde_json::Value>> {
        let (tx, rx) = tokio::sync::oneshot::channel();
        let wrapper = CommandWrapper {
            id: Uuid::new_v4().to_string(),
            command,
            result_tx: tx,
        };

        let mut state = self.state.lock().await;
        state.pending.push_back(wrapper);

        rx
    }

    /// Try to dequeue a command for execution
    async fn try_dequeue(&self) -> Option<CommandWrapper> {
        let mut state = self.state.lock().await;
        if state.has_capacity() && state.has_pending() {
            state.active += 1;
            state.pending.pop_front()
        } else {
            None
        }
    }

    /// Mark a command as completed
    async fn mark_completed(&self) {
        let mut state = self.state.lock().await;
        state.active = state.active.saturating_sub(1);
    }

    /// Get lane status
    pub async fn status(&self) -> LaneStatus {
        let state = self.state.lock().await;
        LaneStatus {
            pending: state.pending.len(),
            active: state.active,
            min: state.config.min_concurrency,
            max: state.config.max_concurrency,
        }
    }
}

/// Lane status
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LaneStatus {
    pub pending: usize,
    pub active: usize,
    pub min: usize,
    pub max: usize,
}

/// Command queue
#[allow(dead_code)]
pub struct CommandQueue {
    lanes: Arc<Mutex<HashMap<LaneId, Arc<Lane>>>>,
    event_emitter: EventEmitter,
}

impl CommandQueue {
    /// Create a new command queue
    pub fn new(event_emitter: EventEmitter) -> Self {
        Self {
            lanes: Arc::new(Mutex::new(HashMap::new())),
            event_emitter,
        }
    }

    /// Register a lane
    pub async fn register_lane(&self, lane: Arc<Lane>) {
        let mut lanes = self.lanes.lock().await;
        lanes.insert(lane.id().to_string(), lane);
    }

    /// Submit a command to a lane
    pub async fn submit(
        &self,
        lane_id: &str,
        command: Box<dyn Command>,
    ) -> Result<tokio::sync::oneshot::Receiver<Result<serde_json::Value>>> {
        let lanes = self.lanes.lock().await;
        let lane = lanes
            .get(lane_id)
            .ok_or_else(|| BoxError::QueueError(format!("Lane not found: {}", lane_id)))?;

        Ok(lane.enqueue(command).await)
    }

    /// Start the scheduler
    pub async fn start_scheduler(self: Arc<Self>) {
        tokio::spawn(async move {
            loop {
                self.schedule_next().await;
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }
        });
    }

    /// Schedule the next command
    async fn schedule_next(&self) {
        // Find the highest-priority lane with pending commands
        let lanes = self.lanes.lock().await;

        // Collect lanes with their priorities
        let mut lane_priorities = Vec::new();
        for lane in lanes.values() {
            let priority = lane.priority().await;
            lane_priorities.push((priority, Arc::clone(lane)));
        }

        // Sort by priority (lower number = higher priority)
        lane_priorities.sort_by_key(|(priority, _)| *priority);

        for (_, lane) in lane_priorities {
            if let Some(wrapper) = lane.try_dequeue().await {
                let lane_clone = Arc::clone(&lane);
                tokio::spawn(async move {
                    let result = wrapper.command.execute().await;
                    let _ = wrapper.result_tx.send(result);
                    lane_clone.mark_completed().await;
                });
                break;
            }
        }
    }

    /// Get queue status for all lanes
    pub async fn status(&self) -> HashMap<LaneId, LaneStatus> {
        let lanes = self.lanes.lock().await;
        let mut status = HashMap::new();

        for (id, lane) in lanes.iter() {
            status.insert(id.clone(), lane.status().await);
        }

        status
    }
}

/// Built-in lane IDs
pub mod lane_ids {
    pub const SYSTEM: &str = "system";
    pub const CONTROL: &str = "control";
    pub const QUERY: &str = "query";
    pub const SESSION: &str = "session";
    pub const SKILL: &str = "skill";
    pub const PROMPT: &str = "prompt";
}
