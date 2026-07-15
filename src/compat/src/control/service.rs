use std::collections::BTreeMap;
use std::num::NonZeroU16;
use std::sync::Arc;

use a3s_box_core::{
    resolve_execution, CreateExecutionRequest, ExecutionLease, ExecutionManager,
    ExecutionManagerError, ExecutionPortConnector, NetworkMode,
};
use chrono::{DateTime, Duration, Utc};
use thiserror::Error;

use super::{
    Clock, CompareAndSwapResult, EnvdMode, IdentityProviderError, LifecycleError, LifecycleFailure,
    LifecyclePolicy, LifecycleState, NewSandboxRecord, RepositoryError, SandboxCredentials,
    SandboxIdentityProvider, SandboxListFilter, SandboxPage, SandboxRecord, SandboxRepository,
    SecretToken, TemplateProvider, TemplateProviderError, TokenIssuer, TokenIssuerError,
    TokenResolver, TokenScope,
};
use crate::routing::ENVD_PORT;

const RUNTIME_ENVD_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const RUNTIME_ENVD_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);
const RUNTIME_ENVD_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);

#[derive(Debug, Clone)]
pub struct CreateSandboxRequest {
    pub owner_id: String,
    pub template_id: String,
    pub timeout_seconds: u32,
    pub lifecycle: LifecyclePolicy,
    pub metadata: BTreeMap<String, String>,
    pub env_vars: BTreeMap<String, String>,
    pub secure: bool,
    pub allow_internet_access: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ConnectionDisposition {
    Created,
    AlreadyRunning,
    Resumed,
}

pub struct SandboxConnection {
    pub record: SandboxRecord,
    pub envd_access_token: SecretToken,
    pub traffic_access_token: SecretToken,
    pub disposition: ConnectionDisposition,
}

impl std::fmt::Debug for SandboxConnection {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("SandboxConnection")
            .field("record", &self.record)
            .field("envd_access_token", &self.envd_access_token)
            .field("traffic_access_token", &self.traffic_access_token)
            .field("disposition", &self.disposition)
            .finish()
    }
}

