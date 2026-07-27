//! Durable identity for one Sandbox runtime generation.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

pub(crate) const SANDBOX_RUNTIME_RECORD_SCHEMA: &str = "a3s.box.sandbox-runtime.v2";

/// Runtime-owned paths and process identities needed for detached recovery.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub(crate) struct SandboxRuntimeRecord {
    pub(crate) schema: String,
    pub(crate) container_id: String,
    pub(crate) runtime_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) runtime_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) agent_path: Option<PathBuf>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) agent_sha256: Option<String>,
    pub(crate) runtime_root: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) runtime_socket: Option<PathBuf>,
    pub(crate) bundle_dir: PathBuf,
    pub(crate) init_pid: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) owner_pid: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) owner_pid_start_time: Option<u64>,
    #[serde(default)]
    pub(crate) log_worker_pid: Option<u32>,
    #[serde(default)]
    pub(crate) log_worker_pid_start_time: Option<u64>,
}

impl SandboxRuntimeRecord {
    #[allow(clippy::too_many_arguments)]
    pub(crate) fn a3s_oci(
        container_id: String,
        runtime_path: PathBuf,
        runtime_sha256: String,
        agent_path: PathBuf,
        agent_sha256: String,
        runtime_root: PathBuf,
        runtime_socket: PathBuf,
        bundle_dir: PathBuf,
        init_pid: u32,
        generation: u64,
        owner_pid: u32,
        owner_pid_start_time: u64,
        log_worker_pid: u32,
        log_worker_pid_start_time: u64,
    ) -> Self {
        Self {
            schema: SANDBOX_RUNTIME_RECORD_SCHEMA.to_string(),
            container_id,
            runtime_path,
            runtime_sha256: Some(runtime_sha256),
            agent_path: Some(agent_path),
            agent_sha256: Some(agent_sha256),
            runtime_root,
            runtime_socket: Some(runtime_socket),
            bundle_dir,
            init_pid,
            generation: Some(generation),
            owner_pid: Some(owner_pid),
            owner_pid_start_time: Some(owner_pid_start_time),
            log_worker_pid: Some(log_worker_pid),
            log_worker_pid_start_time: Some(log_worker_pid_start_time),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn current_record_serializes_without_a_backend_selector() {
        let record = SandboxRuntimeRecord::a3s_oci(
            "box-id".to_string(),
            PathBuf::from("/runtime"),
            "a".repeat(64),
            PathBuf::from("/agent"),
            "b".repeat(64),
            PathBuf::from("/run/a3s-oci/box-id"),
            PathBuf::from("/run/a3s-oci/box-id/runtime.sock"),
            PathBuf::from("/boxes/box-id/sandbox/bundle"),
            10,
            11,
            12,
            13,
            14,
            15,
        );

        let value = serde_json::to_value(record).unwrap();

        assert_eq!(value["schema"], SANDBOX_RUNTIME_RECORD_SCHEMA);
        assert!(value.get("backend").is_none());
    }
}
