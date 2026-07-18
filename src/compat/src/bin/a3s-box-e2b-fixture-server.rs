use std::collections::BTreeMap;
use std::num::NonZeroU16;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use a3s_box_compat::control::{
    Clock, ControlService, ControlServiceDependencies, IdentityProviderResult, IssuedToken,
    MemorySandboxRepository, ResolvedTemplate, SandboxIdentity, SandboxIdentityProvider,
    SecretToken, StoredToken, TemplateProvider, TemplateProviderError, TemplateProviderResult,
    TokenIssuer, TokenIssuerError, TokenIssuerResult, TokenResolver, TokenScope, TokenVerifier,
};
use a3s_box_compat::http::{
    lifecycle_router, AuthenticatedAccount, AuthenticationError, AuthenticationResult,
    CredentialScheme, CredentialVerifier, CursorDecoder, CursorError, CursorResult,
    LifecycleHttpConfig, LifecycleHttpState, PresentedCredential,
};
use a3s_box_compat::snapshot::{
    MemorySnapshotRepository, SnapshotService, SnapshotServiceDependencies,
    SnapshotTemplateProvider,
};
use a3s_box_compat::volume::{
    A3sRuntimeVolumeStore, IdentityVolumeIdMapper, MemoryVolumeRepository, VolumeFilesystem,
    VolumeService, VolumeServiceDependencies,
};
use a3s_box_core::{
    resolve_execution, BoxConfig, CreateExecutionRequest, ExecutionGeneration, ExecutionId,
    ExecutionIsolation, ExecutionLease, ExecutionManager, ExecutionManagerError,
    ExecutionManagerResult, ExecutionPortConnector, ExecutionPortStream, ExecutionReservation,
    ExecutionSnapshot, ExecutionSnapshotId, ExecutionState, ExecutionStatus, KillOutcome,
    OperationId, ReconcileOutcome, ResourceConfig,
};
use anyhow::{bail, Context, Result};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use sha2::{Digest, Sha256};

const API_KEY: &str = "e2b_a1b2c3";

#[tokio::main]
async fn main() {
    if let Err(error) = run().await {
        eprintln!("Error: {error:#}");
        std::process::exit(1);
    }
}

async fn run() -> Result<()> {
    let port_file = parse_port_file()?;
    let volume_home = port_file.with_extension("volume-runtime");
    let clock = Arc::new(FixedClock(fixture_time()?));
    let tokens = Arc::new(FixtureTokens);
    let executions = Arc::new(FixtureExecutionManager::new(clock.clone()));
    let snapshot_repository = Arc::new(MemorySnapshotRepository::default());
    let snapshots = Arc::new(SnapshotService::new(SnapshotServiceDependencies {
        repository: snapshot_repository.clone(),
        executions: executions.clone(),
        clock: clock.clone(),
    }));
    let templates = Arc::new(SnapshotTemplateProvider::new(
        Arc::new(FixtureTemplates),
        snapshot_repository,
    ));
    let volumes = Arc::new(VolumeService::new(VolumeServiceDependencies {
        repository: Arc::new(MemoryVolumeRepository::default()),
        runtime: Arc::new(A3sRuntimeVolumeStore::new(&volume_home)),
        clock: clock.clone(),
        token_issuer: tokens.clone(),
        token_resolver: tokens.clone(),
        token_verifier: tokens.clone(),
        filesystem: Arc::new(VolumeFilesystem::new(Arc::new(
            IdentityVolumeIdMapper::current(),
        ))),
    }));
    let service = Arc::new(
        ControlService::new(ControlServiceDependencies {
            repository: Arc::new(MemorySandboxRepository::default()),
            executions: executions.clone(),
            ports: executions,
            clock,
            identities: Arc::new(FixtureIdentities::default()),
            templates,
            token_issuer: tokens.clone(),
            token_resolver: tokens,
        })
        .with_volume_mount_resolver(volumes.clone())
        .with_snapshot_service(snapshots.clone()),
    );
    let state = LifecycleHttpState::new(
        service,
        Arc::new(FixtureCredentialVerifier),
        Arc::new(FixtureCursorDecoder),
        LifecycleHttpConfig {
            domain: Some("fixture.invalid".to_string()),
            ..LifecycleHttpConfig::default()
        },
    )
    .with_volume_service(volumes)
    .with_snapshot_service(snapshots);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("bind fixture listener")?;
    let address = listener.local_addr().context("read fixture address")?;
    tokio::fs::write(&port_file, address.port().to_string())
        .await
        .with_context(|| format!("write fixture port file {}", port_file.display()))?;
    let listener = listener
        .into_std()
        .context("convert fixture listener to std")?;
    axum::Server::from_tcp(listener)
        .context("create fixture HTTP server")?
        .serve(lifecycle_router(state).into_make_service())
        .await
        .context("serve fixture HTTP requests")
}

