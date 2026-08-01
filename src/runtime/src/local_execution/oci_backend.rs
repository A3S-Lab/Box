//! Public-SDK-only A3S OCI lifecycle boundary for managed local execution.

use std::path::PathBuf;
use std::sync::Arc;

use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionIsolation, ExecutionManagerError,
    ExecutionManagerResult, ExecutionState, KillOutcome, OperationId as BoxOperationId,
};
use a3s_oci_sdk::oci_spec::runtime::ContainerState;
use a3s_oci_sdk::{
    ContainerId, ContainerRecord, ContainerTarget, CreateAttachments, CreateRequest, DeleteMode,
    DeleteRequest, DriverKind, ErrorCode, ExitStatus, IoMode, IsolationClass, IsolationRequest,
    KillRequest, LocalIpcEndpoint, OciBundle, OperationContext, OperationId as OciOperationId,
    ProcessIo, RuntimeClient, RuntimeInfo, RuntimeOperation, Signal, StartRequest, StateRequest,
    WaitRequest, ATTACHMENT_SCHEMA_V1,
};
use async_trait::async_trait;
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{
    LocalExecutionBackend, LocalExecutionHandle, LocalExecutionObservation,
    LocalExecutionTermination,
};
use crate::{BoxRecord, ManagedExecutionState};

/// Durable schema for one Box-to-OCI runtime attachment.
pub const OCI_RUNTIME_BINDING_SCHEMA_VERSION: &str = "a3s.box.oci-runtime-binding.v2";

const REQUIRED_LIFECYCLE_OPERATIONS: &[RuntimeOperation] = &[
    RuntimeOperation::Create,
    RuntimeOperation::State,
    RuntimeOperation::Start,
    RuntimeOperation::Kill,
    RuntimeOperation::Delete,
    RuntimeOperation::Wait,
];
const DEFAULT_KILL_SIGNAL: i32 = 9;

/// Serializable platform-local endpoint retained for runtime reconciliation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "kebab-case")]
pub enum OciRuntimeEndpoint {
    /// Absolute Unix-domain socket owned by the local runtime service.
    UnixSocket { path: PathBuf },
    /// Local Windows named pipe below `\\.\pipe\`.
    WindowsNamedPipe { name: String },
}

impl OciRuntimeEndpoint {
    /// Construct an absolute Unix-domain socket endpoint.
    pub fn unix_socket(path: impl Into<PathBuf>) -> ExecutionManagerResult<Self> {
        let endpoint = Self::UnixSocket { path: path.into() };
        endpoint.validate()?;
        Ok(endpoint)
    }

    /// Construct a local Windows named-pipe endpoint.
    pub fn windows_named_pipe(name: impl Into<String>) -> ExecutionManagerResult<Self> {
        let endpoint = Self::WindowsNamedPipe { name: name.into() };
        endpoint.validate()?;
        Ok(endpoint)
    }

    fn validate(&self) -> ExecutionManagerResult<()> {
        match self {
            Self::UnixSocket { path } if path.is_absolute() => Ok(()),
            Self::UnixSocket { path } => Err(ExecutionManagerError::InvalidRequest(format!(
                "A3S OCI Unix endpoint must be absolute: {}",
                path.display()
            ))),
            Self::WindowsNamedPipe { name }
                if name.to_ascii_lowercase().starts_with(r"\\.\pipe\")
                    && name.len() > r"\\.\pipe\".len()
                    && !name.as_bytes().contains(&0) =>
            {
                Ok(())
            }
            Self::WindowsNamedPipe { name } => Err(ExecutionManagerError::InvalidRequest(format!(
                "A3S OCI named-pipe endpoint is not local and non-empty: {name:?}"
            ))),
        }
    }

    fn to_sdk(&self) -> ExecutionManagerResult<LocalIpcEndpoint> {
        self.validate()?;
        #[cfg(unix)]
        {
            return match self {
                Self::UnixSocket { path } => LocalIpcEndpoint::unix_socket(path.clone())
                    .map_err(|error| sdk_error("connect", error)),
                Self::WindowsNamedPipe { .. } => Err(ExecutionManagerError::Unavailable(
                    "a Windows A3S OCI endpoint cannot be opened on this host".to_string(),
                )),
            };
        }
        #[cfg(windows)]
        {
            return match self {
                Self::WindowsNamedPipe { name } => {
                    LocalIpcEndpoint::windows_named_pipe(name.clone())
                        .map_err(|error| sdk_error("connect", error))
                }
                Self::UnixSocket { .. } => Err(ExecutionManagerError::Unavailable(
                    "a Unix A3S OCI endpoint cannot be opened on this host".to_string(),
                )),
            };
        }
        #[allow(unreachable_code)]
        Err(ExecutionManagerError::Unavailable(
            "A3S OCI local IPC is unsupported on this host".to_string(),
        ))
    }
}

/// Exact runtime identity persisted separately from the Box execution identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OciRuntimeBinding {
    /// Version of this durable binding schema.
    pub schema_version: String,
    /// Local service endpoint used to reopen the runtime.
    pub endpoint: OciRuntimeEndpoint,
    /// Exact container ID and runtime generation.
    pub target: ContainerTarget,
    /// Runtime-selected driver; Box never persists a requested hypervisor.
    pub driver: DriverKind,
    /// Effective isolation returned by the runtime.
    pub isolation: IsolationClass,
    /// Immutable OCI configuration evidence returned by the runtime.
    pub config_digest: String,
    /// Complete versioned create-time attachment evidence returned by the runtime.
    pub attachments_digest: String,
}

