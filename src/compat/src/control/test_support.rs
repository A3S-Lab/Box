use std::collections::BTreeMap;
use std::convert::Infallible;
use std::num::NonZeroU16;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a3s_box_core::{
    resolve_execution, BoxConfig, CreateExecutionRequest, ExecutionGeneration, ExecutionId,
    ExecutionIsolation, ExecutionLease, ExecutionManager, ExecutionManagerError,
    ExecutionManagerResult, ExecutionPortConnector, ExecutionPortStream, ExecutionReservation,
    ExecutionState, ExecutionStatus, KillOutcome, NetworkMode, OperationId, ReconcileOutcome,
    ResourceConfig,
};
use async_trait::async_trait;
use chrono::{DateTime, TimeZone, Utc};
use sha2::{Digest, Sha256};

use super::*;

pub(crate) struct TestHarness {
    pub service: Arc<ControlService>,
    pub executions: Arc<RecordingExecutionManager>,
}

impl TestHarness {
    pub fn new() -> Self {
        let clock = Arc::new(FixedClock(test_time()));
        let executions = Arc::new(RecordingExecutionManager::new(clock.clone()));
        let tokens = Arc::new(TestTokens);
        let service = Arc::new(ControlService::new(ControlServiceDependencies {
            repository: Arc::new(MemorySandboxRepository::default()),
            executions: executions.clone(),
            ports: executions.clone(),
            clock,
            identities: Arc::new(TestIdentities::default()),
            templates: Arc::new(TestTemplates),
            token_issuer: tokens.clone(),
            token_resolver: tokens,
        }));
        Self {
            service,
            executions,
        }
    }
}

pub(crate) fn create_request(owner_id: &str) -> CreateSandboxRequest {
    CreateSandboxRequest {
        owner_id: owner_id.to_string(),
        template_id: "fixture-template".to_string(),
        timeout_seconds: 321,
        lifecycle: LifecyclePolicy {
            on_timeout: OnTimeoutAction::Pause,
            auto_resume: false,
            keep_memory_on_pause: false,
        },
        metadata: BTreeMap::from([
            ("purpose".to_string(), "fixture".to_string()),
            ("team".to_string(), "alpha beta".to_string()),
        ]),
        env_vars: BTreeMap::from([
            ("ALPHA".to_string(), "one".to_string()),
            ("BETA".to_string(), "two".to_string()),
        ]),
        secure: true,
        allow_internet_access: Some(false),
    }
}

pub(crate) fn test_time() -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 14, 12, 0, 0)
        .single()
        .unwrap()
}

struct FixedClock(DateTime<Utc>);

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

#[derive(Default)]
struct TestIdentities {
    sequence: AtomicU64,
}

impl SandboxIdentityProvider for TestIdentities {
    fn next_identity(&self) -> IdentityProviderResult<SandboxIdentity> {
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        Ok(SandboxIdentity {
            sandbox_id: SandboxId::new(format!("sandbox-{sequence}")).unwrap(),
            operation_id: OperationId::new(format!("operation-{sequence}")).unwrap(),
        })
    }
}

struct TestTemplates;

#[async_trait]
impl TemplateProvider for TestTemplates {
    async fn resolve(&self, template_id: &str) -> TemplateProviderResult<ResolvedTemplate> {
        if !matches!(
            template_id,
            "fixture-template" | "code-interpreter-v1" | "runtime-envd-template"
        ) {
            return Err(TemplateProviderError::NotFound(template_id.to_string()));
        }
        Ok(ResolvedTemplate {
            config: BoxConfig {
                isolation: ExecutionIsolation::Sandbox,
                resources: ResourceConfig {
                    vcpus: 2,
                    memory_mb: 512,
                    disk_mb: 1024,
                    timeout: 300,
                },
                ..BoxConfig::default()
            },
            envd_version: "0.1.3".to_string(),
            envd_mode: if template_id == "runtime-envd-template" {
                EnvdMode::Runtime
            } else {
                EnvdMode::Broker
            },
            routing: if template_id == "code-interpreter-v1" {
                crate::routing::SandboxRoutePolicy::default()
                    .with_port(crate::routing::CODE_INTERPRETER_PORT, TokenScope::Traffic)
                    .unwrap()
            } else {
                crate::routing::SandboxRoutePolicy::default()
            },
        })
    }
}

struct TestTokens;

#[async_trait]
impl TokenIssuer for TestTokens {
    async fn issue(&self, scope: TokenScope) -> TokenIssuerResult<IssuedToken> {
        let secret = match scope {
            TokenScope::Envd => "fixture-envd-token",
            TokenScope::Traffic => "fixture-traffic-token",
        };
        Ok(IssuedToken {
            secret: SecretToken::new(secret)?,
            stored: stored_token(secret),
        })
    }
}

