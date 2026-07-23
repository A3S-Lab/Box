//! a3s-box SDK — drive a3s-box from Rust.
//!
//! Provides [`client`]: typed, runtime-backed local management APIs for boxes,
//! pause/unpause/stop/remove/prune lifecycle transitions, images, volumes,
//! networks, snapshot create/restore/list/remove/prune, image build/pull/push,
//! and guest control sockets.

mod box_state;

pub mod bridge;
pub mod client;
pub mod sandbox;

#[cfg(feature = "pipeline-cli")]
pub mod pipeline;

pub use client::{
    A3sBoxClient, A3sBoxPaths, BoxLogLine, BoxStatsSummary, BoxSummary, BuildImage,
    BuildImageSummary, ClientError, CreateNetwork, CreateSnapshot, CreateVolume,
    ImageHealthCheckSummary, ImageHistoryEntry, ImageInspectSummary, ImageSummary,
    ListBoxesOptions, NetworkEndpointSummary, NetworkSummary, PullImage, PushImage,
    PushImageSummary, ReadBoxLogsOptions, RegistryCredentials, RemoveBox, RemoveBoxSummary,
    RestoreSnapshot, Result, RuntimeDiagnostics, RuntimeDiskUsage, RuntimeVirtualizationSummary,
    SnapshotSummary, StopBox, StopBoxSummary, StopOutcome, TagImage, VolumeSummary,
};
pub use sandbox::{
    CommandResult, CommandRunOptions, Commands, Filesystem, FilesystemOptions, Sandbox,
    SandboxCommand, SandboxCreateOptions, SandboxInfo, WriteInfo, DEFAULT_SANDBOX_IMAGE,
    DEFAULT_SANDBOX_TIMEOUT_SECONDS,
};

pub use a3s_box_core::{
    BoxConfig, CreateExecutionRequest, ExecOutput, ExecRequest, ExecutionGeneration,
    ExecutionHealthCheck, ExecutionId, ExecutionIsolation, ExecutionLease, ExecutionManager,
    ExecutionManagerError, ExecutionRecordPolicy, ExecutionReservation, ExecutionRestartPolicy,
    ExecutionState, ExecutionStatus, FileOp, FileRequest, FileResponse, FilesystemEntry,
    FilesystemEntryKind, FilesystemOp, FilesystemRequest, FilesystemResponse, KillOutcome,
    OperationId, Platform, ReconcileOutcome, RestartExecutionOptions,
};
pub use a3s_box_runtime::{RegistryAuth, RegistryProtocol, SignaturePolicy};

#[cfg(unix)]
pub use a3s_box_runtime::{
    AttestationPolicy, AttestationReport, AttestationRequest, ExecClient, PtyClient,
    RaTlsAttestationClient, StreamingExec, StreamingExecInput, StreamingPty, StreamingPtyInput,
    VerificationResult,
};