impl OciRuntimeBinding {
    fn from_record(
        endpoint: OciRuntimeEndpoint,
        expected_id: &ContainerId,
        record: &ContainerRecord,
    ) -> ExecutionManagerResult<Self> {
        validate_record_id(record, expected_id, "bind")?;
        if record.generation.0 == 0 {
            return Err(ExecutionManagerError::Internal(
                "A3S OCI returned runtime generation zero".to_string(),
            ));
        }
        let binding = Self {
            schema_version: OCI_RUNTIME_BINDING_SCHEMA_VERSION.to_string(),
            endpoint,
            target: ContainerTarget::exact(expected_id.clone(), record.generation),
            driver: record.driver,
            isolation: record.isolation,
            config_digest: record.config_digest.clone(),
            attachments_digest: record.attachments_digest.clone().ok_or_else(|| {
                ExecutionManagerError::Internal(
                    "A3S OCI returned no versioned attachment evidence".to_string(),
                )
            })?,
        };
        binding.validate()?;
        Ok(binding)
    }

    /// Validate the versioned endpoint, exact target, and immutable evidence.
    pub fn validate(&self) -> ExecutionManagerResult<()> {
        if self.schema_version != OCI_RUNTIME_BINDING_SCHEMA_VERSION {
            return Err(ExecutionManagerError::Internal(format!(
                "unsupported A3S OCI binding schema {:?}",
                self.schema_version
            )));
        }
        self.endpoint.validate()?;
        let generation = self.target.generation.ok_or_else(|| {
            ExecutionManagerError::Internal(
                "A3S OCI binding does not contain an exact runtime generation".to_string(),
            )
        })?;
        if generation.0 == 0 {
            return Err(ExecutionManagerError::Internal(
                "A3S OCI binding contains runtime generation zero".to_string(),
            ));
        }
        validate_config_digest(&self.config_digest)?;
        validate_attachments_digest(&self.attachments_digest)?;
        Ok(())
    }

    /// Validate that this runtime identity belongs to one Box execution.
    pub fn validate_for(&self, execution_id: &ExecutionId) -> ExecutionManagerResult<()> {
        self.validate()?;
        let expected = runtime_container_id(execution_id)?;
        if self.target.id != expected {
            return Err(ExecutionManagerError::Internal(format!(
                "A3S OCI binding target {} does not belong to Box execution {execution_id}",
                self.target.id
            )));
        }
        Ok(())
    }

    fn validate_record(&self, record: &ContainerRecord) -> ExecutionManagerResult<()> {
        self.validate()?;
        validate_record_id(record, &self.target.id, "reconcile")?;
        if Some(record.generation) != self.target.generation
            || record.driver != self.driver
            || record.isolation != self.isolation
            || record.config_digest != self.config_digest
            || record.attachments_digest.as_deref() != Some(self.attachments_digest.as_str())
        {
            return Err(ExecutionManagerError::Internal(format!(
                "A3S OCI runtime evidence drifted for {} generation {:?}",
                self.target.id, self.target.generation
            )));
        }
        Ok(())
    }
}

/// Product-owned OCI bundle and host bookkeeping required before lifecycle launch.
#[derive(Debug, Clone)]
pub struct OciPreparedExecution {
    pub bundle: OciBundle,
    pub attachments: CreateAttachments,
    pub console_log: PathBuf,
    pub anonymous_volumes: Vec<String>,
}

impl OciPreparedExecution {
    /// Build a non-interactive prepared execution with a caller-owned console.
    pub fn new(bundle: OciBundle, console_log: impl Into<PathBuf>) -> ExecutionManagerResult<Self> {
        let io = ProcessIo {
            stdin: IoMode::Null,
            stdout: IoMode::Null,
            stderr: IoMode::Null,
            terminal_size: None,
        };
        let attachments = CreateAttachments::from_bundle(&bundle, io)
            .map_err(|error| sdk_error("prepare attachments", error))?;
        Self::with_attachments(bundle, attachments, console_log)
    }

    /// Build a prepared execution with explicit, already-authorized classifications.
    pub fn with_attachments(
        bundle: OciBundle,
        attachments: CreateAttachments,
        console_log: impl Into<PathBuf>,
    ) -> ExecutionManagerResult<Self> {
        attachments
            .validate(&bundle)
            .map_err(|error| sdk_error("prepare attachments", error))?;
        Ok(Self {
            bundle,
            attachments,
            console_log: console_log.into(),
            anonymous_volumes: Vec::new(),
        })
    }

    fn validate_for(&self, record: &BoxRecord) -> ExecutionManagerResult<()> {
        self.attachments
            .validate(&self.bundle)
            .map_err(|error| sdk_error("validate attachments", error))?;
        if self.console_log != record.console_log {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "A3S OCI preparation changed the durable console path for {}",
                record.id
            )));
        }
        if self.anonymous_volumes != record.anonymous_volumes {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "A3S OCI preparation introduced uncommitted anonymous-volume state for {}",
                record.id
            )));
        }
        Ok(())
    }
}

