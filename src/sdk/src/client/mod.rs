//! Direct runtime-backed management API for a3s-box.
//!
//! This module intentionally returns typed Rust data instead of parsing CLI
//! tables or JSON text. Container metadata is read from the shared `boxes.json`
//! state model, while image, volume, network, and snapshot operations call
//! `a3s-box-runtime` stores directly.

use std::collections::{HashMap, HashSet};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use a3s_box_core::log::{json_log_path, LogDriver, LogEntry, RuntimeConsoleFilter};
use a3s_box_core::network::{IsolationMode, NetworkConfig, NetworkEndpoint, NetworkMode};
use a3s_box_core::platform::Platform;
use a3s_box_core::snapshot::SnapshotMetadata;
use a3s_box_core::vmm::parse_signal_name;
use a3s_box_core::volume::VolumeConfig;
use a3s_box_core::{
    CreateExecutionRequest, ExecOutput, ExecRequest, ExecutionGeneration, ExecutionId,
    ExecutionLease, ExecutionManager, ExecutionReservation, ExecutionSessionManager,
    ExecutionSnapshot, ExecutionSnapshotId, ExecutionStatus, FileRequest, FileResponse,
    FilesystemRequest, FilesystemResponse, KillOutcome, OperationId, ReconcileOutcome,
    RestartExecutionOptions, StoredImage,
};
use a3s_box_runtime::oci::BuildResult as RuntimeBuildResult;
use a3s_box_runtime::{
    is_process_alive, is_process_alive_with_identity, load_resolved_image_config,
    BuildConfig as RuntimeBuildConfig, ImagePuller, ImageReference, ImageStore, NetworkStore,
    OciImage, PushResult, RegistryAuth, RegistryProtocol, RegistryPusher, SignaturePolicy,
    SnapshotStore, VolumeStore,
};
use serde::{Deserialize, Serialize};
use sysinfo::{Pid, System};

#[cfg(all(test, unix))]
use a3s_box_runtime::pid_start_time;
#[cfg(unix)]
use a3s_box_runtime::{AttestationReport, AttestationRequest, ExecClient, PtyClient};

use crate::box_state::{BoxRecord, StateFile};

include!("types.rs");
include!("summaries.rs");
include!("core.rs");
include!("builders.rs");
include!("lifecycle.rs");
include!("support.rs");

#[cfg(test)]
mod tests;
