//! Metrics and observability

use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Box metrics
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxMetrics {
    /// Time since VM boot (seconds)
    pub uptime_seconds: u64,

    /// Total tokens consumed across all sessions
    pub total_tokens: usize,

    /// Total tool invocations
    pub total_tool_calls: usize,

    /// Number of active sessions
    pub active_sessions: usize,

    /// Cache size in MB
    pub cache_size_mb: f64,

    /// Current VM memory usage in MB
    pub memory_used_mb: f64,
}

/// Metrics collector
pub struct MetricsCollector {
    metrics: Arc<RwLock<BoxMetrics>>,
    start_time: chrono::DateTime<chrono::Utc>,
}

impl MetricsCollector {
    /// Create a new metrics collector
    pub fn new() -> Self {
        Self {
            metrics: Arc::new(RwLock::new(BoxMetrics {
                uptime_seconds: 0,
                total_tokens: 0,
                total_tool_calls: 0,
                active_sessions: 0,
                cache_size_mb: 0.0,
                memory_used_mb: 0.0,
            })),
            start_time: chrono::Utc::now(),
        }
    }

    /// Get current metrics
    pub async fn get_metrics(&self) -> BoxMetrics {
        let mut metrics = self.metrics.read().await.clone();
        metrics.uptime_seconds = (chrono::Utc::now() - self.start_time).num_seconds() as u64;
        metrics
    }

    /// Increment token count
    pub async fn add_tokens(&self, count: usize) {
        let mut metrics = self.metrics.write().await;
        metrics.total_tokens += count;
    }

    /// Increment tool call count
    pub async fn add_tool_call(&self) {
        let mut metrics = self.metrics.write().await;
        metrics.total_tool_calls += 1;
    }

    /// Update active session count
    pub async fn set_active_sessions(&self, count: usize) {
        let mut metrics = self.metrics.write().await;
        metrics.active_sessions = count;
    }

    /// Update cache size
    pub async fn set_cache_size(&self, size_mb: f64) {
        let mut metrics = self.metrics.write().await;
        metrics.cache_size_mb = size_mb;
    }

    /// Update memory usage
    pub async fn set_memory_usage(&self, usage_mb: f64) {
        let mut metrics = self.metrics.write().await;
        metrics.memory_used_mb = usage_mb;
    }
}

impl Default for MetricsCollector {
    fn default() -> Self {
        Self::new()
    }
}

/// Log entry
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogEntry {
    /// Log stream (runtime, agent, tools)
    pub stream: LogStream,

    /// Timestamp
    pub timestamp: chrono::DateTime<chrono::Utc>,

    /// Log level
    pub level: LogLevel,

    /// Message
    pub message: String,
}

/// Log stream
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum LogStream {
    Runtime,
    Agent,
    Tools,
}

/// Log level
#[derive(Debug, Clone, Copy, Serialize, Deserialize)]
pub enum LogLevel {
    Debug,
    Info,
    Warn,
    Error,
}

/// Log collector
pub struct LogCollector {
    // TODO: Implement log collection and streaming
    _placeholder: (),
}

impl LogCollector {
    /// Create a new log collector
    pub fn new() -> Self {
        Self { _placeholder: () }
    }

    /// Stream all logs
    pub async fn stream_all(&self) -> LogStream {
        // TODO: Implement
        todo!()
    }

    /// Stream logs from a specific stream
    pub async fn stream_filtered(&self, _stream: LogStream) -> LogStream {
        // TODO: Implement
        todo!()
    }
}

impl Default for LogCollector {
    fn default() -> Self {
        Self::new()
    }
}

// Avoid naming conflict with LogStream type
#[allow(dead_code)]
type LogStreamType = LogStream;