fn parse_port_file() -> Result<PathBuf> {
    let mut arguments = std::env::args().skip(1);
    let mut port_file = None;
    while let Some(argument) = arguments.next() {
        match argument.as_str() {
            "--port-file" => {
                port_file = Some(PathBuf::from(
                    arguments.next().context("--port-file requires a path")?,
                ));
            }
            _ => bail!("unknown argument {argument}"),
        }
    }
    port_file.context("--port-file is required")
}

fn fixture_time() -> Result<DateTime<Utc>> {
    Utc.with_ymd_and_hms(2026, 7, 14, 12, 0, 0)
        .single()
        .context("fixture timestamp is invalid")
}

struct FixedClock(DateTime<Utc>);

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

#[derive(Default)]
struct FixtureIdentities {
    next: AtomicU64,
}

impl SandboxIdentityProvider for FixtureIdentities {
    fn next_identity(&self) -> IdentityProviderResult<SandboxIdentity> {
        let sequence = self.next.fetch_add(1, Ordering::Relaxed) + 1;
        let sandbox_id = match sequence {
            1 => "fixture-sandbox".to_string(),
            2 => "fixture-restored".to_string(),
            3 => "fixture-interpreter".to_string(),
            value => format!("fixture-sandbox-{value}"),
        };
        Ok(SandboxIdentity {
            sandbox_id: a3s_box_compat::control::SandboxId::new(sandbox_id)
                .map_err(|error| fixture_identity_error(error.to_string()))?,
            operation_id: OperationId::new(format!("fixture-operation-{sequence}"))
                .map_err(|error| fixture_identity_error(error.to_string()))?,
        })
    }
}

fn fixture_identity_error(message: String) -> a3s_box_compat::control::IdentityProviderError {
    a3s_box_compat::control::IdentityProviderError::Unavailable(message)
}

struct FixtureTemplates;

#[async_trait]
impl TemplateProvider for FixtureTemplates {
    async fn resolve(
        &self,
        _owner_id: &str,
        template_id: &str,
    ) -> TemplateProviderResult<ResolvedTemplate> {
        if !matches!(template_id, "fixture-template" | "code-interpreter-v1") {
            return Err(TemplateProviderError::NotFound(template_id.to_string()));
        }
        Ok(ResolvedTemplate {
            config: BoxConfig {
                isolation: ExecutionIsolation::Sandbox,
                image: format!("fixture.invalid/{template_id}:latest"),
                resources: ResourceConfig {
                    vcpus: 2,
                    memory_mb: 512,
                    disk_mb: 1024,
                    timeout: 300,
                },
                ..BoxConfig::default()
            },
            envd_version: "0.1.3".to_string(),
            envd_mode: a3s_box_compat::control::EnvdMode::Broker,
            routing: if template_id == "code-interpreter-v1" {
                a3s_box_compat::routing::SandboxRoutePolicy::default()
                    .with_port(
                        a3s_box_compat::routing::CODE_INTERPRETER_PORT,
                        TokenScope::Traffic,
                    )
                    .map_err(|error| TemplateProviderError::Invalid(error.to_string()))?
            } else {
                a3s_box_compat::routing::SandboxRoutePolicy::default()
            },
            rootfs_snapshot_id: None,
        })
    }
}

struct FixtureTokens;