/// Product-owned preparation boundary consumed by the OCI lifecycle backend.
#[async_trait]
pub trait OciBundleProvider: Send + Sync {
    /// Prepare one immutable OCI bundle after runtime capability preflight.
    async fn prepare(&self, record: &BoxRecord) -> ExecutionManagerResult<OciPreparedExecution>;

    /// Remove only Box-owned preparation artifacts after runtime cleanup.
    async fn cleanup(&self, _record: &BoxRecord) -> ExecutionManagerResult<()> {
        Ok(())
    }
}

/// Result of a create/start composition through the public OCI SDK.
#[derive(Debug, Clone)]
pub struct OciRuntimeLaunch {
    pub record: ContainerRecord,
    pub binding: OciRuntimeBinding,
}

/// SDK-only lifecycle adapter. It contains no Box image, VM, or driver internals.
#[derive(Clone)]
pub struct OciLifecycleAdapter {
    endpoint: OciRuntimeEndpoint,
    client: RuntimeClient,
}

impl OciLifecycleAdapter {
    /// Connect to an out-of-process A3S OCI host service.
    pub async fn connect(endpoint: OciRuntimeEndpoint) -> ExecutionManagerResult<Self> {
        let sdk_endpoint = endpoint.to_sdk()?;
        let client = RuntimeClient::connect(&sdk_endpoint)
            .await
            .map_err(|error| sdk_error("connect", error))?;
        Self::from_client(endpoint, client)
    }

    /// Inject a public SDK client, primarily for an in-process host service or test.
    pub fn from_client(
        endpoint: OciRuntimeEndpoint,
        client: RuntimeClient,
    ) -> ExecutionManagerResult<Self> {
        endpoint.validate()?;
        Ok(Self { endpoint, client })
    }

    /// Fail before product preparation unless one launch-ready driver can meet
    /// the requested isolation and the exact lifecycle surface is advertised.
    pub async fn require_isolation(
        &self,
        isolation: ExecutionIsolation,
    ) -> ExecutionManagerResult<RuntimeInfo> {
        let info = self
            .client
            .features()
            .await
            .map_err(|error| sdk_error("features", error))?;
        for operation in REQUIRED_LIFECYCLE_OPERATIONS {
            if !info.operations.contains(operation) {
                return Err(ExecutionManagerError::Unavailable(format!(
                    "A3S OCI Runtime does not advertise {operation:?}"
                )));
            }
        }
        if !info.attachments.supports_schema(ATTACHMENT_SCHEMA_V1) {
            return Err(ExecutionManagerError::Unavailable(format!(
                "A3S OCI Runtime does not advertise attachment schema {ATTACHMENT_SCHEMA_V1}"
            )));
        }
        let required = oci_isolation_request(isolation).class();
        if !info.drivers.drivers.iter().any(|capability| {
            capability.can_launch() && capability.isolation_classes.contains(&required)
        }) {
            return Err(ExecutionManagerError::Unavailable(format!(
                "A3S OCI Runtime has no launch-ready driver for {required:?}"
            )));
        }
        Ok(info)
    }

    async fn launch_preflighted(
        &self,
        info: &RuntimeInfo,
        execution_id: &ExecutionId,
        execution_generation: ExecutionGeneration,
        operation_seed: &BoxOperationId,
        prepared: OciPreparedExecution,
        isolation: ExecutionIsolation,
    ) -> ExecutionManagerResult<OciRuntimeLaunch> {
        let id = runtime_container_id(execution_id)?;
        let requested = oci_isolation_request(isolation);
        let expected_config_digest = prepared.bundle.config_digest().to_string();
        let expected_attachments_digest = prepared
            .attachments
            .digest()
            .map_err(|error| sdk_error("digest attachments", error))?;
        let created = self
            .client
            .create(CreateRequest {
                context: operation_context(
                    operation_seed.as_str(),
                    execution_generation,
                    "create",
                    requested.class(),
                )?,
                id: id.clone(),
                bundle: prepared.bundle,
                isolation: requested.clone(),
                attachments: prepared.attachments,
            })
            .await
            .map_err(|error| sdk_error("create", error))?;
        if let Err(error) = validate_created_record(
            info,
            &created,
            &id,
            requested.class(),
            Some(&expected_config_digest),
            Some(&expected_attachments_digest),
        ) {
            self.cleanup_failed_launch(
                execution_id,
                execution_generation,
                &created,
                requested.class(),
            )
            .await;
            return Err(error);
        }

        let target = ContainerTarget::exact(id.clone(), created.generation);
        let started = match self
            .client
            .start(StartRequest {
                context: operation_context(
                    operation_seed.as_str(),
                    execution_generation,
                    "start",
                    requested.class(),
                )?,
                target: target.clone(),
            })
            .await
        {
            Ok(record) => record,
            // A failed response is not proof that start had no effect. Leave
            // the exact generation intact so Box can reconcile it through
            // state with the same durable Starting claim.
            Err(error) => return Err(sdk_error("start", error)),
        };
        if let Err(error) =
            validate_started_record(info, &created, &started, &target, requested.class())
        {
            self.cleanup_failed_launch(
                execution_id,
                execution_generation,
                &created,
                requested.class(),
            )
            .await;
            return Err(error);
        }
        let binding = OciRuntimeBinding::from_record(self.endpoint.clone(), &id, &started)?;
        Ok(OciRuntimeLaunch {
            record: started,
            binding,
        })
    }

