use std::collections::BTreeMap;
use std::fmt;

use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionLease, OperationId, ResolvedExecutionPlan,
    ResourceConfig,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::credential::SandboxCredentials;
use crate::routing::SandboxRoutePolicy;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SandboxId(String);

impl SandboxId {
    pub fn new(value: impl Into<String>) -> Result<Self, LifecycleError> {
        let value = value.into();
        let bytes = value.as_bytes();
        if bytes.is_empty()
            || bytes.len() > 48
            || !is_sandbox_id_alphanumeric(bytes[0])
            || !is_sandbox_id_alphanumeric(bytes[bytes.len() - 1])
            || !bytes
                .iter()
                .all(|byte| is_sandbox_id_alphanumeric(*byte) || *byte == b'-')
        {
            return Err(LifecycleError::InvalidIdentity(
                "sandbox ID must be 1-48 lowercase DNS-label characters".to_string(),
            ));
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

const fn is_sandbox_id_alphanumeric(byte: u8) -> bool {
    byte.is_ascii_lowercase() || byte.is_ascii_digit()
}

impl fmt::Display for SandboxId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for SandboxId {
    type Error = LifecycleError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<SandboxId> for String {
    fn from(value: SandboxId) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "u64", into = "u64")]
pub struct SandboxGeneration(u64);

impl SandboxGeneration {
    pub const INITIAL: Self = Self(1);

    pub fn new(value: u64) -> Result<Self, LifecycleError> {
        if value == 0 {
            return Err(LifecycleError::InvalidIdentity(
                "sandbox generation must be greater than zero".to_string(),
            ));
        }
        Ok(Self(value))
    }

    pub const fn get(self) -> u64 {
        self.0
    }

    fn next(self) -> Result<Self, LifecycleError> {
        self.0
            .checked_add(1)
            .map(Self)
            .ok_or(LifecycleError::GenerationExhausted)
    }
}

impl TryFrom<u64> for SandboxGeneration {
    type Error = LifecycleError;

    fn try_from(value: u64) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<SandboxGeneration> for u64 {
    fn from(value: SandboxGeneration) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleState {
    Creating,
    Running,
    Pausing,
    Paused,
    Resuming,
    Killing,
    Killed,
    Failed,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum PublicSandboxState {
    Running,
    Paused,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OnTimeoutAction {
    Kill,
    Pause,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LifecyclePolicy {
    pub on_timeout: OnTimeoutAction,
    pub auto_resume: bool,
    pub keep_memory_on_pause: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LifecycleFailure {
    PolicyRejected,
    RuntimeFailed,
    ReconciliationFailed,
}

#[derive(Debug, Clone)]
pub struct NewSandboxRecord {
    pub sandbox_id: SandboxId,
    pub operation_id: OperationId,
    pub owner_id: String,
    pub template_id: String,
    pub plan: ResolvedExecutionPlan,
    pub resources: ResourceConfig,
    pub lifecycle: LifecyclePolicy,
    pub created_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub metadata: BTreeMap<String, String>,
    pub envd_version: String,
    pub secure: bool,
    pub allow_internet_access: Option<bool>,
    pub credentials: SandboxCredentials,
    pub routing: SandboxRoutePolicy,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SandboxRecord {
    sandbox_id: SandboxId,
    operation_id: OperationId,
    owner_id: String,
    execution_id: Option<ExecutionId>,
    execution_generation: Option<ExecutionGeneration>,
    generation: SandboxGeneration,
    template_id: String,
    plan: ResolvedExecutionPlan,
    resources: ResourceConfig,
    lifecycle: LifecyclePolicy,
    state: LifecycleState,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    expires_at: DateTime<Utc>,
    metadata: BTreeMap<String, String>,
    envd_version: String,
    secure: bool,
    allow_internet_access: Option<bool>,
    credentials: SandboxCredentials,
    #[serde(default)]
    routing: SandboxRoutePolicy,
    failure: Option<LifecycleFailure>,
}

impl SandboxRecord {
    pub fn creating(new: NewSandboxRecord) -> Result<Self, LifecycleError> {
        if new.owner_id.trim().is_empty()
            || new.template_id.trim().is_empty()
            || new.envd_version.trim().is_empty()
        {
            return Err(LifecycleError::InvalidIdentity(
                "owner ID, template ID, and envd version cannot be empty".to_string(),
            ));
        }
        if new.expires_at < new.created_at {
            return Err(LifecycleError::InvalidExpiry);
        }
        Ok(Self {
            sandbox_id: new.sandbox_id,
            operation_id: new.operation_id,
            owner_id: new.owner_id,
            execution_id: None,
            execution_generation: None,
            generation: SandboxGeneration::INITIAL,
            template_id: new.template_id,
            plan: new.plan,
            resources: new.resources,
            lifecycle: new.lifecycle,
            state: LifecycleState::Creating,
            created_at: new.created_at,
            started_at: None,
            expires_at: new.expires_at,
            metadata: new.metadata,
            envd_version: new.envd_version,
            secure: new.secure,
            allow_internet_access: new.allow_internet_access,
            credentials: new.credentials,
            routing: new.routing,
            failure: None,
        })
    }

    pub fn sandbox_id(&self) -> &SandboxId {
        &self.sandbox_id
    }

    pub fn operation_id(&self) -> &OperationId {
        &self.operation_id
    }

    pub fn owner_id(&self) -> &str {
        &self.owner_id
    }

    pub fn execution_id(&self) -> Option<&ExecutionId> {
        self.execution_id.as_ref()
    }

    pub const fn execution_generation(&self) -> Option<ExecutionGeneration> {
        self.execution_generation
    }

    pub const fn generation(&self) -> SandboxGeneration {
        self.generation
    }

    pub fn template_id(&self) -> &str {
        &self.template_id
    }

    pub fn plan(&self) -> &ResolvedExecutionPlan {
        &self.plan
    }

    pub fn resources(&self) -> &ResourceConfig {
        &self.resources
    }

    pub fn lifecycle(&self) -> &LifecyclePolicy {
        &self.lifecycle
    }

    pub const fn state(&self) -> LifecycleState {
        self.state
    }

    pub const fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }

    pub const fn started_at(&self) -> Option<DateTime<Utc>> {
        self.started_at
    }

    pub const fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }

    pub fn metadata(&self) -> &BTreeMap<String, String> {
        &self.metadata
    }

    pub fn envd_version(&self) -> &str {
        &self.envd_version
    }

    pub const fn secure(&self) -> bool {
        self.secure
    }

    pub const fn allow_internet_access(&self) -> Option<bool> {
        self.allow_internet_access
    }

    pub fn credentials(&self) -> &SandboxCredentials {
        &self.credentials
    }

    pub fn routing(&self) -> &SandboxRoutePolicy {
        &self.routing
    }

    pub const fn failure(&self) -> Option<LifecycleFailure> {
        self.failure
    }

    pub const fn public_state(&self) -> Option<PublicSandboxState> {
        match self.state {
            LifecycleState::Running => Some(PublicSandboxState::Running),
            LifecycleState::Paused => Some(PublicSandboxState::Paused),
            _ => None,
        }
    }

    pub const fn is_terminal(&self) -> bool {
        matches!(self.state, LifecycleState::Killed | LifecycleState::Failed)
    }

    pub(crate) fn validate_persisted(&self) -> Result<(), LifecycleError> {
        super::validation::validate_persisted_record(self)
    }

    pub fn mark_running(
        &mut self,
        lease: ExecutionLease,
    ) -> Result<SandboxGeneration, LifecycleError> {
        self.require_state(&[LifecycleState::Creating, LifecycleState::Resuming])?;
        self.validate_execution_lease(&lease)?;
        let next = self.generation.next()?;
        self.execution_id = Some(lease.execution_id);
        self.execution_generation = Some(lease.generation);
        self.resources = lease.resources;
        self.started_at.get_or_insert(lease.started_at);
        self.state = LifecycleState::Running;
        self.failure = None;
        self.generation = next;
        Ok(next)
    }

    pub fn begin_pause(&mut self) -> Result<SandboxGeneration, LifecycleError> {
        self.transition(&[LifecycleState::Running], LifecycleState::Pausing)
    }

    pub fn mark_paused(
        &mut self,
        lease: ExecutionLease,
    ) -> Result<SandboxGeneration, LifecycleError> {
        self.require_state(&[LifecycleState::Pausing])?;
        self.validate_execution_lease(&lease)?;
        let next = self.generation.next()?;
        self.execution_id = Some(lease.execution_id);
        self.execution_generation = Some(lease.generation);
        self.resources = lease.resources;
        self.started_at.get_or_insert(lease.started_at);
        self.state = LifecycleState::Paused;
        self.generation = next;
        Ok(next)
    }

    pub fn begin_resume(&mut self) -> Result<SandboxGeneration, LifecycleError> {
        if self.execution_id.is_none() || self.execution_generation.is_none() {
            return Err(LifecycleError::MissingExecution);
        }
        self.transition(&[LifecycleState::Paused], LifecycleState::Resuming)
    }

    pub fn begin_kill(&mut self) -> Result<SandboxGeneration, LifecycleError> {
        self.transition(
            &[
                LifecycleState::Creating,
                LifecycleState::Running,
                LifecycleState::Pausing,
                LifecycleState::Paused,
                LifecycleState::Resuming,
                LifecycleState::Failed,
            ],
            LifecycleState::Killing,
        )
    }

    pub fn mark_killed(&mut self) -> Result<SandboxGeneration, LifecycleError> {
        self.transition(&[LifecycleState::Killing], LifecycleState::Killed)
    }

    pub fn mark_failed(
        &mut self,
        failure: LifecycleFailure,
    ) -> Result<SandboxGeneration, LifecycleError> {
        self.require_state(&[
            LifecycleState::Creating,
            LifecycleState::Running,
            LifecycleState::Pausing,
            LifecycleState::Paused,
            LifecycleState::Resuming,
        ])?;
        let next = self.generation.next()?;
        self.state = LifecycleState::Failed;
        self.failure = Some(failure);
        self.generation = next;
        Ok(next)
    }

    pub fn replace_expiry(
        &mut self,
        expires_at: DateTime<Utc>,
    ) -> Result<SandboxGeneration, LifecycleError> {
        self.require_state(&[LifecycleState::Running, LifecycleState::Paused])?;
        let next = self.generation.next()?;
        self.expires_at = expires_at;
        self.generation = next;
        Ok(next)
    }

    fn transition(
        &mut self,
        allowed: &[LifecycleState],
        target: LifecycleState,
    ) -> Result<SandboxGeneration, LifecycleError> {
        self.require_state(allowed)?;
        let next = self.generation.next()?;
        self.state = target;
        self.generation = next;
        Ok(next)
    }

    fn validate_execution_lease(&self, lease: &ExecutionLease) -> Result<(), LifecycleError> {
        if lease.plan != self.plan {
            return Err(LifecycleError::ExecutionPlanMismatch);
        }
        if self
            .execution_id
            .as_ref()
            .is_some_and(|execution_id| execution_id != &lease.execution_id)
        {
            return Err(LifecycleError::ExecutionIdentityMismatch);
        }
        if self
            .execution_generation
            .is_some_and(|generation| lease.generation <= generation)
        {
            return Err(LifecycleError::ExecutionGenerationMismatch);
        }
        Ok(())
    }

    fn require_state(&self, allowed: &[LifecycleState]) -> Result<(), LifecycleError> {
        if allowed.contains(&self.state) {
            return Ok(());
        }
        Err(LifecycleError::InvalidTransition {
            from: self.state,
            allowed: allowed.to_vec(),
        })
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum LifecycleError {
    #[error("invalid lifecycle identity: {0}")]
    InvalidIdentity(String),
    #[error("sandbox expiry cannot precede creation")]
    InvalidExpiry,
    #[error("invalid credential material")]
    InvalidCredentialMaterial,
    #[error("invalid lifecycle transition from {from:?}; expected one of {allowed:?}")]
    InvalidTransition {
        from: LifecycleState,
        allowed: Vec<LifecycleState>,
    },
    #[error("sandbox generation is exhausted")]
    GenerationExhausted,
    #[error("runtime returned a different resolved execution plan")]
    ExecutionPlanMismatch,
    #[error("runtime returned a different execution identity")]
    ExecutionIdentityMismatch,
    #[error("runtime returned a stale execution generation")]
    ExecutionGenerationMismatch,
    #[error("sandbox has no runtime execution to resume")]
    MissingExecution,
    #[error("invalid persisted sandbox state: {0}")]
    InvalidPersistedState(String),
}
