//! Canonical persisted metadata schema for local box executions.

use std::collections::HashMap;
use std::path::PathBuf;

use a3s_box_core::config::ResourceLimits;
use a3s_box_core::log::LogConfig;
use a3s_box_core::{ExecutionIsolation, NetworkMode};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Metadata record for a single local box execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BoxRecord {
    /// Full UUID.
    pub id: String,
    /// First 12 hex characters of the UUID, without dashes.
    pub short_id: String,
    /// User-assigned or generated name.
    pub name: String,
    /// OCI image reference.
    pub image: String,
    /// Requested execution isolation. Records written before this field default to MicroVM.
    #[serde(default)]
    pub isolation: ExecutionIsolation,
    /// Persisted lifecycle state.
    pub status: String,
    /// Shim process PID while the execution is active.
    pub pid: Option<u32>,
    /// Start-time identity token used to reject a reused PID.
    #[serde(default)]
    pub pid_start_time: Option<u64>,
    /// Number of virtual CPUs.
    pub cpus: u32,
    /// Memory in MiB.
    pub memory_mb: u32,
    /// Volume mounts encoded as host-to-guest pairs.
    pub volumes: Vec<String>,
    /// virtio-fs cache mode for host directory volumes.
    #[serde(default)]
    pub virtiofs_cache: Option<String>,
    /// Environment variables.
    pub env: HashMap<String, String>,
    /// Command override.
    pub cmd: Vec<String>,
    /// Entrypoint override.
    #[serde(default)]
    pub entrypoint: Option<Vec<String>>,
    /// Host-side execution directory.
    pub box_dir: PathBuf,
    /// Path to the exec socket.
    #[serde(default)]
    pub exec_socket_path: PathBuf,
    /// Path to the console log.
    pub console_log: PathBuf,
    /// Creation timestamp.
    pub created_at: DateTime<Utc>,
    /// Start timestamp.
    pub started_at: Option<DateTime<Utc>>,
    /// Whether the execution is removed automatically after it stops.
    pub auto_remove: bool,
    /// Custom hostname.
    #[serde(default)]
    pub hostname: Option<String>,
    /// User inside the workload.
    #[serde(default)]
    pub user: Option<String>,
    /// Working directory inside the workload.
    #[serde(default)]
    pub workdir: Option<String>,
    /// Restart policy.
    #[serde(default = "default_restart_policy")]
    pub restart_policy: String,
    /// Port mappings.
    #[serde(default)]
    pub port_map: Vec<String>,
    /// User-defined labels.
    #[serde(default)]
    pub labels: HashMap<String, String>,
    /// Whether the execution was explicitly stopped by a user.
    #[serde(default)]
    pub stopped_by_user: bool,
    /// Automatic restart count.
    #[serde(default)]
    pub restart_count: u32,
    /// Maximum restart count for a bounded on-failure policy.
    #[serde(default)]
    pub max_restart_count: u32,
    /// Last captured exit code.
    #[serde(default)]
    pub exit_code: Option<i32>,
    /// Health-check configuration.
    #[serde(default)]
    pub health_check: Option<HealthCheck>,
    /// Whether an image-defined health check was disabled explicitly.
    #[serde(default)]
    pub healthcheck_disabled: bool,
    /// Current health state.
    #[serde(default = "default_health_status")]
    pub health_status: String,
    /// Consecutive health-check failures.
    #[serde(default)]
    pub health_retries: u32,
    /// Timestamp of the most recent health check.
    #[serde(default)]
    pub health_last_check: Option<DateTime<Utc>>,
    /// Network mode.
    #[serde(default)]
    pub network_mode: NetworkMode,
    /// Attached bridge network name.
    #[serde(default)]
    pub network_name: Option<String>,
    /// Attached named volumes.
    #[serde(default)]
    pub volume_names: Vec<String>,
    /// tmpfs mounts.
    #[serde(default)]
    pub tmpfs: Vec<String>,
    /// Anonymous volumes materialized from OCI declarations.
    #[serde(default)]
    pub anonymous_volumes: Vec<String>,
    /// Host resource controls.
    #[serde(default)]
    pub resource_limits: ResourceLimits,
    /// Logging configuration.
    #[serde(default)]
    pub log_config: LogConfig,
    /// Custom host-to-IP mappings.
    #[serde(default)]
    pub add_host: Vec<String>,
    /// Target OCI platform.
    #[serde(default)]
    pub platform: Option<String>,
    /// Whether to run an init process as PID 1.
    #[serde(default)]
    pub init: bool,
    /// Whether the root filesystem is read-only.
    #[serde(default)]
    pub read_only: bool,
    /// Added Linux capabilities.
    #[serde(default)]
    pub cap_add: Vec<String>,
    /// Dropped Linux capabilities.
    #[serde(default)]
    pub cap_drop: Vec<String>,
    /// OCI security options.
    #[serde(default)]
    pub security_opt: Vec<String>,
    /// Whether extended privileges are enabled.
    #[serde(default)]
    pub privileged: bool,
    /// Device mappings.
    #[serde(default)]
    pub devices: Vec<String>,
    /// GPU selection.
    #[serde(default)]
    pub gpus: Option<String>,
    /// Shared-memory size in bytes.
    #[serde(default)]
    pub shm_size: Option<u64>,
    /// Signal used for graceful stop.
    #[serde(default)]
    pub stop_signal: Option<String>,
    /// Graceful stop timeout in seconds.
    #[serde(default)]
    pub stop_timeout: Option<u64>,
    /// Whether the OOM killer is disabled.
    #[serde(default)]
    pub oom_kill_disable: bool,
    /// Host OOM score adjustment.
    #[serde(default)]
    pub oom_score_adj: Option<i32>,
}