    async fn cleanup_failed_launch(
        &self,
        execution_id: &ExecutionId,
        execution_generation: ExecutionGeneration,
        created: &ContainerRecord,
        required_isolation: IsolationClass,
    ) {
        let Ok(id) = runtime_container_id(execution_id) else {
            return;
        };
        if created.state.id() != id.as_str() || created.generation.0 == 0 {
            return;
        }
        // Never trust a malformed response to select a cleanup target.
        let target = ContainerTarget::exact(id, created.generation);
        let context = operation_context(
            execution_id.as_str(),
            execution_generation,
            "failed-launch-delete",
            required_isolation,
        );
        if let Ok(context) = context {
            let _ = self
                .client
                .delete(DeleteRequest {
                    context,
                    target,
                    mode: DeleteMode::Force,
                })
                .await;
        }
    }

    async fn state_current(
        &self,
        execution_id: &ExecutionId,
    ) -> ExecutionManagerResult<Option<ContainerRecord>> {
        let id = runtime_container_id(execution_id)?;
        match self
            .client
            .state(StateRequest {
                target: ContainerTarget::current(id.clone()),
            })
            .await
        {
            Ok(record) => {
                validate_record_id(&record, &id, "state")?;
                Ok(Some(record))
            }
            Err(error) if error.code == ErrorCode::NotFound => Ok(None),
            Err(error) => Err(sdk_error("state", error)),
        }
    }

    async fn state_exact(
        &self,
        binding: &OciRuntimeBinding,
    ) -> ExecutionManagerResult<Option<ContainerRecord>> {
        binding.validate()?;
        match self
            .client
            .state(StateRequest {
                target: binding.target.clone(),
            })
            .await
        {
            Ok(record) => {
                binding.validate_record(&record)?;
                Ok(Some(record))
            }
            Err(error) if error.code == ErrorCode::NotFound => Ok(None),
            Err(error) => Err(sdk_error("state", error)),
        }
    }

    async fn start_existing(
        &self,
        info: &RuntimeInfo,
        execution_id: &ExecutionId,
        execution_generation: ExecutionGeneration,
        operation_seed: &BoxOperationId,
        created: &ContainerRecord,
        isolation: ExecutionIsolation,
    ) -> ExecutionManagerResult<OciRuntimeLaunch> {
        let id = runtime_container_id(execution_id)?;
        let required = oci_isolation_request(isolation).class();
        validate_created_record(info, created, &id, required, None, None)?;
        let target = ContainerTarget::exact(id.clone(), created.generation);
        let started = self
            .client
            .start(StartRequest {
                context: operation_context(
                    operation_seed.as_str(),
                    execution_generation,
                    "start",
                    required,
                )?,
                target: target.clone(),
            })
            .await
            .map_err(|error| sdk_error("start", error))?;
        validate_started_record(info, created, &started, &target, required)?;
        let binding = OciRuntimeBinding::from_record(self.endpoint.clone(), &id, &started)?;
        Ok(OciRuntimeLaunch {
            record: started,
            binding,
        })
    }

    async fn wait(
        &self,
        binding: &OciRuntimeBinding,
        timeout_ms: Option<u64>,
    ) -> ExecutionManagerResult<ExitStatus> {
        binding.validate()?;
        let status = self
            .client
            .wait(WaitRequest {
                target: binding.target.clone(),
                timeout_ms,
            })
            .await
            .map_err(|error| sdk_error("wait", error))?;
        status
            .validate()
            .map_err(|error| sdk_error("wait", error))?;
        Ok(status)
    }

    async fn wait_until(
        &self,
        binding: &OciRuntimeBinding,
        timeout_ms: u64,
    ) -> ExecutionManagerResult<Option<ExitStatus>> {
        binding.validate()?;
        match self
            .client
            .wait(WaitRequest {
                target: binding.target.clone(),
                timeout_ms: Some(timeout_ms),
            })
            .await
        {
            Ok(status) => {
                status
                    .validate()
                    .map_err(|error| sdk_error("wait", error))?;
                Ok(Some(status))
            }
            Err(error) if error.code == ErrorCode::DeadlineExceeded => Ok(None),
            Err(error) => Err(sdk_error("wait", error)),
        }
    }

    async fn kill(
        &self,
        execution_id: &ExecutionId,
        execution_generation: ExecutionGeneration,
        binding: &OciRuntimeBinding,
        signal: Signal,
    ) -> ExecutionManagerResult<ContainerRecord> {
        binding.validate_for(execution_id)?;
        let killed = self
            .client
            .kill(KillRequest {
                context: operation_context(
                    execution_id.as_str(),
                    execution_generation,
                    "kill",
                    signal.get(),
                )?,
                target: binding.target.clone(),
                signal,
                all: true,
            })
            .await
            .map_err(|error| sdk_error("kill", error))?;
        binding.validate_record(&killed)?;
        Ok(killed)
    }

