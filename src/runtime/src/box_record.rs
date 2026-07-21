//! Canonical persisted metadata schema for local box executions.

use std::collections::HashMap;
use std::path::PathBuf;

use a3s_box_core::config::ResourceLimits;
use a3s_box_core::log::LogConfig;
use a3s_box_core::{
    CreateExecutionRequest, ExecutionGeneration, ExecutionIsolation, ExecutionSnapshotId,
    NetworkMode, OperationId, ResolvedExecutionPlan,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

pub use a3s_box_core::ExecutionHealthCheck as HealthCheck;

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
    /// Runtime lifecycle identity and recoverable creation intent.
    ///
    /// Legacy CLI-created records omit this field. Managed executions persist
    /// it before launch so an operation can be reconciled after a service
    /// restart without creating a second execution.
    #[serde(default)]
    pub managed_execution: Option<ManagedExecutionMetadata>,
    /// Persisted lifecycle state.
    ///
    /// Legacy records use `created`, `running`, `paused`, `stopped`, and
    /// `dead`. Managed executions additionally use the durable transition
    /// states defined by [`ManagedExecutionState`].
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
        if self.managed_execution.is_some() {
            return self
                .managed_state()
                .is_ok_and(|state| state.is_some_and(ManagedExecutionState::keeps_resources));
        }
        matches!(self.status.as_str(), "running" | "paused")
    }

    /// Parse the lifecycle state of a managed execution.
    ///
    /// Legacy records return `None`. Unknown managed states fail closed so a
    /// runtime service cannot operate on a record written by incompatible
    /// code.
    pub fn managed_state(&self) -> a3s_box_core::Result<Option<ManagedExecutionState>> {
        let Some(metadata) = self.managed_execution.as_ref() else {
            return Ok(None);
        };
        let state = ManagedExecutionState::from_status(&self.status)?;
        validate_pending_operation(state, metadata)?;
        Ok(Some(state))
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

/// Durable lifecycle state for an execution owned by `ExecutionManager`.
///
/// Transitional states are persisted before backend side effects. This lets
/// a restarted manager distinguish work that was never claimed from work that
/// may already have reached the runtime.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedExecutionState {
    Creating,
    Created,
    Starting,
    Running,
    Pausing,
    Paused,
    Resuming,
    Snapshotting,
    Killing,
    RestartStopping,
    RestartStarting,
    Removing,
    Stopped,
    Failed,
}

impl ManagedExecutionState {
    /// Canonical value written to [`BoxRecord::status`].
    pub const fn as_status(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Created => "created",
            Self::Starting => "starting",
            Self::Running => "running",
            Self::Pausing => "pausing",
            Self::Paused => "paused",
            Self::Resuming => "resuming",
            Self::Snapshotting => "snapshotting",
            Self::Killing => "killing",
            Self::RestartStopping => "restart_stopping",
            Self::RestartStarting => "restart_starting",
            Self::Removing => "removing",
            Self::Stopped => "stopped",
            Self::Failed => "failed",
        }
    }

    /// Parse a persisted managed lifecycle state.
    pub fn from_status(status: &str) -> a3s_box_core::Result<Self> {
        match status {
            "creating" => Ok(Self::Creating),
            "created" => Ok(Self::Created),
            "starting" => Ok(Self::Starting),
            "running" => Ok(Self::Running),
            "pausing" => Ok(Self::Pausing),
            "paused" => Ok(Self::Paused),
            "resuming" => Ok(Self::Resuming),
            "snapshotting" => Ok(Self::Snapshotting),
            "killing" => Ok(Self::Killing),
            "restart_stopping" => Ok(Self::RestartStopping),
            "restart_starting" => Ok(Self::RestartStarting),
            "removing" => Ok(Self::Removing),
            "stopped" => Ok(Self::Stopped),
            "dead" | "failed" => Ok(Self::Failed),
            other => Err(a3s_box_core::BoxError::StateError(format!(
                "unknown managed execution state: {other}"
            ))),
        }
    }

    /// Whether host resources may still belong to this execution.
    pub const fn keeps_resources(self) -> bool {
        !matches!(
            self,
            Self::Creating | Self::Created | Self::Stopped | Self::Failed
        )
    }

    /// Whether no further lifecycle operation can revive this execution.
    pub const fn is_terminal(self) -> bool {
        matches!(self, Self::Stopped | Self::Failed)
    }
}

impl std::fmt::Display for ManagedExecutionState {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_status())
    }
}

