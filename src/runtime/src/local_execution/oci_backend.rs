//! Public-SDK-only A3S OCI lifecycle boundary for managed local execution.

use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use a3s_box_core::{
    pty::PtyRequest, ExecOutput, ExecRequest as BoxExecRequest, ExecutionCpuStats,
    ExecutionEventBatch, ExecutionEventKind, ExecutionEventsRequest, ExecutionGeneration,
    ExecutionId, ExecutionIsolation, ExecutionManagerError, ExecutionManagerResult,
    ExecutionMemoryStats, ExecutionProcess, ExecutionProcessInfo, ExecutionProcessInventory,
    ExecutionResourceUpdate, ExecutionRuntimeEvent, ExecutionState, ExecutionStats,
    FileOp as BoxFileOp, FileRequest as BoxFileRequest, FileResponse as BoxFileResponse,
    FilesystemEntry as BoxFilesystemEntry, FilesystemEntryKind as BoxFilesystemEntryKind,
    FilesystemOp as BoxFilesystemOp, FilesystemRequest as BoxFilesystemRequest,
    FilesystemResponse as BoxFilesystemResponse, KillOutcome, OperationId as BoxOperationId,
    MAX_BOUNDED_FILE_BYTES,
};
use a3s_oci_sdk::oci_spec::runtime::ContainerState;
use a3s_oci_sdk::{
    runtime_bundle_handoff_directory as sdk_runtime_bundle_handoff_directory,
    AttachmentCapabilities, ContainerId, ContainerOperationRequest, ContainerRecord,
    ContainerTarget, CreateAttachments, CreateRequest, DeleteMode, DeleteRequest, DriverKind,
    ErrorCode, EventsRequest, ExitStatus, FileOp as OciFileOp, FileRequest as OciFileRequest,
    FileResponse as OciFileResponse, FilesystemEntry as OciFilesystemEntry,
    FilesystemEntryKind as OciFilesystemEntryKind, FilesystemOp as OciFilesystemOp,
    FilesystemRequest as OciFilesystemRequest, FilesystemResponse as OciFilesystemResponse, IoMode,
    IsolationClass, IsolationRequest, KillRequest, LinuxResources, LocalIpcEndpoint, OciBundle,
    OperationContext, OperationId as OciOperationId, ProcessIo, ProcessRecord, ProcessesRequest,
    RuntimeClient, RuntimeEventKind, RuntimeInfo, RuntimeOperation, Signal, StartRequest,
    StateRequest, StatsRequest, UpdateRequest, WaitRequest, ATTACHMENT_SCHEMA_V1,
    MAX_FILE_TRANSFER_BYTES, RUNTIME_BUNDLE_HANDOFF_BUNDLE_DIRECTORY,
    RUNTIME_BUNDLE_HANDOFF_EXTENSION, RUNTIME_BUNDLE_HANDOFF_EXTENSION_VERSION,
    RUNTIME_BUNDLE_HANDOFF_ROOT_DIRECTORY,
};
use async_trait::async_trait;
use base64::{engine::general_purpose::STANDARD, Engine as _};
use chrono::Utc;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::resources::ExecutionResourceGuard;
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

/// Stable runtime identities and negotiated capabilities available to product preparation.
#[derive(Debug, Clone)]
pub struct OciBundlePreparationContext {
    runtime_container_id: ContainerId,
    create_context: OperationContext,
    isolation: IsolationRequest,
    attachment_capabilities: AttachmentCapabilities,
    execution_generation: ExecutionGeneration,
    operation_seed: BoxOperationId,
}

impl OciBundlePreparationContext {
    /// Exact runtime container identity derived from the Box execution identity.
    pub fn runtime_container_id(&self) -> &ContainerId {
        &self.runtime_container_id
    }

    /// Exact create operation context that will be sent to OCI Runtime.
    pub fn create_context(&self) -> &OperationContext {
        &self.create_context
    }

    /// Minimum isolation requested from OCI Runtime.
    pub fn isolation(&self) -> &IsolationRequest {
        &self.isolation
    }

    /// Attachment extensions advertised by the connected runtime service.
    pub fn attachment_capabilities(&self) -> &AttachmentCapabilities {
        &self.attachment_capabilities
    }

    /// Resolve the only operation-scoped directory accepted for bundle ownership handoff.
    pub fn runtime_bundle_handoff_directory(
        &self,
        runtime_root: impl AsRef<Path>,
    ) -> ExecutionManagerResult<PathBuf> {
        if !self.attachment_capabilities.supports_extension(
            RUNTIME_BUNDLE_HANDOFF_EXTENSION,
            RUNTIME_BUNDLE_HANDOFF_EXTENSION_VERSION,
        ) {
            return Err(ExecutionManagerError::Unavailable(format!(
                "A3S OCI Runtime does not advertise {RUNTIME_BUNDLE_HANDOFF_EXTENSION} version {RUNTIME_BUNDLE_HANDOFF_EXTENSION_VERSION}"
            )));
        }
        sdk_runtime_bundle_handoff_directory(
            runtime_root,
            &self.runtime_container_id,
            &self.create_context.operation_id,
        )
        .map_err(|error| sdk_error("resolve bundle handoff", error))
    }