#[derive(Debug, Error)]
pub enum ControlServiceError {
    #[error("invalid sandbox request: {0}")]
    InvalidRequest(String),
    #[error("sandbox not found: {0}")]
    NotFound(super::SandboxId),
    #[error("sandbox lifecycle conflict: {0}")]
    Conflict(super::SandboxId),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    Execution(#[from] a3s_box_core::ExecutionManagerError),
    #[error(transparent)]
    Identity(#[from] IdentityProviderError),
    #[error(transparent)]
    Template(#[from] TemplateProviderError),
    #[error(transparent)]
    Credential(#[from] TokenIssuerError),
    #[error("sandbox lifecycle failed: {0}")]
    Lifecycle(#[from] LifecycleError),
}

pub type ControlServiceResult<T> = std::result::Result<T, ControlServiceError>;

#[derive(Clone)]
pub struct ControlService {
    repository: Arc<dyn SandboxRepository>,
    executions: Arc<dyn ExecutionManager>,
    ports: Arc<dyn ExecutionPortConnector>,
    clock: Arc<dyn Clock>,
    identities: Arc<dyn SandboxIdentityProvider>,
    templates: Arc<dyn TemplateProvider>,
    token_issuer: Arc<dyn TokenIssuer>,
    token_resolver: Arc<dyn TokenResolver>,
}

pub struct ControlServiceDependencies {
    pub repository: Arc<dyn SandboxRepository>,
    pub executions: Arc<dyn ExecutionManager>,
    pub ports: Arc<dyn ExecutionPortConnector>,
    pub clock: Arc<dyn Clock>,
    pub identities: Arc<dyn SandboxIdentityProvider>,
    pub templates: Arc<dyn TemplateProvider>,
    pub token_issuer: Arc<dyn TokenIssuer>,
    pub token_resolver: Arc<dyn TokenResolver>,
}

impl ControlService {
    pub fn new(dependencies: ControlServiceDependencies) -> Self {
        Self {
            repository: dependencies.repository,
            executions: dependencies.executions,
            ports: dependencies.ports,
            clock: dependencies.clock,
            identities: dependencies.identities,
            templates: dependencies.templates,
            token_issuer: dependencies.token_issuer,
            token_resolver: dependencies.token_resolver,
        }
    }

    pub async fn create(
        &self,
        request: CreateSandboxRequest,
    ) -> ControlServiceResult<SandboxConnection> {
        if request.template_id.trim().is_empty() {
            return Err(ControlServiceError::InvalidRequest(
                "template ID cannot be empty".to_string(),
            ));
        }

        let identity = self.identities.next_identity()?;
        let template = self.templates.resolve(&request.template_id).await?;
        let mut config = template.config;
        config.resources.timeout = u64::from(request.timeout_seconds);
        config.extra_env.extend(request.env_vars);
        match request.allow_internet_access {
            Some(false) => config.network = NetworkMode::None,
            Some(true) if matches!(config.network, NetworkMode::None) => {
                config.network = NetworkMode::Tsi;
            }
            _ => {}
        }
        let plan = resolve_execution(&config)
            .map_err(|error| ControlServiceError::InvalidRequest(error.to_string()))?;
        let now = self.clock.now();
        let expires_at = expiry_from(now, request.timeout_seconds)?;
        let envd = self.token_issuer.issue(TokenScope::Envd).await?;
        let traffic = self.token_issuer.issue(TokenScope::Traffic).await?;

        let mut record = SandboxRecord::creating(NewSandboxRecord {
            sandbox_id: identity.sandbox_id,
            operation_id: identity.operation_id,
            owner_id: request.owner_id,
            template_id: request.template_id,
            plan,
            resources: config.resources.clone(),
            lifecycle: request.lifecycle,
            created_at: now,
            expires_at,
            metadata: request.metadata.clone(),
            envd_version: template.envd_version,
            envd_mode: template.envd_mode,
            secure: request.secure,
            allow_internet_access: request.allow_internet_access,
            credentials: SandboxCredentials {
                envd: envd.stored,
                traffic: traffic.stored,
            },
            routing: template.routing,
        })?;
        self.repository.insert(record.clone()).await?;

        let execution_request = CreateExecutionRequest {
            external_sandbox_id: record.sandbox_id().to_string(),
            config,
            labels: request.metadata,
            policy: Default::default(),
        };
        let lease = match self
            .executions
            .create_and_start(execution_request, record.operation_id())
            .await
        {
            Ok(lease) => lease,
            Err(error) => {
                let expected = record.generation();
                record.mark_failed(LifecycleFailure::RuntimeFailed)?;
                self.replace(expected, record).await?;
                return Err(error.into());
            }
        };
        if template.envd_mode == EnvdMode::Runtime {
            if let Err(readiness_error) = self.wait_for_runtime_envd(&lease).await {
                let cleanup = self
                    .executions
                    .kill(&lease.execution_id, lease.generation)
                    .await;
                let expected = record.generation();
                record.mark_failed(LifecycleFailure::RuntimeFailed)?;
                self.replace(expected, record).await?;
                return Err(match cleanup {
                    Ok(_) => readiness_error,
                    Err(cleanup_error) => ExecutionManagerError::Internal(format!(
                        "{readiness_error}; runtime cleanup failed: {cleanup_error}"
                    )),
                }
                .into());
            }
        }

        let expected = record.generation();
        if let Err(error) = record.mark_running(lease) {
            record.mark_failed(LifecycleFailure::RuntimeFailed)?;
            self.replace(expected, record).await?;
            return Err(error.into());
        }
        self.replace(expected, record.clone()).await?;

        Ok(SandboxConnection {
            record,
            envd_access_token: envd.secret,
            traffic_access_token: traffic.secret,
            disposition: ConnectionDisposition::Created,
        })
    }

    async fn wait_for_runtime_envd(
        &self,
        lease: &ExecutionLease,
    ) -> Result<(), ExecutionManagerError> {
        let port = NonZeroU16::new(ENVD_PORT).ok_or_else(|| {
            ExecutionManagerError::Internal("envd port must be non-zero".to_string())
        })?;
        let deadline = tokio::time::Instant::now() + RUNTIME_ENVD_READY_TIMEOUT;
        loop {
            let last_error = match self
                .ports
                .connect_port(
                    &lease.execution_id,
                    lease.generation,
                    port,
                    RUNTIME_ENVD_CONNECT_TIMEOUT,
                )
                .await
            {
                Ok(stream) => {
                    drop(stream);
                    return Ok(());
                }
                Err(error @ ExecutionManagerError::InvalidRequest(_))
                | Err(error @ ExecutionManagerError::Internal(_)) => return Err(error),
                Err(error) => error,
            };
            if tokio::time::Instant::now() >= deadline {
                return Err(ExecutionManagerError::Unavailable(format!(
                    "runtime envd did not become ready within {} seconds: {}",
                    RUNTIME_ENVD_READY_TIMEOUT.as_secs(),
                    last_error
                )));
            }
            tokio::time::sleep(RUNTIME_ENVD_RETRY_INTERVAL).await;
        }
    }

    pub async fn connect(
        &self,
        owner_id: &str,
        sandbox_id: &super::SandboxId,
        timeout_seconds: u32,
    ) -> ControlServiceResult<SandboxConnection> {
        let mut record = self.require_visible(owner_id, sandbox_id).await?;
        let disposition = match record.state() {
            LifecycleState::Running => ConnectionDisposition::AlreadyRunning,
            LifecycleState::Paused => {
                let execution_id = record
                    .execution_id()
                    .cloned()
                    .ok_or_else(|| ControlServiceError::Conflict(sandbox_id.clone()))?;
                let execution_generation = record
                    .execution_generation()
                    .ok_or_else(|| ControlServiceError::Conflict(sandbox_id.clone()))?;
                let expected = record.generation();
                record.begin_resume()?;
                self.replace(expected, record.clone()).await?;

                let lease = match self
                    .executions
                    .resume(&execution_id, execution_generation)
                    .await
                {
                    Ok(lease) => lease,
                    Err(error) => {
                        let expected = record.generation();
                        record.mark_failed(LifecycleFailure::RuntimeFailed)?;
                        self.replace(expected, record).await?;
                        return Err(error.into());
                    }
                };
                let expected = record.generation();
                record.mark_running(lease)?;
                self.replace(expected, record.clone()).await?;
                ConnectionDisposition::Resumed
            }
            _ => return Err(ControlServiceError::Conflict(sandbox_id.clone())),
        };

        let expected = record.generation();
        record.replace_expiry(expiry_from(self.clock.now(), timeout_seconds)?)?;
        self.replace(expected, record.clone()).await?;
        self.connection(record, disposition).await
    }

    pub async fn get(
        &self,
        owner_id: &str,
        sandbox_id: &super::SandboxId,
    ) -> ControlServiceResult<SandboxRecord> {
        self.require_visible(owner_id, sandbox_id).await
    }

    pub async fn list(&self, filter: &SandboxListFilter) -> ControlServiceResult<SandboxPage> {
        Ok(self.repository.list(filter).await?)
    }

    pub async fn set_timeout(
        &self,
        owner_id: &str,
        sandbox_id: &super::SandboxId,
        timeout_seconds: u32,
    ) -> ControlServiceResult<()> {
        let mut record = self.require_visible(owner_id, sandbox_id).await?;
        let expected = record.generation();
        record.replace_expiry(expiry_from(self.clock.now(), timeout_seconds)?)?;
        self.replace(expected, record).await
    }

    pub async fn kill(
        &self,
        owner_id: &str,
        sandbox_id: &super::SandboxId,
    ) -> ControlServiceResult<bool> {
        let Some(mut record) = self.repository.get(sandbox_id).await? else {
            return Ok(false);
        };
        if record.owner_id() != owner_id || record.is_terminal() {
            return Ok(false);
        }

        let execution = record
            .execution_id()
            .cloned()
            .zip(record.execution_generation());
        let expected = record.generation();
        record.begin_kill()?;
        self.replace(expected, record.clone()).await?;

        if let Some((execution_id, generation)) = execution {
            self.executions.kill(&execution_id, generation).await?;
        }

        let expected = record.generation();
        record.mark_killed()?;
        self.replace(expected, record).await?;
        Ok(true)
    }

    async fn require_visible(
        &self,
        owner_id: &str,
        sandbox_id: &super::SandboxId,
    ) -> ControlServiceResult<SandboxRecord> {
        let record = self
            .repository
            .get(sandbox_id)
            .await?
            .ok_or_else(|| ControlServiceError::NotFound(sandbox_id.clone()))?;
        if record.owner_id() != owner_id || record.public_state().is_none() {
            return Err(ControlServiceError::NotFound(sandbox_id.clone()));
        }
        Ok(record)
    }

    async fn connection(
        &self,
        record: SandboxRecord,
        disposition: ConnectionDisposition,
    ) -> ControlServiceResult<SandboxConnection> {
        let envd_access_token = self
            .token_resolver
            .resolve(TokenScope::Envd, &record.credentials().envd)
            .await?;
        let traffic_access_token = self
            .token_resolver
            .resolve(TokenScope::Traffic, &record.credentials().traffic)
            .await?;
        Ok(SandboxConnection {
            record,
            envd_access_token,
            traffic_access_token,
            disposition,
        })
    }

    async fn replace(
        &self,
        expected: super::SandboxGeneration,
        replacement: SandboxRecord,
    ) -> ControlServiceResult<()> {
        let sandbox_id = replacement.sandbox_id().clone();
        match self
            .repository
            .compare_and_swap(&sandbox_id, expected, replacement)
            .await?
        {
            CompareAndSwapResult::Updated => Ok(()),
            CompareAndSwapResult::NotFound => Err(ControlServiceError::NotFound(sandbox_id)),
            CompareAndSwapResult::Conflict { .. } => Err(ControlServiceError::Conflict(sandbox_id)),
        }
    }
}

fn expiry_from(now: DateTime<Utc>, timeout_seconds: u32) -> ControlServiceResult<DateTime<Utc>> {
    now.checked_add_signed(Duration::seconds(i64::from(timeout_seconds)))
        .ok_or_else(|| ControlServiceError::InvalidRequest("sandbox timeout is too large".into()))
}