#[async_trait]
impl TokenResolver for TestTokens {
    async fn resolve(
        &self,
        _scope: TokenScope,
        stored: &StoredToken,
    ) -> TokenIssuerResult<SecretToken> {
        SecretToken::new(
            std::str::from_utf8(stored.ciphertext())
                .map_err(|_| TokenIssuerError::InvalidMaterial)?,
        )
    }
}

fn stored_token(secret: &str) -> StoredToken {
    let ciphertext = secret.as_bytes().to_vec();
    let digest = Sha256::digest(&ciphertext).to_vec();
    StoredToken::new(1, ciphertext, digest).unwrap()
}

#[derive(Clone)]
struct TestExecution {
    lease: ExecutionLease,
    state: ExecutionState,
}

pub(crate) struct RecordingExecutionManager {
    clock: Arc<dyn Clock>,
    fail_create: AtomicBool,
    fail_ports: AtomicBool,
    port_requests: Mutex<Vec<(String, u64, u16)>>,
    runtime_envd_status: AtomicU16,
    runtime_envd_requests: Arc<Mutex<Vec<(String, String, serde_json::Value)>>>,
    requests: Mutex<Vec<CreateExecutionRequest>>,
    operations: Mutex<BTreeMap<String, String>>,
    executions: Mutex<BTreeMap<String, TestExecution>>,
}

impl RecordingExecutionManager {
    pub(crate) fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            fail_create: AtomicBool::new(false),
            fail_ports: AtomicBool::new(false),
            port_requests: Mutex::new(Vec::new()),
            runtime_envd_status: AtomicU16::new(hyper::StatusCode::NO_CONTENT.as_u16()),
            runtime_envd_requests: Arc::new(Mutex::new(Vec::new())),
            requests: Mutex::new(Vec::new()),
            operations: Mutex::new(BTreeMap::new()),
            executions: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn requests(&self) -> Vec<CreateExecutionRequest> {
        self.requests.lock().unwrap().clone()
    }

    pub fn fail_create(&self) {
        self.fail_create.store(true, Ordering::Relaxed);
    }

    pub fn fail_ports(&self) {
        self.fail_ports.store(true, Ordering::Relaxed);
    }

    pub fn fail_runtime_envd_init(&self) {
        self.runtime_envd_status
            .store(hyper::StatusCode::BAD_REQUEST.as_u16(), Ordering::Relaxed);
    }

    pub fn port_requests(&self) -> Vec<(String, u64, u16)> {
        self.port_requests.lock().unwrap().clone()
    }

    pub fn runtime_envd_requests(&self) -> Vec<(String, String, serde_json::Value)> {
        self.runtime_envd_requests.lock().unwrap().clone()
    }

    pub fn execution_state(&self, execution_id: &str) -> Option<ExecutionState> {
        self.executions
            .lock()
            .unwrap()
            .get(execution_id)
            .map(|execution| execution.state)
    }

