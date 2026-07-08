//! SDK-local reader/writer for the shared `boxes.json` state format.
//!
//! The state file is still the source of truth for container metadata, but the
//! SDK must not depend on the CLI crate just to read it. Keep this module
//! format-compatible with the CLI state schema.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

/// Persistent state file backed by JSON.
pub(crate) struct StateFile {
    path: PathBuf,
    records: Vec<BoxRecord>,
}

impl StateFile {
    /// Load state from disk. Creates an empty state if the file does not exist.
    pub(crate) fn load(path: &Path) -> std::io::Result<Self> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            return Ok(Self {
                path: path.to_path_buf(),
                records: Vec::new(),
            });
        }

        let data = std::fs::read_to_string(path)?;
        let records = serde_json::from_str::<Vec<BoxRecord>>(&data)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;

        Ok(Self {
            path: path.to_path_buf(),
            records,
        })
    }

    /// Save state atomically under the same advisory lock used by the CLI.
    pub(crate) fn save(&self) -> std::io::Result<()> {
        let _lock = StateLock::acquire(&self.path)?;
        self.write_to_disk()
    }

    /// Atomically apply a synchronous mutation to state under the state lock.
    pub(crate) fn modify<R>(
        path: &Path,
        f: impl FnOnce(&mut StateFile) -> std::io::Result<R>,
    ) -> std::io::Result<R> {
        let _lock = StateLock::acquire(path)?;
        let mut state = Self::load_unlocked(path)?;
        let output = f(&mut state)?;
        state.write_to_disk()?;
        Ok(output)
    }

    fn load_unlocked(path: &Path) -> std::io::Result<Self> {
        if !path.exists() {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            return Ok(Self {
                path: path.to_path_buf(),
                records: Vec::new(),
            });
        }

        let data = std::fs::read_to_string(path)?;
        let records = serde_json::from_str::<Vec<BoxRecord>>(&data)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;

        Ok(Self {
            path: path.to_path_buf(),
            records,
        })
    }

    fn write_to_disk(&self) -> std::io::Result<()> {
        if let Some(parent) = self.path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let data = serde_json::to_vec_pretty(&self.records).map_err(std::io::Error::other)?;
        let tmp_path = self.path.with_extension("json.tmp");
        a3s_box_core::fs_atomic::write_durable(&tmp_path, &self.path, &data)
    }

    pub(crate) fn find_by_id(&self, id: &str) -> Option<&BoxRecord> {
        self.records.iter().find(|record| record.id == id)
    }

    pub(crate) fn find_by_id_mut(&mut self, id: &str) -> Option<&mut BoxRecord> {
        self.records.iter_mut().find(|record| record.id == id)
    }

    pub(crate) fn remove_by_id(&mut self, id: &str) -> bool {
        let before = self.records.len();
        self.records.retain(|record| record.id != id);
        self.records.len() < before
    }

    pub(crate) fn find_by_name(&self, name: &str) -> Option<&BoxRecord> {
        self.records.iter().find(|record| record.name == name)
    }

    pub(crate) fn records_mut(&mut self) -> &mut Vec<BoxRecord> {
        &mut self.records
    }

    pub(crate) fn find_by_id_prefix(&self, prefix: &str) -> Vec<&BoxRecord> {
        self.records
            .iter()
            .filter(|record| record.id.starts_with(prefix) || record.short_id.starts_with(prefix))
            .collect()
    }

    pub(crate) fn list(&self, all: bool) -> Vec<&BoxRecord> {
        self.records
            .iter()
            .filter(|record| all || record.status == "running")
            .collect()
    }
}

struct StateLock {
    #[cfg(unix)]
    _file: std::fs::File,
}

impl StateLock {
    #[cfg(unix)]
    fn acquire(state_path: &Path) -> std::io::Result<Self> {
        use std::os::unix::io::AsRawFd;

        let lock_path = state_path
            .parent()
            .map(|parent| parent.join("boxes.json.lock"))
            .unwrap_or_else(|| PathBuf::from("boxes.json.lock"));
        if let Some(parent) = lock_path.parent() {
            std::fs::create_dir_all(parent)?;
        }

        let file = std::fs::OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(lock_path)?;

        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX) } != 0 {
            return Err(std::io::Error::last_os_error());
        }

        Ok(Self { _file: file })
    }

    #[cfg(not(unix))]
    fn acquire(_state_path: &Path) -> std::io::Result<Self> {
        Ok(Self {})
    }
}