/// Durable lifecycle metadata for an execution owned by [`ExecutionManager`].
///
/// [`ExecutionManager`]: a3s_box_core::ExecutionManager
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ManagedExecutionMetadata {
    /// Idempotency key of the create operation.
    pub operation_id: OperationId,
    /// Runtime generation used to reject stale lifecycle requests.
    pub generation: ExecutionGeneration,
    /// Full creation intent required to recover an interrupted launch.
    pub request: CreateExecutionRequest,
    /// Backend resolution validated before any launch side effects.
    pub plan: ResolvedExecutionPlan,
    /// Lifecycle side effect claimed before calling the backend.
    #[serde(default)]
    pub pending_operation: Option<ManagedExecutionOperation>,
    /// Most recent completed restart retained for idempotent response replay.
    #[serde(default)]
    pub last_restart: Option<ManagedRestartCompletion>,
    /// Provider terminal timestamp retained for deterministic observation replay.
    #[serde(default)]
    pub finished_at: Option<DateTime<Utc>>,
    /// Whether a paused execution still owns a live, memory-preserved runtime.
    ///
    /// Records written before filesystem-only pause support always represented
    /// warm pauses, so the backwards-compatible default is `true`.
    #[serde(default = "default_paused_with_memory")]
    pub paused_with_memory: bool,
}

/// Recoverable backend operation associated with a transitional state.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ManagedExecutionOperation {
    Start,
    Pause {
        keep_memory: bool,
    },
    Resume,
    Snapshot {
        snapshot_id: ExecutionSnapshotId,
        source_state: ManagedExecutionState,
    },
    Kill {
        #[serde(default)]
        signal: Option<i32>,
        #[serde(default)]
        timeout_secs: Option<u64>,
    },
    Remove,
    Restart {
        operation_id: OperationId,
        source_generation: ExecutionGeneration,
        source_state: ManagedExecutionState,
        #[serde(default)]
        stop_timeout_secs: Option<u64>,
    },
}

/// Durable result of the most recent restart operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ManagedRestartOutcome {
    Running,
    Failed,
}

/// Restart identity retained after its transitional state has completed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ManagedRestartCompletion {
    pub operation_id: OperationId,
    pub source_generation: ExecutionGeneration,
    pub target_generation: ExecutionGeneration,
    pub outcome: ManagedRestartOutcome,
    #[serde(default)]
    pub stop_timeout_secs: Option<u64>,
}

impl ManagedExecutionMetadata {
    /// Build validated recovery metadata from one creation request.
    pub fn new(
        operation_id: OperationId,
        generation: ExecutionGeneration,
        request: CreateExecutionRequest,
    ) -> a3s_box_core::Result<Self> {
        if request.external_sandbox_id.trim().is_empty() {
            return Err(a3s_box_core::BoxError::ConfigError(
                "external sandbox ID cannot be empty".to_string(),
            ));
        }
        let plan = a3s_box_core::resolve_execution(&request.config)?;
        Ok(Self {
            operation_id,
            generation,
            request,
            plan,
            pending_operation: None,
            last_restart: None,
            finished_at: None,
            paused_with_memory: true,
        })
    }

    /// Validate deserialized metadata before it participates in reconciliation.
    pub fn validate(&self) -> a3s_box_core::Result<()> {
        if self.request.external_sandbox_id.trim().is_empty() {
            return Err(a3s_box_core::BoxError::StateError(
                "managed execution has an empty external sandbox ID".to_string(),
            ));
        }
        let resolved = a3s_box_core::resolve_execution(&self.request.config)?;
        if resolved != self.plan {
            return Err(a3s_box_core::BoxError::StateError(
                "managed execution plan does not match its persisted creation request".to_string(),
            ));
        }
        if let Some(completed) = &self.last_restart {
            let expected_target = next_generation(completed.source_generation)?;
            if completed.target_generation != expected_target {
                return Err(a3s_box_core::BoxError::StateError(format!(
                    "completed restart {} has inconsistent generations",
                    completed.operation_id
                )));
            }
            validate_stop_timeout(completed.stop_timeout_secs)?;
        }
        Ok(())
    }
}