#[async_trait]
impl TokenIssuer for FixtureTokens {
    async fn issue(&self, scope: TokenScope) -> TokenIssuerResult<IssuedToken> {
        let secret = match scope {
            TokenScope::Envd => "fixture-envd-token",
            TokenScope::Traffic => "fixture-traffic-token",
            TokenScope::Volume => "fixture-volume-token",
        };
        Ok(IssuedToken {
            secret: SecretToken::new(secret)?,
            stored: store_fixture_token(secret)?,
        })
    }
}

#[async_trait]
impl TokenResolver for FixtureTokens {
    async fn resolve(
        &self,
        _scope: TokenScope,
        stored: &StoredToken,
    ) -> TokenIssuerResult<SecretToken> {
        let digest = Sha256::digest(stored.ciphertext());
        if &digest[..] != stored.digest() {
            return Err(TokenIssuerError::InvalidMaterial);
        }
        let value = std::str::from_utf8(stored.ciphertext())
            .map_err(|_| TokenIssuerError::InvalidMaterial)?;
        SecretToken::new(value)
    }
}

#[async_trait]
impl TokenVerifier for FixtureTokens {
    async fn verify(
        &self,
        scope: TokenScope,
        presented: &SecretToken,
        stored: &StoredToken,
    ) -> TokenIssuerResult<bool> {
        if scope != TokenScope::Volume {
            return Ok(false);
        }
        let digest = Sha256::digest(presented.expose_secret().as_bytes());
        Ok(digest[..] == stored.digest()[..])
    }
}

fn store_fixture_token(secret: &str) -> TokenIssuerResult<StoredToken> {
    let ciphertext = secret.as_bytes().to_vec();
    let digest = Sha256::digest(&ciphertext).to_vec();
    StoredToken::new(1, ciphertext, digest).map_err(|_| TokenIssuerError::InvalidMaterial)
}

struct FixtureCredentialVerifier;

#[async_trait]
impl CredentialVerifier for FixtureCredentialVerifier {
    async fn verify(
        &self,
        credential: &PresentedCredential,
    ) -> AuthenticationResult<AuthenticatedAccount> {
        if credential.scheme() != CredentialScheme::ApiKey || credential.expose_secret() != API_KEY
        {
            return Err(AuthenticationError::Invalid);
        }
        Ok(AuthenticatedAccount {
            owner_id: "fixture-owner".to_string(),
            client_id: "fixture-client".to_string(),
        })
    }
}

struct FixtureCursorDecoder;

impl CursorDecoder for FixtureCursorDecoder {
    fn decode(&self, value: &str) -> CursorResult<Option<a3s_box_compat::control::SandboxCursor>> {
        if value == "cursor-0" {
            Ok(None)
        } else {
            Err(CursorError::Invalid)
        }
    }
}

#[derive(Clone)]
struct FixtureExecution {
    lease: ExecutionLease,
    state: ExecutionState,
}

struct FixtureExecutionManager {
    clock: Arc<dyn Clock>,
    operations: Mutex<BTreeMap<String, String>>,
    executions: Mutex<BTreeMap<String, FixtureExecution>>,
    snapshots: Mutex<BTreeMap<String, u64>>,
}

impl FixtureExecutionManager {
    fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            operations: Mutex::new(BTreeMap::new()),
            executions: Mutex::new(BTreeMap::new()),
            snapshots: Mutex::new(BTreeMap::new()),
        }
    }

    fn operations(&self) -> ExecutionManagerResult<MutexGuard<'_, BTreeMap<String, String>>> {
        self.operations.lock().map_err(|_| {
            ExecutionManagerError::Unavailable("fixture operation lock poisoned".into())
        })
    }

    fn executions(
        &self,
    ) -> ExecutionManagerResult<MutexGuard<'_, BTreeMap<String, FixtureExecution>>> {
        self.executions.lock().map_err(|_| {
            ExecutionManagerError::Unavailable("fixture execution lock poisoned".into())
        })
    }

    fn snapshots(&self) -> ExecutionManagerResult<MutexGuard<'_, BTreeMap<String, u64>>> {
        self.snapshots.lock().map_err(|_| {
            ExecutionManagerError::Unavailable("fixture snapshot lock poisoned".into())
        })
    }

    fn reservation(execution: &FixtureExecution) -> ExecutionReservation {
        ExecutionReservation {
            execution_id: execution.lease.execution_id.clone(),
            generation: execution.lease.generation,
            plan: execution.lease.plan.clone(),
            resources: execution.lease.resources.clone(),
            created_at: execution.lease.started_at,
        }
    }
}

