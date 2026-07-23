//! Versioned JSON bridge used by the pure Python and TypeScript SDKs.
//!
//! The bridge is a machine-only boundary. Language clients send one request to
//! `a3s-box sdk-bridge` on stdin and receive one response on stdout. It calls
//! the direct Rust SDK and never parses human-facing CLI output.

use std::collections::BTreeMap;
use std::time::Duration;

use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionIsolation, ExecutionState, FilesystemEntry,
    FilesystemEntryKind,
};
use base64::engine::general_purpose::STANDARD;
use base64::Engine;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};

use crate::{
    A3sBoxClient, ClientError, CommandRunOptions, FilesystemOptions, Sandbox, SandboxCommand,
    SandboxCreateOptions, DEFAULT_SANDBOX_IMAGE, DEFAULT_SANDBOX_TIMEOUT_SECONDS,
};

pub const BRIDGE_PROTOCOL_VERSION: u8 = 1;

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

fn sandbox_info_value(sandbox: &Sandbox) -> Value {
    let info = sandbox.info();
    json!({
        "sandbox_id": info.sandbox_id,
        "generation": info.generation,
        "state": state_name(info.state),
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
    DEFAULT_SANDBOX_IMAGE.to_string()
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
        assert_eq!(image, DEFAULT_SANDBOX_IMAGE);
        assert_eq!(timeout_seconds, DEFAULT_SANDBOX_TIMEOUT_SECONDS);
        assert_eq!(isolation, ExecutionIsolation::Microvm);
    }

    #[test]
    fn create_request_maps_language_options_to_the_runtime_facade() {
        let (request, _) = SandboxCreateOptions {
            image: "python:3.12-alpine".to_string(),
            timeout_seconds: 120,
            envs: BTreeMap::from([("MODE".to_string(), "test".to_string())]),
            metadata: BTreeMap::from([("suite".to_string(), "sdk".to_string())]),
            name: Some("local-sdk".to_string()),
            cpus: Some(4),
            memory_mb: Some(2048),
            isolation: ExecutionIsolation::Sandbox,
        }
        .into_runtime_request()
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