fn validate_pending_operation(
    state: ManagedExecutionState,
    metadata: &ManagedExecutionMetadata,
) -> a3s_box_core::Result<()> {
    let operation = metadata.pending_operation.as_ref();
    let consistent = matches!(
        (state, operation),
        (
            ManagedExecutionState::Starting,
            Some(ManagedExecutionOperation::Start)
        ) | (
            ManagedExecutionState::Pausing,
            Some(ManagedExecutionOperation::Pause { .. })
        ) | (
            ManagedExecutionState::Resuming,
            Some(ManagedExecutionOperation::Resume)
        ) | (
            ManagedExecutionState::Snapshotting,
            Some(ManagedExecutionOperation::Snapshot { .. })
        ) | (
            ManagedExecutionState::Killing,
            Some(ManagedExecutionOperation::Kill { .. })
        ) | (
            ManagedExecutionState::Removing,
            Some(ManagedExecutionOperation::Remove)
        ) | (
            ManagedExecutionState::RestartStopping | ManagedExecutionState::RestartStarting,
            Some(ManagedExecutionOperation::Restart { .. })
        ) | (
            ManagedExecutionState::Creating
                | ManagedExecutionState::Created
                | ManagedExecutionState::Running
                | ManagedExecutionState::Paused
                | ManagedExecutionState::Stopped
                | ManagedExecutionState::Failed,
            None
        )
    );
    if !consistent {
        return Err(a3s_box_core::BoxError::StateError(format!(
            "managed execution state {state} has inconsistent pending operation"
        )));
    }

    if let Some(ManagedExecutionOperation::Restart {
        source_generation,
        source_state,
        stop_timeout_secs,
        ..
    }) = operation
    {
        if !matches!(
            source_state,
            ManagedExecutionState::Created
                | ManagedExecutionState::Running
                | ManagedExecutionState::Paused
                | ManagedExecutionState::Stopped
                | ManagedExecutionState::Failed
        ) {
            return Err(a3s_box_core::BoxError::StateError(
                "restart source state is not stable".to_string(),
            ));
        }
        let expected = match state {
            ManagedExecutionState::RestartStopping => *source_generation,
            ManagedExecutionState::RestartStarting => next_generation(*source_generation)?,
            _ => {
                return Err(a3s_box_core::BoxError::StateError(
                    "restart operation is attached to a non-restart state".to_string(),
                ))
            }
        };
        if metadata.generation != expected {
            return Err(a3s_box_core::BoxError::StateError(format!(
                "restart state {state} has generation {}, expected {}",
                metadata.generation.get(),
                expected.get()
            )));
        }
        validate_stop_timeout(*stop_timeout_secs)?;
    }
    if let Some(ManagedExecutionOperation::Snapshot { source_state, .. }) = operation {
        if state != ManagedExecutionState::Snapshotting
            || !matches!(
                source_state,
                ManagedExecutionState::Running | ManagedExecutionState::Paused
            )
        {
            return Err(a3s_box_core::BoxError::StateError(
                "snapshot operation has an invalid source state".to_string(),
            ));
        }
    }
    if let Some(ManagedExecutionOperation::Kill {
        signal,
        timeout_secs,
    }) = operation
    {
        if signal.is_some_and(|signal| signal <= 0) {
            return Err(a3s_box_core::BoxError::StateError(
                "kill signal must be positive".to_string(),
            ));
        }
        validate_stop_timeout(*timeout_secs)?;
    }
    if !metadata.paused_with_memory {
        let valid_cold_pause_state = match (state, operation) {
            (
                ManagedExecutionState::Pausing,
                Some(ManagedExecutionOperation::Pause { keep_memory }),
            ) => !keep_memory,
            (ManagedExecutionState::Paused | ManagedExecutionState::Resuming, _) => true,
            (
                ManagedExecutionState::Snapshotting,
                Some(ManagedExecutionOperation::Snapshot { source_state, .. }),
            ) => *source_state == ManagedExecutionState::Paused,
            // These transitions may be claimed from a cold-paused execution.
            (ManagedExecutionState::Killing | ManagedExecutionState::Removing, _) => true,
            (
                ManagedExecutionState::RestartStopping,
                Some(ManagedExecutionOperation::Restart { source_state, .. }),
            ) => *source_state == ManagedExecutionState::Paused,
            _ => false,
        };
        if !valid_cold_pause_state {
            return Err(a3s_box_core::BoxError::StateError(format!(
                "managed execution state {state} cannot retain a filesystem-only pause"
            )));
        }
    }
    Ok(())
}

fn validate_stop_timeout(timeout_secs: Option<u64>) -> a3s_box_core::Result<()> {
    if timeout_secs.is_some_and(|timeout| timeout.checked_mul(1_000).is_none()) {
        Err(a3s_box_core::BoxError::StateError(
            "managed stop timeout is too large".to_string(),
        ))
    } else {
        Ok(())
    }
}

fn next_generation(generation: ExecutionGeneration) -> a3s_box_core::Result<ExecutionGeneration> {
    let value = generation.get().checked_add(1).ok_or_else(|| {
        a3s_box_core::BoxError::StateError("execution generation is exhausted".to_string())
    })?;
    ExecutionGeneration::new(value).map_err(|error| {
        a3s_box_core::BoxError::StateError(format!("invalid execution generation: {error}"))
    })
}

