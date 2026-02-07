//! Metrics and observability

use serde::{Deserialize, Serialize};
use std::path::PathBuf;
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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
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

/// Log collector that tails a console output file and categorizes log entries.
pub struct LogCollector {
    console_path: Option<PathBuf>,
    entries: Arc<RwLock<Vec<LogEntry>>>,
}

impl LogCollector {
    /// Create a new log collector.
    pub fn new(console_path: Option<PathBuf>) -> Self {
        Self {
            console_path,
            entries: Arc::new(RwLock::new(Vec::new())),
        }
    }

    /// Start tailing the console output file in the background.
    ///
    /// Spawns a tokio task that reads new lines from the console file,
    /// parses them into `LogEntry` values, and stores them in the buffer.
    pub fn start(&self) {
        let path = match &self.console_path {
            Some(p) => p.clone(),
            None => return,
        };

        let entries = self.entries.clone();

        tokio::spawn(async move {
            // Wait for the file to exist
            loop {
                if path.exists() {
                    break;
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            }

            let mut offset: u64 = 0;
            loop {
                match tokio::fs::read_to_string(&path).await {
                    Ok(content) => {
                        let bytes = content.as_bytes();
                        if (bytes.len() as u64) > offset {
                            let new_content = &content[offset as usize..];
                            let new_lines: Vec<&str> =
                                new_content.lines().collect();

                            let mut parsed: Vec<LogEntry> = new_lines
                                .into_iter()
                                .filter(|line| !line.is_empty())
                                .map(parse_log_line)
                                .collect();

                            if !parsed.is_empty() {
                                let mut store = entries.write().await;
                                store.append(&mut parsed);
                            }

                            offset = bytes.len() as u64;
                        }
                    }
                    Err(e) => {
                        tracing::debug!(error = %e, "Failed to read console log file");
                    }
                }

                tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;
            }
        });
    }

    /// Get all collected log entries.
    pub async fn stream_all(&self) -> Vec<LogEntry> {
        self.entries.read().await.clone()
    }

    /// Get log entries filtered by stream type.
    pub async fn stream_filtered(&self, stream: LogStream) -> Vec<LogEntry> {
        self.entries
            .read()
            .await
            .iter()
            .filter(|entry| entry.stream == stream)
            .cloned()
            .collect()
    }
}

impl Default for LogCollector {
    fn default() -> Self {
        Self::new(None)
    }
}

/// Parse a log line into a `LogEntry`, categorizing by stream prefix.
fn parse_log_line(line: &str) -> LogEntry {
    let (stream, message) = if let Some(msg) = line.strip_prefix("[runtime] ") {
        (LogStream::Runtime, msg.to_string())
    } else if let Some(msg) = line.strip_prefix("[agent] ") {
        (LogStream::Agent, msg.to_string())
    } else if let Some(msg) = line.strip_prefix("[tools] ") {
        (LogStream::Tools, msg.to_string())
    } else {
        (LogStream::Runtime, line.to_string())
    };

    let level = detect_log_level(&message);

    LogEntry {
        stream,
        timestamp: chrono::Utc::now(),
        level,
        message,
    }
}

/// Detect log level from message content.
fn detect_log_level(message: &str) -> LogLevel {
    let lower = message.to_lowercase();
    if lower.contains("error") || lower.contains("fatal") || lower.contains("panic") {
        LogLevel::Error
    } else if lower.contains("warn") {
        LogLevel::Warn
    } else if lower.contains("debug") || lower.contains("trace") {
        LogLevel::Debug
    } else {
        LogLevel::Info
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_runtime_log_line() {
        let entry = parse_log_line("[runtime] Starting VM...");
        assert_eq!(entry.stream, LogStream::Runtime);
        assert_eq!(entry.message, "Starting VM...");
    }

    #[test]
    fn test_parse_agent_log_line() {
        let entry = parse_log_line("[agent] Loaded model successfully");
        assert_eq!(entry.stream, LogStream::Agent);
        assert_eq!(entry.message, "Loaded model successfully");
    }

    #[test]
    fn test_parse_tools_log_line() {
        let entry = parse_log_line("[tools] Executing read_file");
        assert_eq!(entry.stream, LogStream::Tools);
        assert_eq!(entry.message, "Executing read_file");
    }

    #[test]
    fn test_parse_unprefixed_defaults_to_runtime() {
        let entry = parse_log_line("Some generic message");
        assert_eq!(entry.stream, LogStream::Runtime);
        assert_eq!(entry.message, "Some generic message");
    }

    #[test]
    fn test_detect_error_level() {
        let level = detect_log_level("Error: connection refused");
        assert!(matches!(level, LogLevel::Error));
    }

    #[test]
    fn test_detect_warn_level() {
        let level = detect_log_level("Warning: low memory");
        assert!(matches!(level, LogLevel::Warn));
    }

    #[test]
    fn test_detect_debug_level() {
        let level = detect_log_level("debug: entering function");
        assert!(matches!(level, LogLevel::Debug));
    }

    #[test]
    fn test_detect_info_level_default() {
        let level = detect_log_level("Server started on port 8080");
        assert!(matches!(level, LogLevel::Info));
    }

    #[tokio::test]
    async fn test_stream_all_empty() {
        let collector = LogCollector::new(None);
        let entries = collector.stream_all().await;
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn test_stream_filtered_empty() {
        let collector = LogCollector::new(None);
        let entries = collector.stream_filtered(LogStream::Agent).await;
        assert!(entries.is_empty());
    }

    #[tokio::test]
    async fn test_stream_filtered_returns_matching() {
        let collector = LogCollector::new(None);

        // Manually populate entries
        {
            let mut entries = collector.entries.write().await;
            entries.push(parse_log_line("[agent] hello"));
            entries.push(parse_log_line("[runtime] world"));
            entries.push(parse_log_line("[agent] foo"));
        }

        let agent_entries = collector.stream_filtered(LogStream::Agent).await;
        assert_eq!(agent_entries.len(), 2);
        assert_eq!(agent_entries[0].message, "hello");
        assert_eq!(agent_entries[1].message, "foo");

        let runtime_entries = collector.stream_filtered(LogStream::Runtime).await;
        assert_eq!(runtime_entries.len(), 1);
    }
}
