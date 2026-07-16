use a3s_box_core::{BoxConfig, ExecutionSnapshotId, OperationId};
use async_trait::async_trait;
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::SandboxId;
use crate::routing::SandboxRoutePolicy;

pub trait Clock: Send + Sync {
    fn now(&self) -> DateTime<Utc>;
}

#[derive(Debug, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now(&self) -> DateTime<Utc> {
        Utc::now()
    }
}

#[derive(Debug, Clone)]
pub struct SandboxIdentity {
    pub sandbox_id: SandboxId,
    pub operation_id: OperationId,
}

#[derive(Debug, Error)]
pub enum IdentityProviderError {
    #[error("sandbox identity provider is unavailable: {0}")]
    Unavailable(String),
}

pub type IdentityProviderResult<T> = std::result::Result<T, IdentityProviderError>;

pub trait SandboxIdentityProvider: Send + Sync {
    fn next_identity(&self) -> IdentityProviderResult<SandboxIdentity>;
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EnvdMode {
    #[default]
    Broker,
    Runtime,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ResolvedTemplate {
    pub config: BoxConfig,
    pub envd_version: String,
    pub envd_mode: EnvdMode,
    pub routing: SandboxRoutePolicy,
    /// Validated runtime-managed filesystem lower for a dynamic snapshot
    /// template. Static configured templates leave this unset.
    #[serde(default)]
    pub rootfs_snapshot_id: Option<ExecutionSnapshotId>,
}

#[derive(Debug, Error)]
pub enum TemplateProviderError {
    #[error("sandbox template not found: {0}")]
    NotFound(String),
    #[error("sandbox template is invalid: {0}")]
    Invalid(String),
    #[error("sandbox template provider is unavailable: {0}")]
    Unavailable(String),
}

pub type TemplateProviderResult<T> = std::result::Result<T, TemplateProviderError>;

#[async_trait]
pub trait TemplateProvider: Send + Sync {
    async fn resolve(
        &self,
        owner_id: &str,
        template_id: &str,
    ) -> TemplateProviderResult<ResolvedTemplate>;
}
