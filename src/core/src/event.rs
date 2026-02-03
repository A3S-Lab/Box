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
    pub fn subscribe_filtered(
        &self,
        filter: impl Fn(&BoxEvent) -> bool + Send + Sync + 'static,
    ) -> EventStream {
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_box_event_new() {
        let event = BoxEvent::new("test.event", EventPayload::Empty);

        assert_eq!(event.key, "test.event");
        assert!(matches!(event.payload, EventPayload::Empty));
    }

    #[test]
    fn test_box_event_empty() {
        let event = BoxEvent::empty("box.ready");

        assert_eq!(event.key, "box.ready");
        assert!(matches!(event.payload, EventPayload::Empty));
    }

    #[test]
    fn test_box_event_with_string() {
        let event = BoxEvent::with_string("session.error", "Connection lost");

        assert_eq!(event.key, "session.error");
        if let EventPayload::String(msg) = &event.payload {
            assert_eq!(msg, "Connection lost");
        } else {
            panic!("Expected string payload");
        }
    }

    #[test]
    fn test_box_event_with_map() {
        let mut map = HashMap::new();
        map.insert("session_id".to_string(), serde_json::json!("sess-123"));
        map.insert("tokens".to_string(), serde_json::json!(1500));

        let event = BoxEvent::with_map("session.created", map);

        assert_eq!(event.key, "session.created");
        if let EventPayload::Map(m) = &event.payload {
            assert_eq!(m.get("session_id").unwrap(), &serde_json::json!("sess-123"));
            assert_eq!(m.get("tokens").unwrap(), &serde_json::json!(1500));
        } else {
            panic!("Expected map payload");
        }
    }

    #[test]
    fn test_box_event_timestamp() {
        let before = chrono::Utc::now();
        let event = BoxEvent::empty("test.event");
        let after = chrono::Utc::now();

        assert!(event.timestamp >= before);
        assert!(event.timestamp <= after);
    }

    #[test]
    fn test_event_emitter_new() {
        let emitter = EventEmitter::new(100);
        // Should not panic
        let _receiver = emitter.subscribe();
    }

    #[test]
    fn test_event_emitter_clone() {
        let emitter = EventEmitter::new(100);
        let cloned = emitter.clone();

        // Both should work
        emitter.emit(BoxEvent::empty("test.1"));
        cloned.emit(BoxEvent::empty("test.2"));
    }

    #[tokio::test]
    async fn test_event_emitter_subscribe() {
        let emitter = EventEmitter::new(100);
        let mut receiver = emitter.subscribe();

        emitter.emit(BoxEvent::empty("test.event"));

        let event = receiver.recv().await.unwrap();
        assert_eq!(event.key, "test.event");
    }

    #[tokio::test]
    async fn test_event_emitter_multiple_subscribers() {
        let emitter = EventEmitter::new(100);
        let mut receiver1 = emitter.subscribe();
        let mut receiver2 = emitter.subscribe();

        emitter.emit(BoxEvent::with_string("broadcast", "hello"));

        let event1 = receiver1.recv().await.unwrap();
        let event2 = receiver2.recv().await.unwrap();

        assert_eq!(event1.key, "broadcast");
        assert_eq!(event2.key, "broadcast");
    }

    #[tokio::test]
    async fn test_event_emitter_multiple_events() {
        let emitter = EventEmitter::new(100);
        let mut receiver = emitter.subscribe();

        emitter.emit(BoxEvent::empty("event.1"));
        emitter.emit(BoxEvent::empty("event.2"));
        emitter.emit(BoxEvent::empty("event.3"));

        assert_eq!(receiver.recv().await.unwrap().key, "event.1");
        assert_eq!(receiver.recv().await.unwrap().key, "event.2");
        assert_eq!(receiver.recv().await.unwrap().key, "event.3");
    }

    #[tokio::test]
    async fn test_event_stream_filtered() {
        let emitter = EventEmitter::new(100);
        let mut stream = emitter.subscribe_filtered(|e| e.key.starts_with("session."));

        emitter.emit(BoxEvent::empty("box.ready"));
        emitter.emit(BoxEvent::empty("session.created"));
        emitter.emit(BoxEvent::empty("prompt.started"));
        emitter.emit(BoxEvent::empty("session.destroyed"));

        // Should only receive session events
        let event1 = stream.recv().await.unwrap();
        assert_eq!(event1.key, "session.created");

        let event2 = stream.recv().await.unwrap();
        assert_eq!(event2.key, "session.destroyed");
    }

    #[tokio::test]
    async fn test_event_stream_filter_by_key() {
        let emitter = EventEmitter::new(100);
        let mut stream = emitter.subscribe_filtered(|e| e.key == events::BOX_READY);

        emitter.emit(BoxEvent::empty(events::BOX_ERROR));
        emitter.emit(BoxEvent::empty(events::BOX_READY));
        emitter.emit(BoxEvent::empty(events::BOX_TIMEOUT));

        let event = stream.recv().await.unwrap();
        assert_eq!(event.key, events::BOX_READY);
    }

    #[test]
    fn test_event_payload_empty_serialization() {
        let payload = EventPayload::Empty;
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: EventPayload = serde_json::from_str(&json).unwrap();
        assert!(matches!(parsed, EventPayload::Empty));
    }

    #[test]
    fn test_event_payload_string_serialization() {
        let payload = EventPayload::String("test message".to_string());
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: EventPayload = serde_json::from_str(&json).unwrap();

        if let EventPayload::String(s) = parsed {
            assert_eq!(s, "test message");
        } else {
            panic!("Expected string payload");
        }
    }

    #[test]
    fn test_event_payload_map_serialization() {
        let mut map = HashMap::new();
        map.insert("key1".to_string(), serde_json::json!("value1"));
        map.insert("key2".to_string(), serde_json::json!(42));

        let payload = EventPayload::Map(map);
        let json = serde_json::to_string(&payload).unwrap();
        let parsed: EventPayload = serde_json::from_str(&json).unwrap();

        if let EventPayload::Map(m) = parsed {
            assert_eq!(m.get("key1").unwrap(), &serde_json::json!("value1"));
            assert_eq!(m.get("key2").unwrap(), &serde_json::json!(42));
        } else {
            panic!("Expected map payload");
        }
    }

    #[test]
    fn test_box_event_serialization() {
        let event = BoxEvent::with_string("test.event", "hello");
        let json = serde_json::to_string(&event).unwrap();

        assert!(json.contains("test.event"));
        assert!(json.contains("hello"));
        assert!(json.contains("timestamp"));

        let parsed: BoxEvent = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.key, "test.event");
    }

    #[test]
    fn test_box_event_debug() {
        let event = BoxEvent::empty("debug.test");
        let debug_str = format!("{:?}", event);

        assert!(debug_str.contains("BoxEvent"));
        assert!(debug_str.contains("debug.test"));
    }

    #[test]
    fn test_box_event_clone() {
        let event = BoxEvent::with_string("clone.test", "original");
        let cloned = event.clone();

        assert_eq!(event.key, cloned.key);
        assert_eq!(event.timestamp, cloned.timestamp);
    }

    #[test]
    fn test_event_catalog_box_events() {
        assert_eq!(events::BOX_READY, "box.ready");
        assert_eq!(events::BOX_ERROR, "box.error");
        assert_eq!(events::BOX_TIMEOUT, "box.timeout");
    }

    #[test]
    fn test_event_catalog_session_events() {
        assert_eq!(events::SESSION_CREATED, "session.created");
        assert_eq!(events::SESSION_DESTROYED, "session.destroyed");
        assert_eq!(events::SESSION_CONTEXT_WARNING, "session.context.warning");
        assert_eq!(events::SESSION_CONTEXT_COMPACTED, "session.context.compacted");
    }

    #[test]
    fn test_event_catalog_prompt_events() {
        assert_eq!(events::PROMPT_STARTED, "prompt.started");
        assert_eq!(events::PROMPT_COMPLETED, "prompt.completed");
        assert_eq!(events::PROMPT_CANCELLED, "prompt.cancelled");
        assert_eq!(events::PROMPT_TEXT_DELTA, "prompt.text.delta");
        assert_eq!(events::PROMPT_TOOL_CALLED, "prompt.tool.called");
        assert_eq!(events::PROMPT_TOOL_COMPLETED, "prompt.tool.completed");
        assert_eq!(events::PROMPT_STEP_STARTED, "prompt.step.started");
        assert_eq!(events::PROMPT_STEP_COMPLETED, "prompt.step.completed");
    }

    #[test]
    fn test_event_catalog_skill_events() {
        assert_eq!(events::SKILL_ACTIVATING, "skill.activating");
        assert_eq!(events::SKILL_ACTIVATED, "skill.activated");
        assert_eq!(events::SKILL_DEACTIVATED, "skill.deactivated");
        assert_eq!(events::SKILL_TOOL_DOWNLOADING, "skill.tool.downloading");
        assert_eq!(events::SKILL_TOOL_DOWNLOADED, "skill.tool.downloaded");
        assert_eq!(events::SKILL_TOOL_FAILED, "skill.tool.failed");
    }

    #[test]
    fn test_event_catalog_queue_events() {
        assert_eq!(events::QUEUE_LANE_PRESSURE, "queue.lane.pressure");
        assert_eq!(events::QUEUE_LANE_IDLE, "queue.lane.idle");
    }

    #[test]
    fn test_event_key_naming_convention() {
        // All event keys should follow dot-separated lowercase format
        let all_events = vec![
            events::BOX_READY,
            events::SESSION_CREATED,
            events::PROMPT_STARTED,
            events::SKILL_ACTIVATED,
            events::QUEUE_LANE_PRESSURE,
        ];

        for event_key in all_events {
            assert!(event_key.chars().all(|c| c.is_lowercase() || c == '.'));
            assert!(event_key.contains('.'));
        }
    }
}