    async fn delete(
        &self,
        execution_id: &ExecutionId,
        execution_generation: ExecutionGeneration,
        binding: &OciRuntimeBinding,
        mode: DeleteMode,
    ) -> ExecutionManagerResult<()> {
        binding.validate_for(execution_id)?;
        let request = DeleteRequest {
            context: operation_context(
                execution_id.as_str(),
                execution_generation,
                "delete",
                mode,
            )?,
            target: binding.target.clone(),
            mode,
        };
        match self.client.delete(request).await {
            Ok(()) => Ok(()),
            Err(error) if error.code == ErrorCode::NotFound => Ok(()),
            Err(error) => Err(sdk_error("delete", error)),
        }
    }
}

/// Opt-in canonical local-execution backend over one A3S OCI host service.
#[derive(Clone)]
pub struct OciLocalExecutionBackend {
    adapter: OciLifecycleAdapter,
    provider: Arc<dyn OciBundleProvider>,
}

impl OciLocalExecutionBackend {
    /// Construct from an already connected public SDK client.
    pub fn from_client(
        endpoint: OciRuntimeEndpoint,
        client: RuntimeClient,
        provider: Arc<dyn OciBundleProvider>,
    ) -> ExecutionManagerResult<Self> {
        Ok(Self {
            adapter: OciLifecycleAdapter::from_client(endpoint, client)?,
            provider,
        })
    }

    /// Connect to a local host service and retain only its public SDK boundary.
    pub async fn connect(
        endpoint: OciRuntimeEndpoint,
        provider: Arc<dyn OciBundleProvider>,
    ) -> ExecutionManagerResult<Self> {
        Ok(Self {
            adapter: OciLifecycleAdapter::connect(endpoint).await?,
            provider,
        })
    }

    fn metadata<'a>(
        &self,
        record: &'a BoxRecord,
    ) -> ExecutionManagerResult<&'a crate::ManagedExecutionMetadata> {
        let metadata = record.managed_execution.as_ref().ok_or_else(|| {
            ExecutionManagerError::Internal(format!(
                "execution {} has no managed lifecycle metadata",
                record.id
            ))
        })?;
        metadata
            .validate()
            .map_err(|error| ExecutionManagerError::Internal(error.to_string()))?;
        Ok(metadata)
    }

    fn execution_id(&self, record: &BoxRecord) -> ExecutionManagerResult<ExecutionId> {
        ExecutionId::new(record.id.clone())
            .map_err(|error| ExecutionManagerError::Internal(error.to_string()))
    }

    fn binding(&self, record: &BoxRecord) -> ExecutionManagerResult<Option<OciRuntimeBinding>> {
        let execution_id = self.execution_id(record)?;
        let binding = self.metadata(record)?.oci_runtime.clone();
        if let Some(binding) = &binding {
            binding.validate_for(&execution_id)?;
            if binding.endpoint != self.adapter.endpoint {
                return Err(ExecutionManagerError::Internal(format!(
                    "execution {execution_id} belongs to a different A3S OCI endpoint"
                )));
            }
        }
        Ok(binding)
    }

    fn handle(
        &self,
        record: &BoxRecord,
        binding: OciRuntimeBinding,
        console_log: PathBuf,
        anonymous_volumes: Vec<String>,
    ) -> LocalExecutionHandle {
        LocalExecutionHandle {
            started_at: record.started_at.unwrap_or_else(Utc::now),
            // OCI init PIDs are runtime identities and may be guest PIDs. Never
            // reinterpret them as a Box-owned host process identity.
            pid: None,
            pid_start_time: None,
            exec_socket_path: PathBuf::new(),
            console_log,
            anonymous_volumes,
            oci_runtime: Some(binding),
        }
    }

    async fn current_runtime(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<Option<(ContainerRecord, OciRuntimeBinding)>> {
        let execution_id = self.execution_id(record)?;
        if let Some(binding) = self.binding(record)? {
            return Ok(self
                .adapter
                .state_exact(&binding)
                .await?
                .map(|runtime| (runtime, binding)));
        }
        let Some(runtime) = self.adapter.state_current(&execution_id).await? else {
            return Ok(None);
        };
        let info = self.adapter.require_isolation(record.isolation).await?;
        validate_recovered_record(
            &info,
            &runtime,
            &runtime_container_id(&execution_id)?,
            oci_isolation_request(record.isolation).class(),
        )?;
        let binding = OciRuntimeBinding::from_record(
            self.adapter.endpoint.clone(),
            &runtime_container_id(&execution_id)?,
            &runtime,
        )?;
        Ok(Some((runtime, binding)))
    }

    async fn terminal_observation(
        &self,
        record: &BoxRecord,
        binding: &OciRuntimeBinding,
    ) -> ExecutionManagerResult<LocalExecutionObservation> {
        let execution_id = self.execution_id(record)?;
        let generation = self.metadata(record)?.generation;
        let status = self.adapter.wait(binding, Some(0)).await?;
        self.adapter
            .delete(&execution_id, generation, binding, DeleteMode::StoppedOnly)
            .await?;
        self.provider.cleanup(record).await?;
        Ok(LocalExecutionObservation {
            state: ExecutionState::Stopped,
            handle: None,
            exit_code: Some(exit_code(&status)?),
        })
    }
}

