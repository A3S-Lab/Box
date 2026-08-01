//! Backend-neutral lifecycle interface for managed A3S executions.

use std::collections::BTreeMap;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::pin::Pin;
use std::time::Duration;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tokio::io::{AsyncRead, AsyncWrite};

use crate::config::{BoxConfig, ResourceConfig, ResourceLimits};
use crate::execution::ResolvedExecutionPlan;
use crate::log::{LogConfig, LogEntry};

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

/// Opaque identifier for one runtime-managed filesystem snapshot.
///
/// Snapshot identifiers are used as directory names below the runtime's
/// managed snapshot root. Keeping the lexical contract here prevents callers
/// from turning a protocol template reference into an arbitrary host path.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct ExecutionSnapshotId(String);

impl ExecutionSnapshotId {
    pub fn new(value: impl Into<String>) -> ExecutionManagerResult<Self> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(ExecutionManagerError::InvalidRequest(
                "execution snapshot ID must match [A-Za-z0-9_-]{1,128}".to_string(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ExecutionSnapshotId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for ExecutionSnapshotId {
    type Error = ExecutionManagerError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<ExecutionSnapshotId> for String {
    fn from(value: ExecutionSnapshotId) -> Self {
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
    /// Runtime-owned Linux tmpfs root containing transient Secret files.
    ///
    /// This path is persisted only so a reconstructed backend can distinguish
    /// caller-owned bind mounts from transient files that A3S Box is allowed to
    /// prepare for the Sandbox user namespace. Secret values never enter this
    /// policy or the creation request.
    #[serde(default)]
    pub managed_secret_root: Option<PathBuf>,
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
            managed_secret_root: None,
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
    /// Runtime-managed filesystem snapshot used as this execution's immutable
    /// rootfs lower. The runtime derives the host path from this validated ID;
    /// callers never supply a host path.
    #[serde(default)]
    pub rootfs_snapshot_id: Option<ExecutionSnapshotId>,
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

/// Result of atomically capturing one execution filesystem.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionSnapshot {
    pub snapshot_id: ExecutionSnapshotId,
    pub size_bytes: u64,
    /// Stable state restored after the temporary snapshot pause.
    pub state: ExecutionState,
    /// Generation-fenced runtime evidence after snapshot completion.
    pub lease: ExecutionLease,
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

/// Per-operation controls persisted with an explicit termination request.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct KillExecutionOptions {
    /// POSIX signal delivered before forced termination. `None` uses the
    /// persisted execution policy or the backend default.
    #[serde(default)]
    pub signal: Option<i32>,
    /// Grace period before forced termination. `None` uses the persisted
    /// execution policy or the backend default.
    #[serde(default)]
    pub timeout_secs: Option<u64>,
}

/// Per-operation controls persisted with an idempotent restart.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct RestartExecutionOptions {
    /// Graceful stop deadline for the old runtime. `None` uses persisted
    /// execution policy or the backend default.
    #[serde(default)]
    pub stop_timeout_secs: Option<u64>,
}

/// Maximum number of ordered runtime events returned by one bounded poll.
pub const MAX_EXECUTION_EVENT_BATCH_ITEMS: u32 = 4_096;

/// Partial live update for cgroup-backed workload controls.
///
/// Every `None` field preserves the currently persisted value. Provisioned
/// vCPU count, hard memory size, rlimits, and device policy are deliberately
/// absent because changing them requires a new execution generation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionResourceUpdate {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_reservation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub memory_swap: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pids_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_shares: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_quota: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpu_period: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cpuset_cpus: Option<String>,
}

impl ExecutionResourceUpdate {
    /// Whether the request carries no resource mutation.
    #[must_use]
    pub const fn is_empty(&self) -> bool {
        self.memory_reservation.is_none()
            && self.memory_swap.is_none()
            && self.pids_limit.is_none()
            && self.cpu_shares.is_none()
            && self.cpu_quota.is_none()
            && self.cpu_period.is_none()
            && self.cpuset_cpus.is_none()
    }

    /// Validate backend-independent value constraints before a durable claim.
    pub fn validate(&self) -> ExecutionManagerResult<()> {
        if self.is_empty() {
            return Err(ExecutionManagerError::InvalidRequest(
                "resource update must change at least one supported field".to_string(),
            ));
        }
        if self.memory_swap.is_some_and(|value| value < -1) {
            return Err(ExecutionManagerError::InvalidRequest(
                "memory swap must be -1 (unlimited) or non-negative".to_string(),
            ));
        }
        if self.pids_limit == Some(0) {
            return Err(ExecutionManagerError::InvalidRequest(
                "PID limit must be greater than zero".to_string(),
            ));
        }
        if self
            .cpu_shares
            .is_some_and(|value| !(2..=262_144).contains(&value))
        {
            return Err(ExecutionManagerError::InvalidRequest(
                "CPU shares must be between 2 and 262144".to_string(),
            ));
        }
        if self.cpu_quota.is_some_and(|value| value <= 0) {
            return Err(ExecutionManagerError::InvalidRequest(
                "CPU quota must be greater than zero".to_string(),
            ));
        }
        if self.cpu_period == Some(0) {
            return Err(ExecutionManagerError::InvalidRequest(
                "CPU period must be greater than zero".to_string(),
            ));
        }
        if self
            .cpuset_cpus
            .as_deref()
            .is_some_and(|value| !valid_cpuset(value))
        {
            return Err(ExecutionManagerError::InvalidRequest(
                "CPU set must be a comma-separated list of indices or ascending ranges".to_string(),
            ));
        }
        Ok(())
    }

    /// Merge this partial request into a complete persisted limit snapshot.
    pub fn apply_to(&self, limits: &mut ResourceLimits) {
        if let Some(value) = self.memory_reservation {
            limits.memory_reservation = Some(value);
        }
        if let Some(value) = self.memory_swap {
            limits.memory_swap = Some(value);
        }
        if let Some(value) = self.pids_limit {
            limits.pids_limit = Some(value);
        }
        if let Some(value) = self.cpu_shares {
            limits.cpu_shares = Some(value);
        }
        if let Some(value) = self.cpu_quota {
            limits.cpu_quota = Some(value);
        }
        if let Some(value) = self.cpu_period {
            limits.cpu_period = Some(value);
        }
        if let Some(value) = self.cpuset_cpus.as_ref() {
            limits.cpuset_cpus = Some(value.clone());
        }
    }
}

/// One runtime-visible init or exec process in an exact Box generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionProcessInfo {
    pub process_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub pid: Option<u32>,
    pub terminal: bool,
}

/// Exact-generation process inventory returned by the selected runtime.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionProcessInventory {
    pub execution_id: ExecutionId,
    pub generation: ExecutionGeneration,
    pub processes: Vec<ExecutionProcessInfo>,
}

impl ExecutionProcessInventory {
    pub fn validate(&self) -> ExecutionManagerResult<()> {
        let mut identifiers = std::collections::BTreeSet::new();
        for process in &self.processes {
            if process.process_id.trim().is_empty() {
                return Err(ExecutionManagerError::Internal(
                    "runtime process inventory contains an empty process ID".to_string(),
                ));
            }
            if process.pid == Some(0) {
                return Err(ExecutionManagerError::Internal(format!(
                    "runtime process {} contains PID zero",
                    process.process_id
                )));
            }
            if !identifiers.insert(process.process_id.as_str()) {
                return Err(ExecutionManagerError::Internal(format!(
                    "runtime process inventory contains duplicate process ID {}",
                    process.process_id
                )));
            }
        }
        Ok(())
    }
}

/// Normalized CPU counters from one exact runtime generation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionCpuStats {
    pub usage_ns: u64,
    pub user_ns: u64,
    pub system_ns: u64,
    pub throttled_ns: u64,
}

/// Normalized memory counters from one exact runtime generation.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionMemoryStats {
    pub usage_bytes: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_bytes: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub peak_bytes: Option<u64>,
}

