//! Backend-neutral lifecycle interface for managed A3S executions.

use std::collections::BTreeMap;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::config::{BoxConfig, ResourceConfig};
use crate::execution::ResolvedExecutionPlan;
use crate::log::LogConfig;

/// Stable identifier assigned to one runtime execution.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ExecutionId(String);

impl ExecutionId {
    pub fn new(value: impl Into<String>) -> ExecutionManagerResult<Self> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ExecutionManagerError::InvalidRequest(
                "execution ID cannot be empty".to_string(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ExecutionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for ExecutionId {
    type Error = ExecutionManagerError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ExecutionId> for String {
    fn from(value: ExecutionId) -> Self {
        value.0
    }
}

/// Idempotency identity for a lifecycle operation.
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct OperationId(String);

impl OperationId {
    pub fn new(value: impl Into<String>) -> ExecutionManagerResult<Self> {
        let value = value.into();
        if value.trim().is_empty() {
            return Err(ExecutionManagerError::InvalidRequest(
                "operation ID cannot be empty".to_string(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for OperationId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for OperationId {
    type Error = ExecutionManagerError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<OperationId> for String {
    fn from(value: OperationId) -> Self {
        value.0
    }
}

/// Runtime generation used to reject stale lifecycle operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct ExecutionGeneration(u64);

impl ExecutionGeneration {
    pub const INITIAL: Self = Self(1);

    pub fn new(value: u64) -> ExecutionManagerResult<Self> {
        if value == 0 {
            return Err(ExecutionManagerError::InvalidRequest(
                "execution generation must be greater than zero".to_string(),
            ));
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u64 {
        self.0
    }
}

impl TryFrom<u64> for ExecutionGeneration {
    type Error = ExecutionManagerError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ExecutionGeneration> for u64 {
    fn from(value: ExecutionGeneration) -> Self {
        value.0
    }
}

/// Restart behavior persisted with a local execution.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionRestartPolicy {
    /// Never restart automatically.
    #[default]
    No,
    /// Restart after every exit.
    Always,
    /// Restart only after an unsuccessful exit.
    OnFailure,
    /// Restart unless a user explicitly stopped the execution.
    UnlessStopped,
}

impl ExecutionRestartPolicy {
    /// Canonical value stored in the backwards-compatible local record.
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::No => "no",
            Self::Always => "always",
            Self::OnFailure => "on-failure",
            Self::UnlessStopped => "unless-stopped",
        }
    }
}

/// Health-check behavior persisted with a local execution.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionHealthCheck {
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

/// Caller-owned policy projected into the canonical local execution record.
///
/// The complete value is persisted with the creation request so retries cannot
/// silently reuse an execution with different lifecycle or local resource
/// policy. Runtime launch requirements remain in [`BoxConfig`].
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionRecordPolicy {
    /// User-visible local name. `None` lets the runtime assign a safe name.
    #[serde(default)]
    pub name: Option<String>,
    /// Remove the execution automatically after it stops.
    #[serde(default)]
    pub auto_remove: bool,
    /// Automatic restart behavior.
    #[serde(default)]
    pub restart_policy: ExecutionRestartPolicy,
    /// Maximum automatic restart count, where zero means unlimited.
    #[serde(default)]
    pub max_restart_count: u32,
    /// Effective caller or cached-image health check.
    #[serde(default)]
    pub health_check: Option<ExecutionHealthCheck>,
    /// Prevent a later image-defined health check from being enabled.
    #[serde(default)]
    pub healthcheck_disabled: bool,
    /// Runtime log driver policy.
    #[serde(default)]
    pub log_config: LogConfig,
    /// Named volumes represented by the resolved mounts in [`BoxConfig`].
    #[serde(default)]
    pub volume_names: Vec<String>,
    /// Requested OCI platform retained for inspection and image selection.
    #[serde(default)]
    pub platform: Option<String>,
    /// Whether the caller requested an init process.
    #[serde(default)]
    pub init: bool,
    /// Requested host device mappings.
    #[serde(default)]
    pub devices: Vec<String>,
    /// Requested GPU selection.
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
    /// Whether the caller requested OOM-killer suppression.
    #[serde(default)]
    pub oom_kill_disable: bool,
    /// Requested host OOM score adjustment.
    #[serde(default)]
    pub oom_score_adj: Option<i32>,
}

impl Default for ExecutionRecordPolicy {
    fn default() -> Self {
        Self {
            name: None,
            auto_remove: false,
            restart_policy: ExecutionRestartPolicy::No,
            max_restart_count: 0,
            health_check: None,
            healthcheck_disabled: false,
            log_config: LogConfig::default(),
            volume_names: Vec::new(),
            platform: None,
            init: false,
            devices: Vec::new(),
            gpus: None,
            shm_size: None,
            stop_signal: None,
            stop_timeout: None,
            oom_kill_disable: false,
            oom_score_adj: None,
        }
    }
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

/// A fully resolved request submitted to the runtime lifecycle facade.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CreateExecutionRequest {
    /// Public identity used only as an untrusted diagnostic label.
    pub external_sandbox_id: String,
    /// Backend-neutral runtime configuration resolved from template policy.
    pub config: BoxConfig,
    /// Labels persisted with the internal execution.
    pub labels: BTreeMap<String, String>,
    /// Caller-owned lifecycle and local record policy.
    #[serde(default)]
    pub policy: ExecutionRecordPolicy,
}

/// Durable evidence returned after an execution is created but not started.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionReservation {
    pub execution_id: ExecutionId,
    pub generation: ExecutionGeneration,
    pub plan: ResolvedExecutionPlan,
    pub resources: ResourceConfig,
    pub created_at: DateTime<Utc>,
}

/// Evidence returned when a runtime execution is ready.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionLease {
    pub execution_id: ExecutionId,
    pub generation: ExecutionGeneration,
    pub plan: ResolvedExecutionPlan,
    pub resources: ResourceConfig,
    pub started_at: DateTime<Utc>,
}

/// Runtime state visible through the backend-neutral lifecycle facade.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionState {
    Created,
    Creating,
    Running,
    Paused,
    Stopped,
    Failed,
}

/// Current state and generation of one execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionStatus {
    pub execution_id: ExecutionId,
    pub generation: ExecutionGeneration,
    pub state: ExecutionState,
    pub plan: ResolvedExecutionPlan,
}

/// Result of an idempotent runtime kill request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum KillOutcome {
    Killed,
    AlreadyStopped,
}

/// Per-operation controls persisted with an idempotent restart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartExecutionOptions {
    /// Graceful stop deadline for the old runtime. `None` uses persisted
    /// execution policy or the backend default.
    #[serde(default)]
    pub stop_timeout_secs: Option<u64>,
}

/// Runtime evidence recovered after a service restart.
#[derive(Debug, Clone)]
pub enum ReconcileOutcome {
    Absent,
    Created(ExecutionReservation),
    Creating,
    Ready(ExecutionLease),
    Failed,
}

/// Errors returned by the lifecycle facade without exposing backend internals.
#[derive(Debug, Error)]
pub enum ExecutionManagerError {
    #[error("invalid execution request: {0}")]
    InvalidRequest(String),
    #[error("execution not found: {0}")]
    NotFound(ExecutionId),
    #[error("execution conflict for {execution_id}: {message}")]
    Conflict {
        execution_id: ExecutionId,
        message: String,
    },
    #[error("execution backend unavailable: {0}")]
    Unavailable(String),
    #[error("execution lifecycle failed: {0}")]
    Internal(String),
}

pub type ExecutionManagerResult<T> = std::result::Result<T, ExecutionManagerError>;

/// Backend-neutral lifecycle facade shared by the CLI, SDK, and remote service.
#[async_trait]
pub trait ExecutionManager: Send + Sync {
    /// Persist exactly one unstarted execution reservation for `operation_id`.
    async fn create(
        &self,
        _request: CreateExecutionRequest,
        _operation_id: &OperationId,
    ) -> ExecutionManagerResult<ExecutionReservation> {
        Err(ExecutionManagerError::Unavailable(
            "this execution manager does not support staged create".to_string(),
        ))
    }

    /// Start one created execution after fencing stale callers by generation.
    async fn start(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<ExecutionLease> {
        Err(ExecutionManagerError::Unavailable(
            "this execution manager does not support staged start".to_string(),
        ))
    }

    /// Create and start exactly one execution for `operation_id`.
    ///
    /// Retrying after a crash reuses the durable reservation and continues its
    /// start instead of allocating a second execution.
    async fn create_and_start(
        &self,
        request: CreateExecutionRequest,
        operation_id: &OperationId,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let reservation = self.create(request, operation_id).await?;
        self.start(&reservation.execution_id, reservation.generation)
            .await
    }

    async fn inspect(&self, execution_id: &ExecutionId) -> ExecutionManagerResult<ExecutionStatus>;

    /// Pause one execution and return the generation-fenced paused lease.
    async fn pause(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        keep_memory: bool,
    ) -> ExecutionManagerResult<ExecutionLease>;

    async fn resume(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<ExecutionLease>;

    /// Terminate the current runtime, advance its generation exactly once,
    /// and start it again under an idempotent operation identity.
    async fn restart(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        operation_id: &OperationId,
    ) -> ExecutionManagerResult<ExecutionLease> {
        self.restart_with_options(
            execution_id,
            generation,
            operation_id,
            RestartExecutionOptions::default(),
        )
        .await
    }

    /// Restart with controls that become part of the durable operation intent.
    async fn restart_with_options(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _operation_id: &OperationId,
        _options: RestartExecutionOptions,
    ) -> ExecutionManagerResult<ExecutionLease> {
        Err(ExecutionManagerError::Unavailable(
            "this execution manager does not support restart".to_string(),
        ))
    }

    async fn kill(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<KillOutcome>;

    async fn reconcile(
        &self,
        operation_id: &OperationId,
    ) -> ExecutionManagerResult<ReconcileOutcome>;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identifiers_reject_empty_values() {
        assert!(matches!(
            ExecutionId::new("  "),
            Err(ExecutionManagerError::InvalidRequest(_))
        ));
        assert!(matches!(
            OperationId::new(""),
            Err(ExecutionManagerError::InvalidRequest(_))
        ));
    }

    #[test]
    fn generation_rejects_zero() {
        assert!(matches!(
            ExecutionGeneration::new(0),
            Err(ExecutionManagerError::InvalidRequest(_))
        ));
        assert_eq!(ExecutionGeneration::INITIAL.get(), 1);
        assert!(serde_json::from_str::<ExecutionGeneration>("0").is_err());
    }

    #[test]
    fn identifier_deserialization_preserves_invariants() {
        assert!(serde_json::from_str::<ExecutionId>("\"\"").is_err());
        assert!(serde_json::from_str::<OperationId>("\" \"").is_err());
    }

    #[test]
    fn legacy_creation_requests_default_record_policy() {
        let request: CreateExecutionRequest = serde_json::from_value(serde_json::json!({
            "external_sandbox_id": "sandbox-1",
            "config": BoxConfig::default(),
            "labels": {"purpose": "compatibility"}
        }))
        .unwrap();

        assert_eq!(request.policy, ExecutionRecordPolicy::default());
        assert_eq!(request.policy.restart_policy, ExecutionRestartPolicy::No);
    }

    #[test]
    fn restart_policy_has_stable_record_values() {
        assert_eq!(ExecutionRestartPolicy::No.as_str(), "no");
        assert_eq!(ExecutionRestartPolicy::Always.as_str(), "always");
        assert_eq!(ExecutionRestartPolicy::OnFailure.as_str(), "on-failure");
        assert_eq!(
            ExecutionRestartPolicy::UnlessStopped.as_str(),
            "unless-stopped"
        );
        assert_eq!(
            serde_json::to_value(ExecutionRestartPolicy::OnFailure).unwrap(),
            "on-failure"
        );
    }
}