fn default_restart_policy() -> String {
    "no".to_string()
}

fn default_health_status() -> String {
    "none".to_string()
}

const fn default_paused_with_memory() -> bool {
    true
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
        assert!(record.managed_execution.is_none());
        assert_eq!(record.virtiofs_cache.as_deref(), Some("always"));
        assert_eq!(record.restart_policy, "no");
        assert_eq!(record.health_status, "none");
        assert_eq!(
            serde_json::to_value(record).unwrap()["virtiofs_cache"],
            "always"
        );
    }

    #[test]
    fn managed_execution_metadata_round_trips_recovery_intent() {
        let mut config = a3s_box_core::BoxConfig {
            image: "alpine:latest".to_string(),
            isolation: ExecutionIsolation::Sandbox,
            ..Default::default()
        };
        config.resources.vcpus = 1;
        config.resources.memory_mb = 128;
        let metadata = ManagedExecutionMetadata::new(
            OperationId::new("create-op-1").unwrap(),
            ExecutionGeneration::INITIAL,
            CreateExecutionRequest {
                external_sandbox_id: "sandbox-1".to_string(),
                config,
                labels: Default::default(),
                policy: Default::default(),
                rootfs_snapshot_id: None,
            },
        )
        .unwrap();
        let mut value = minimal_record();
        value["managed_execution"] = serde_json::to_value(metadata).unwrap();
        value["managed_execution"]
            .as_object_mut()
            .unwrap()
            .remove("paused_with_memory");

        let record: BoxRecord = serde_json::from_value(value).unwrap();
        let encoded = serde_json::to_value(&record).unwrap();
        assert_eq!(
            record.managed_state().unwrap(),
            Some(ManagedExecutionState::Created)
        );
        assert!(!record.is_active());
        let managed = record.managed_execution.unwrap();

        assert_eq!(managed.operation_id.as_str(), "create-op-1");
        assert_eq!(managed.generation, ExecutionGeneration::INITIAL);
        assert_eq!(managed.request.external_sandbox_id, "sandbox-1");
        assert!(managed.paused_with_memory);
        assert_eq!(
            managed.request.config.isolation,
            ExecutionIsolation::Sandbox
        );
        assert_eq!(encoded["managed_execution"]["generation"], 1);
        assert_eq!(encoded["managed_execution"]["paused_with_memory"], true);
    }

    #[test]
    fn legacy_kill_operation_defaults_new_termination_options() {
        let operation: ManagedExecutionOperation =
            serde_json::from_value(serde_json::json!({ "kind": "kill" })).unwrap();

        assert_eq!(
            operation,
            ManagedExecutionOperation::Kill {
                signal: None,
                timeout_secs: None,
            }
        );
    }

    #[test]
    fn managed_execution_rejects_a_cold_pause_marker_in_running_state() {
        let config = a3s_box_core::BoxConfig {
            image: "alpine:latest".to_string(),
            isolation: ExecutionIsolation::Sandbox,
            ..Default::default()
        };
        let mut metadata = ManagedExecutionMetadata::new(
            OperationId::new("create-op-cold-invalid").unwrap(),
            ExecutionGeneration::INITIAL,
            CreateExecutionRequest {
                external_sandbox_id: "sandbox-cold-invalid".to_string(),
                config,
                labels: Default::default(),
                policy: Default::default(),
                rootfs_snapshot_id: None,
            },
        )
        .unwrap();
        metadata.paused_with_memory = false;
        let mut value = minimal_record();
        value["status"] = serde_json::json!("running");
        value["managed_execution"] = serde_json::to_value(metadata).unwrap();
        let record: BoxRecord = serde_json::from_value(value).unwrap();

        assert!(record.managed_state().is_err());
    }

    #[test]
    fn managed_execution_validation_rejects_plan_drift() {
        let config = a3s_box_core::BoxConfig {
            image: "alpine:latest".to_string(),
            isolation: ExecutionIsolation::Sandbox,
            ..Default::default()
        };
        let mut metadata = ManagedExecutionMetadata::new(
            OperationId::new("create-op-1").unwrap(),
            ExecutionGeneration::INITIAL,
            CreateExecutionRequest {
                external_sandbox_id: "sandbox-1".to_string(),
                config,
                labels: Default::default(),
                policy: Default::default(),
                rootfs_snapshot_id: None,
            },
        )
        .unwrap();
        metadata.plan =
            a3s_box_core::resolve_execution(&a3s_box_core::BoxConfig::default()).unwrap();

        assert!(metadata.validate().is_err());
    }
}