/// One typed, exact-generation runtime resource snapshot.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionStats {
    pub execution_id: ExecutionId,
    pub generation: ExecutionGeneration,
    pub timestamp_unix_ns: u64,
    pub cpu: ExecutionCpuStats,
    pub memory: ExecutionMemoryStats,
    pub process_count: u64,
    pub metrics: BTreeMap<String, u64>,
}

impl ExecutionStats {
    pub fn validate(&self) -> ExecutionManagerResult<()> {
        if self.timestamp_unix_ns == 0 {
            return Err(ExecutionManagerError::Internal(
                "runtime stats timestamp must be positive".to_string(),
            ));
        }
        let accounted = self
            .cpu
            .user_ns
            .checked_add(self.cpu.system_ns)
            .ok_or_else(|| {
                ExecutionManagerError::Internal(
                    "runtime CPU user and system counters overflow".to_string(),
                )
            })?;
        if accounted > self.cpu.usage_ns {
            return Err(ExecutionManagerError::Internal(
                "runtime CPU user and system counters exceed total usage".to_string(),
            ));
        }
        if self
            .memory
            .peak_bytes
            .is_some_and(|peak| peak < self.memory.usage_bytes)
        {
            return Err(ExecutionManagerError::Internal(
                "runtime memory peak is below current usage".to_string(),
            ));
        }
        if let Some(name) = self.metrics.keys().find(|name| {
            name.is_empty()
                || name.len() > 256
                || name
                    .chars()
                    .any(|character| character.is_control() || character.is_whitespace())
        }) {
            return Err(ExecutionManagerError::Internal(format!(
                "runtime metric name is invalid: {name:?}"
            )));
        }
        Ok(())
    }
}