#[async_trait]
impl LocalExecutionBackend for OciLocalExecutionBackend {
    async fn preflight(&self, record: &BoxRecord) -> ExecutionManagerResult<()> {
        self.metadata(record)?;
        self.adapter.require_isolation(record.isolation).await?;
        Ok(())
    }

    async fn start(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        let execution_id = self.execution_id(record)?;
        let metadata = self.metadata(record)?;
        if let Some((runtime, binding)) = self.current_runtime(record).await? {
            if *runtime.state.status() == ContainerState::Running {
                return Ok(self.handle(
                    record,
                    binding,
                    record.console_log.clone(),
                    record.anonymous_volumes.clone(),
                ));
            }
            return Err(ExecutionManagerError::Conflict {
                execution_id,
                message: format!(
                    "A3S OCI already contains runtime state {:?} for this generation",
                    runtime.state.status()
                ),
            });
        }

        let info = self.adapter.require_isolation(record.isolation).await?;
        let prepared = self.provider.prepare(record).await?;
        if let Err(error) = prepared.validate_for(record) {
            return match self.provider.cleanup(record).await {
                Ok(()) => Err(error),
                Err(cleanup) => Err(ExecutionManagerError::Internal(format!(
                    "{error}; Box OCI preparation cleanup also failed: {cleanup}"
                ))),
            };
        }
        let console_log = prepared.console_log.clone();
        let anonymous_volumes = prepared.anonymous_volumes.clone();
        let launch = self
            .adapter
            .launch_preflighted(
                &info,
                &execution_id,
                metadata.generation,
                &metadata.operation_id,
                prepared,
                record.isolation,
            )
            .await;
        match launch {
            Ok(launch) if *launch.record.state.status() == ContainerState::Running => {
                Ok(self.handle(record, launch.binding, console_log, anonymous_volumes))
            }
            Ok(_) => Err(ExecutionManagerError::Unavailable(format!(
                "execution {execution_id} completed while A3S OCI startup was being published"
            ))),
            Err(error) => {
                // Unknown create/start outcomes must be reconciled, not erased.
                // Cleanup product preparation only when the runtime proves no
                // current generation exists for the deterministic ID.
                match self.adapter.state_current(&execution_id).await {
                    Ok(Some(_)) => return Err(error),
                    Err(reconcile_error) => {
                        return Err(ExecutionManagerError::Unavailable(format!(
                            "{error}; A3S OCI launch ownership could not be reconciled and Box preparation was retained: {reconcile_error}"
                        )))
                    }
                    Ok(None) => {}
                }
                let cleanup = self.provider.cleanup(record).await;
                match cleanup {
                    Ok(()) => Err(error),
                    Err(cleanup) => Err(ExecutionManagerError::Internal(format!(
                        "{error}; Box OCI preparation cleanup also failed: {cleanup}"
                    ))),
                }
            }
        }
    }

