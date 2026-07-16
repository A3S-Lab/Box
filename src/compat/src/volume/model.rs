use std::fmt;

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::control::StoredToken;

#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct VolumeId(String);

impl VolumeId {
    pub fn new(value: impl Into<String>) -> Result<Self, VolumeModelError> {
        let value = value.into();
        if value.is_empty()
            || value.len() > 128
            || !value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
        {
            return Err(VolumeModelError::InvalidId);
        }
        Ok(Self(value))
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for VolumeId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl TryFrom<String> for VolumeId {
    type Error = VolumeModelError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::new(value)
    }
}

impl From<VolumeId> for String {
    fn from(value: VolumeId) -> Self {
        value.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VolumeState {
    Creating,
    Active,
    Deleting,
}

impl VolumeState {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Creating => "creating",
            Self::Active => "active",
            Self::Deleting => "deleting",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeRecord {
    volume_id: VolumeId,
    owner_id: String,
    name: String,
    runtime_name: String,
    token: StoredToken,
    state: VolumeState,
    created_at: DateTime<Utc>,
}

impl VolumeRecord {
    pub fn creating(
        volume_id: VolumeId,
        owner_id: impl Into<String>,
        name: impl Into<String>,
        runtime_name: impl Into<String>,
        token: StoredToken,
        created_at: DateTime<Utc>,
    ) -> Result<Self, VolumeModelError> {
        let record = Self {
            volume_id,
            owner_id: owner_id.into(),
            name: name.into(),
            runtime_name: runtime_name.into(),
            token,
            state: VolumeState::Creating,
            created_at,
        };
        record.validate()?;
        Ok(record)
    }

    pub fn volume_id(&self) -> &VolumeId {
        &self.volume_id
    }

    pub fn owner_id(&self) -> &str {
        &self.owner_id
    }

    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn runtime_name(&self) -> &str {
        &self.runtime_name
    }

    pub fn token(&self) -> &StoredToken {
        &self.token
    }

    pub const fn state(&self) -> VolumeState {
        self.state
    }

    pub const fn created_at(&self) -> DateTime<Utc> {
        self.created_at
    }

    pub fn mark_active(&mut self) -> Result<(), VolumeModelError> {
        if self.state != VolumeState::Creating {
            return Err(VolumeModelError::InvalidTransition);
        }
        self.state = VolumeState::Active;
        Ok(())
    }

    pub fn begin_delete(&mut self) -> Result<(), VolumeModelError> {
        if self.state != VolumeState::Active {
            return Err(VolumeModelError::InvalidTransition);
        }
        self.state = VolumeState::Deleting;
        Ok(())
    }

    pub fn abort_delete(&mut self) -> Result<(), VolumeModelError> {
        if self.state != VolumeState::Deleting {
            return Err(VolumeModelError::InvalidTransition);
        }
        self.state = VolumeState::Active;
        Ok(())
    }

    pub fn validate(&self) -> Result<(), VolumeModelError> {
        if self.owner_id.trim().is_empty()
            || !valid_volume_name(&self.name)
            || !valid_runtime_name(&self.runtime_name)
        {
            return Err(VolumeModelError::InvalidRecord);
        }
        Ok(())
    }
}

pub fn valid_volume_name(value: &str) -> bool {
    !value.is_empty()
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_'))
}

fn valid_runtime_name(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

#[derive(Debug, Error, Clone, Copy, PartialEq, Eq)]
pub enum VolumeModelError {
    #[error("invalid volume ID")]
    InvalidId,
    #[error("invalid volume record")]
    InvalidRecord,
    #[error("invalid volume state transition")]
    InvalidTransition,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn validates_protocol_volume_names_without_using_them_as_paths() {
        for valid in ["data", "Data_01", "a-b"] {
            assert!(valid_volume_name(valid));
        }
        for invalid in ["", "data/other", "../escape", "with space"] {
            assert!(!valid_volume_name(invalid));
        }
    }
}