/// Bounded cursor request for exact-generation ordered runtime events.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionEventsRequest {
    pub after_sequence: u64,
    pub limit: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_timeout_ms: Option<u64>,
}

impl ExecutionEventsRequest {
    pub fn validate(&self) -> ExecutionManagerResult<()> {
        if self.limit == 0 || self.limit > MAX_EXECUTION_EVENT_BATCH_ITEMS {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "event batch limit must be between 1 and {MAX_EXECUTION_EVENT_BATCH_ITEMS}"
            )));
        }
        Ok(())
    }
}

/// Backend-neutral ordered runtime event kind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionEventKind {
    ContainerCreating,
    ContainerCreated,
    ContainerStarted,
    ContainerStopped,
    ContainerDeleted,
    ContainerPaused,
    ContainerResumed,
    ResourcesUpdated,
    ProcessCreated,
    ProcessStarted,
    ProcessExited,
    OutputDropped,
    RuntimeWarning,
}

/// One event from the runtime's durable global order.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionRuntimeEvent {
    pub sequence: u64,
    pub timestamp_unix_ns: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub process_id: Option<String>,
    pub kind: ExecutionEventKind,
    pub attributes: BTreeMap<String, String>,
}

/// One bounded event poll result for an exact Box generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecutionEventBatch {
    pub execution_id: ExecutionId,
    pub generation: ExecutionGeneration,
    pub events: Vec<ExecutionRuntimeEvent>,
    pub next_sequence: u64,
}

impl ExecutionEventBatch {
    pub fn validate_after(&self, after_sequence: u64) -> ExecutionManagerResult<()> {
        if self.next_sequence < after_sequence {
            return Err(ExecutionManagerError::Internal(
                "runtime event cursor regressed".to_string(),
            ));
        }
        let mut previous = after_sequence;
        for event in &self.events {
            if event.sequence == 0 || event.sequence <= previous {
                return Err(ExecutionManagerError::Internal(
                    "runtime events are not strictly ordered after the requested cursor"
                        .to_string(),
                ));
            }
            if event.timestamp_unix_ns == 0 {
                return Err(ExecutionManagerError::Internal(format!(
                    "runtime event {} has timestamp zero",
                    event.sequence
                )));
            }
            previous = event.sequence;
        }
        if self.next_sequence < previous {
            return Err(ExecutionManagerError::Internal(
                "runtime event next cursor precedes the returned batch".to_string(),
            ));
        }
        Ok(())
    }
}

fn valid_cpuset(value: &str) -> bool {
    let value = value.trim();
    !value.is_empty()
        && value.split(',').all(|item| {
            let item = item.trim();
            match item.split_once('-') {
                Some((lower, upper)) => parse_cpu_index(lower)
                    .zip(parse_cpu_index(upper))
                    .is_some_and(|(lower, upper)| lower <= upper),
                None => parse_cpu_index(item).is_some(),
            }
        })
}