    async fn inspect(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionObservation> {
        let execution_id = self.execution_id(record)?;
        let metadata = self.metadata(record)?;
        let had_binding = self.binding(record)?.is_some();
        let Some((mut runtime, mut binding)) = self.current_runtime(record).await? else {
            if had_binding {
                self.provider.cleanup(record).await?;
            }
            return Err(ExecutionManagerError::NotFound(execution_id));
        };

        if *runtime.state.status() == ContainerState::Created
            && matches!(
                record.managed_state(),
                Ok(Some(
                    ManagedExecutionState::Starting | ManagedExecutionState::RestartStarting
                ))
            )
        {
            let info = self.adapter.require_isolation(record.isolation).await?;
            let launch = self
                .adapter
                .start_existing(
                    &info,
                    &execution_id,
                    metadata.generation,
                    &metadata.operation_id,
                    &runtime,
                    record.isolation,
                )
                .await?;
            runtime = launch.record;
            binding = launch.binding;
        }

        match *runtime.state.status() {
            ContainerState::Creating | ContainerState::Created => Ok(LocalExecutionObservation {
                state: ExecutionState::Creating,
                handle: None,
                exit_code: None,
            }),
            ContainerState::Running if runtime.is_paused() => Ok(LocalExecutionObservation {
                state: ExecutionState::Paused,
                handle: Some(self.handle(
                    record,
                    binding,
                    record.console_log.clone(),
                    record.anonymous_volumes.clone(),
                )),
                exit_code: None,
            }),
            ContainerState::Running => Ok(LocalExecutionObservation {
                state: ExecutionState::Running,
                handle: Some(self.handle(
                    record,
                    binding,
                    record.console_log.clone(),
                    record.anonymous_volumes.clone(),
                )),
                exit_code: None,
            }),
            ContainerState::Stopped => self.terminal_observation(record, &binding).await,
        }
    }

    async fn pause(
        &self,
        record: &BoxRecord,
        _keep_memory: bool,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        Err(ExecutionManagerError::Unavailable(format!(
            "A3S OCI pause is not yet routed through OciLocalExecutionBackend for {}",
            record.id
        )))
    }

    async fn resume(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        Err(ExecutionManagerError::Unavailable(format!(
            "A3S OCI resume is not yet routed through OciLocalExecutionBackend for {}",
            record.id
        )))
    }

    async fn kill(&self, record: &BoxRecord) -> ExecutionManagerResult<KillOutcome> {
        Ok(self.kill_with_status(record).await?.outcome)
    }

    async fn kill_with_status(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionTermination> {
        let execution_id = self.execution_id(record)?;
        let generation = self.metadata(record)?.generation;
        let Some((runtime, binding)) = self.current_runtime(record).await? else {
            self.provider.cleanup(record).await?;
            return Ok(LocalExecutionTermination {
                outcome: KillOutcome::AlreadyStopped,
                exit_code: record.exit_code,
            });
        };
        let was_stopped = *runtime.state.status() == ContainerState::Stopped;
        let mut status = None;
        if !was_stopped {
            let signal_number = record
                .stop_signal
                .as_deref()
                .map(a3s_box_core::vmm::parse_signal_name)
                .unwrap_or(DEFAULT_KILL_SIGNAL);
            let signal = Signal::new(signal_number).map_err(|error| sdk_error("kill", error))?;
            let graceful_timeout_ms = if signal_number == DEFAULT_KILL_SIGNAL {
                None
            } else {
                record
                    .stop_timeout
                    .map(|timeout_secs| {
                        timeout_secs.checked_mul(1_000).ok_or_else(|| {
                            ExecutionManagerError::InvalidRequest(format!(
                                "stop timeout is too large for execution {execution_id}"
                            ))
                        })
                    })
                    .transpose()?
            };
            self.adapter
                .kill(&execution_id, generation, &binding, signal)
                .await?;
            if let Some(timeout_ms) = graceful_timeout_ms {
                status = self.adapter.wait_until(&binding, timeout_ms).await?;
                if status.is_none() {
                    let force = Signal::new(DEFAULT_KILL_SIGNAL)
                        .map_err(|error| sdk_error("kill", error))?;
                    self.adapter
                        .kill(&execution_id, generation, &binding, force)
                        .await?;
                }
            }
        }
        let status = match status {
            Some(status) => status,
            None => self.adapter.wait(&binding, None).await?,
        };
        self.adapter
            .delete(&execution_id, generation, &binding, DeleteMode::StoppedOnly)
            .await?;
        self.provider.cleanup(record).await?;
        Ok(LocalExecutionTermination {
            outcome: if was_stopped {
                KillOutcome::AlreadyStopped
            } else {
                KillOutcome::Killed
            },
            exit_code: Some(exit_code(&status)?),
        })
    }
}

/// Map product isolation to an OCI isolation requirement without selecting a driver.
#[must_use]
pub const fn oci_isolation_request(isolation: ExecutionIsolation) -> IsolationRequest {
    match isolation {
        ExecutionIsolation::Microvm => IsolationRequest::DedicatedVm,
        ExecutionIsolation::Sandbox => IsolationRequest::SharedHostKernel,
    }
}

fn runtime_container_id(execution_id: &ExecutionId) -> ExecutionManagerResult<ContainerId> {
    ContainerId::new(format!("a3s-box-{execution_id}"))
        .map_err(|error| ExecutionManagerError::Internal(error.to_string()))
}

fn operation_context(
    seed: &str,
    generation: ExecutionGeneration,
    operation: &str,
    payload: impl Serialize,
) -> ExecutionManagerResult<OperationContext> {
    let payload = serde_json::to_vec(&payload).map_err(|error| {
        ExecutionManagerError::Internal(format!(
            "failed to encode A3S OCI {operation} operation identity: {error}"
        ))
    })?;
    let mut digest = Sha256::new();
    digest.update(seed.as_bytes());
    digest.update([0]);
    digest.update(generation.get().to_be_bytes());
    digest.update([0]);
    digest.update(operation.as_bytes());
    digest.update([0]);
    digest.update(payload);
    let id = format!("a3s-box-{operation}-{}", hex::encode(digest.finalize()));
    OciOperationId::new(id)
        .map(OperationContext::new)
        .map_err(|error| sdk_error(operation, error))
}

fn validate_created_record(
    info: &RuntimeInfo,
    record: &ContainerRecord,
    expected_id: &ContainerId,
    required_isolation: IsolationClass,
    expected_config_digest: Option<&str>,
    expected_attachments_digest: Option<&str>,
) -> ExecutionManagerResult<()> {
    validate_record_id(record, expected_id, "create")?;
    if *record.state.status() != ContainerState::Created
        || record.generation.0 == 0
        || record.isolation != required_isolation
    {
        return Err(ExecutionManagerError::Internal(format!(
            "A3S OCI create returned an invalid state, generation, or isolation for {expected_id}"
        )));
    }
    validate_config_digest(&record.config_digest)?;
    let attachments_digest = validate_record_attachments(record, "create")?;
    if expected_config_digest.is_some_and(|expected| record.config_digest != expected) {
        return Err(ExecutionManagerError::Internal(format!(
            "A3S OCI create returned configuration evidence that differs from the submitted bundle for {expected_id}"
        )));
    }
    if expected_attachments_digest.is_some_and(|expected| attachments_digest != expected) {
        return Err(ExecutionManagerError::Internal(format!(
            "A3S OCI create returned attachment evidence that differs from the submitted manifest for {expected_id}"
        )));
    }
    validate_selected_driver(info, record)
}

fn validate_recovered_record(
    info: &RuntimeInfo,
    record: &ContainerRecord,
    expected_id: &ContainerId,
    required_isolation: IsolationClass,
) -> ExecutionManagerResult<()> {
    validate_record_id(record, expected_id, "recover")?;
    if record.generation.0 == 0 || record.isolation != required_isolation {
        return Err(ExecutionManagerError::Internal(format!(
            "A3S OCI recovery returned an invalid generation or isolation for {expected_id}"
        )));
    }
    validate_config_digest(&record.config_digest)?;
    validate_record_attachments(record, "recover")?;
    validate_selected_driver(info, record)
}

fn validate_started_record(
    info: &RuntimeInfo,
    created: &ContainerRecord,
    started: &ContainerRecord,
    target: &ContainerTarget,
    required_isolation: IsolationClass,
) -> ExecutionManagerResult<()> {
    validate_record_id(started, &target.id, "start")?;
    if Some(started.generation) != target.generation
        || started.driver != created.driver
        || started.isolation != required_isolation
        || started.config_digest != created.config_digest
        || started.attachments_digest != created.attachments_digest
        || !matches!(
            started.state.status(),
            ContainerState::Running | ContainerState::Stopped
        )
    {
        return Err(ExecutionManagerError::Internal(format!(
            "A3S OCI start changed exact runtime evidence for {} generation {:?}",
            target.id, target.generation
        )));
    }
    validate_record_attachments(started, "start")?;
    validate_selected_driver(info, started)
}

fn validate_selected_driver(
    info: &RuntimeInfo,
    record: &ContainerRecord,
) -> ExecutionManagerResult<()> {
    let valid = info.drivers.drivers.iter().any(|capability| {
        capability.driver == record.driver
            && capability.can_launch()
            && capability.isolation_classes.contains(&record.isolation)
    });
    if !valid {
        return Err(ExecutionManagerError::Internal(format!(
            "A3S OCI selected unadvertised or non-launch-ready driver {:?}",
            record.driver
        )));
    }
    Ok(())
}

fn validate_record_id(
    record: &ContainerRecord,
    expected_id: &ContainerId,
    operation: &str,
) -> ExecutionManagerResult<()> {
    if record.state.id() != expected_id.as_str() {
        return Err(ExecutionManagerError::Internal(format!(
            "A3S OCI {operation} returned container {}, expected {expected_id}",
            record.state.id()
        )));
    }
    Ok(())
}

fn validate_config_digest(config_digest: &str) -> ExecutionManagerResult<()> {
    validate_sha256_evidence(config_digest, "configuration")
}

fn validate_attachments_digest(attachments_digest: &str) -> ExecutionManagerResult<()> {
    validate_sha256_evidence(attachments_digest, "attachment")
}

fn validate_record_attachments<'a>(
    record: &'a ContainerRecord,
    operation: &str,
) -> ExecutionManagerResult<&'a str> {
    let digest = record.attachments_digest.as_deref().ok_or_else(|| {
        ExecutionManagerError::Internal(format!(
            "A3S OCI {operation} returned no versioned attachment evidence"
        ))
    })?;
    validate_attachments_digest(digest)?;
    Ok(digest)
}