    /// Verify that a claimed handoff directory has the exact negotiated suffix.
    pub fn validate_runtime_bundle_handoff_directory(
        &self,
        directory: &Path,
    ) -> ExecutionManagerResult<()> {
        let operation_directory = directory.parent();
        let container_directory = operation_directory.and_then(Path::parent);
        let handoff_root = container_directory.and_then(Path::parent);
        let runtime_root = handoff_root.and_then(Path::parent);
        let exact_shape = directory.is_absolute()
            && directory.file_name().and_then(|name| name.to_str())
                == Some(RUNTIME_BUNDLE_HANDOFF_BUNDLE_DIRECTORY)
            && operation_directory
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                == Some(self.create_context.operation_id.as_str())
            && container_directory
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                == Some(self.runtime_container_id.as_str())
            && handoff_root
                .and_then(Path::file_name)
                .and_then(|name| name.to_str())
                == Some(RUNTIME_BUNDLE_HANDOFF_ROOT_DIRECTORY);
        let Some(runtime_root) = runtime_root.filter(|_| exact_shape) else {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "A3S OCI bundle handoff does not match the exact runtime/container/create-operation layout: {}",
                directory.display()
            )));
        };
        let expected = self.runtime_bundle_handoff_directory(runtime_root)?;
        if directory != expected {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "A3S OCI bundle handoff path is not normalized: {}",
                directory.display()
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

    /// Bind a portable bundle prepared at the negotiated operation-scoped handoff path.
    pub fn with_runtime_bundle_handoff(
        mut self,
        context: &OciBundlePreparationContext,
        runtime_root: impl AsRef<Path>,
    ) -> ExecutionManagerResult<Self> {
        let expected = context.runtime_bundle_handoff_directory(runtime_root)?;
        if !exact_handoff_directory_matches(self.bundle.directory(), &expected) {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "A3S OCI bundle handoff must use exact operation path {}: {}",
                expected.display(),
                self.bundle.directory().display()
            )));
        }
        self.attachments = self
            .attachments
            .clone()
            .with_runtime_bundle_handoff(&self.bundle)
            .map_err(|error| sdk_error("prepare bundle handoff", error))?;
        self.attachments
            .validate(&self.bundle)
            .map_err(|error| sdk_error("validate bundle handoff", error))?;
        Ok(self)
    }

    fn validate_for(
        &self,
        record: &BoxRecord,
        context: &OciBundlePreparationContext,
    ) -> ExecutionManagerResult<()> {
        self.attachments
            .validate(&self.bundle)
            .map_err(|error| sdk_error("validate attachments", error))?;
        if self.attachments.uses_runtime_bundle_handoff() {
            context.validate_runtime_bundle_handoff_directory(self.bundle.directory())?;
        }
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

#[cfg(not(windows))]
fn exact_handoff_directory_matches(directory: &Path, expected: &Path) -> bool {
    directory == expected
}

#[cfg(windows)]
fn exact_handoff_directory_matches(directory: &Path, expected: &Path) -> bool {
    // `OciBundle::load` resolves an existing directory with
    // `std::fs::canonicalize`. Windows returns that same directory in the
    // verbatim `\\?\C:\...` namespace, while the operation-scoped SDK path is
    // intentionally constructed before publication as `C:\...`. Strip only
    // that namespace spelling (including its UNC form); do not resolve either
    // input here, because accepting a symlink alias would weaken the exact
    // container/operation ownership boundary.
    windows_path_without_verbatim_prefix(directory)
        == windows_path_without_verbatim_prefix(expected)
}

#[cfg(windows)]
fn windows_path_without_verbatim_prefix(path: &Path) -> Vec<u16> {
    use std::os::windows::ffi::OsStrExt as _;

    const BACKSLASH: u16 = b'\\' as u16;
    const QUESTION_MARK: u16 = b'?' as u16;
    const COLON: u16 = b':' as u16;
    const UNC: [u16; 4] = [b'U' as u16, b'N' as u16, b'C' as u16, BACKSLASH];

    let encoded = path.as_os_str().encode_wide().collect::<Vec<_>>();
    let Some(rest) = encoded.strip_prefix(&[BACKSLASH, BACKSLASH, QUESTION_MARK, BACKSLASH]) else {
        return encoded;
    };
    let has_drive_prefix = rest.len() >= 3
        && ((b'A' as u16..=b'Z' as u16).contains(&rest[0])
            || (b'a' as u16..=b'z' as u16).contains(&rest[0]))
        && rest[1] == COLON;
    if has_drive_prefix {
        return rest.to_vec();
    }
    if let Some(unc_rest) = rest.strip_prefix(&UNC) {
        let mut normalized = Vec::with_capacity(unc_rest.len() + 2);
        normalized.extend([BACKSLASH, BACKSLASH]);
        normalized.extend_from_slice(unc_rest);
        return normalized;
    }
    encoded
}

/// Product-owned preparation boundary consumed by the OCI lifecycle backend.
#[async_trait]
pub trait OciBundleProvider: Send + Sync {
    /// Reject a runtime/provider mismatch before product or runtime mutation.
    fn preflight(
        &self,
        _record: &BoxRecord,
        _context: &OciBundlePreparationContext,
    ) -> ExecutionManagerResult<()> {
        Ok(())
    }

    /// Prepare one immutable OCI bundle after runtime capability preflight.
    async fn prepare(
        &self,
        record: &BoxRecord,
        context: &OciBundlePreparationContext,
    ) -> ExecutionManagerResult<OciPreparedExecution>;

    /// Ensure the Box-owned init-output projection is ready before OCI start.
    /// Test and embedding providers may retain raw runtime output only.
    async fn ensure_log_projection(
        &self,
        _record: &BoxRecord,
        _binding: &OciRuntimeBinding,
    ) -> ExecutionManagerResult<()> {
        Ok(())
    }

    /// Wait until the exact init stdout/stderr streams have reached EOF and
    /// Box's configured log driver has consumed them.
    async fn wait_log_projection_drained(
        &self,
        _record: &BoxRecord,
        _binding: &OciRuntimeBinding,
    ) -> ExecutionManagerResult<()> {
        Ok(())
    }

    /// Wait until the exact projection worker has stopped after the runtime
    /// owner was lost. This path must not claim that output was fully drained:
    /// owner-death recovery deliberately preserves an unknown exit status and
    /// may also have lost the tail of the init output stream.
    async fn wait_log_projection_stopped_after_owner_loss(
        &self,
        _record: &BoxRecord,
        _binding: &OciRuntimeBinding,
    ) -> ExecutionManagerResult<()> {
        Ok(())
    }

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

    pub(super) fn client(&self) -> RuntimeClient {
        self.client.clone()
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

    async fn require_operation(
        &self,
        operation: RuntimeOperation,
        label: &str,
    ) -> ExecutionManagerResult<()> {
        let info = self
            .client
            .features()
            .await
            .map_err(|error| sdk_error("features", error))?;
        if !info.operations.contains(&operation) {
            return Err(ExecutionManagerError::Unavailable(format!(
                "A3S OCI Runtime does not advertise {label}"
            )));
        }
        Ok(())
    }

    async fn launch_preflighted<BeforeStart, BeforeStartFuture>(
        &self,
        info: &RuntimeInfo,
        execution_id: &ExecutionId,
        preparation: OciBundlePreparationContext,
        prepared: OciPreparedExecution,
        before_start: BeforeStart,
    ) -> ExecutionManagerResult<OciRuntimeLaunch>
    where
        BeforeStart: FnOnce(OciRuntimeBinding) -> BeforeStartFuture,
        BeforeStartFuture: Future<Output = ExecutionManagerResult<()>>,
    {
        let id = preparation.runtime_container_id.clone();
        let requested = preparation.isolation.clone();
        let expected_config_digest = prepared.bundle.config_digest().to_string();
        let expected_attachments_digest = prepared
            .attachments
            .digest()
            .map_err(|error| sdk_error("digest attachments", error))?;
        let created = self
            .client
            .create(CreateRequest {
                context: preparation.create_context,
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
                preparation.execution_generation,
                &created,
                requested.class(),
            )
            .await;
            return Err(error);
        }

        let created_binding = OciRuntimeBinding::from_record(self.endpoint.clone(), &id, &created)?;
        before_start(created_binding).await?;

        let target = ContainerTarget::exact(id.clone(), created.generation);
        let started = match self
            .client
            .start(StartRequest {
                context: operation_context(
                    preparation.operation_seed.as_str(),
                    preparation.execution_generation,
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
                preparation.execution_generation,
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

    /// Read terminal evidence only after the runtime has authoritatively
    /// reported this exact generation as stopped. A recovered runtime is
    /// allowed to refuse an exact exit result when no authenticated reaper
    /// survived owner death; Box must preserve that uncertainty rather than
    /// inventing an exit code.
    async fn wait_stopped(
        &self,
        record: &ContainerRecord,
        binding: &OciRuntimeBinding,
    ) -> ExecutionManagerResult<Option<ExitStatus>> {
        binding.validate_record(record)?;
        if *record.state.status() != ContainerState::Stopped {
            return Err(ExecutionManagerError::Internal(format!(
                "A3S OCI terminal wait was requested for non-stopped container {}",
                binding.target.id
            )));
        }
        match self
            .client
            .wait(WaitRequest {
                target: binding.target.clone(),
                timeout_ms: Some(0),
            })
            .await
        {
            Ok(status) => {
                status
                    .validate()
                    .map_err(|error| sdk_error("wait", error))?;
                Ok(Some(status))
            }
            Err(error) if error.code == ErrorCode::FailedPrecondition => Ok(None),
            Err(error) => Err(sdk_error("wait", error)),
        }
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

    async fn set_paused(
        &self,
        execution_id: &ExecutionId,
        execution_generation: ExecutionGeneration,
        operation_seed: &str,
        binding: &OciRuntimeBinding,
        paused: bool,
    ) -> ExecutionManagerResult<ContainerRecord> {
        binding.validate_for(execution_id)?;
        let (operation, label) = if paused {
            (RuntimeOperation::Pause, "pause")
        } else {
            (RuntimeOperation::Resume, "resume")
        };
        self.require_operation(operation, label).await?;
        let request = ContainerOperationRequest {
            context: operation_context(
                operation_seed,
                execution_generation,
                label,
                (&binding.target, paused),
            )?,
            target: binding.target.clone(),
        };
        let result = if paused {
            self.client.pause(request).await
        } else {
            self.client.resume(request).await
        };
        let record = match result {
            Ok(record) => record,
            Err(error) if error.code == ErrorCode::NotFound => {
                return Err(ExecutionManagerError::NotFound(execution_id.clone()))
            }
            Err(error) => return Err(sdk_error(label, error)),
        };
        validate_freezer_record(binding, &record, paused, label)?;
        Ok(record)
    }

    async fn update_resources(
        &self,
        execution_id: &ExecutionId,
        execution_generation: ExecutionGeneration,
        operation_seed: &BoxOperationId,
        binding: &OciRuntimeBinding,
        resources: LinuxResources,
    ) -> ExecutionManagerResult<ContainerRecord> {
        binding.validate_for(execution_id)?;
        self.require_operation(RuntimeOperation::Update, "update")
            .await?;
        let updated = match self
            .client
            .update(UpdateRequest {
                // The caller key and exact target define mutation identity.
                // The runtime journals the full request and rejects reuse of
                // this ID with changed resource content.
                context: operation_context(
                    operation_seed.as_str(),
                    execution_generation,
                    "update",
                    &binding.target,
                )?,
                target: binding.target.clone(),
                resources,
            })
            .await
        {
            Ok(record) => record,
            Err(error) if error.code == ErrorCode::NotFound => {
                return Err(ExecutionManagerError::NotFound(execution_id.clone()))
            }
            Err(error) => return Err(sdk_error("update", error)),
        };
        validate_updated_record(binding, &updated)?;
        Ok(updated)
    }

    async fn processes(
        &self,
        execution_id: &ExecutionId,
        binding: &OciRuntimeBinding,
    ) -> ExecutionManagerResult<Vec<ProcessRecord>> {
        binding.validate_for(execution_id)?;
        self.require_operation(RuntimeOperation::Processes, "processes")
            .await?;
        let processes = match self
            .client
            .processes(ProcessesRequest {
                target: binding.target.clone(),
            })
            .await
        {
            Ok(processes) => processes,
            Err(error) if error.code == ErrorCode::NotFound => {
                return Err(ExecutionManagerError::NotFound(execution_id.clone()))
            }
            Err(error) => return Err(sdk_error("processes", error)),
        };
        validate_process_records(binding, &processes)?;
        Ok(processes)
    }

    async fn stats(
        &self,
        execution_id: &ExecutionId,
        binding: &OciRuntimeBinding,
    ) -> ExecutionManagerResult<a3s_oci_sdk::ContainerStats> {
        binding.validate_for(execution_id)?;
        self.require_operation(RuntimeOperation::Stats, "stats")
            .await?;
        let stats = match self
            .client
            .stats(StatsRequest {
                target: binding.target.clone(),
            })
            .await
        {
            Ok(stats) => stats,
            Err(error) if error.code == ErrorCode::NotFound => {
                return Err(ExecutionManagerError::NotFound(execution_id.clone()))
            }
            Err(error) => return Err(sdk_error("stats", error)),
        };
        stats
            .validate()
            .map_err(|error| sdk_error("stats", error))?;
        if stats.target != binding.target {
            return Err(ExecutionManagerError::Internal(format!(
                "A3S OCI stats returned a different target for {execution_id}"
            )));
        }
        Ok(stats)
    }

    async fn events(
        &self,
        execution_id: &ExecutionId,
        binding: &OciRuntimeBinding,
        request: &ExecutionEventsRequest,
    ) -> ExecutionManagerResult<a3s_oci_sdk::EventBatch> {
        binding.validate_for(execution_id)?;
        self.require_operation(RuntimeOperation::Events, "events")
            .await?;
        let batch = match self
            .client
            .events(EventsRequest {
                container: Some(binding.target.clone()),
                after_sequence: request.after_sequence,
                limit: request.limit,
                wait_timeout_ms: request.wait_timeout_ms,
            })
            .await
        {
            Ok(batch) => batch,
            Err(error) if error.code == ErrorCode::NotFound => {
                return Err(ExecutionManagerError::NotFound(execution_id.clone()))
            }
            Err(error) => return Err(sdk_error("events", error)),
        };
        validate_event_batch(binding, request.after_sequence, &batch)?;
        Ok(batch)
    }

    async fn file(
        &self,
        execution_id: &ExecutionId,
        execution_generation: ExecutionGeneration,
        binding: &OciRuntimeBinding,
        request: BoxFileRequest,
    ) -> ExecutionManagerResult<BoxFileResponse> {
        binding.validate_for(execution_id)?;
        self.require_operation(RuntimeOperation::File, "file")
            .await?;
        let maximum_download_size = match (request.op, request.max_bytes) {
            (BoxFileOp::Upload, Some(_)) => {
                return Err(ExecutionManagerError::InvalidRequest(
                    "max_bytes is only valid for file downloads".to_string(),
                ))
            }
            (BoxFileOp::Download, Some(limit)) if limit == 0 || limit > MAX_BOUNDED_FILE_BYTES => {
                return Err(ExecutionManagerError::InvalidRequest(format!(
                    "download max_bytes must be between 1 and {MAX_BOUNDED_FILE_BYTES}"
                )))
            }
            (BoxFileOp::Download, limit) => limit,
            (BoxFileOp::Upload, None) => None,
        };
        let expected_upload_size = match request.op {
            BoxFileOp::Upload => {
                let encoded = request.data.as_deref().ok_or_else(|| {
                    ExecutionManagerError::InvalidRequest(
                        "file upload requires base64 data".to_string(),
                    )
                })?;
                let maximum_encoded = MAX_FILE_TRANSFER_BYTES.div_ceil(3) * 4;
                if encoded.len() > maximum_encoded {
                    return Err(ExecutionManagerError::InvalidRequest(format!(
                        "file upload payload exceeds {MAX_FILE_TRANSFER_BYTES} decoded bytes"
                    )));
                }
                let decoded = STANDARD.decode(encoded).map_err(|error| {
                    ExecutionManagerError::InvalidRequest(format!(
                        "file upload data is not valid base64: {error}"
                    ))
                })?;
                if decoded.len() > MAX_FILE_TRANSFER_BYTES {
                    return Err(ExecutionManagerError::InvalidRequest(format!(
                        "file upload payload exceeds {MAX_FILE_TRANSFER_BYTES} decoded bytes"
                    )));
                }
                Some(decoded.len() as u64)
            }
            BoxFileOp::Download => None,
        };
        let operation = match request.op {
            BoxFileOp::Upload => OciFileOp::Upload,
            BoxFileOp::Download => OciFileOp::Download,
        };
        let context = if request.op == BoxFileOp::Upload {
            Some(operation_context(
                &format!("session-{}", uuid::Uuid::new_v4().simple()),
                execution_generation,
                "file",
                (&binding.target, &request),
            )?)
        } else {
            None
        };
        let sdk_request = OciFileRequest {
            target: binding.target.clone(),
            op: operation,
            path: request.guest_path,
            data: request.data,
            user: request.user,
            context,
        };
        let response = match self.client.file(sdk_request.clone()).await {
            Err(error) if error.retryable => self.client.file(sdk_request).await,
            result => result,
        }
        .map_err(|error| sdk_error("file", error))?;
        validate_file_response(
            binding,
            &response,
            operation,
            expected_upload_size,
            maximum_download_size,
        )?;
        Ok(BoxFileResponse {
            success: true,
            data: response.data,
            size: response.size,
            error: None,
        })
    }

    async fn filesystem(
        &self,
        execution_id: &ExecutionId,
        execution_generation: ExecutionGeneration,
        binding: &OciRuntimeBinding,
        request: BoxFilesystemRequest,
    ) -> ExecutionManagerResult<BoxFilesystemResponse> {
        binding.validate_for(execution_id)?;
        self.require_operation(RuntimeOperation::Filesystem, "filesystem")
            .await?;
        let operation = match request.op {
            BoxFilesystemOp::Stat => OciFilesystemOp::Stat,
            BoxFilesystemOp::MakeDir => OciFilesystemOp::MakeDir,
            BoxFilesystemOp::Move => OciFilesystemOp::Move,
            BoxFilesystemOp::ListDir => OciFilesystemOp::ListDir,
            BoxFilesystemOp::Remove => OciFilesystemOp::Remove,
        };
        let context = if operation.is_mutating() {
            Some(operation_context(
                &format!("session-{}", uuid::Uuid::new_v4().simple()),
                execution_generation,
                "filesystem",
                (&binding.target, &request),
            )?)
        } else {
            None
        };
        let sdk_request = OciFilesystemRequest {
            target: binding.target.clone(),
            op: operation,
            path: request.path,
            destination: request.destination,
            depth: request.depth,
            user: request.user,
            context,
        };
        let response = match self.client.filesystem(sdk_request.clone()).await {
            Err(error) if error.retryable => self.client.filesystem(sdk_request).await,
            result => result,
        }
        .map_err(|error| sdk_error("filesystem", error))?;
        validate_filesystem_response(binding, &response, operation)?;
        Ok(BoxFilesystemResponse {
            success: true,
            entry: response.entry.map(map_filesystem_entry),
            entries: response
                .entries
                .into_iter()
                .map(map_filesystem_entry)
                .collect(),
            error: None,
        })
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

    pub(super) fn metadata<'a>(
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
        if metadata.runtime_route == crate::ManagedRuntimeRoute::BoxVm {
            return Err(ExecutionManagerError::Internal(format!(
                "managed execution {} is pinned to the Box VM route",
                record.id
            )));
        }
        Ok(metadata)
    }

    pub(super) fn execution_id(&self, record: &BoxRecord) -> ExecutionManagerResult<ExecutionId> {
        ExecutionId::new(record.id.clone())
            .map_err(|error| ExecutionManagerError::Internal(error.to_string()))
    }

    pub(super) fn binding(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<Option<OciRuntimeBinding>> {
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

    pub(super) fn client(&self) -> RuntimeClient {
        self.adapter.client()
    }

    fn preparation_context(
        &self,
        record: &BoxRecord,
        info: &RuntimeInfo,
    ) -> ExecutionManagerResult<OciBundlePreparationContext> {
        let execution_id = self.execution_id(record)?;
        let metadata = self.metadata(record)?;
        let isolation = oci_isolation_request(record.isolation);
        let create_context = operation_context(
            metadata.operation_id.as_str(),
            metadata.generation,
            "create",
            isolation.class(),
        )?;
        Ok(OciBundlePreparationContext {
            runtime_container_id: runtime_container_id(&execution_id)?,
            create_context,
            isolation,
            attachment_capabilities: info.attachments.clone(),
            execution_generation: metadata.generation,
            operation_seed: metadata.operation_id.clone(),
        })
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
        runtime: &ContainerRecord,
        binding: &OciRuntimeBinding,
    ) -> ExecutionManagerResult<LocalExecutionObservation> {
        let execution_id = self.execution_id(record)?;
        let generation = self.metadata(record)?.generation;
        let status = self.adapter.wait_stopped(runtime, binding).await?;
        if status.is_some() {
            self.provider.ensure_log_projection(record, binding).await?;
            self.provider
                .wait_log_projection_drained(record, binding)
                .await?;
        } else {
            self.provider
                .wait_log_projection_stopped_after_owner_loss(record, binding)
                .await?;
        }
        self.adapter
            .delete(&execution_id, generation, binding, DeleteMode::StoppedOnly)
            .await?;
        self.provider.cleanup(record).await?;
        Ok(LocalExecutionObservation {
            state: ExecutionState::Stopped,
            handle: None,
            exit_code: status.as_ref().map(exit_code).transpose()?,
        })
    }
}

fn managed_resource_home(record: &BoxRecord) -> ExecutionManagerResult<PathBuf> {
    let boxes = record.box_dir.parent().ok_or_else(|| {
        ExecutionManagerError::Internal(format!(
            "managed OCI execution {} has no boxes directory",
            record.id
        ))
    })?;
    let home = boxes.parent().ok_or_else(|| {
        ExecutionManagerError::Internal(format!(
            "managed OCI execution {} has no runtime home directory",
            record.id
        ))
    })?;
    if boxes.file_name().and_then(|name| name.to_str()) != Some("boxes")
        || home.join("boxes").join(&record.id) != record.box_dir
    {
        return Err(ExecutionManagerError::Internal(format!(
            "managed OCI execution {} has an unexpected host directory {}",
            record.id,
            record.box_dir.display()
        )));
    }
    Ok(home.to_path_buf())
}

async fn rollback_execution_resources(resources: ExecutionResourceGuard, record: &BoxRecord) {
    let execution_id = record.id.clone();
    if let Err(error) = tokio::task::spawn_blocking(move || resources.rollback()).await {
        tracing::warn!(
            %execution_id,
            %error,
            "Managed OCI resource rollback task failed"
        );
    }
}

#[async_trait]
impl LocalExecutionBackend for OciLocalExecutionBackend {
    fn route_for_create(
        &self,
        _record: &BoxRecord,
    ) -> ExecutionManagerResult<crate::ManagedRuntimeRoute> {
        Ok(crate::ManagedRuntimeRoute::OciSdk)
    }

    async fn preflight(&self, record: &BoxRecord) -> ExecutionManagerResult<()> {
        self.metadata(record)?;
        let info = self.adapter.require_isolation(record.isolation).await?;
        let context = self.preparation_context(record, &info)?;
        self.provider.preflight(record, &context)
    }

    async fn start(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        let execution_id = self.execution_id(record)?;
        self.metadata(record)?;
        if let Some((runtime, binding)) = self.current_runtime(record).await? {
            if *runtime.state.status() == ContainerState::Running {
                self.provider
                    .ensure_log_projection(record, &binding)
                    .await?;
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
        let preparation = self.preparation_context(record, &info)?;
        self.provider.preflight(record, &preparation)?;
        let resource_home = managed_resource_home(record)?;
        let resource_record = record.clone();
        let resources = tokio::task::spawn_blocking(move || {
            ExecutionResourceGuard::prepare(&resource_home, &resource_record)
        })
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "managed OCI resource preparation task failed for {}: {error}",
                record.id
            ))
        })??;
        let prepared = match self.provider.prepare(record, &preparation).await {
            Ok(prepared) => prepared,
            Err(error) => {
                rollback_execution_resources(resources, record).await;
                return Err(error);
            }
        };
        if let Err(error) = prepared.validate_for(record, &preparation) {
            let cleanup = self.provider.cleanup(record).await;
            rollback_execution_resources(resources, record).await;
            return match cleanup {
                Ok(()) => Err(error),
                Err(cleanup) => Err(ExecutionManagerError::Internal(format!(
                    "{error}; Box OCI preparation cleanup also failed: {cleanup}"
                ))),
            };
        }
        let console_log = prepared.console_log.clone();
        let anonymous_volumes = prepared.anonymous_volumes.clone();
        let provider = Arc::clone(&self.provider);
        let projection_record = record.clone();
        let launch = self
            .adapter
            .launch_preflighted(
                &info,
                &execution_id,
                preparation,
                prepared,
                move |binding| async move {
                    provider
                        .ensure_log_projection(&projection_record, &binding)
                        .await
                },
            )
            .await;
        match launch {
            Ok(launch) if *launch.record.state.status() == ContainerState::Running => {
                resources.disarm();
                Ok(self.handle(record, launch.binding, console_log, anonymous_volumes))
            }
            Ok(_) => {
                // A runtime generation exists and owns the prepared rootfs even
                // when it completed before Box could publish Running. Terminal
                // reconciliation performs the normal provider/resource cleanup.
                resources.disarm();
                Err(ExecutionManagerError::Unavailable(format!(
                    "execution {execution_id} completed while A3S OCI startup was being published"
                )))
            }
            Err(error) => {
                // Unknown create/start outcomes must be reconciled, not erased.
                // Cleanup product preparation only when the runtime proves no
                // current generation exists for the deterministic ID.
                match self.adapter.state_current(&execution_id).await {
                    Ok(Some(_)) => {
                        resources.disarm();
                        return Err(error);
                    }
                    Err(reconcile_error) => {
                        resources.disarm();
                        return Err(ExecutionManagerError::Unavailable(format!(
                            "{error}; A3S OCI launch ownership could not be reconciled and Box preparation was retained: {reconcile_error}"
                        )));
                    }
                    Ok(None) => {}
                }
                let cleanup = self.provider.cleanup(record).await;
                rollback_execution_resources(resources, record).await;
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
            self.provider
                .ensure_log_projection(record, &binding)
                .await?;
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

        if *runtime.state.status() == ContainerState::Running {
            self.provider
                .ensure_log_projection(record, &binding)
                .await?;
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
            ContainerState::Stopped => self.terminal_observation(record, &runtime, &binding).await,
        }
    }

    async fn pause(
        &self,
        record: &BoxRecord,
        keep_memory: bool,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        if !keep_memory {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "A3S OCI in-place pause requires memory retention for {}",
                record.id
            )));
        }
        let execution_id = self.execution_id(record)?;
        let generation = self.metadata(record)?.generation;
        let operation_seed = freezer_operation_seed(record, &execution_id, true)?;
        let binding = self.binding(record)?.ok_or_else(|| {
            ExecutionManagerError::Internal(format!(
                "execution {execution_id} has no exact A3S OCI binding to pause"
            ))
        })?;
        self.adapter
            .set_paused(&execution_id, generation, &operation_seed, &binding, true)
            .await?;
        Ok(self.handle(
            record,
            binding,
            record.console_log.clone(),
            record.anonymous_volumes.clone(),
        ))
    }

    async fn resume(&self, record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        let execution_id = self.execution_id(record)?;
        let generation = self.metadata(record)?.generation;
        let operation_seed = freezer_operation_seed(record, &execution_id, false)?;
        let binding = self.binding(record)?.ok_or_else(|| {
            ExecutionManagerError::Internal(format!(
                "execution {execution_id} has no exact A3S OCI binding to resume"
            ))
        })?;
        self.adapter
            .set_paused(&execution_id, generation, &operation_seed, &binding, false)
            .await?;
        Ok(self.handle(
            record,
            binding,
            record.console_log.clone(),
            record.anonymous_volumes.clone(),
        ))
    }

    async fn preflight_resource_update(
        &self,
        record: &BoxRecord,
        update: &ExecutionResourceUpdate,
    ) -> ExecutionManagerResult<()> {
        update.validate()?;
        let execution_id = self.execution_id(record)?;
        self.binding(record)?.ok_or_else(|| {
            ExecutionManagerError::Internal(format!(
                "execution {execution_id} has no exact A3S OCI binding to update"
            ))
        })?;
        self.adapter
            .require_operation(RuntimeOperation::Update, "update")
            .await?;
        compile_resource_update(record, update)?;
        Ok(())
    }

    async fn update_resources(
        &self,
        record: &BoxRecord,
        operation_id: &BoxOperationId,
        update: &ExecutionResourceUpdate,
    ) -> ExecutionManagerResult<()> {
        let execution_id = self.execution_id(record)?;
        let generation = self.metadata(record)?.generation;
        let binding = self.binding(record)?.ok_or_else(|| {
            ExecutionManagerError::Internal(format!(
                "execution {execution_id} has no exact A3S OCI binding to update"
            ))
        })?;
        let resources = compile_resource_update(record, update)?;
        self.adapter
            .update_resources(&execution_id, generation, operation_id, &binding, resources)
            .await?;
        Ok(())
    }

    async fn list_processes(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<ExecutionProcessInventory> {
        let execution_id = self.execution_id(record)?;
        let generation = self.metadata(record)?.generation;
        let binding = self.binding(record)?.ok_or_else(|| {
            ExecutionManagerError::Internal(format!(
                "execution {execution_id} has no exact A3S OCI binding for process inventory"
            ))
        })?;
        let mut processes = self
            .adapter
            .processes(&execution_id, &binding)
            .await?
            .into_iter()
            .map(|process| ExecutionProcessInfo {
                process_id: process.target.process_id.to_string(),
                pid: process.pid,
                terminal: process.terminal,
            })
            .collect::<Vec<_>>();
        processes.sort_by(|left, right| left.process_id.cmp(&right.process_id));
        let inventory = ExecutionProcessInventory {
            execution_id,
            generation,
            processes,
        };
        inventory.validate()?;
        Ok(inventory)
    }

    async fn stats(&self, record: &BoxRecord) -> ExecutionManagerResult<ExecutionStats> {
        let execution_id = self.execution_id(record)?;
        let generation = self.metadata(record)?.generation;
        let binding = self.binding(record)?.ok_or_else(|| {
            ExecutionManagerError::Internal(format!(
                "execution {execution_id} has no exact A3S OCI binding for stats"
            ))
        })?;
        let stats = self.adapter.stats(&execution_id, &binding).await?;
        let stats = ExecutionStats {
            execution_id,
            generation,
            timestamp_unix_ns: stats.timestamp_unix_ns,
            cpu: ExecutionCpuStats {
                usage_ns: stats.cpu.usage_ns,
                user_ns: stats.cpu.user_ns,
                system_ns: stats.cpu.system_ns,
                throttled_ns: stats.cpu.throttled_ns,
            },
            memory: ExecutionMemoryStats {
                usage_bytes: stats.memory.usage_bytes,
                limit_bytes: stats.memory.limit_bytes,
                peak_bytes: stats.memory.peak_bytes,
            },
            process_count: stats.process_count,
            metrics: stats.metrics,
        };
        stats.validate()?;
        Ok(stats)
    }

    async fn events(
        &self,
        record: &BoxRecord,
        request: ExecutionEventsRequest,
    ) -> ExecutionManagerResult<ExecutionEventBatch> {
        request.validate()?;
        let execution_id = self.execution_id(record)?;
        let generation = self.metadata(record)?.generation;
        let binding = self.binding(record)?.ok_or_else(|| {
            ExecutionManagerError::Internal(format!(
                "execution {execution_id} has no exact A3S OCI binding for events"
            ))
        })?;
        let batch = self
            .adapter
            .events(&execution_id, &binding, &request)
            .await?;
        let events = batch
            .events
            .into_iter()
            .map(|event| {
                Ok(ExecutionRuntimeEvent {
                    sequence: event.sequence,
                    timestamp_unix_ns: event.timestamp_unix_ns,
                    process_id: event.process_id.map(|process_id| process_id.to_string()),
                    kind: map_event_kind(event.kind)?,
                    attributes: event.attributes,
                })
            })
            .collect::<ExecutionManagerResult<Vec<_>>>()?;
        let batch = ExecutionEventBatch {
            execution_id,
            generation,
            events,
            next_sequence: batch.next_sequence,
        };
        batch.validate_after(request.after_sequence)?;
        Ok(batch)
    }

    async fn execute(
        &self,
        record: &BoxRecord,
        request: BoxExecRequest,
    ) -> ExecutionManagerResult<ExecOutput> {
        super::oci_session::execute(self, record, request).await
    }

    async fn start_process(
        &self,
        record: &BoxRecord,
        request: BoxExecRequest,
    ) -> ExecutionManagerResult<ExecutionProcess> {
        super::oci_session::start_process(self, record, request).await
    }

    async fn start_pty(
        &self,
        record: &BoxRecord,
        request: PtyRequest,
    ) -> ExecutionManagerResult<ExecutionProcess> {
        super::oci_session::start_pty(self, record, request).await
    }

    async fn transfer_file(
        &self,
        record: &BoxRecord,
        request: BoxFileRequest,
    ) -> ExecutionManagerResult<BoxFileResponse> {
        let execution_id = self.execution_id(record)?;
        let generation = self.metadata(record)?.generation;
        let binding = self.binding(record)?.ok_or_else(|| {
            ExecutionManagerError::Internal(format!(
                "execution {execution_id} has no exact A3S OCI binding for file transfer"
            ))
        })?;
        self.adapter
            .file(&execution_id, generation, &binding, request)
            .await
    }

    async fn filesystem(
        &self,
        record: &BoxRecord,
        request: BoxFilesystemRequest,
    ) -> ExecutionManagerResult<BoxFilesystemResponse> {
        let execution_id = self.execution_id(record)?;
        let generation = self.metadata(record)?.generation;
        let binding = self.binding(record)?.ok_or_else(|| {
            ExecutionManagerError::Internal(format!(
                "execution {execution_id} has no exact A3S OCI binding for filesystem access"
            ))
        })?;
        self.adapter
            .filesystem(&execution_id, generation, &binding, request)
            .await
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
        self.provider
            .ensure_log_projection(record, &binding)
            .await?;
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
        self.provider
            .wait_log_projection_drained(record, &binding)
            .await?;
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

fn freezer_operation_seed(
    record: &BoxRecord,
    execution_id: &ExecutionId,
    paused: bool,
) -> ExecutionManagerResult<String> {
    let operation = record
        .managed_execution
        .as_ref()
        .and_then(|metadata| metadata.pending_operation.as_ref());
    match (paused, operation) {
        (true, Some(crate::ManagedExecutionOperation::Pause { operation_id, .. }))
        | (false, Some(crate::ManagedExecutionOperation::Resume { operation_id })) => {
            // Legacy transitional records predate explicit freezer mutation
            // IDs. OCI routing did not exist when they were written, so the
            // exact Box identity remains a safe one-time recovery seed.
            Ok(operation_id
                .as_ref()
                .map(|operation_id| operation_id.as_str())
                .unwrap_or_else(|| execution_id.as_str())
                .to_string())
        }
        (
            _,
            Some(crate::ManagedExecutionOperation::Snapshot {
                snapshot_id,
                source_state: ManagedExecutionState::Running,
                operation_id,
                ..
            }),
        ) => Ok(operation_id
            .as_ref()
            .map(|operation_id| operation_id.as_str().to_string())
            .unwrap_or_else(|| format!("legacy-snapshot-{execution_id}-{snapshot_id}"))),
        _ => Err(ExecutionManagerError::Internal(format!(
            "execution {execution_id} has no matching durable OCI freezer claim"
        ))),
    }
}

pub(super) fn operation_context(
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

fn validate_freezer_record(
    binding: &OciRuntimeBinding,
    record: &ContainerRecord,
    paused: bool,
    operation: &str,
) -> ExecutionManagerResult<()> {
    binding.validate_record(record)?;
    if *record.state.status() != ContainerState::Running || record.is_paused() != paused {
        return Err(ExecutionManagerError::Internal(format!(
            "A3S OCI {operation} returned an invalid freezer state for {} generation {:?}",
            binding.target.id, binding.target.generation
        )));
    }
    Ok(())
}

fn compile_resource_update(
    record: &BoxRecord,
    update: &ExecutionResourceUpdate,
) -> ExecutionManagerResult<LinuxResources> {
    update.validate()?;
    let metadata = record.managed_execution.as_ref().ok_or_else(|| {
        ExecutionManagerError::Internal(format!(
            "execution {} has no managed resource intent",
            record.id
        ))
    })?;
    if record.resource_limits != metadata.request.config.resource_limits {
        return Err(ExecutionManagerError::Internal(format!(
            "execution {} has divergent compatibility and managed resource limits",
            record.id
        )));
    }
    let mut config = metadata.request.config.clone();
    update.apply_to(&mut config.resource_limits);
    let resources = crate::sandbox::oci::SandboxResources::from_box_config(&config)
        .map_err(|error| ExecutionManagerError::InvalidRequest(error.to_string()))?;
    crate::sandbox::oci::compile_resources(&resources)
        .map_err(|error| ExecutionManagerError::InvalidRequest(error.to_string()))
}

fn validate_updated_record(
    binding: &OciRuntimeBinding,
    record: &ContainerRecord,
) -> ExecutionManagerResult<()> {
    binding.validate_record(record)?;
    if *record.state.status() != ContainerState::Running || record.is_paused() {
        return Err(ExecutionManagerError::Internal(format!(
            "A3S OCI update returned an invalid running state for {} generation {:?}",
            binding.target.id, binding.target.generation
        )));
    }
    Ok(())
}

fn validate_file_response(
    binding: &OciRuntimeBinding,
    response: &OciFileResponse,
    operation: OciFileOp,
    expected_upload_size: Option<u64>,
    maximum_download_size: Option<u64>,
) -> ExecutionManagerResult<()> {
    if response.target != binding.target || response.size > MAX_FILE_TRANSFER_BYTES as u64 {
        return Err(ExecutionManagerError::Internal(
            "A3S OCI returned invalid file target or size evidence".to_string(),
        ));
    }
    match operation {
        OciFileOp::Upload
            if response.data.is_some() || Some(response.size) != expected_upload_size =>
        {
            Err(ExecutionManagerError::Internal(
                "A3S OCI returned an invalid file upload acknowledgement".to_string(),
            ))
        }
        OciFileOp::Download => {
            if maximum_download_size.is_some_and(|limit| response.size > limit) {
                return Err(ExecutionManagerError::Internal(
                    "A3S OCI file response exceeds the requested download limit".to_string(),
                ));
            }
            let data = response.data.as_deref().ok_or_else(|| {
                ExecutionManagerError::Internal(
                    "A3S OCI omitted the downloaded file payload".to_string(),
                )
            })?;
            let maximum_encoded = MAX_FILE_TRANSFER_BYTES.div_ceil(3) * 4;
            if data.len() > maximum_encoded {
                return Err(ExecutionManagerError::Internal(format!(
                    "A3S OCI file payload exceeds {MAX_FILE_TRANSFER_BYTES} decoded bytes"
                )));
            }
            let decoded = STANDARD.decode(data).map_err(|error| {
                ExecutionManagerError::Internal(format!(
                    "A3S OCI returned invalid base64 file data: {error}"
                ))
            })?;
            if decoded.len() as u64 != response.size {
                return Err(ExecutionManagerError::Internal(
                    "A3S OCI file size does not match its decoded payload".to_string(),
                ));
            }
            Ok(())
        }
        OciFileOp::Upload => Ok(()),
    }
}

fn validate_filesystem_response(
    binding: &OciRuntimeBinding,
    response: &OciFilesystemResponse,
    operation: OciFilesystemOp,
) -> ExecutionManagerResult<()> {
    const MAX_ENTRIES: usize = 4_096;
    const MAX_RESPONSE_BYTES: usize = 12 * 1024 * 1024;
    let valid_shape = match operation {
        OciFilesystemOp::Stat | OciFilesystemOp::MakeDir | OciFilesystemOp::Move => {
            response.entry.is_some() && response.entries.is_empty()
        }
        OciFilesystemOp::ListDir => response.entry.is_none(),
        OciFilesystemOp::Remove => response.entry.is_none() && response.entries.is_empty(),
    };
    if response.target != binding.target || !valid_shape || response.entries.len() > MAX_ENTRIES {
        return Err(ExecutionManagerError::Internal(
            "A3S OCI returned invalid filesystem target or response shape".to_string(),
        ));
    }
    let encoded = serde_json::to_vec(response).map_err(|error| {
        ExecutionManagerError::Internal(format!(
            "failed to size A3S OCI filesystem response: {error}"
        ))
    })?;
    if encoded.len() > MAX_RESPONSE_BYTES {
        return Err(ExecutionManagerError::Internal(format!(
            "A3S OCI filesystem response exceeds {MAX_RESPONSE_BYTES} bytes"
        )));
    }
    Ok(())
}

fn map_filesystem_entry(entry: OciFilesystemEntry) -> BoxFilesystemEntry {
    BoxFilesystemEntry {
        name: entry.name,
        kind: match entry.kind {
            OciFilesystemEntryKind::Unspecified => BoxFilesystemEntryKind::Unspecified,
            OciFilesystemEntryKind::File => BoxFilesystemEntryKind::File,
            OciFilesystemEntryKind::Directory => BoxFilesystemEntryKind::Directory,
        },
        path: entry.path,
        size: entry.size,
        mode: entry.mode,
        permissions: entry.permissions,
        owner: entry.owner,
        group: entry.group,
        modified_seconds: entry.modified_seconds,
        modified_nanos: entry.modified_nanos,
        symlink_target: entry.symlink_target,
        metadata: entry.metadata,
    }
}

fn validate_process_records(
    binding: &OciRuntimeBinding,
    records: &[ProcessRecord],
) -> ExecutionManagerResult<()> {
    let mut process_ids = BTreeSet::new();
    for record in records {
        if record.target.container != binding.target {
            return Err(ExecutionManagerError::Internal(format!(
                "A3S OCI process inventory crossed the exact target for {}",
                binding.target.id
            )));
        }
        if record.pid == Some(0) {
            return Err(ExecutionManagerError::Internal(format!(
                "A3S OCI process {} returned PID zero",
                record.target.process_id
            )));
        }
        if !process_ids.insert(record.target.process_id.as_str()) {
            return Err(ExecutionManagerError::Internal(format!(
                "A3S OCI process inventory returned duplicate process {}",
                record.target.process_id
            )));
        }
    }
    Ok(())
}

fn validate_event_batch(
    binding: &OciRuntimeBinding,
    after_sequence: u64,
    batch: &a3s_oci_sdk::EventBatch,
) -> ExecutionManagerResult<()> {
    if batch.next_sequence < after_sequence {
        return Err(ExecutionManagerError::Internal(
            "A3S OCI event cursor regressed".to_string(),
        ));
    }
    let mut previous = after_sequence;
    for event in &batch.events {
        if event.container != binding.target {
            return Err(ExecutionManagerError::Internal(format!(
                "A3S OCI events crossed the exact target for {}",
                binding.target.id
            )));
        }
        if event.sequence == 0 || event.sequence <= previous {
            return Err(ExecutionManagerError::Internal(
                "A3S OCI events are not strictly ordered after the requested cursor".to_string(),
            ));
        }
        if event.timestamp_unix_ns == 0 {
            return Err(ExecutionManagerError::Internal(format!(
                "A3S OCI event {} has timestamp zero",
                event.sequence
            )));
        }
        previous = event.sequence;
    }
    if batch.next_sequence < previous {
        return Err(ExecutionManagerError::Internal(
            "A3S OCI event next cursor precedes the returned batch".to_string(),
        ));
    }
    Ok(())
}

fn map_event_kind(kind: RuntimeEventKind) -> ExecutionManagerResult<ExecutionEventKind> {
    match kind {
        RuntimeEventKind::ContainerCreating => Ok(ExecutionEventKind::ContainerCreating),
        RuntimeEventKind::ContainerCreated => Ok(ExecutionEventKind::ContainerCreated),
        RuntimeEventKind::ContainerStarted => Ok(ExecutionEventKind::ContainerStarted),
        RuntimeEventKind::ContainerStopped => Ok(ExecutionEventKind::ContainerStopped),
        RuntimeEventKind::ContainerDeleted => Ok(ExecutionEventKind::ContainerDeleted),
        RuntimeEventKind::ContainerPaused => Ok(ExecutionEventKind::ContainerPaused),
        RuntimeEventKind::ContainerResumed => Ok(ExecutionEventKind::ContainerResumed),
        RuntimeEventKind::ResourcesUpdated => Ok(ExecutionEventKind::ResourcesUpdated),
        RuntimeEventKind::ProcessCreated => Ok(ExecutionEventKind::ProcessCreated),
        RuntimeEventKind::ProcessStarted => Ok(ExecutionEventKind::ProcessStarted),
        RuntimeEventKind::ProcessExited => Ok(ExecutionEventKind::ProcessExited),
        RuntimeEventKind::OutputDropped => Ok(ExecutionEventKind::OutputDropped),
        RuntimeEventKind::RuntimeWarning => Ok(ExecutionEventKind::RuntimeWarning),
        _ => Err(ExecutionManagerError::Unavailable(
            "A3S OCI Runtime returned an event kind unsupported by this Box build".to_string(),
        )),
    }
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

pub(super) fn exit_code(status: &ExitStatus) -> ExecutionManagerResult<i32> {
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

pub(super) fn sdk_error(operation: &str, error: a3s_oci_sdk::Error) -> ExecutionManagerError {
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