    fn reservation(execution: &TestExecution) -> ExecutionReservation {
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
impl ExecutionPortConnector for RecordingExecutionManager {
    async fn connect_port(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        port: NonZeroU16,
        _timeout: Duration,
    ) -> ExecutionManagerResult<ExecutionPortStream> {
        self.port_requests.lock().unwrap().push((
            execution_id.to_string(),
            generation.get(),
            port.get(),
        ));
        if self.fail_ports.load(Ordering::Relaxed) {
            return Err(ExecutionManagerError::InvalidRequest(
                "test runtime envd is unavailable".to_string(),
            ));
        }
        let (stream, peer) = tokio::io::duplex(64 * 1024);
        let status =
            hyper::StatusCode::from_u16(self.runtime_envd_status.load(Ordering::Relaxed)).unwrap();
        let requests = self.runtime_envd_requests.clone();
        tokio::spawn(async move {
            let service =
                hyper::service::service_fn(move |request: hyper::Request<hyper::Body>| {
                    let requests = requests.clone();
                    async move {
                        let method = request.method().to_string();
                        let path = request.uri().path().to_string();
                        let body = hyper::body::to_bytes(request.into_body()).await.unwrap();
                        let body = serde_json::from_slice(&body).unwrap();
                        requests.lock().unwrap().push((method, path, body));
                        Ok::<_, Infallible>(
                            hyper::Response::builder()
                                .status(status)
                                .body(hyper::Body::empty())
                                .unwrap(),
                        )
                    }
                });
            hyper::server::conn::Http::new()
                .http1_only(true)
                .serve_connection(peer, service)
                .await
                .unwrap();
        });
        Ok(Box::pin(stream))
    }
}

#[async_trait]
impl ExecutionManager for RecordingExecutionManager {
    async fn create(
        &self,
        request: CreateExecutionRequest,
        operation_id: &OperationId,
    ) -> ExecutionManagerResult<ExecutionReservation> {
        if self.fail_create.load(Ordering::Relaxed) {
            return Err(ExecutionManagerError::Unavailable("test failure".into()));
        }
        if let Some(execution_id) = self
            .operations
            .lock()
            .unwrap()
            .get(operation_id.as_str())
            .cloned()
        {
            let executions = self.executions.lock().unwrap();
            let execution = executions.get(&execution_id).ok_or_else(|| {
                ExecutionManagerError::Internal("missing test execution".to_string())
            })?;
            return Ok(Self::reservation(execution));
        }
        self.requests.lock().unwrap().push(request.clone());
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
        self.operations
            .lock()
            .unwrap()
            .insert(operation_id.as_str().to_string(), execution_id.to_string());
        self.executions.lock().unwrap().insert(
            execution_id.to_string(),
            TestExecution {
                lease: lease.clone(),
                state: ExecutionState::Created,
            },
        );
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
        let mut executions = self.executions.lock().unwrap();
        let execution = executions
            .get_mut(execution_id.as_str())
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        if execution.lease.generation != generation {
            return Err(ExecutionManagerError::Conflict {
                execution_id: execution_id.clone(),
                message: "stale test start".to_string(),
            });
        }
        match execution.state {
            ExecutionState::Created => execution.state = ExecutionState::Running,
            ExecutionState::Running => {}
            state => {
                return Err(ExecutionManagerError::Conflict {
                    execution_id: execution_id.clone(),
                    message: format!("cannot start test execution in state {state:?}"),
                });
            }
        }
        Ok(execution.lease.clone())
    }

    async fn inspect(&self, execution_id: &ExecutionId) -> ExecutionManagerResult<ExecutionStatus> {
        let executions = self.executions.lock().unwrap();
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

    async fn pause(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        _keep_memory: bool,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let mut executions = self.executions.lock().unwrap();
        let execution = executions
            .get_mut(execution_id.as_str())
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        if execution.lease.generation != generation || execution.state != ExecutionState::Running {
            return Err(ExecutionManagerError::Conflict {
                execution_id: execution_id.clone(),
                message: "stale test pause".to_string(),
            });
        }
        execution.state = ExecutionState::Paused;
        let next_generation = generation.get().checked_add(1).ok_or_else(|| {
            ExecutionManagerError::Internal("test execution generation is exhausted".into())
        })?;
        execution.lease.generation = ExecutionGeneration::new(next_generation)?;
        Ok(execution.lease.clone())
    }

    async fn resume(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<ExecutionLease> {
        let mut executions = self.executions.lock().unwrap();
        let execution = executions
            .get_mut(execution_id.as_str())
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        if execution.lease.generation != generation || execution.state != ExecutionState::Paused {
            return Err(ExecutionManagerError::Conflict {
                execution_id: execution_id.clone(),
                message: "stale test resume".to_string(),
            });
        }
        execution.state = ExecutionState::Running;
        let next_generation = generation.get().checked_add(1).ok_or_else(|| {
            ExecutionManagerError::Internal("test execution generation is exhausted".into())
        })?;
        execution.lease.generation = ExecutionGeneration::new(next_generation)?;
        Ok(execution.lease.clone())
    }

    async fn kill(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<KillOutcome> {
        let mut executions = self.executions.lock().unwrap();
        let execution = executions
            .get_mut(execution_id.as_str())
            .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
        if execution.lease.generation != generation {
            return Err(ExecutionManagerError::Conflict {
                execution_id: execution_id.clone(),
                message: "stale test kill".to_string(),
            });
        }
        if execution.state == ExecutionState::Stopped {
            Ok(KillOutcome::AlreadyStopped)
        } else {
            execution.state = ExecutionState::Stopped;
            Ok(KillOutcome::Killed)
        }
    }

    async fn reconcile(
        &self,
        operation_id: &OperationId,
    ) -> ExecutionManagerResult<ReconcileOutcome> {
        let Some(execution_id) = self
            .operations
            .lock()
            .unwrap()
            .get(operation_id.as_str())
            .cloned()
        else {
            return Ok(ReconcileOutcome::Absent);
        };
        let executions = self.executions.lock().unwrap();
        let execution = executions
            .get(&execution_id)
            .ok_or_else(|| ExecutionManagerError::Internal("missing test execution".to_string()))?;
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

pub(crate) fn assert_sandbox_request(request: &CreateExecutionRequest) {
    assert_eq!(request.config.isolation, ExecutionIsolation::Sandbox);
    assert_eq!(request.config.network, NetworkMode::None);
    assert_eq!(request.config.resources.timeout, 321);
    assert_eq!(
        request.config.extra_env,
        vec![
            ("ALPHA".to_string(), "one".to_string()),
            ("BETA".to_string(), "two".to_string()),
        ]
    );
}
