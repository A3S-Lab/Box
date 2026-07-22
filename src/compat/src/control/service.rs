use std::collections::BTreeMap;
use std::num::NonZeroU16;
use std::sync::Arc;

use a3s_box_core::{
    resolve_execution, CreateExecutionRequest, ExecutionLease, ExecutionManager,
    ExecutionManagerError, ExecutionPortConnector, NetworkMode,
};
use chrono::{DateTime, Utc};
use hyper::body::{Body, HttpBody};
use hyper::client::conn;
use hyper::header::{CONTENT_TYPE, HOST};
use hyper::{Method, Request, StatusCode};
use serde::{Deserialize, Serialize};
use thiserror::Error;
use tracing::{debug, error};

use super::lifetime::ready_lifetime;
use super::{
    Clock, CompareAndSwapResult, EnvdMode, IdentityProviderError, LifecycleError, LifecycleFailure,
    LifecyclePolicy, LifecycleState, NewSandboxRecord, RepositoryError, SandboxCredentials,
    SandboxIdentityProvider, SandboxListFilter, SandboxPage, SandboxRecord, SandboxRepository,
    SecretToken, TemplateProvider, TemplateProviderError, TokenIssuer, TokenIssuerError,
    TokenResolver, TokenScope,
};
use crate::routing::ENVD_PORT;
use crate::snapshot::{SnapshotRecord, SnapshotService, SnapshotServiceError};
use crate::volume::{ResolvedVolumeMount, VolumeMount, VolumeMountResolver, VolumeServiceError};

const RUNTIME_ENVD_READY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
const RUNTIME_ENVD_CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_millis(250);
const RUNTIME_ENVD_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const RUNTIME_ENVD_RETRY_INTERVAL: std::time::Duration = std::time::Duration::from_millis(25);
const RUNTIME_ENVD_DEFAULT_USER: &str = "user";

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

