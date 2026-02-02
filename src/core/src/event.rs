use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::broadcast;

/// Event key type
pub type EventKey = String;

/// Event payload
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum EventPayload {
    Empty,
    String(String),
    Map(HashMap<String, serde_json::Value>),
}

/// Box event
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxEvent {
    /// Event key (e.g., "box.ready", "session.created")
    pub key: EventKey,

    /// Event payload
    pub payload: EventPayload,

    /// Timestamp
    pub timestamp: chrono::DateTime<chrono::Utc>,
}

impl BoxEvent {
    /// Create a new event
    pub fn new(key: impl Into<String>, payload: EventPayload) -> Self {
        Self {
            key: key.into(),
            payload,
            timestamp: chrono::Utc::now(),
        }
    }

    /// Create an event with no payload
    pub fn empty(key: impl Into<String>) -> Self {
        Self::new(key, EventPayload::Empty)
    }

    /// Create an event with a string payload
    pub fn with_string(key: impl Into<String>, message: impl Into<String>) -> Self {
        Self::new(key, EventPayload::String(message.into()))
    }

    /// Create an event with a map payload
    pub fn with_map(key: impl Into<String>, map: HashMap<String, serde_json::Value>) -> Self {
        Self::new(key, EventPayload::Map(map))
    }
}

/// Event emitter
#[derive(Clone)]
pub struct EventEmitter {
    sender: Arc<broadcast::Sender<BoxEvent>>,
}

impl EventEmitter {
    /// Create a new event emitter
    pub fn new(capacity: usize) -> Self {
        let (sender, _) = broadcast::channel(capacity);
        Self {
            sender: Arc::new(sender),
        }
    }

    /// Emit an event
    pub fn emit(&self, event: BoxEvent) {
        let _ = self.sender.send(event);
    }

    /// Subscribe to events
    pub fn subscribe(&self) -> broadcast::Receiver<BoxEvent> {
        self.sender.subscribe()
    }

    /// Subscribe to events with a filter
    pub fn subscribe_filtered(&self, filter: impl Fn(&BoxEvent) -> bool + Send + Sync + 'static) -> EventStream {
        EventStream {
            receiver: self.sender.subscribe(),
            filter: Arc::new(filter),
        }
    }
}

/// Event stream with filtering
pub struct EventStream {
    receiver: broadcast::Receiver<BoxEvent>,
    filter: Arc<dyn Fn(&BoxEvent) -> bool + Send + Sync>,
}

impl EventStream {
    /// Receive the next matching event
    pub async fn recv(&mut self) -> Option<BoxEvent> {
        loop {
            match self.receiver.recv().await {
                Ok(event) => {
                    if (self.filter)(&event) {
                        return Some(event);
                    }
                }
                Err(_) => return None,
            }
        }
    }
}

/// Event catalog - predefined event keys
pub mod events {
    // Box events
    pub const BOX_READY: &str = "box.ready";
    pub const BOX_ERROR: &str = "box.error";
    pub const BOX_TIMEOUT: &str = "box.timeout";

    // Session events
    pub const SESSION_CREATED: &str = "session.created";
    pub const SESSION_DESTROYED: &str = "session.destroyed";
    pub const SESSION_CONTEXT_WARNING: &str = "session.context.warning";
    pub const SESSION_CONTEXT_COMPACTED: &str = "session.context.compacted";

    // Prompt events
    pub const PROMPT_STARTED: &str = "prompt.started";
    pub const PROMPT_COMPLETED: &str = "prompt.completed";
    pub const PROMPT_CANCELLED: &str = "prompt.cancelled";
    pub const PROMPT_TEXT_DELTA: &str = "prompt.text.delta";
    pub const PROMPT_TOOL_CALLED: &str = "prompt.tool.called";
    pub const PROMPT_TOOL_COMPLETED: &str = "prompt.tool.completed";
    pub const PROMPT_STEP_STARTED: &str = "prompt.step.started";
    pub const PROMPT_STEP_COMPLETED: &str = "prompt.step.completed";

    // Skill events
    pub const SKILL_ACTIVATING: &str = "skill.activating";
    pub const SKILL_ACTIVATED: &str = "skill.activated";
    pub const SKILL_DEACTIVATED: &str = "skill.deactivated";
    pub const SKILL_TOOL_DOWNLOADING: &str = "skill.tool.downloading";
    pub const SKILL_TOOL_DOWNLOADED: &str = "skill.tool.downloaded";
    pub const SKILL_TOOL_FAILED: &str = "skill.tool.failed";

    // Queue events
    pub const QUEUE_LANE_PRESSURE: &str = "queue.lane.pressure";
    pub const QUEUE_LANE_IDLE: &str = "queue.lane.idle";
}