#[async_trait]
impl ExecutionManager for FixtureExecutionManager {
    async fn create(
        &self,
        request: CreateExecutionRequest,
        operation_id: &OperationId,
    ) -> ExecutionManagerResult<ExecutionReservation> {
        if let Some(execution_id) = self.operations()?.get(operation_id.as_str()).cloned() {
            return self
                .executions()?
                .get(&execution_id)
                .map(Self::reservation)
                .ok_or_else(|| {
                    ExecutionManagerError::Internal(
                        "fixture operation references a missing execution".into(),
                    )
                });
        }

        let plan = resolve_execution(&request.config)
            .map_err(|error| ExecutionManagerError::InvalidRequest(error.to_string()))?;
        let execution_id = ExecutionId::new(format!("execution-{}", operation_id.as_str()))?;
        let lease = ExecutionLease {
            execution_id: execution_id.clone(),
            generation: ExecutionGeneration::INITIAL,
            plan,
            resources: request.config.resources,
            started_at: self.clock.now(),
        };
        self.executions()?.insert(
            execution_id.to_string(),
            FixtureExecution {
                lease: lease.clone(),
                state: ExecutionState::Created,
            },
        );
        self.operations()?
            .insert(operation_id.as_str().to_string(), execution_id.to_string());
        Ok(ExecutionReservation {
            execution_id,
            generation: lease.generation,
            plan: lease.plan,
            resources: lease.resources,
            created_at: lease.started_at,
        })
    }

    async fn start(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let mut executions = self.executions()?;
        let execution = executions
            .get_mut(execution_id.as_str())
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        if execution.lease.generation != generation {
            return Err(ExecutionManagerError::Conflict {
                execution_id: execution_id.clone(),
                message: "stale fixture start".to_string(),
            });
        }
        match execution.state {
            ExecutionState::Created => execution.state = ExecutionState::Running,
            ExecutionState::Running => {}
            state => {
                return Err(ExecutionManagerError::Conflict {
                    execution_id: execution_id.clone(),
                    message: format!("cannot start fixture execution in state {state:?}"),
                });
            }
        }
        Ok(execution.lease.clone())
    }

    async fn inspect(&self, execution_id: &ExecutionId) -> ExecutionManagerResult<ExecutionStatus> {
        let executions = self.executions()?;
        let execution = executions
            .get(execution_id.as_str())
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        Ok(ExecutionStatus {
            execution_id: execution_id.clone(),
            generation: execution.lease.generation,
            state: execution.state,
            plan: execution.lease.plan.clone(),
        })
    }

