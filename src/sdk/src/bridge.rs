//! Versioned JSON bridge used by the pure Python and TypeScript SDKs.
//!
//! The bridge is a machine-only boundary. Language clients send one request to
//! `a3s-box sdk-bridge` on stdin and receive one response on stdout. It calls
//! the direct Rust SDK and never parses human-facing CLI output.

use std::collections::BTreeMap;

use a3s_box_core::config::ResourceConfig;
use a3s_box_core::{
    BoxConfig, CreateExecutionRequest, ExecRequest, ExecutionGeneration, ExecutionId,
    ExecutionIsolation, ExecutionRecordPolicy, ExecutionState, FileOp, FileRequest,
    FilesystemEntry, FilesystemEntryKind, FilesystemOp, FilesystemRequest, OperationId,
};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{A3sBoxClient, ClientError};

pub const BRIDGE_PROTOCOL_VERSION: u8 = 1;
const DEFAULT_IMAGE: &str = "alpine:3.20";
const DEFAULT_TIMEOUT_SECONDS: u64 = 3600;
const KEEPALIVE_COMMAND: &[&str] = &["/bin/sh", "-c", "while :; do sleep 3600; done"];

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum BridgeRequest {
    SandboxCreate {
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
    },
    SandboxInspect {
        sandbox_id: String,
    },
    SandboxKill {
        sandbox_id: String,
        generation: u64,
    },
    SandboxPause {
        sandbox_id: String,
        generation: u64,
        #[serde(default = "default_true")]
        keep_memory: bool,
    },
    SandboxResume {
        sandbox_id: String,
        generation: u64,
    },
    CommandRun {
        sandbox_id: String,
        generation: u64,
        argv: Vec<String>,
        #[serde(default)]
        timeout_ms: Option<u64>,
        #[serde(default)]
        env: BTreeMap<String, String>,
        #[serde(default)]
        cwd: Option<String>,
        #[serde(default)]
        user: Option<String>,
        #[serde(default)]
        stdin_base64: Option<String>,
    },
    FileWrite {
        sandbox_id: String,
        generation: u64,
        path: String,
        data_base64: String,
        #[serde(default)]
        user: Option<String>,
    },
    FileRead {
        sandbox_id: String,
        generation: u64,
        path: String,
        #[serde(default)]
        user: Option<String>,
    },
    FilesystemStat {
        sandbox_id: String,
        generation: u64,
        path: String,
        #[serde(default)]
        user: Option<String>,
    },
    FilesystemList {
        sandbox_id: String,
        generation: u64,
        path: String,
        #[serde(default = "default_depth")]
        depth: u32,
        #[serde(default)]
        user: Option<String>,
    },
    FilesystemMakeDir {
        sandbox_id: String,
        generation: u64,
        path: String,
        #[serde(default)]
        user: Option<String>,
    },
    FilesystemMove {
        sandbox_id: String,
        generation: u64,
        path: String,
        destination: String,
        #[serde(default)]
        user: Option<String>,
    },
    FilesystemRemove {
        sandbox_id: String,
        generation: u64,
        path: String,
        #[serde(default)]
        user: Option<String>,
    },
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
        BridgeRequest::SandboxCreate {
            image,
            timeout_seconds,
            env,
            labels,
            name,
            cpus,
            memory_mb,
            isolation,
        } => {
            let (request, operation) = create_request(
                image,
                timeout_seconds,
                env,
                labels,
                name,
                cpus,
                memory_mb,
                isolation,
            )?;
            let lease = client.run_box(request, &operation).await?;
            Ok(sandbox_value(
                &lease.execution_id,
                lease.generation,
                ExecutionState::Running,
            ))
        }
        BridgeRequest::SandboxInspect { sandbox_id } => {
            let execution_id = execution_id(sandbox_id)?;
            let status = client.inspect_execution(&execution_id).await?;
            Ok(sandbox_value(
                &status.execution_id,
                status.generation,
                status.state,
            ))
        }
        BridgeRequest::SandboxKill {
            sandbox_id,
            generation,
        } => {
            let execution_id = execution_id(sandbox_id)?;
            let generation = parse_generation(generation)?;
            client.kill_execution(&execution_id, generation).await?;
            client.remove_execution(&execution_id, generation).await?;
            Ok(sandbox_value(
                &execution_id,
                generation,
                ExecutionState::Stopped,
            ))
        }
        BridgeRequest::SandboxPause {
            sandbox_id,
            generation,
            keep_memory,
        } => {
            let execution_id = execution_id(sandbox_id)?;
            let lease = client
                .pause_execution(&execution_id, parse_generation(generation)?, keep_memory)
                .await?;
            Ok(sandbox_value(
                &lease.execution_id,
                lease.generation,
                ExecutionState::Paused,
            ))
        }
        BridgeRequest::SandboxResume {
            sandbox_id,
            generation,
        } => {
            let execution_id = execution_id(sandbox_id)?;
            let lease = client
                .resume_execution(&execution_id, parse_generation(generation)?)
                .await?;
            Ok(sandbox_value(
                &lease.execution_id,
                lease.generation,
                ExecutionState::Running,
            ))
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
            if argv.is_empty() {
                return Err(invalid("command argv cannot be empty"));
            }
            let stdin = stdin_base64
                .map(|encoded| {
                    STANDARD
                        .decode(encoded)
                        .map_err(|error| invalid(format!("stdin_base64 is invalid: {error}")))
                })
                .transpose()?;
            let request = ExecRequest {
                request_id: Some(format!("sdk-command-{}", uuid::Uuid::new_v4())),
                cmd: argv,
                timeout_ns: timeout_ms.unwrap_or_default().saturating_mul(1_000_000),
                env: env
                    .into_iter()
                    .map(|(key, value)| format!("{key}={value}"))
                    .collect(),
                working_dir: cwd,
                rootfs: None,
                stdin,
                stdin_streaming: false,
                user,
                streaming: false,
            };
            #[cfg(unix)]
            {
                let output = client
                    .execute_execution(
                        &execution_id(sandbox_id)?,
                        parse_generation(generation)?,
                        request,
                    )
                    .await?;
                Ok(json!({
                    "stdout_base64": STANDARD.encode(output.stdout),
                    "stderr_base64": STANDARD.encode(output.stderr),
                    "exit_code": output.exit_code,
                    "truncated": output.truncated,
                }))
            }
            #[cfg(not(unix))]
            {
                let _ = (client, sandbox_id, generation, request);
                Err(unavailable(
                    "local command sessions are not available on this host",
                ))
            }
        }
        BridgeRequest::FileWrite {
            sandbox_id,
            generation,
            path,
            data_base64,
            user,
        } => {
            STANDARD
                .decode(&data_base64)
                .map_err(|error| invalid(format!("data_base64 is invalid: {error}")))?;
            transfer_file(
                client,
                sandbox_id,
                generation,
                path,
                FileOp::Upload,
                Some(data_base64),
                user,
            )
            .await
        }
        BridgeRequest::FileRead {
            sandbox_id,
            generation,
            path,
            user,
        } => {
            transfer_file(
                client,
                sandbox_id,
                generation,
                path,
                FileOp::Download,
                None,
                user,
            )
            .await
        }
        BridgeRequest::FilesystemStat {
            sandbox_id,
            generation,
            path,
            user,
        } => {
            filesystem(
                client,
                sandbox_id,
                generation,
                FilesystemRequest {
                    op: FilesystemOp::Stat,
                    path,
                    destination: None,
                    depth: 0,
                    user,
                },
            )
            .await
        }
        BridgeRequest::FilesystemList {
            sandbox_id,
            generation,
            path,
            depth,
            user,
        } => {
            filesystem(
                client,
                sandbox_id,
                generation,
                FilesystemRequest {
                    op: FilesystemOp::ListDir,
                    path,
                    destination: None,
                    depth,
                    user,
                },
            )
            .await
        }
        BridgeRequest::FilesystemMakeDir {
            sandbox_id,
            generation,
            path,
            user,
        } => {
            filesystem(
                client,
                sandbox_id,
                generation,
                FilesystemRequest {
                    op: FilesystemOp::MakeDir,
                    path,
                    destination: None,
                    depth: 0,
                    user,
                },
            )
            .await
        }
        BridgeRequest::FilesystemMove {
            sandbox_id,
            generation,
            path,
            destination,
            user,
        } => {
            filesystem(
                client,
                sandbox_id,
                generation,
                FilesystemRequest {
                    op: FilesystemOp::Move,
                    path,
                    destination: Some(destination),
                    depth: 0,
                    user,
                },
            )
            .await
        }
        BridgeRequest::FilesystemRemove {
            sandbox_id,
            generation,
            path,
            user,
        } => {
            filesystem(
                client,
                sandbox_id,
                generation,
                FilesystemRequest {
                    op: FilesystemOp::Remove,
                    path,
                    destination: None,
                    depth: 0,
                    user,
                },
            )
            .await
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn create_request(
    image: String,
    timeout_seconds: u64,
    env: BTreeMap<String, String>,
    labels: BTreeMap<String, String>,
    name: Option<String>,
    cpus: Option<u32>,
    memory_mb: Option<u32>,
    isolation: ExecutionIsolation,
) -> Result<(CreateExecutionRequest, OperationId), BridgeFailure> {
    if image.trim().is_empty() {
        return Err(invalid("image cannot be empty"));
    }
    if timeout_seconds == 0 {
        return Err(invalid("timeout_seconds must be greater than zero"));
    }
    if cpus == Some(0) {
        return Err(invalid("cpus must be greater than zero"));
    }
    if memory_mb == Some(0) {
        return Err(invalid("memory_mb must be greater than zero"));
    }

    let identity = uuid::Uuid::new_v4();
    let mut resources = ResourceConfig {
        timeout: timeout_seconds,
        ..ResourceConfig::default()
    };
    if let Some(cpus) = cpus {
        resources.vcpus = cpus;
    }
    if let Some(memory_mb) = memory_mb {
        resources.memory_mb = memory_mb;
    }
    let config = BoxConfig {
        isolation,
        image,
        resources,
        cmd: KEEPALIVE_COMMAND
            .iter()
            .map(|part| (*part).to_string())
            .collect(),
        extra_env: env.into_iter().collect(),
        ..BoxConfig::default()
    };
    let operation = OperationId::new(format!("sdk-create-{identity}"))
        .map_err(|error| invalid(error.to_string()))?;
    Ok((
        CreateExecutionRequest {
            external_sandbox_id: format!("local-{identity}"),
            config,
            labels,
            policy: ExecutionRecordPolicy {
                name,
                auto_remove: true,
                ..ExecutionRecordPolicy::default()
            },
            rootfs_snapshot_id: None,
        },
        operation,
    ))
}

async fn transfer_file(
    client: &A3sBoxClient,
    sandbox_id: String,
    raw_generation: u64,
    path: String,
    op: FileOp,
    data: Option<String>,
    user: Option<String>,
) -> Result<Value, BridgeFailure> {
    #[cfg(unix)]
    {
        let response = client
            .transfer_execution_file(
                &execution_id(sandbox_id)?,
                parse_generation(raw_generation)?,
                FileRequest {
                    op,
                    guest_path: path.clone(),
                    data,
                    user,
                },
            )
            .await?;
        if !response.success {
            return Err(guest_failure(
                response
                    .error
                    .unwrap_or_else(|| "file operation failed".to_string()),
            ));
        }
        match op {
            FileOp::Upload => Ok(json!({
                "path": path,
                "size": response.size,
            })),
            FileOp::Download => Ok(json!({
                "path": path,
                "data_base64": response.data.unwrap_or_default(),
                "size": response.size,
            })),
        }
    }
    #[cfg(not(unix))]
    {
        let _ = (client, sandbox_id, raw_generation, path, op, data, user);
        Err(unavailable(
            "local file sessions are not available on this host",
        ))
    }
}

async fn filesystem(
    client: &A3sBoxClient,
    sandbox_id: String,
    raw_generation: u64,
    request: FilesystemRequest,
) -> Result<Value, BridgeFailure> {
    #[cfg(unix)]
    {
        let response = client
            .filesystem_execution(
                &execution_id(sandbox_id)?,
                parse_generation(raw_generation)?,
                request,
            )
            .await?;
        if !response.success {
            return Err(guest_failure(
                response
                    .error
                    .unwrap_or_else(|| "filesystem operation failed".to_string()),
            ));
        }
        Ok(json!({
            "entry": response.entry.as_ref().map(entry_value),
            "entries": response.entries.iter().map(entry_value).collect::<Vec<_>>(),
        }))
    }
    #[cfg(not(unix))]
    {
        let _ = (client, sandbox_id, raw_generation, request);
        Err(unavailable(
            "local filesystem sessions are not available on this host",
        ))
    }
}

fn sandbox_value(
    execution_id: &ExecutionId,
    generation: ExecutionGeneration,
    state: ExecutionState,
) -> Value {
    json!({
        "sandbox_id": execution_id.as_str(),
        "generation": generation.get(),
        "state": state_name(state),
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
    DEFAULT_IMAGE.to_string()
}

const fn default_timeout_seconds() -> u64 {
    DEFAULT_TIMEOUT_SECONDS
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

#[cfg(not(unix))]
fn unavailable(message: impl Into<String>) -> BridgeFailure {
    BridgeFailure {
        code: "unavailable",
        message: message.into(),
    }
}

fn guest_failure(message: String) -> BridgeFailure {
    let code = if message.to_ascii_lowercase().contains("not found") {
        "not_found"
    } else {
        "runtime_error"
    };
    BridgeFailure { code, message }
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
mod tests {
    use super::*;

    #[test]
    fn create_request_defaults_to_a_local_microvm() {
        let request: BridgeRequest =
            serde_json::from_str(r#"{"operation":"sandbox_create"}"#).unwrap();
        let BridgeRequest::SandboxCreate {
            image,
            timeout_seconds,
            isolation,
            ..
        } = request
        else {
            panic!("expected create request");
        };
        assert_eq!(image, DEFAULT_IMAGE);
        assert_eq!(timeout_seconds, DEFAULT_TIMEOUT_SECONDS);
        assert_eq!(isolation, ExecutionIsolation::Microvm);
    }

    #[test]
    fn create_request_maps_language_options_to_the_runtime_facade() {
        let (request, _) = create_request(
            "python:3.12-alpine".to_string(),
            120,
            BTreeMap::from([("MODE".to_string(), "test".to_string())]),
            BTreeMap::from([("suite".to_string(), "sdk".to_string())]),
            Some("local-sdk".to_string()),
            Some(4),
            Some(2048),
            ExecutionIsolation::Sandbox,
        )
        .unwrap();

        assert_eq!(request.config.image, "python:3.12-alpine");
        assert_eq!(request.config.resources.timeout, 120);
        assert_eq!(request.config.resources.vcpus, 4);
        assert_eq!(request.config.resources.memory_mb, 2048);
        assert_eq!(request.config.isolation, ExecutionIsolation::Sandbox);
        assert_eq!(
            request.config.cmd,
            ["/bin/sh", "-c", "while :; do sleep 3600; done"]
        );
        assert_eq!(request.policy.name.as_deref(), Some("local-sdk"));
        assert!(request.policy.auto_remove);
        assert_eq!(request.labels.get("suite").map(String::as_str), Some("sdk"));
    }

    #[tokio::test]
    async fn malformed_json_returns_a_versioned_error_envelope() {
        let response = dispatch_json("{").await;
        assert_eq!(response.protocol_version, BRIDGE_PROTOCOL_VERSION);
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "invalid_request");
    }

    #[test]
    fn zero_generation_is_rejected_before_runtime_access() {
        let error = parse_generation(0).unwrap_err();
        assert_eq!(error.code, "invalid_request");
    }
}
