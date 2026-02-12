//! Logging driver types and configuration.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Logging driver type.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum LogDriver {
    /// Docker-compatible JSON lines format (default).
    JsonFile,
    /// Disable logging entirely.
    None,
}

impl Default for LogDriver {
    fn default() -> Self {
        Self::JsonFile
    }
}

impl std::fmt::Display for LogDriver {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::JsonFile => write!(f, "json-file"),
            Self::None => write!(f, "none"),
        }
    }
}

impl std::str::FromStr for LogDriver {
    type Err = String;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        match s {
            "json-file" => Ok(Self::JsonFile),
            "none" => Ok(Self::None),
            _ => Err(format!("unknown log driver: '{}' (supported: json-file, none)", s)),
        }
    }
}

/// Logging configuration for a box.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct LogConfig {
    pub driver: LogDriver,
    #[serde(default)]
    pub options: HashMap<String, String>,
}

impl Default for LogConfig {
    fn default() -> Self {
        Self {
            driver: LogDriver::JsonFile,
            options: HashMap::new(),
        }
    }
}

impl LogConfig {
    /// Maximum log file size in bytes before rotation.
    /// Default: 10 MiB. Set via `max-size` option (e.g., "10m", "1g").
    pub fn max_size(&self) -> u64 {
        self.options
            .get("max-size")
            .and_then(|s| parse_size(s).ok())
            .unwrap_or(10 * 1024 * 1024)
    }

    /// Maximum number of rotated log files to keep.
    /// Default: 3. Set via `max-file` option.
    pub fn max_file(&self) -> u32 {
        self.options
            .get("max-file")
            .and_then(|s| s.parse().ok())
            .unwrap_or(3)
    }
}

/// A single structured log entry (Docker-compatible JSON format).
#[derive(Debug, Serialize, Deserialize)]
pub struct LogEntry {
    /// The log message (including trailing newline).
    pub log: String,
    /// The output stream: "stdout" or "stderr".
    pub stream: String,
    /// RFC 3339 timestamp with nanosecond precision.
    pub time: String,
}

/// Parse a human-readable size string (e.g., "10m", "1g", "4096") into bytes.
fn parse_size(s: &str) -> std::result::Result<u64, String> {
    let s = s.trim().to_lowercase();
    if let Ok(n) = s.parse::<u64>() {
        return Ok(n);
    }
    let (num, mult) = if s.ends_with("gb") || s.ends_with('g') {
        (s.trim_end_matches("gb").trim_end_matches('g'), 1024u64 * 1024 * 1024)
    } else if s.ends_with("mb") || s.ends_with('m') {
        (s.trim_end_matches("mb").trim_end_matches('m'), 1024u64 * 1024)
    } else if s.ends_with("kb") || s.ends_with('k') {
        (s.trim_end_matches("kb").trim_end_matches('k'), 1024u64)
    } else if s.ends_with('b') {
        (s.trim_end_matches('b'), 1u64)
    } else {
        return Err(format!("unrecognized size format: {s}"));
    };
    let n: u64 = num.parse().map_err(|_| format!("invalid number: {num}"))?;
    Ok(n * mult)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_log_driver_from_str() {
        assert_eq!("json-file".parse::<LogDriver>().unwrap(), LogDriver::JsonFile);
        assert_eq!("none".parse::<LogDriver>().unwrap(), LogDriver::None);
        assert!("unknown".parse::<LogDriver>().is_err());
    }

    #[test]
    fn test_log_config_defaults() {
        let config = LogConfig::default();
        assert_eq!(config.driver, LogDriver::JsonFile);
        assert_eq!(config.max_size(), 10 * 1024 * 1024);
        assert_eq!(config.max_file(), 3);
    }

    #[test]
    fn test_log_config_custom_options() {
        let mut config = LogConfig::default();
        config.options.insert("max-size".to_string(), "50m".to_string());
        config.options.insert("max-file".to_string(), "5".to_string());
        assert_eq!(config.max_size(), 50 * 1024 * 1024);
        assert_eq!(config.max_file(), 5);
    }

    #[test]
    fn test_parse_size() {
        assert_eq!(parse_size("1024").unwrap(), 1024);
        assert_eq!(parse_size("10m").unwrap(), 10 * 1024 * 1024);
        assert_eq!(parse_size("1g").unwrap(), 1024 * 1024 * 1024);
        assert_eq!(parse_size("512k").unwrap(), 512 * 1024);
        assert!(parse_size("abc").is_err());
    }

    #[test]
    fn test_log_entry_serialization() {
        let entry = LogEntry {
            log: "hello\n".to_string(),
            stream: "stdout".to_string(),
            time: "2026-02-12T06:00:00.000000000Z".to_string(),
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("\"log\":\"hello\\n\""));
        assert!(json.contains("\"stream\":\"stdout\""));
    }
}