    async fn read_logs(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<Vec<a3s_box_core::log::LogEntry>> {
        let executions = self.executions()?;
        let execution = executions
            .get(execution_id.as_str())
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        if execution.lease.generation != generation {
            return Err(ExecutionManagerError::Conflict {
                execution_id: execution_id.clone(),
                message: "stale fixture log read".to_string(),
            });
        }
        Ok([
            ("stdout", "starting\n", 0_i64),
            ("stderr", "failed once\n", 1_i64),
            ("stdout", "ready\n", 2_i64),
        ]
        .into_iter()
        .map(|(stream, message, offset)| a3s_box_core::log::LogEntry {
            log: message.to_string(),
            stream: stream.to_string(),
            time: (execution.lease.started_at + chrono::Duration::seconds(offset)).to_rfc3339(),
        })
        .collect())
    }

    async fn create_filesystem_snapshot(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<ExecutionSnapshot> {
        let mut executions = self.executions()?;
        let execution = executions
            .get_mut(execution_id.as_str())
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        if execution.lease.generation != generation
            || !matches!(
                execution.state,
                ExecutionState::Running | ExecutionState::Paused
            )
        {
            return Err(ExecutionManagerError::Conflict {
                execution_id: execution_id.clone(),
                message: "stale fixture snapshot".to_string(),
            });
        }
        let result = ExecutionSnapshot {
            snapshot_id: snapshot_id.clone(),
            size_bytes: 4_096,
            state: execution.state,
            lease: execution.lease.clone(),
        };
        drop(executions);
        self.snapshots()?
            .insert(snapshot_id.to_string(), result.size_bytes);
        Ok(result)
    }

    async fn filesystem_snapshot_size(
        &self,
        snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<Option<u64>> {
        Ok(self.snapshots()?.get(snapshot_id.as_str()).copied())
    }

    async fn delete_filesystem_snapshot(
        &self,
        snapshot_id: &ExecutionSnapshotId,
    ) -> ExecutionManagerResult<bool> {
        Ok(self.snapshots()?.remove(snapshot_id.as_str()).is_some())
    }

    async fn pause(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        _keep_memory: bool,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let mut executions = self.executions()?;
        let execution = executions
            .get_mut(execution_id.as_str())
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        if execution.lease.generation != generation || execution.state != ExecutionState::Running {
            return Err(ExecutionManagerError::Conflict {
                execution_id: execution_id.clone(),
                message: "stale fixture pause".to_string(),
            });
        }
        let next_generation = generation.get().checked_add(1).ok_or_else(|| {
            ExecutionManagerError::Internal("fixture execution generation is exhausted".into())
        })?;
        execution.lease.generation = ExecutionGeneration::new(next_generation)?;
        execution.state = ExecutionState::Paused;
        Ok(execution.lease.clone())
    }

    async fn resume(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let mut executions = self.executions()?;
        let execution = executions
            .get_mut(execution_id.as_str())
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        if execution.lease.generation != generation || execution.state != ExecutionState::Paused {
            return Err(ExecutionManagerError::Conflict {
                execution_id: execution_id.clone(),
                message: "stale fixture resume".to_string(),
            });
        }
        let next_generation = generation.get().checked_add(1).ok_or_else(|| {
            ExecutionManagerError::Internal("fixture execution generation is exhausted".into())
        })?;
        execution.lease.generation = ExecutionGeneration::new(next_generation)?;
        execution.state = ExecutionState::Running;
        Ok(execution.lease.clone())
    }

    async fn kill(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<KillOutcome> {
        let mut executions = self.executions()?;
        let execution = executions
            .get_mut(execution_id.as_str())
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        if execution.lease.generation != generation {
            return Err(ExecutionManagerError::Conflict {
                execution_id: execution_id.clone(),
                message: "stale fixture kill".to_string(),
            });
        }
        if execution.state == ExecutionState::Stopped {
            return Ok(KillOutcome::AlreadyStopped);
        }
        execution.state = ExecutionState::Stopped;
        Ok(KillOutcome::Killed)
    }

    async fn reconcile(
        &self,
        operation_id: &OperationId,
    ) -> ExecutionManagerResult<ReconcileOutcome> {
        let Some(execution_id) = self.operations()?.get(operation_id.as_str()).cloned() else {
            return Ok(ReconcileOutcome::Absent);
        };
        let executions = self.executions()?;
        let execution = executions.get(&execution_id).ok_or_else(|| {
            ExecutionManagerError::Internal(
                "fixture operation references a missing execution".into(),
            )
        })?;
        Ok(match execution.state {
            ExecutionState::Created => ReconcileOutcome::Created(Self::reservation(execution)),
            ExecutionState::Creating => ReconcileOutcome::Creating,
            ExecutionState::Running | ExecutionState::Paused => {
                ReconcileOutcome::Ready(execution.lease.clone())
            }
            ExecutionState::Stopped | ExecutionState::Failed => ReconcileOutcome::Failed,
        })
    }
}

#[async_trait]
impl ExecutionPortConnector for FixtureExecutionManager {
    async fn connect_port(
        &self,
        execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _port: NonZeroU16,
        _timeout: Duration,
    ) -> ExecutionManagerResult<ExecutionPortStream> {
        Err(ExecutionManagerError::NotFound(execution_id.clone()))
    }
}