fn parse_cpu_index(value: &str) -> Option<u32> {
    (!value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()))
        .then(|| value.parse().ok())
        .flatten()
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

/// Bidirectional byte stream connected to one generation-fenced workload port.
pub trait ExecutionPortIo: AsyncRead + AsyncWrite + Send + Unpin {}

impl<T> ExecutionPortIo for T where T: AsyncRead + AsyncWrite + Send + Unpin {}

pub type ExecutionPortStream = Pin<Box<dyn ExecutionPortIo>>;

/// Backend-neutral connector used by data-plane gateways.
///
/// Implementations must validate the execution generation atomically with
/// selecting the live runtime. A connector must never fall back to another
/// execution or generation when the requested runtime is unavailable.
#[async_trait]
pub trait ExecutionPortConnector: Send + Sync {
    async fn connect_port(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        port: NonZeroU16,
        timeout: Duration,
    ) -> ExecutionManagerResult<ExecutionPortStream>;
}

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

    /// Read structured stdout/stderr entries after fencing the runtime generation.
    async fn read_logs(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<Vec<LogEntry>> {
        Err(ExecutionManagerError::Unavailable(
            "this execution manager does not expose structured logs".to_string(),
        ))
    }

    /// Return the runtime-visible init and live exec processes for one exact generation.
    async fn list_processes(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<ExecutionProcessInventory> {
        Err(ExecutionManagerError::Unavailable(
            "this execution manager does not expose process inventory".to_string(),
        ))
    }

    /// Return one normalized resource snapshot for an exact generation.
    async fn stats(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<ExecutionStats> {
        Err(ExecutionManagerError::Unavailable(
            "this execution manager does not expose runtime stats".to_string(),
        ))
    }

    /// Poll ordered runtime events without crossing the selected generation.
    async fn events(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _request: ExecutionEventsRequest,
    ) -> ExecutionManagerResult<ExecutionEventBatch> {
        Err(ExecutionManagerError::Unavailable(
            "this execution manager does not expose runtime events".to_string(),
        ))
    }

    /// Apply a replay-safe partial live resource update to one exact generation.
    async fn update_resources(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _operation_id: &OperationId,
        _update: ExecutionResourceUpdate,
    ) -> ExecutionManagerResult<ExecutionLease> {
        Err(ExecutionManagerError::Unavailable(
            "this execution manager does not support live resource updates".to_string(),
        ))
    }

    /// Temporarily quiesce the execution, atomically capture its rootfs in the
    /// runtime-managed snapshot store, and restore its prior stable state.
    async fn create_filesystem_snapshot(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<ExecutionSnapshot> {
        Err(ExecutionManagerError::Unavailable(
            "this execution manager does not support filesystem snapshots".to_string(),
        ))
    }

    /// Return the size of a fully published runtime-managed snapshot, or
    /// `None` when it does not exist.
    async fn filesystem_snapshot_size(
        &self,
        _snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<Option<u64>> {
        Err(ExecutionManagerError::Unavailable(
            "this execution manager does not expose filesystem snapshots".to_string(),
        ))
    }

    /// Delete a runtime-managed snapshot, refusing while an active execution
    /// still uses it as a copy-on-write lower.
    async fn delete_filesystem_snapshot(
        &self,
        _snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<bool> {
        Err(ExecutionManagerError::Unavailable(
            "this execution manager does not support filesystem snapshot deletion".to_string(),
        ))
    }

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

    /// Terminate one execution with controls that survive lifecycle recovery.
    ///
    /// Managers without option-aware termination may delegate to [`Self::kill`].
    async fn kill_with_options(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        _options: KillExecutionOptions,
    ) -> ExecutionManagerResult<KillOutcome> {
        self.kill(execution_id, generation).await
    }

    /// Remove one terminal execution and all runtime-owned resources.
    ///
    /// Implementations must fence removal by generation and make retries
    /// idempotent. Active executions must be stopped explicitly before this
    /// operation; removal must never imply an unrequested kill.
    async fn remove(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<bool> {
        Err(ExecutionManagerError::Unavailable(
            "this execution manager does not support execution removal".to_string(),
        ))
    }

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
    fn snapshot_identifiers_are_safe_managed_directory_names() {
        for valid in ["snapshot-1", "SNAPSHOT_2", "a"] {
            assert_eq!(ExecutionSnapshotId::new(valid).unwrap().as_str(), valid);
        }
        for invalid in [
            "",
            ".",
            "..",
            "../snapshot",
            "snapshot/path",
            "snapshot:tag",
            "snapshot id",
        ] {
            assert!(matches!(
                ExecutionSnapshotId::new(invalid),
                Err(ExecutionManagerError::InvalidRequest(_))
            ));
        }
        assert!(ExecutionSnapshotId::new("x".repeat(129)).is_err());
        assert!(serde_json::from_str::<ExecutionSnapshotId>("\"../snapshot\"").is_err());
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
        assert!(request.rootfs_snapshot_id.is_none());
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

    #[test]
    fn resource_updates_validate_and_preserve_unmentioned_limits() {
        let mut limits = ResourceLimits {
            memory_swap: Some(-1),
            cpu_period: Some(100_000),
            ulimits: vec!["NOFILE=1024:2048".to_string()],
            ..Default::default()
        };
        let update = ExecutionResourceUpdate {
            memory_reservation: Some(64 * 1024 * 1024),
            pids_limit: Some(64),
            cpu_shares: Some(512),
            cpuset_cpus: Some("0-1,3".to_string()),
            ..Default::default()
        };

        update.validate().unwrap();
        update.apply_to(&mut limits);

        assert_eq!(limits.memory_reservation, Some(64 * 1024 * 1024));
        assert_eq!(limits.memory_swap, Some(-1));
        assert_eq!(limits.cpu_period, Some(100_000));
        assert_eq!(limits.pids_limit, Some(64));
        assert_eq!(limits.cpu_shares, Some(512));
        assert_eq!(limits.cpuset_cpus.as_deref(), Some("0-1,3"));
        assert_eq!(limits.ulimits, ["NOFILE=1024:2048"]);

        for invalid in [
            ExecutionResourceUpdate::default(),
            ExecutionResourceUpdate {
                pids_limit: Some(0),
                ..Default::default()
            },
            ExecutionResourceUpdate {
                cpu_shares: Some(1),
                ..Default::default()
            },
            ExecutionResourceUpdate {
                cpu_quota: Some(-1),
                ..Default::default()
            },
            ExecutionResourceUpdate {
                cpuset_cpus: Some("3-1".to_string()),
                ..Default::default()
            },
        ] {
            assert!(matches!(
                invalid.validate(),
                Err(ExecutionManagerError::InvalidRequest(_))
            ));
        }
    }

    #[test]
    fn event_batches_require_strict_order_and_nonregressing_cursors() {
        let execution_id = ExecutionId::new("events").unwrap();
        let batch = ExecutionEventBatch {
            execution_id: execution_id.clone(),
            generation: ExecutionGeneration::INITIAL,
            events: vec![
                ExecutionRuntimeEvent {
                    sequence: 4,
                    timestamp_unix_ns: 10,
                    process_id: None,
                    kind: ExecutionEventKind::ContainerStarted,
                    attributes: BTreeMap::new(),
                },
                ExecutionRuntimeEvent {
                    sequence: 7,
                    timestamp_unix_ns: 11,
                    process_id: Some("init".to_string()),
                    kind: ExecutionEventKind::ProcessStarted,
                    attributes: BTreeMap::new(),
                },
            ],
            next_sequence: 7,
        };
        batch.validate_after(3).unwrap();

        let mut duplicate = batch.clone();
        duplicate.events[1].sequence = 4;
        assert!(matches!(
            duplicate.validate_after(3),
            Err(ExecutionManagerError::Internal(_))
        ));

        let mut regressed = batch;
        regressed.next_sequence = 3;
        assert!(matches!(
            regressed.validate_after(3),
            Err(ExecutionManagerError::Internal(_))
        ));
    }
}