/// Metadata record for a single box instance.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct BoxRecord {
    /// Full UUID.
    pub(crate) id: String,
    /// First 12 hex chars of the UUID, without dashes.
    pub(crate) short_id: String,
    /// User-assigned or auto-generated name.
    pub(crate) name: String,
    /// OCI image reference.
    pub(crate) image: String,
    /// "created" | "running" | "paused" | "stopped" | "dead".
    pub(crate) status: String,
    /// Shim process PID, when running.
    pub(crate) pid: Option<u32>,
    /// Start-time identity token captured when `pid` was recorded.
    #[serde(default)]
    pub(crate) pid_start_time: Option<u64>,
    /// Number of vCPUs.
    pub(crate) cpus: u32,
    /// Memory in MB.
    pub(crate) memory_mb: u32,
    /// Volume mounts ("host:guest" pairs).
    pub(crate) volumes: Vec<String>,
    /// Environment variables.
    pub(crate) env: HashMap<String, String>,
    /// Command override.
    pub(crate) cmd: Vec<String>,
    /// Entrypoint override.
    #[serde(default)]
    pub(crate) entrypoint: Option<Vec<String>>,
    /// Box working directory.
    pub(crate) box_dir: PathBuf,
    /// Path to exec socket.
    #[serde(default)]
    pub(crate) exec_socket_path: PathBuf,
    /// Path to console log.
    pub(crate) console_log: PathBuf,
    /// Creation timestamp.
    pub(crate) created_at: DateTime<Utc>,
    /// Start timestamp.
    pub(crate) started_at: Option<DateTime<Utc>>,
    /// Whether to auto-remove on stop.
    pub(crate) auto_remove: bool,
    /// Custom hostname for the box.
    #[serde(default)]
    pub(crate) hostname: Option<String>,
    /// User to run as inside the box.
    #[serde(default)]
    pub(crate) user: Option<String>,
    /// Working directory inside the box.
    #[serde(default)]
    pub(crate) workdir: Option<String>,
    /// Restart policy.
    #[serde(default = "default_restart_policy")]
    pub(crate) restart_policy: String,
    /// Port mappings ("host_port:guest_port" pairs).
    #[serde(default)]
    pub(crate) port_map: Vec<String>,
    /// User-defined labels.
    #[serde(default)]
    pub(crate) labels: HashMap<String, String>,
    /// Whether the box was explicitly stopped by the user.
    #[serde(default)]
    pub(crate) stopped_by_user: bool,
    /// Number of automatic restarts performed.
    #[serde(default)]
    pub(crate) restart_count: u32,
    /// Maximum restart count for "on-failure:N" policy.
    #[serde(default)]
    pub(crate) max_restart_count: u32,
    /// Exit code from the last run.
    #[serde(default)]
    pub(crate) exit_code: Option<i32>,
    /// Health check configuration.
    #[serde(default)]
    pub(crate) health_check: Option<HealthCheck>,
    /// Whether image-defined health checks were explicitly disabled.
    #[serde(default)]
    pub(crate) healthcheck_disabled: bool,
    /// Current health status.
    #[serde(default = "default_health_status")]
    pub(crate) health_status: String,
    /// Consecutive health check failures.
    #[serde(default)]
    pub(crate) health_retries: u32,
    /// Timestamp of last health check.
    #[serde(default)]
    pub(crate) health_last_check: Option<DateTime<Utc>>,
    /// Network mode for this box.
    #[serde(default)]
    pub(crate) network_mode: a3s_box_core::NetworkMode,
    /// Network name, if connected to a bridge network.
    #[serde(default)]
    pub(crate) network_name: Option<String>,
    /// Named volumes attached to this box.
    #[serde(default)]
    pub(crate) volume_names: Vec<String>,
    /// tmpfs mounts for this box.
    #[serde(default)]
    pub(crate) tmpfs: Vec<String>,
    /// Anonymous volumes auto-created from OCI VOLUME directives.
    #[serde(default)]
    pub(crate) anonymous_volumes: Vec<String>,
    /// Resource limits.
    #[serde(default)]
    pub(crate) resource_limits: a3s_box_core::config::ResourceLimits,
    /// Logging configuration.
    #[serde(default)]
    pub(crate) log_config: a3s_box_core::log::LogConfig,
    /// Custom host-to-IP mappings.
    #[serde(default)]
    pub(crate) add_host: Vec<String>,
    /// Target platform.
    #[serde(default)]
    pub(crate) platform: Option<String>,
    /// Use init process as PID 1.
    #[serde(default)]
    pub(crate) init: bool,
    /// Read-only root filesystem.
    #[serde(default)]
    pub(crate) read_only: bool,
    /// Added Linux capabilities.
    #[serde(default)]
    pub(crate) cap_add: Vec<String>,
    /// Dropped Linux capabilities.
    #[serde(default)]
    pub(crate) cap_drop: Vec<String>,
    /// Security options.
    #[serde(default)]
    pub(crate) security_opt: Vec<String>,
    /// Extended privileges.
    #[serde(default)]
    pub(crate) privileged: bool,
    /// Device mappings.
    #[serde(default)]
    pub(crate) devices: Vec<String>,
    /// GPU devices.
    #[serde(default)]
    pub(crate) gpus: Option<String>,
    /// Shared memory size in bytes.
    #[serde(default)]
    pub(crate) shm_size: Option<u64>,
    /// Signal to stop the box.
    #[serde(default)]
    pub(crate) stop_signal: Option<String>,
    /// Timeout to stop the box before killing.
    #[serde(default)]
    pub(crate) stop_timeout: Option<u64>,
    /// OOM killer disabled.
    #[serde(default)]
    pub(crate) oom_kill_disable: bool,
    /// OOM score adjustment.
    #[serde(default)]
    pub(crate) oom_score_adj: Option<i32>,
}

impl BoxRecord {
    pub(crate) fn make_short_id(id: &str) -> String {
        id.replace('-', "").chars().take(12).collect()
    }

    pub(crate) fn is_active(&self) -> bool {
        matches!(self.status.as_str(), "running" | "paused")
    }

    pub(crate) fn status_summary(&self) -> String {
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

/// Health check configuration for a box.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct HealthCheck {
    /// Command to run for health check.
    pub(crate) cmd: Vec<String>,
    /// Check interval in seconds.
    #[serde(default = "default_health_interval")]
    pub(crate) interval_secs: u64,
    /// Per-check timeout in seconds.
    #[serde(default = "default_health_timeout")]
    pub(crate) timeout_secs: u64,
    /// Consecutive failures before marking unhealthy.
    #[serde(default = "default_health_retries")]
    pub(crate) retries: u32,
    /// Grace period after start before checks begin.
    #[serde(default)]
    pub(crate) start_period_secs: u64,
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
