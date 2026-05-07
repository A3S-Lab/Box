//! Docker API data models.

use serde::{Deserialize, Serialize};

/// Container creation request.
#[derive(Debug, Deserialize)]
pub struct ContainerCreateRequest {
    #[serde(rename = "Image")]
    pub image: String,

    #[serde(rename = "Cmd")]
    pub cmd: Option<Vec<String>>,

    #[serde(rename = "Entrypoint")]
    pub entrypoint: Option<Vec<String>>,

    #[serde(rename = "Env")]
    pub env: Option<Vec<String>>,

    #[serde(rename = "WorkingDir")]
    pub working_dir: Option<String>,

    #[serde(rename = "User")]
    pub user: Option<String>,

    #[serde(rename = "Hostname")]
    pub hostname: Option<String>,

    #[serde(rename = "Labels")]
    pub labels: Option<std::collections::HashMap<String, String>>,

    #[serde(rename = "HostConfig")]
    pub host_config: Option<HostConfig>,
}

/// Host configuration for container.
#[derive(Debug, Deserialize)]
pub struct HostConfig {
    #[serde(rename = "Binds")]
    pub binds: Option<Vec<String>>,

    #[serde(rename = "NetworkMode")]
    pub network_mode: Option<String>,

    #[serde(rename = "PortBindings")]
    pub port_bindings: Option<std::collections::HashMap<String, Vec<PortBinding>>>,

    #[serde(rename = "RestartPolicy")]
    pub restart_policy: Option<RestartPolicy>,

    #[serde(rename = "AutoRemove")]
    pub auto_remove: Option<bool>,

    #[serde(rename = "Memory")]
    pub memory: Option<i64>,

    #[serde(rename = "CpuShares")]
    pub cpu_shares: Option<i64>,

    #[serde(rename = "Privileged")]
    pub privileged: Option<bool>,

    #[serde(rename = "ReadonlyRootfs")]
    pub readonly_rootfs: Option<bool>,
}

/// Port binding configuration.
#[derive(Debug, Deserialize)]
pub struct PortBinding {
    #[serde(rename = "HostIp")]
    pub host_ip: Option<String>,

    #[serde(rename = "HostPort")]
    pub host_port: String,
}

/// Restart policy configuration.
#[derive(Debug, Deserialize)]
pub struct RestartPolicy {
    #[serde(rename = "Name")]
    pub name: String,

    #[serde(rename = "MaximumRetryCount")]
    pub maximum_retry_count: Option<u32>,
}

/// Container creation response.
#[derive(Debug, Serialize)]
pub struct ContainerCreateResponse {
    #[serde(rename = "Id")]
    pub id: String,

    #[serde(rename = "Warnings")]
    pub warnings: Option<Vec<String>>,
}