impl BoxRecord {
    /// Generate the stable short ID used by local CLI and SDK lookup.
    pub fn make_short_id(id: &str) -> String {
        id.replace('-', "").chars().take(12).collect()
    }

    /// Whether the persisted lifecycle state represents an active execution.
    pub fn is_active(&self) -> bool {
        matches!(self.status.as_str(), "running" | "paused")
    }

    /// Render a concise lifecycle status with health, exit, and restart annotations.
    pub fn status_summary(&self) -> String {
        let mut annotations = Vec::new();
        if self.is_active() && self.health_check.is_some() && self.health_status != "none" {
            annotations.push(self.health_status.clone());
        }
        if matches!(self.status.as_str(), "stopped" | "dead") {
            if let Some(exit_code) = self.exit_code {
                annotations.push(format!("Exit {exit_code}"));
            }
        }
        if self.restart_count > 0 {
            annotations.push(format!("Restarts: {}", self.restart_count));
        }
        if annotations.is_empty() {
            self.status.clone()
        } else {
            format!("{} ({})", self.status, annotations.join(", "))
        }
    }
}

/// Health-check configuration persisted with a box record.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HealthCheck {
    /// Command executed by the health check.
    pub cmd: Vec<String>,
    /// Interval between checks in seconds.
    #[serde(default = "default_health_interval")]
    pub interval_secs: u64,
    /// Per-check timeout in seconds.
    #[serde(default = "default_health_timeout")]
    pub timeout_secs: u64,
    /// Consecutive failures before the execution is unhealthy.
    #[serde(default = "default_health_retries")]
    pub retries: u32,
    /// Grace period after startup in seconds.
    #[serde(default)]
    pub start_period_secs: u64,
}

fn default_restart_policy() -> String {
    "no".to_string()
}

fn default_health_status() -> String {
    "none".to_string()
}

fn default_health_interval() -> u64 {
    30
}

fn default_health_timeout() -> u64 {
    5
}

fn default_health_retries() -> u32 {
    3
}

#[cfg(test)]
mod tests {
    use super::*;

    fn minimal_record() -> serde_json::Value {
        serde_json::json!({
            "id": "11111111-1111-4111-8111-111111111111",
            "short_id": "111111111111",
            "name": "fixture",
            "image": "alpine:latest",
            "status": "created",
            "pid": null,
            "cpus": 1,
            "memory_mb": 128,
            "volumes": [],
            "env": {},
            "cmd": ["sh"],
            "box_dir": "/tmp/fixture",
            "console_log": "/tmp/fixture/console.log",
            "created_at": "2026-07-14T12:00:00Z",
            "started_at": null,
            "auto_remove": false
        })
    }

    #[test]
    fn legacy_records_default_without_losing_runtime_fields() {
        let mut value = minimal_record();
        value["virtiofs_cache"] = serde_json::json!("always");
        let record: BoxRecord = serde_json::from_value(value).unwrap();

        assert_eq!(record.isolation, ExecutionIsolation::Microvm);
        assert_eq!(record.virtiofs_cache.as_deref(), Some("always"));
        assert_eq!(record.restart_policy, "no");
        assert_eq!(record.health_status, "none");
        assert_eq!(
            serde_json::to_value(record).unwrap()["virtiofs_cache"],
            "always"
        );
    }
}