fn validate_sha256_evidence(value: &str, label: &str) -> ExecutionManagerResult<()> {
    let valid = value.strip_prefix("sha256:").is_some_and(|digest| {
        digest.len() == 64
            && digest
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    if !valid {
        return Err(ExecutionManagerError::Internal(format!(
            "A3S OCI binding contains invalid {label} evidence"
        )));
    }
    Ok(())
}

fn exit_code(status: &ExitStatus) -> ExecutionManagerResult<i32> {
    if let Some(exit_code) = status.exit_code {
        return Ok(exit_code);
    }
    let signal = status.signal.ok_or_else(|| {
        ExecutionManagerError::Internal(
            "A3S OCI terminal status contains neither an exit code nor a signal".to_string(),
        )
    })?;
    128_i32.checked_add(signal).ok_or_else(|| {
        ExecutionManagerError::Internal(format!(
            "A3S OCI terminal signal {signal} cannot be represented as a Box exit code"
        ))
    })
}

fn sdk_error(operation: &str, error: a3s_oci_sdk::Error) -> ExecutionManagerError {
    match error.code {
        ErrorCode::InvalidArgument => {
            ExecutionManagerError::InvalidRequest(format!("A3S OCI {operation}: {error}"))
        }
        ErrorCode::Conflict | ErrorCode::FailedPrecondition => ExecutionManagerError::Unavailable(
            format!("A3S OCI {operation} rejected the lifecycle boundary: {error}"),
        ),
        _ => ExecutionManagerError::Unavailable(format!("A3S OCI {operation}: {error}")),
    }
}

#[cfg(test)]
#[path = "oci_backend_tests.rs"]
mod tests;
