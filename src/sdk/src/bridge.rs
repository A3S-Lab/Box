//! Versioned JSON bridge used by the pure Python and TypeScript SDKs.
//!
//! The bridge is a machine-only boundary. Language clients send one request to
//! `a3s-box sdk-bridge` on stdin and receive one response on stdout. It calls
//! the direct Rust SDK and never parses human-facing CLI output.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionIsolation, ExecutionSnapshot, ExecutionSnapshotId,
    ExecutionState, FilesystemEntry, FilesystemEntryKind, Platform, PortMapping,
};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    A3sBoxClient, BuildImage, ClientError, CommandRunOptions, CreateNetwork, CreateVolume,
    FilesystemOptions, PullImage, PushImage, Sandbox, SandboxCommand, SandboxCreateOptions,
    SandboxNetwork, TagImage, TmpfsMount, VolumeMount, DEFAULT_SANDBOX_IMAGE,
    DEFAULT_SANDBOX_TIMEOUT_SECONDS,
};

mod request;

pub use request::{
    BridgeRegistryCredentials, BridgeRegistryProtocol, BridgeRequest, BridgeSignaturePolicy,
    BRIDGE_OPERATIONS,
};

pub const BRIDGE_PROTOCOL_VERSION: u8 = 1;

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct BridgeSandboxCreateRequest {
    #[serde(default = "default_image")]
    image: String,
    #[serde(default = "default_timeout_seconds")]
    timeout_seconds: u64,
    #[serde(default)]
    env: BTreeMap<String, String>,
    #[serde(default)]
    labels: BTreeMap<String, String>,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    cpus: Option<u32>,
    #[serde(default)]
    memory_mb: Option<u32>,
    #[serde(default)]
    isolation: ExecutionIsolation,
    #[serde(default)]
    filesystem_snapshot_id: Option<String>,
    #[serde(default)]
    workspace: Option<String>,
    #[serde(default)]
    workdir: Option<String>,
    #[serde(default)]
    user: Option<String>,
    #[serde(default)]
    hostname: Option<String>,
    #[serde(default)]
    mounts: Vec<BridgeVolumeMount>,
    #[serde(default)]
    tmpfs: Vec<BridgeTmpfsMount>,
    #[serde(default)]
    network: BridgeSandboxNetwork,
    #[serde(default)]
    ports: Vec<BridgePortMapping>,
    #[serde(default)]
    dns: Vec<String>,
    #[serde(default)]
    host_aliases: BTreeMap<String, String>,
    #[serde(default)]
    read_only: bool,
    #[serde(default)]
    persistent: bool,
    #[serde(default = "default_true")]
    auto_remove: bool,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BridgeVolumeMount {
    Bind {
        source: String,
        target: String,
        #[serde(default)]
        read_only: bool,
    },
    Named {
        name: String,
        target: String,
        #[serde(default)]
        read_only: bool,
    },
}

impl BridgeVolumeMount {
    fn into_mount(self) -> VolumeMount {
        match self {
            Self::Bind {
                source,
                target,
                read_only,
            } => VolumeMount::bind(source, target).read_only(read_only),
            Self::Named {
                name,
                target,
                read_only,
            } => VolumeMount::named(name, target).read_only(read_only),
        }
    }
}

#[derive(Debug, Default, Deserialize, PartialEq, Eq)]
#[serde(tag = "mode", rename_all = "snake_case")]
pub enum BridgeSandboxNetwork {
    #[default]
    Tsi,
    #[serde(rename = "none")]
    Disabled,
    Bridge {
        name: String,
    },
}

impl From<BridgeSandboxNetwork> for SandboxNetwork {
    fn from(value: BridgeSandboxNetwork) -> Self {
        match value {
            BridgeSandboxNetwork::Tsi => Self::Tsi,
            BridgeSandboxNetwork::Disabled => Self::Disabled,
            BridgeSandboxNetwork::Bridge { name } => Self::bridge(name),
        }
    }
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct BridgePortMapping {
    pub host_port: u16,
    pub guest_port: u16,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
pub struct BridgeTmpfsMount {
    pub target: String,
    #[serde(default)]
    pub size_bytes: Option<u64>,
    #[serde(default)]
    pub read_only: bool,
}

#[derive(Debug, Serialize)]
pub struct BridgeResponse {
    pub protocol_version: u8,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<BridgeError>,
}

#[derive(Debug, Serialize)]
pub struct BridgeError {
    pub code: &'static str,
    pub message: String,
}

#[derive(Debug)]
struct BridgeFailure {
    code: &'static str,
    message: String,
}

impl BridgeResponse {
    fn success(result: Value) -> Self {
        Self {
            protocol_version: BRIDGE_PROTOCOL_VERSION,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    fn failure(error: BridgeFailure) -> Self {
        Self {
            protocol_version: BRIDGE_PROTOCOL_VERSION,
            ok: false,
            result: None,
            error: Some(BridgeError {
                code: error.code,
                message: error.message,
            }),
        }
    }
}

/// Decode and execute one bridge request with the default local SDK client.
pub async fn dispatch_json(input: &str) -> BridgeResponse {
    let request = match serde_json::from_str(input) {
        Ok(request) => request,
        Err(error) => {
            return BridgeResponse::failure(BridgeFailure {
                code: "invalid_request",
                message: format!("invalid SDK bridge request: {error}"),
            })
        }
    };
    handle_request(&A3sBoxClient::new(), request).await
}

pub async fn handle_request(client: &A3sBoxClient, request: BridgeRequest) -> BridgeResponse {
    match execute_request(client, request).await {
        Ok(result) => BridgeResponse::success(result),
        Err(error) => BridgeResponse::failure(error),
    }
}

async fn execute_request(
    client: &A3sBoxClient,
    request: BridgeRequest,
) -> Result<Value, BridgeFailure> {
    match request {
        BridgeRequest::SdkCapabilities => Ok(json!({
            "protocol_version": BRIDGE_PROTOCOL_VERSION,
            "operations": BRIDGE_OPERATIONS,
        })),
        BridgeRequest::ImageBuild {
            context_dir,
            dockerfile,
            tag,
            build_args,
            quiet,
            platforms,
            target,
            no_cache,
        } => {
            let context_dir = PathBuf::from(context_dir);
            // Bridge stdout is reserved for one JSON response envelope. The
            // runtime builder's human progress renderer writes to stdout, so
            // language SDK builds must stay quiet until progress is exposed as
            // a separate structured event channel.
            if !quiet {
                return Err(invalid(
                    "language SDK image builds must be quiet because bridge stdout is reserved for JSON; structured build progress is not implemented yet",
                ));
            }
            let mut request = BuildImage::new(&context_dir).quiet(true).no_cache(no_cache);
            if let Some(path) = dockerfile {
                let path = PathBuf::from(path);
                request = request.dockerfile_path(if path.is_relative() {
                    context_dir.join(path)
                } else {
                    path
                });
            }
            if let Some(tag) = tag {
                request = request.tag(tag);
            }
            for (key, value) in build_args {
                request = request.build_arg(key, value);
            }
            for platform in platforms {
                request =
                    request.platform(Platform::parse(&platform).map_err(ClientError::Validation)?);
            }
            if let Some(target) = target {
                request = request.target(target);
            }
            serialize_value(client.build_image(request).await?)
        }
        BridgeRequest::ImagePull {
            reference,
            force,
            platform,
            credentials,
            signature_policy,
        } => {
            let mut request = PullImage::new(reference)
                .force(force)
                .signature_policy(signature_policy.into());
            if let Some(platform) = platform {
                Platform::parse(&platform).map_err(ClientError::Validation)?;
                request = request.platform(platform);
            }
            if let Some(credentials) = credentials {
                request = request.credentials(credentials.into());
            }
            serialize_value(client.pull_image(request).await?)
        }
        BridgeRequest::ImageGet { reference } => {
            serialize_field("image", client.get_image(&reference).await?)
        }
        BridgeRequest::ImageList => serialize_field("images", client.list_images().await?),
        BridgeRequest::ImageInspect { reference } => {
            serialize_field("image", client.inspect_image(&reference).await?)
        }
        BridgeRequest::ImageHistory { reference } => {
            serialize_field("history", client.image_history(&reference).await?)
        }
        BridgeRequest::ImageTag { source, target } => {
            serialize_value(client.tag_image(TagImage::new(source, target)).await?)
        }
        BridgeRequest::ImagePush {
            source,
            target,
            credentials,
            registry_protocol,
        } => {
            let mut request = PushImage::new(source, target);
            if let Some(credentials) = credentials {
                request = request.credentials(credentials.into());
            }
            if let Some(registry_protocol) = registry_protocol {
                request = request.registry_protocol(registry_protocol.into());
            }
            serialize_value(client.push_image(request).await?)
        }
        BridgeRequest::ImageRemove { reference } => {
            client.remove_image(&reference).await?;
            Ok(json!({
                "reference": reference,
                "removed": true,
            }))
        }
        BridgeRequest::ImageEvict => serialize_field("references", client.evict_images().await?),
        BridgeRequest::VolumeCreate {
            name,
            labels,
            size_limit,
        } => {
            let mut request = CreateVolume::new(name).size_limit(size_limit);
            for (key, value) in labels {
                request = request.label(key, value);
            }
            serialize_value(client.create_volume(request)?)
        }
        BridgeRequest::VolumeGet { name } => serialize_field("volume", client.get_volume(&name)?),
        BridgeRequest::VolumeList => serialize_field("volumes", client.list_volumes()?),
        BridgeRequest::VolumeRemove { name, force } => {
            serialize_value(client.remove_volume(&name, force)?)
        }
        BridgeRequest::VolumePrune => serialize_field("names", client.prune_volumes()?),
        BridgeRequest::NetworkCreate {
            name,
            subnet,
            labels,
        } => {
            let mut request = CreateNetwork::new(name).subnet(subnet);
            for (key, value) in labels {
                request = request.label(key, value);
            }
            serialize_value(client.create_network(request)?)
        }
        BridgeRequest::NetworkGet { name } => {
            serialize_field("network", client.get_network(&name)?)
        }
        BridgeRequest::NetworkList => serialize_field("networks", client.list_networks()?),
        BridgeRequest::NetworkRemove { name } => serialize_value(client.remove_network(&name)?),
        BridgeRequest::NetworkPrune => serialize_field("names", client.prune_networks()?),
        BridgeRequest::SandboxCreate(request) => {
            let BridgeSandboxCreateRequest {
                image,
                timeout_seconds,
                env,
                labels,
                name,
                cpus,
                memory_mb,
                isolation,
                filesystem_snapshot_id,
                workspace,
                workdir,
                user,
                hostname,
                mounts,
                tmpfs,
                network,
                ports,
                dns,
                host_aliases,
                read_only,
                persistent,
                auto_remove,
            } = *request;
            let rootfs_snapshot_id = filesystem_snapshot_id
                .map(ExecutionSnapshotId::new)
                .transpose()
                .map_err(|error| invalid(error.to_string()))?;
            let ports = ports
                .into_iter()
                .map(|port| {
                    PortMapping::tcp(port.host_port, port.guest_port)
                        .map_err(ClientError::Validation)
                })
                .collect::<crate::Result<Vec<_>>>()?;
            let tmpfs = tmpfs
                .into_iter()
                .map(|mount| {
                    let mut value = TmpfsMount::new(mount.target).read_only(mount.read_only);
                    if let Some(size_bytes) = mount.size_bytes {
                        value = value.size_bytes(size_bytes);
                    }
                    value
                })
                .collect();
            let sandbox = Sandbox::create_with_client(
                client.clone(),
                SandboxCreateOptions {
                    image,
                    timeout_seconds,
                    envs: env,
                    metadata: labels,
                    name,
                    cpus,
                    memory_mb,
                    isolation,
                    rootfs_snapshot_id,
                    workspace: workspace.map(PathBuf::from),
                    workdir,
                    user,
                    hostname,
                    mounts: mounts
                        .into_iter()
                        .map(BridgeVolumeMount::into_mount)
                        .collect(),
                    tmpfs,
                    network: network.into(),
                    ports,
                    dns_servers: dns,
                    host_aliases,
                    read_only,
                    persistent,
                    auto_remove,
                },
            )
            .await?;
            Ok(sandbox_info_value(&sandbox))
        }
        BridgeRequest::SandboxInspect { sandbox_id } => {
            let sandbox = Sandbox::connect_with_client(client.clone(), sandbox_id).await?;
            Ok(sandbox_info_value(&sandbox))
        }
        BridgeRequest::SandboxKill {
            sandbox_id,
            generation,
        } => {
            let sandbox = bridge_sandbox(client, sandbox_id, generation, ExecutionState::Running)?;
            sandbox.kill().await?;
            Ok(sandbox_info_value(&sandbox))
        }
        BridgeRequest::SandboxPause {
            sandbox_id,
            generation,
            keep_memory,
        } => {
            let sandbox = bridge_sandbox(client, sandbox_id, generation, ExecutionState::Running)?;
            sandbox.pause(keep_memory).await?;
            Ok(sandbox_info_value(&sandbox))
        }
        BridgeRequest::SandboxResume {
            sandbox_id,
            generation,
        } => {
            let sandbox = bridge_sandbox(client, sandbox_id, generation, ExecutionState::Paused)?;
            sandbox.resume().await?;
            Ok(sandbox_info_value(&sandbox))
        }
        BridgeRequest::SandboxSnapshotCreate {
            sandbox_id,
            generation,
            snapshot_id,
        } => {
            let sandbox = connected_sandbox(client, sandbox_id, generation).await?;
            let snapshot_id = ExecutionSnapshotId::new(snapshot_id)
                .map_err(|error| invalid(error.to_string()))?;
            let snapshot = sandbox.create_filesystem_snapshot(snapshot_id).await?;
            Ok(execution_snapshot_value(&snapshot))
        }
        BridgeRequest::FilesystemSnapshotSize { snapshot_id } => {
            let snapshot_id = ExecutionSnapshotId::new(snapshot_id)
                .map_err(|error| invalid(error.to_string()))?;
            let size_bytes = client.execution_snapshot_size(&snapshot_id).await?;
            Ok(json!({
                "snapshot_id": snapshot_id,
                "size_bytes": size_bytes,
            }))
        }
        BridgeRequest::FilesystemSnapshotDelete { snapshot_id } => {
            let snapshot_id = ExecutionSnapshotId::new(snapshot_id)
                .map_err(|error| invalid(error.to_string()))?;
            let deleted = client.delete_execution_snapshot(&snapshot_id).await?;
            Ok(json!({
                "snapshot_id": snapshot_id,
                "deleted": deleted,
            }))
        }
        BridgeRequest::CommandRun {
            sandbox_id,
            generation,
            argv,
            timeout_ms,
            env,
            cwd,
            user,
            stdin_base64,
        } => {
            let stdin = stdin_base64
                .map(|encoded| {
                    STANDARD
                        .decode(encoded)
                        .map_err(|error| invalid(format!("stdin_base64 is invalid: {error}")))
                })
                .transpose()?;
            let sandbox = bridge_sandbox(client, sandbox_id, generation, ExecutionState::Running)?;
            let output = sandbox
                .commands
                .run_with_options(
                    SandboxCommand::Argv(argv),
                    CommandRunOptions {
                        timeout: timeout_ms.map(Duration::from_millis),
                        envs: env,
                        cwd,
                        user,
                        stdin,
                    },
                )
                .await?;
            Ok(json!({
                "stdout_base64": STANDARD.encode(output.stdout_bytes),
                "stderr_base64": STANDARD.encode(output.stderr_bytes),
                "exit_code": output.exit_code,
                "truncated": output.truncated,
            }))
        }
        BridgeRequest::FileWrite {
            sandbox_id,
            generation,
            path,
            data_base64,
            user,
        } => {
            let data = STANDARD
                .decode(&data_base64)
                .map_err(|error| invalid(format!("data_base64 is invalid: {error}")))?;
            let sandbox = bridge_sandbox(client, sandbox_id, generation, ExecutionState::Running)?;
            let result = sandbox
                .files
                .write_with_options(&path, data, FilesystemOptions { user })
                .await?;
            Ok(json!({
                "path": result.path,
                "size": result.size,
            }))
        }
        BridgeRequest::FileRead {
            sandbox_id,
            generation,
            path,
            user,
        } => {
            let sandbox = bridge_sandbox(client, sandbox_id, generation, ExecutionState::Running)?;
            let data = sandbox
                .files
                .read_with_options(&path, FilesystemOptions { user })
                .await?;
            Ok(json!({
                "path": path,
                "data_base64": STANDARD.encode(&data),
                "size": data.len(),
            }))
        }
        BridgeRequest::FilesystemStat {
            sandbox_id,
            generation,
            path,
            user,
        } => {
            let sandbox = bridge_sandbox(client, sandbox_id, generation, ExecutionState::Running)?;
            let entry = sandbox
                .files
                .stat_with_options(path, FilesystemOptions { user })
                .await?;
            Ok(json!({ "entry": entry_value(&entry) }))
        }
        BridgeRequest::FilesystemList {
            sandbox_id,
            generation,
            path,
            depth,
            user,
        } => {
            let sandbox = bridge_sandbox(client, sandbox_id, generation, ExecutionState::Running)?;
            let entries = sandbox
                .files
                .list_with_options(path, depth, FilesystemOptions { user })
                .await?;
            Ok(json!({
                "entries": entries.iter().map(entry_value).collect::<Vec<_>>(),
            }))
        }
        BridgeRequest::FilesystemMakeDir {
            sandbox_id,
            generation,
            path,
            user,
        } => {
            let sandbox = bridge_sandbox(client, sandbox_id, generation, ExecutionState::Running)?;
            sandbox
                .files
                .make_dir_with_options(path, FilesystemOptions { user })
                .await?;
            Ok(json!({ "ok": true }))
        }
        BridgeRequest::FilesystemMove {
            sandbox_id,
            generation,
            path,
            destination,
            user,
        } => {
            let sandbox = bridge_sandbox(client, sandbox_id, generation, ExecutionState::Running)?;
            sandbox
                .files
                .move_path_with_options(path, destination, FilesystemOptions { user })
                .await?;
            Ok(json!({ "ok": true }))
        }
        BridgeRequest::FilesystemRemove {
            sandbox_id,
            generation,
            path,
            user,
        } => {
            let sandbox = bridge_sandbox(client, sandbox_id, generation, ExecutionState::Running)?;
            sandbox
                .files
                .remove_with_options(path, FilesystemOptions { user })
                .await?;
            Ok(json!({ "ok": true }))
        }
    }
}

fn bridge_sandbox(
    client: &A3sBoxClient,
    sandbox_id: String,
    generation: u64,
    state: ExecutionState,
) -> Result<Sandbox, BridgeFailure> {
    Ok(Sandbox::from_known_state(
        client.clone(),
        execution_id(sandbox_id)?,
        parse_generation(generation)?,
        state,
        ExecutionIsolation::Microvm,
    ))
}

async fn connected_sandbox(
    client: &A3sBoxClient,
    sandbox_id: String,
    generation: u64,
) -> Result<Sandbox, BridgeFailure> {
    let execution_id = execution_id(sandbox_id)?;
    let expected_generation = parse_generation(generation)?;
    let status = client.inspect_execution(&execution_id).await?;
    if status.generation != expected_generation {
        return Err(invalid(format!(
            "sandbox {} generation changed from {} to {}",
            execution_id,
            expected_generation.get(),
            status.generation.get()
        )));
    }
    Ok(Sandbox::from_known_state(
        client.clone(),
        execution_id,
        status.generation,
        status.state,
        status.plan.requested_isolation,
    ))
}

fn sandbox_info_value(sandbox: &Sandbox) -> Value {
    let info = sandbox.info();
    json!({
        "sandbox_id": info.sandbox_id,
        "generation": info.generation,
        "state": state_name(info.state),
    })
}

fn execution_snapshot_value(snapshot: &ExecutionSnapshot) -> Value {
    json!({
        "snapshot_id": snapshot.snapshot_id,
        "size_bytes": snapshot.size_bytes,
        "state": state_name(snapshot.state),
        "generation": snapshot.lease.generation.get(),
    })
}

fn entry_value(entry: &FilesystemEntry) -> Value {
    json!({
        "name": entry.name,
        "type": match entry.kind {
            FilesystemEntryKind::File => "file",
            FilesystemEntryKind::Directory => "directory",
            FilesystemEntryKind::Unspecified => "unspecified",
        },
        "path": entry.path,
        "size": entry.size,
        "mode": entry.mode,
        "permissions": entry.permissions,
        "owner": entry.owner,
        "group": entry.group,
        "modified_seconds": entry.modified_seconds,
        "modified_nanos": entry.modified_nanos,
        "symlink_target": entry.symlink_target,
    })
}

fn serialize_value(value: impl Serialize) -> Result<Value, BridgeFailure> {
    serde_json::to_value(value).map_err(|error| BridgeFailure {
        code: "runtime_error",
        message: format!("failed to encode SDK bridge result: {error}"),
    })
}

fn serialize_field(name: &str, value: impl Serialize) -> Result<Value, BridgeFailure> {
    let mut result = serde_json::Map::new();
    result.insert(name.to_string(), serialize_value(value)?);
    Ok(Value::Object(result))
}

fn state_name(state: ExecutionState) -> &'static str {
    match state {
        ExecutionState::Created => "created",
        ExecutionState::Creating => "creating",
        ExecutionState::Running => "running",
        ExecutionState::Paused => "paused",
        ExecutionState::Stopped => "stopped",
        ExecutionState::Failed => "failed",
    }
}

fn execution_id(value: String) -> Result<ExecutionId, BridgeFailure> {
    ExecutionId::new(value).map_err(|error| invalid(error.to_string()))
}

fn parse_generation(value: u64) -> Result<ExecutionGeneration, BridgeFailure> {
    ExecutionGeneration::new(value).map_err(|error| invalid(error.to_string()))
}

fn default_image() -> String {
    DEFAULT_SANDBOX_IMAGE.to_string()
}

fn default_network_subnet() -> String {
    "10.89.0.0/24".to_string()
}

const fn default_timeout_seconds() -> u64 {
    DEFAULT_SANDBOX_TIMEOUT_SECONDS
}

const fn default_depth() -> u32 {
    1
}

const fn default_true() -> bool {
    true
}

fn invalid(message: impl Into<String>) -> BridgeFailure {
    BridgeFailure {
        code: "invalid_request",
        message: message.into(),
    }
}

impl From<ClientError> for BridgeFailure {
    fn from(error: ClientError) -> Self {
        let code = match &error {
            ClientError::BoxNotFound(_) => "not_found",
            ClientError::AmbiguousBoxQuery { .. } | ClientError::Validation(_) => "invalid_request",
            ClientError::Execution(a3s_box_core::ExecutionManagerError::NotFound(_)) => "not_found",
            ClientError::Execution(a3s_box_core::ExecutionManagerError::InvalidRequest(_)) => {
                "invalid_request"
            }
            ClientError::Execution(a3s_box_core::ExecutionManagerError::Conflict { .. }) => {
                "conflict"
            }
            ClientError::Execution(a3s_box_core::ExecutionManagerError::Unavailable(_)) => {
                "unavailable"
            }
            ClientError::Guest(message) => {
                if message.to_ascii_lowercase().contains("not found") {
                    "not_found"
                } else {
                    "runtime_error"
                }
            }
            ClientError::State(_)
            | ClientError::Runtime(_)
            | ClientError::Execution(a3s_box_core::ExecutionManagerError::Internal(_)) => {
                "runtime_error"
            }
        };
        Self {
            code,
            message: error.to_string(),
        }
    }
}

#[cfg(test)]
#[path = "bridge/tests.rs"]
mod tests;
