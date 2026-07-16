use std::fmt;

use a3s_box_core::{ExecutionGeneration, ExecutionId, ExecutionSnapshotId};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::control::{PublicSandboxState, ResolvedTemplate, SandboxId};

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct SnapshotId(String);

impl SnapshotId {
    pub fn new(value: impl Into<String>) -> Result<Self, SnapshotModelError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(SnapshotModelError::InvalidId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for SnapshotId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for SnapshotId {
    type Error = SnapshotModelError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<SnapshotId> for String {
    fn from(value: SnapshotId) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotState {
    Creating,
    Active,
    Deleting,
}

impl SnapshotState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Active => "active",
            Self::Deleting => "deleting",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SnapshotRecord {
    snapshot_id: SnapshotId,
    content_id: ExecutionSnapshotId,
    owner_id: String,
    source_sandbox_id: SandboxId,
    source_execution_id: ExecutionId,
    source_execution_generation: ExecutionGeneration,
    source_state: PublicSandboxState,
    name: Option<String>,
    namespace: String,
    reference: String,
    template: ResolvedTemplate,
    state: SnapshotState,
    created_at: DateTime<Utc>,
    size_bytes: Option<u64>,
}

impl SnapshotRecord {
    #[allow(clippy::too_many_arguments)]
    pub fn creating(
        snapshot_id: SnapshotId,
        content_id: ExecutionSnapshotId,
        owner_id: impl Into<String>,
        source_sandbox_id: SandboxId,
        source_execution_id: ExecutionId,
        source_execution_generation: ExecutionGeneration,
        source_state: PublicSandboxState,
        name: Option<String>,
        namespace: impl Into<String>,
        mut template: ResolvedTemplate,
        created_at: DateTime<Utc>,
    ) -> Result<Self, SnapshotModelError> {
        let owner_id = owner_id.into();
        let namespace = namespace.into();
        if let Some(name) = name.as_deref() {
            validate_snapshot_name(name)?;
        }
        let reference = match name.as_deref() {
            Some(name) => format!("{namespace}/{name}:default"),
            None => format!("{snapshot_id}:default"),
        };
        template.rootfs_snapshot_id = Some(content_id.clone());
        let record = Self {
            snapshot_id,
            content_id,
            owner_id,
            source_sandbox_id,
            source_execution_id,
            source_execution_generation,
            source_state,
            name,
            namespace,
            reference,
            template,
            state: SnapshotState::Creating,
            created_at,
            size_bytes: None,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn snapshot_id(&self) -> &SnapshotId {
        &self.snapshot_id
    }

    pub fn content_id(&self) -> &ExecutionSnapshotId {
        &self.content_id
    }

    pub fn owner_id(&self) -> &str {
        &self.owner_id
    }

    pub fn source_sandbox_id(&self) -> &SandboxId {
        &self.source_sandbox_id
    }

    pub fn source_execution_id(&self) -> &ExecutionId {
        &self.source_execution_id
    }

    pub const fn source_execution_generation(&self) -> ExecutionGeneration {
        self.source_execution_generation
    }

    pub const fn source_state(&self) -> PublicSandboxState {
        self.source_state
    }

    pub fn name(&self) -> Option<&str> {
        self.name.as_deref()
    }

    pub fn reference(&self) -> &str {
        &self.reference
    }

    pub fn names(&self) -> Vec<String> {
        self.name
            .as_ref()
            .map(|_| vec![self.reference.clone()])
            .unwrap_or_default()
    }

    pub fn template(&self) -> &ResolvedTemplate {
        &self.template
    }

    pub const fn state(&self) -> SnapshotState {
        self.state
    }

    pub const fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }

    pub const fn size_bytes(&self) -> Option<u64> {
        self.size_bytes
    }

    pub fn mark_active(&mut self, size_bytes: u64) -> Result<(), SnapshotModelError> {
        if self.state != SnapshotState::Creating {
            return Err(SnapshotModelError::InvalidTransition);
        }
        self.size_bytes = Some(size_bytes);
        self.state = SnapshotState::Active;
        Ok(())
    }

    pub fn begin_delete(&mut self) -> Result<(), SnapshotModelError> {
        if self.state != SnapshotState::Active {
            return Err(SnapshotModelError::InvalidTransition);
        }
        self.state = SnapshotState::Deleting;
        Ok(())
    }

    pub fn abort_delete(&mut self) -> Result<(), SnapshotModelError> {
        if self.state != SnapshotState::Deleting {
            return Err(SnapshotModelError::InvalidTransition);
        }
        self.state = SnapshotState::Active;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), SnapshotModelError> {
        if self.owner_id.trim().is_empty()
            || !valid_namespace(&self.namespace)
            || self
                .name
                .as_deref()
                .is_some_and(|name| validate_snapshot_name(name).is_err())
            || self.reference
                != match self.name.as_deref() {
                    Some(name) => format!("{}/{name}:default", self.namespace),
                    None => format!("{}:default", self.snapshot_id),
                }
            || self.template.rootfs_snapshot_id.as_ref() != Some(&self.content_id)
            || (self.state == SnapshotState::Creating) != self.size_bytes.is_none()
        {
            return Err(SnapshotModelError::InvalidRecord);
        }
        Ok(())
    }
}

pub fn validate_snapshot_name(value: &str) -> Result<(), SnapshotModelError> {
    if value.is_empty()
        || value.len() > 128
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'))
    {
        return Err(SnapshotModelError::InvalidName);
    }
    Ok(())
}

fn valid_namespace(value: &str) -> bool {
    value.starts_with("a3s-")
        && value.len() == 16
        && value[4..]
            .bytes()
            .all(|byte| byte.is_ascii_digit() || matches!(byte, b'a'..=b'f'))
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotModelError {
    #[error("invalid snapshot ID")]
    InvalidId,
    #[error("invalid snapshot name")]
    InvalidName,
    #[error("invalid snapshot record")]
    InvalidRecord,
    #[error("invalid snapshot state transition")]
    InvalidTransition,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_reference_delimiters_in_snapshot_names() {
        for valid in ["state", "State_01", "state-v2.1"] {
            assert!(validate_snapshot_name(valid).is_ok());
        }
        for invalid in ["", "team/state", "state:latest", "../state", "with space"] {
            assert!(validate_snapshot_name(invalid).is_err());
        }
    }
}