#[derive(Debug, Clone, PartialEq)]
pub struct SandboxMetric {
    pub timestamp: DateTime<Utc>,
    pub cpu_count: u32,
    pub cpu_used_pct: f32,
    pub mem_used: u64,
    pub mem_total: u64,
    pub mem_cache: u64,
    pub disk_used: u64,
    pub disk_total: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxLog {
    pub timestamp: DateTime<Utc>,
    pub stream: String,
    pub message: String,
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
    #[error(transparent)]
    Volume(#[from] VolumeServiceError),
    #[error(transparent)]
    Snapshot(#[from] SnapshotServiceError),
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
    volume_mounts: Option<Arc<dyn VolumeMountResolver>>,
    snapshots: Option<Arc<SnapshotService>>,
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
            volume_mounts: None,
            snapshots: None,
        }
    }

    pub fn with_volume_mount_resolver(mut self, resolver: Arc<dyn VolumeMountResolver>) -> Self {
        self.volume_mounts = Some(resolver);
        self
    }

    pub fn with_snapshot_service(mut self, snapshots: Arc<SnapshotService>) -> Self {
        self.snapshots = Some(snapshots);
        self
    }

    pub async fn create(
        &self,
        request: CreateSandboxRequest,
    ) -> ControlServiceResult<SandboxConnection> {
        self.create_with_mounts(request, Vec::new()).await
    }

    pub async fn create_with_mounts(
        &self,
        request: CreateSandboxRequest,
        volume_mounts: Vec<VolumeMount>,
    ) -> ControlServiceResult<SandboxConnection> {
        if request.template_id.trim().is_empty() {
            return Err(ControlServiceError::InvalidRequest(
                "template ID cannot be empty".to_string(),
            ));
        }

        let identity = self.identities.next_identity()?;
        let template = self
            .templates
            .resolve(&request.owner_id, &request.template_id)
            .await?;
        let mut config = template.config;
        let resolved_mounts = self
            .resolve_volume_mounts(&request.owner_id, &volume_mounts)
            .await?;
        config.volumes.extend(
            resolved_mounts
                .iter()
                .map(ResolvedVolumeMount::runtime_spec),
        );
        config.resources.timeout = u64::from(request.timeout_seconds);
        config.extra_env.extend(request.env_vars);
        let runtime_env_vars = config.extra_env.iter().cloned().collect::<BTreeMap<_, _>>();
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
        let (_, expires_at) = ready_lifetime(now, now, u64::from(request.timeout_seconds))
            .map_err(|error| ControlServiceError::InvalidRequest(error.to_string()))?;
        let envd = self.token_issuer.issue(TokenScope::Envd).await?;
        let traffic = self.token_issuer.issue(TokenScope::Traffic).await?;

        let mut record = SandboxRecord::creating_with_mounts(
            NewSandboxRecord {
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
                runtime_env_vars: runtime_env_vars.clone(),
                secure: request.secure,
                allow_internet_access: request.allow_internet_access,
                credentials: SandboxCredentials {
                    envd: envd.stored,
                    traffic: traffic.stored,
                },
                routing: template.routing,
            },
            volume_mounts,
        )?;
        self.repository.insert(record.clone()).await?;

        let mut policy = a3s_box_core::ExecutionRecordPolicy::default();
        for mount in &resolved_mounts {
            if !policy.volume_names.contains(&mount.runtime_name) {
                policy.volume_names.push(mount.runtime_name.clone());
            }
        }
        let execution_request = CreateExecutionRequest {
            external_sandbox_id: record.sandbox_id().to_string(),
            config,
            labels: request.metadata,
            policy,
            rootfs_snapshot_id: template.rootfs_snapshot_id,
        };
        let lease = match self
            .executions
            .create_and_start(execution_request, record.operation_id())
            .await
        {
            Ok(lease) => lease,
            Err(error) => {
                error!(
                    sandbox_id = %record.sandbox_id(),
                    %error,
                    "Sandbox runtime creation failed"
                );
                let expected = record.generation();
                record.mark_failed(LifecycleFailure::RuntimeFailed)?;
                self.replace(expected, record).await?;
                return Err(error.into());
            }
        };
        if template.envd_mode == EnvdMode::Runtime {
            if let Err(readiness_error) = self
                .initialize_runtime_envd(&lease, record.sandbox_id().as_str(), &runtime_env_vars)
                .await
            {
                error!(
                    sandbox_id = %record.sandbox_id(),
                    execution_id = %lease.execution_id,
                    error = %readiness_error,
                    "Sandbox runtime envd initialization failed"
                );
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

        let (ready_at, expires_at) = match ready_lifetime(
            self.clock.now(),
            lease.started_at,
            u64::from(request.timeout_seconds),
        ) {
            Ok(lifetime) => lifetime,
            Err(error) => {
                let cleanup = self
                    .executions
                    .kill(&lease.execution_id, lease.generation)
                    .await;
                let expected = record.generation();
                record.mark_failed(LifecycleFailure::RuntimeFailed)?;
                self.replace(expected, record).await?;
                return Err(match cleanup {
                    Ok(_) => ControlServiceError::InvalidRequest(error.to_string()),
                    Err(cleanup_error) => {
                        ControlServiceError::Execution(ExecutionManagerError::Internal(format!(
                            "{error}; runtime cleanup failed: {cleanup_error}"
                        )))
                    }
                });
            }
        };
        let expected = record.generation();
        if let Err(error) = record.mark_ready(lease, ready_at, expires_at) {
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

    async fn resolve_volume_mounts(
        &self,
        owner_id: &str,
        mounts: &[VolumeMount],
    ) -> ControlServiceResult<Vec<ResolvedVolumeMount>> {
        if mounts.is_empty() {
            return Ok(Vec::new());
        }
        let resolver = self.volume_mounts.as_ref().ok_or_else(|| {
            ControlServiceError::InvalidRequest(
                "volume mounts are unavailable in this service".to_string(),
            )
        })?;
        resolver
            .resolve_mounts(owner_id, mounts)
            .await
            .map_err(Into::into)
    }

    async fn initialize_runtime_envd(
        &self,
        lease: &ExecutionLease,
        lifecycle_id: &str,
        env_vars: &BTreeMap<String, String>,
    ) -> Result<(), ExecutionManagerError> {
        initialize_runtime_envd_for_lease(
            self.ports.as_ref(),
            self.clock.as_ref(),
            lease,
            lifecycle_id,
            env_vars,
        )
        .await
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
                let cold_resume = !record.paused_with_memory();
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
                        record.abort_resume()?;
                        self.replace(expected, record).await?;
                        return Err(error.into());
                    }
                };
                if cold_resume && record.envd_mode() == EnvdMode::Runtime {
                    if let Err(readiness_error) = self
                        .initialize_runtime_envd(
                            &lease,
                            record.sandbox_id().as_str(),
                            record.runtime_env_vars(),
                        )
                        .await
                    {
                        error!(
                            sandbox_id = %record.sandbox_id(),
                            execution_id = %lease.execution_id,
                            error = %readiness_error,
                            "Cold-resumed Sandbox runtime envd initialization failed"
                        );
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
                                "{readiness_error}; cold-resume runtime cleanup failed: {cleanup_error}"
                            )),
                        }
                        .into());
                    }
                }
                let expected = record.generation();
                record.mark_running(lease)?;
                self.replace(expected, record.clone()).await?;
                ConnectionDisposition::Resumed
            }
            _ => return Err(ControlServiceError::Conflict(sandbox_id.clone())),
        };

        let refreshed_expiry = expiry_from(self.clock.now(), timeout_seconds)?;
        if refreshed_expiry > record.expires_at() {
            let expected = record.generation();
            record.replace_expiry(refreshed_expiry)?;
            self.replace(expected, record.clone()).await?;
        }
        self.connection(record, disposition).await
    }

    pub async fn pause(
        &self,
        owner_id: &str,
        sandbox_id: &super::SandboxId,
        keep_memory: bool,
    ) -> ControlServiceResult<()> {
        let mut record = self.require_visible(owner_id, sandbox_id).await?;
        if record.state() != LifecycleState::Running {
            return Err(ControlServiceError::Conflict(sandbox_id.clone()));
        }
        let execution_id = record
            .execution_id()
            .cloned()
            .ok_or_else(|| ControlServiceError::Conflict(sandbox_id.clone()))?;
        let execution_generation = record
            .execution_generation()
            .ok_or_else(|| ControlServiceError::Conflict(sandbox_id.clone()))?;
        let expected = record.generation();
        record.begin_pause(keep_memory)?;
        self.replace(expected, record.clone()).await?;

        let lease = match self
            .executions
            .pause(&execution_id, execution_generation, keep_memory)
            .await
        {
            Ok(lease) => lease,
            Err(error) => {
                let expected = record.generation();
                record.abort_pause()?;
                self.replace(expected, record).await?;
                return Err(error.into());
            }
        };
        let expected = record.generation();
        record.mark_paused(lease)?;
        self.replace(expected, record).await
    }

    pub async fn resume(
        &self,
        owner_id: &str,
        sandbox_id: &super::SandboxId,
        timeout_seconds: u32,
        auto_pause: bool,
    ) -> ControlServiceResult<SandboxConnection> {
        let record = self.require_visible(owner_id, sandbox_id).await?;
        if record.state() != LifecycleState::Paused {
            return Err(ControlServiceError::Conflict(sandbox_id.clone()));
        }
        if auto_pause && record.lifecycle().on_timeout != super::OnTimeoutAction::Pause {
            return Err(ControlServiceError::InvalidRequest(
                "autoPause cannot change the sandbox lifecycle policy during resume".to_string(),
            ));
        }
        self.connect(owner_id, sandbox_id, timeout_seconds).await
    }

    pub async fn get(
        &self,
        owner_id: &str,
        sandbox_id: &super::SandboxId,
    ) -> ControlServiceResult<SandboxRecord> {
        self.require_visible(owner_id, sandbox_id).await
    }

    pub async fn create_snapshot(
        &self,
        owner_id: &str,
        sandbox_id: &super::SandboxId,
        name: Option<&str>,
    ) -> ControlServiceResult<SnapshotRecord> {
        let source = self.require_visible(owner_id, sandbox_id).await?;
        let template = self
            .templates
            .resolve(owner_id, source.template_id())
            .await?;
        let snapshots = self.snapshots.as_ref().ok_or_else(|| {
            ControlServiceError::InvalidRequest(
                "filesystem snapshots are unavailable in this service".to_string(),
            )
        })?;
        let pending = snapshots.capture(owner_id, &source, name, template).await?;
        Ok(snapshots.publish(pending).await?)
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

    pub async fn refresh_timeout(
        &self,
        owner_id: &str,
        sandbox_id: &super::SandboxId,
        timeout_seconds: u32,
    ) -> ControlServiceResult<()> {
        let mut record = self.require_visible(owner_id, sandbox_id).await?;
        let refreshed_expiry = expiry_from(self.clock.now(), timeout_seconds)?;
        if refreshed_expiry <= record.expires_at() {
            return Ok(());
        }
        let expected = record.generation();
        record.replace_expiry(refreshed_expiry)?;
        self.replace(expected, record).await
    }

    pub async fn current_metric(
        &self,
        owner_id: &str,
        sandbox_id: &super::SandboxId,
    ) -> ControlServiceResult<Option<SandboxMetric>> {
        let record = self.require_visible(owner_id, sandbox_id).await?;
        if record.state() != LifecycleState::Running || record.envd_mode() != EnvdMode::Runtime {
            return Ok(None);
        }
        let execution_id = record
            .execution_id()
            .ok_or_else(|| ControlServiceError::Conflict(sandbox_id.clone()))?;
        let generation = record
            .execution_generation()
            .ok_or_else(|| ControlServiceError::Conflict(sandbox_id.clone()))?;
        let port = NonZeroU16::new(ENVD_PORT).ok_or_else(|| {
            ExecutionManagerError::Internal("envd port must be non-zero".to_string())
        })?;
        let stream = self
            .ports
            .connect_port(execution_id, generation, port, RUNTIME_ENVD_CONNECT_TIMEOUT)
            .await?;
        let metrics = tokio::time::timeout(
            RUNTIME_ENVD_REQUEST_TIMEOUT,
            read_runtime_envd_metrics(stream),
        )
        .await
        .map_err(|_| {
            ExecutionManagerError::Unavailable(format!(
                "runtime envd metrics timed out after {} ms",
                RUNTIME_ENVD_REQUEST_TIMEOUT.as_millis()
            ))
        })?
        .map_err(ExecutionManagerError::Unavailable)?;
        let timestamp = DateTime::from_timestamp(metrics.ts, 0).ok_or_else(|| {
            ExecutionManagerError::Internal(format!(
                "runtime envd returned an invalid metrics timestamp {}",
                metrics.ts
            ))
        })?;
        Ok(Some(SandboxMetric {
            timestamp,
            cpu_count: metrics.cpu_count,
            cpu_used_pct: metrics.cpu_used_pct,
            mem_used: metrics.mem_used,
            mem_total: metrics.mem_total,
            mem_cache: 0,
            disk_used: metrics.disk_used,
            disk_total: metrics.disk_total,
        }))
    }

    pub async fn logs(
        &self,
        owner_id: &str,
        sandbox_id: &super::SandboxId,
    ) -> ControlServiceResult<Vec<SandboxLog>> {
        let record = self.require_visible(owner_id, sandbox_id).await?;
        let execution_id = record
            .execution_id()
            .ok_or_else(|| ControlServiceError::Conflict(sandbox_id.clone()))?;
        let generation = record
            .execution_generation()
            .ok_or_else(|| ControlServiceError::Conflict(sandbox_id.clone()))?;
        self.executions
            .read_logs(execution_id, generation)
            .await?
            .into_iter()
            .map(|entry| -> ControlServiceResult<SandboxLog> {
                let timestamp = DateTime::parse_from_rfc3339(&entry.time)
                    .map(|value| value.with_timezone(&Utc))
                    .map_err(|error| {
                        ExecutionManagerError::Internal(format!(
                            "runtime returned an invalid structured log timestamp: {error}"
                        ))
                    })?;
                if !matches!(entry.stream.as_str(), "stdout" | "stderr") {
                    return Err(ExecutionManagerError::Internal(format!(
                        "runtime returned an invalid structured log stream {}",
                        entry.stream
                    ))
                    .into());
                }
                Ok(SandboxLog {
                    timestamp,
                    stream: entry.stream,
                    message: entry.log.trim_end_matches(['\r', '\n']).to_string(),
                })
            })
            .collect()
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

pub(super) async fn initialize_runtime_envd_for_lease(
    ports: &dyn ExecutionPortConnector,
    clock: &dyn Clock,
    lease: &ExecutionLease,
    lifecycle_id: &str,
    env_vars: &BTreeMap<String, String>,
) -> Result<(), ExecutionManagerError> {
    let port = NonZeroU16::new(ENVD_PORT)
        .ok_or_else(|| ExecutionManagerError::Internal("envd port must be non-zero".to_string()))?;
    let deadline = tokio::time::Instant::now() + RUNTIME_ENVD_READY_TIMEOUT;
    loop {
        let last_error = match ports
            .connect_port(
                &lease.execution_id,
                lease.generation,
                port,
                RUNTIME_ENVD_CONNECT_TIMEOUT,
            )
            .await
        {
            Ok(stream) => match tokio::time::timeout(
                RUNTIME_ENVD_REQUEST_TIMEOUT,
                send_runtime_envd_init(
                    stream,
                    RuntimeEnvdInitRequest {
                        lifecycle_id,
                        env_vars,
                        timestamp: clock.now(),
                        default_user: RUNTIME_ENVD_DEFAULT_USER,
                    },
                ),
            )
            .await
            {
                Ok(Ok(StatusCode::NO_CONTENT)) => return Ok(()),
                Ok(Ok(status)) if status.is_client_error() => {
                    return Err(ExecutionManagerError::Internal(format!(
                        "runtime envd initialization returned HTTP {status}"
                    )))
                }
                Ok(Ok(status)) => {
                    format!("runtime envd initialization returned HTTP {status}")
                }
                Ok(Err(error)) => error,
                Err(_) => format!(
                    "runtime envd initialization timed out after {} ms",
                    RUNTIME_ENVD_REQUEST_TIMEOUT.as_millis()
                ),
            },
            Err(error @ ExecutionManagerError::InvalidRequest(_))
            | Err(error @ ExecutionManagerError::Internal(_)) => return Err(error),
            Err(error) => error.to_string(),
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

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct RuntimeEnvdInitRequest<'a> {
    #[serde(rename = "lifecycleID")]
    lifecycle_id: &'a str,
    env_vars: &'a BTreeMap<String, String>,
    timestamp: DateTime<Utc>,
    default_user: &'a str,
}

#[derive(Deserialize)]
struct RuntimeEnvdMetrics {
    ts: i64,
    cpu_count: u32,
    cpu_used_pct: f32,
    mem_used: u64,
    mem_total: u64,
    disk_used: u64,
    disk_total: u64,
}

const RUNTIME_ENVD_METRICS_MAX_BYTES: usize = 64 * 1024;

async fn send_runtime_envd_init(
    stream: a3s_box_core::ExecutionPortStream,
    init: RuntimeEnvdInitRequest<'_>,
) -> Result<StatusCode, String> {
    let payload = serde_json::to_vec(&init)
        .map_err(|error| format!("failed to encode runtime envd initialization: {error}"))?;
    let request = Request::builder()
        .method(Method::POST)
        .uri("/init")
        .header(HOST, "127.0.0.1")
        .header(CONTENT_TYPE, "application/json")
        .body(Body::from(payload))
        .map_err(|error| format!("failed to build runtime envd initialization: {error}"))?;
    send_runtime_envd_request(stream, request)
        .await
        .map(|response| response.status())
}

async fn read_runtime_envd_metrics(
    stream: a3s_box_core::ExecutionPortStream,
) -> Result<RuntimeEnvdMetrics, String> {
    let request = Request::builder()
        .method(Method::GET)
        .uri("/metrics")
        .header(HOST, "127.0.0.1")
        .body(Body::empty())
        .map_err(|error| format!("failed to build runtime envd metrics request: {error}"))?;
    let response = send_runtime_envd_request(stream, request).await?;
    if response.status() != StatusCode::OK {
        return Err(format!(
            "runtime envd metrics returned HTTP {}",
            response.status()
        ));
    }
    let mut response_body = response.into_body();
    let mut body = Vec::new();
    while let Some(chunk) = response_body.data().await {
        let chunk =
            chunk.map_err(|error| format!("failed to read runtime envd metrics: {error}"))?;
        if body.len().saturating_add(chunk.len()) > RUNTIME_ENVD_METRICS_MAX_BYTES {
            return Err(format!(
                "runtime envd metrics exceeded {RUNTIME_ENVD_METRICS_MAX_BYTES} bytes"
            ));
        }
        body.extend_from_slice(&chunk);
    }
    serde_json::from_slice(&body)
        .map_err(|error| format!("runtime envd returned invalid metrics JSON: {error}"))
}

async fn send_runtime_envd_request(
    stream: a3s_box_core::ExecutionPortStream,
    request: Request<Body>,
) -> Result<hyper::Response<Body>, String> {
    let (mut sender, connection) = conn::Builder::new()
        .handshake(stream)
        .await
        .map_err(|error| format!("runtime envd HTTP handshake failed: {error}"))?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            debug!(%error, "runtime envd HTTP connection closed");
        }
    });
    sender
        .send_request(request)
        .await
        .map_err(|error| format!("runtime envd request failed: {error}"))
}

fn expiry_from(now: DateTime<Utc>, timeout_seconds: u32) -> ControlServiceResult<DateTime<Utc>> {
    ready_lifetime(now, now, u64::from(timeout_seconds))
        .map(|(_, expires_at)| expires_at)
        .map_err(|error| ControlServiceError::InvalidRequest(error.to_string()))
}
