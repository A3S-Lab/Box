use std::collections::BTreeMap;
use std::net::IpAddr;
use std::path::PathBuf;

use a3s_box_core::config::ResourceConfig;
use a3s_box_core::dns::parse_add_host_entries;
use a3s_box_core::network::NetworkMode;
use a3s_box_core::{
    parse_port_mapping, resolve_execution, BoxConfig, CreateExecutionRequest, ExecutionIsolation,
    ExecutionRecordPolicy, ExecutionSnapshotId, OperationId, PortMapping,
};

use crate::{A3sBoxClient, ClientError, Result};

/// Default OCI image used by all native local SDKs.
pub const DEFAULT_SANDBOX_IMAGE: &str = "alpine:3.20";

/// Default lifetime of a locally created Sandbox.
pub const DEFAULT_SANDBOX_TIMEOUT_SECONDS: u64 = 3_600;

const KEEPALIVE_COMMAND: &[&str] = &["/bin/sh", "-c", "while :; do sleep 3600; done"];

/// Source of one typed volume mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VolumeSource {
    /// A host path mounted directly into the box.
    Bind(PathBuf),
    /// An A3S-managed named volume resolved by the runtime client.
    Named(String),
}

/// A typed read-write or read-only mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct VolumeMount {
    pub source: VolumeSource,
    pub target: String,
    pub read_only: bool,
}

impl VolumeMount {
    pub fn bind(source: impl Into<PathBuf>, target: impl Into<String>) -> Self {
        Self {
            source: VolumeSource::Bind(source.into()),
            target: target.into(),
            read_only: false,
        }
    }

    pub fn named(name: impl Into<String>, target: impl Into<String>) -> Self {
        Self {
            source: VolumeSource::Named(name.into()),
            target: target.into(),
            read_only: false,
        }
    }

    pub const fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }
}

/// A typed in-guest tmpfs mount.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TmpfsMount {
    pub target: String,
    pub size_bytes: Option<u64>,
    pub read_only: bool,
}

impl TmpfsMount {
    pub fn new(target: impl Into<String>) -> Self {
        Self {
            target: target.into(),
            size_bytes: None,
            read_only: false,
        }
    }

    pub const fn size_bytes(mut self, size_bytes: u64) -> Self {
        self.size_bytes = Some(size_bytes);
        self
    }

    pub const fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    fn runtime_entry(&self) -> Result<String> {
        validate_guest_path("tmpfs target", &self.target)?;
        if self.size_bytes == Some(0) {
            return Err(ClientError::Validation(
                "tmpfs size must be greater than zero".to_string(),
            ));
        }
        let mut options = Vec::new();
        if let Some(size_bytes) = self.size_bytes {
            options.push(format!("size={size_bytes}"));
        }
        if self.read_only {
            options.push("ro".to_string());
        }
        if options.is_empty() {
            Ok(self.target.clone())
        } else {
            Ok(format!("{}:{}", self.target, options.join(",")))
        }
    }
}

/// Network selected for a Sandbox.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum SandboxNetwork {
    /// Transparent socket impersonation, the default MicroVM network.
    #[default]
    Tsi,
    /// Disable networking entirely.
    Disabled,
    /// Join an existing A3S-managed bridge network.
    Bridge { name: String },
}

impl SandboxNetwork {
    pub fn bridge(name: impl Into<String>) -> Self {
        Self::Bridge { name: name.into() }
    }

    fn runtime_mode(&self, client: &A3sBoxClient) -> Result<NetworkMode> {
        match self {
            Self::Tsi => Ok(NetworkMode::Tsi),
            Self::Disabled => Ok(NetworkMode::None),
            Self::Bridge { name } => {
                if name.trim().is_empty() {
                    return Err(ClientError::Validation(
                        "bridge network name cannot be empty".to_string(),
                    ));
                }
                if client.get_network(name)?.is_none() {
                    return Err(ClientError::Validation(format!(
                        "network '{name}' does not exist; create it before starting the sandbox"
                    )));
                }
                Ok(NetworkMode::Bridge {
                    network: name.clone(),
                })
            }
        }
    }
}

/// Options for [`super::Sandbox::create_with_options`].
///
/// MicroVM isolation is the default. Shared-kernel Sandbox isolation must be
/// selected explicitly with [`SandboxCreateOptions::isolation`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCreateOptions {
    pub image: String,
    pub timeout_seconds: u64,
    pub envs: BTreeMap<String, String>,
    pub metadata: BTreeMap<String, String>,
    pub name: Option<String>,
    pub cpus: Option<u32>,
    pub memory_mb: Option<u32>,
    pub isolation: ExecutionIsolation,
    pub rootfs_snapshot_id: Option<ExecutionSnapshotId>,
    pub workspace: Option<PathBuf>,
    pub workdir: Option<String>,
    pub user: Option<String>,
    pub hostname: Option<String>,
    pub mounts: Vec<VolumeMount>,
    pub tmpfs: Vec<TmpfsMount>,
    pub network: SandboxNetwork,
    pub ports: Vec<PortMapping>,
    pub dns_servers: Vec<String>,
    pub host_aliases: BTreeMap<String, String>,
    pub read_only: bool,
    pub persistent: bool,
    pub auto_remove: bool,
}

impl SandboxCreateOptions {
    pub fn new(image: impl Into<String>) -> Self {
        Self {
            image: image.into(),
            ..Self::default()
        }
    }

    pub const fn timeout_seconds(mut self, timeout_seconds: u64) -> Self {
        self.timeout_seconds = timeout_seconds;
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.envs.insert(key.into(), value.into());
        self
    }

    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.metadata.insert(key.into(), value.into());
        self
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.name = Some(name.into());
        self
    }

    pub const fn cpus(mut self, cpus: u32) -> Self {
        self.cpus = Some(cpus);
        self
    }

    pub const fn memory_mb(mut self, memory_mb: u32) -> Self {
        self.memory_mb = Some(memory_mb);
        self
    }

    pub const fn isolation(mut self, isolation: ExecutionIsolation) -> Self {
        self.isolation = isolation;
        self
    }

    /// Start from a runtime-managed immutable filesystem snapshot.
    ///
    /// Snapshot identifiers are typed and validated by the runtime; callers
    /// cannot provide an arbitrary host path.
    pub fn filesystem_snapshot(mut self, snapshot_id: ExecutionSnapshotId) -> Self {
        self.rootfs_snapshot_id = Some(snapshot_id);
        self
    }

    pub fn workspace(mut self, path: impl Into<PathBuf>) -> Self {
        self.workspace = Some(path.into());
        self
    }

    pub fn workdir(mut self, path: impl Into<String>) -> Self {
        self.workdir = Some(path.into());
        self
    }

    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    pub fn hostname(mut self, hostname: impl Into<String>) -> Self {
        self.hostname = Some(hostname.into());
        self
    }

    pub fn mount(mut self, mount: VolumeMount) -> Self {
        self.mounts.push(mount);
        self
    }

    pub fn tmpfs(mut self, mount: TmpfsMount) -> Self {
        self.tmpfs.push(mount);
        self
    }

    pub fn network(mut self, network: SandboxNetwork) -> Self {
        self.network = network;
        self
    }

    pub fn publish_port(mut self, port: PortMapping) -> Self {
        self.ports.push(port);
        self
    }

    pub fn dns_server(mut self, server: impl Into<String>) -> Self {
        self.dns_servers.push(server.into());
        self
    }

    pub fn host_alias(mut self, host: impl Into<String>, ip: impl Into<String>) -> Self {
        self.host_aliases.insert(host.into(), ip.into());
        self
    }

    pub const fn read_only(mut self, read_only: bool) -> Self {
        self.read_only = read_only;
        self
    }

    pub const fn persistent(mut self, persistent: bool) -> Self {
        self.persistent = persistent;
        self
    }

    pub const fn auto_remove(mut self, auto_remove: bool) -> Self {
        self.auto_remove = auto_remove;
        self
    }

    pub(crate) fn into_runtime_request(
        self,
        client: &A3sBoxClient,
    ) -> Result<(CreateExecutionRequest, OperationId)> {
        self.validate()?;
        let network = self.network.runtime_mode(client)?;
        let (volumes, volume_names) = resolve_mounts(client, self.mounts)?;
        let port_map = self
            .ports
            .into_iter()
            .map(|port| {
                let entry = port.runtime_entry();
                parse_port_mapping(&entry)
                    .map(|mapping| mapping.runtime_entry())
                    .map_err(ClientError::Validation)
            })
            .collect::<Result<Vec<_>>>()?;
        let tmpfs = self
            .tmpfs
            .iter()
            .map(TmpfsMount::runtime_entry)
            .collect::<Result<Vec<_>>>()?;
        let add_hosts = self
            .host_aliases
            .into_iter()
            .map(|(host, ip)| format!("{host}:{ip}"))
            .collect::<Vec<_>>();
        parse_add_host_entries(&add_hosts).map_err(ClientError::Validation)?;

        let identity = uuid::Uuid::new_v4();
        let mut resources = ResourceConfig {
            timeout: self.timeout_seconds,
            ..ResourceConfig::default()
        };
        if let Some(cpus) = self.cpus {
            resources.vcpus = cpus;
        }
        if let Some(memory_mb) = self.memory_mb {
            resources.memory_mb = memory_mb;
        }

        let config = BoxConfig {
            isolation: self.isolation,
            image: self.image,
            workspace: self.workspace.unwrap_or_default(),
            resources,
            cmd: KEEPALIVE_COMMAND
                .iter()
                .map(|part| (*part).to_string())
                .collect(),
            user: self.user,
            workdir: self.workdir,
            hostname: self.hostname,
            volumes,
            extra_env: self.envs.into_iter().collect(),
            port_map,
            dns: self.dns_servers,
            add_hosts,
            network,
            tmpfs,
            read_only: self.read_only,
            persistent: self.persistent,
            ..BoxConfig::default()
        };
        resolve_execution(&config).map_err(ClientError::Runtime)?;

        let operation = OperationId::new(format!("sdk-create-{identity}"))
            .map_err(|error| ClientError::Validation(error.to_string()))?;
        Ok((
            CreateExecutionRequest {
                external_sandbox_id: format!("local-{identity}"),
                config,
                labels: self.metadata,
                policy: ExecutionRecordPolicy {
                    name: self.name,
                    auto_remove: self.auto_remove,
                    volume_names,
                    ..ExecutionRecordPolicy::default()
                },
                rootfs_snapshot_id: self.rootfs_snapshot_id,
            },
            operation,
        ))
    }

    fn validate(&self) -> Result<()> {
        if self.image.trim().is_empty() {
            return Err(ClientError::Validation(
                "sandbox image cannot be empty".to_string(),
            ));
        }
        if self.timeout_seconds == 0 {
            return Err(ClientError::Validation(
                "sandbox timeout must be greater than zero".to_string(),
            ));
        }
        if self.cpus == Some(0) {
            return Err(ClientError::Validation(
                "sandbox CPUs must be greater than zero".to_string(),
            ));
        }
        if self.memory_mb == Some(0) {
            return Err(ClientError::Validation(
                "sandbox memory must be greater than zero".to_string(),
            ));
        }
        if let Some(workdir) = &self.workdir {
            validate_guest_path("working directory", workdir)?;
        }
        for server in &self.dns_servers {
            server.parse::<IpAddr>().map_err(|_| {
                ClientError::Validation(format!("invalid DNS server address '{server}'"))
            })?;
        }
        Ok(())
    }
}

impl Default for SandboxCreateOptions {
    fn default() -> Self {
        Self {
            image: DEFAULT_SANDBOX_IMAGE.to_string(),
            timeout_seconds: DEFAULT_SANDBOX_TIMEOUT_SECONDS,
            envs: BTreeMap::new(),
            metadata: BTreeMap::new(),
            name: None,
            cpus: None,
            memory_mb: None,
            isolation: ExecutionIsolation::Microvm,
            rootfs_snapshot_id: None,
            workspace: None,
            workdir: None,
            user: None,
            hostname: None,
            mounts: Vec::new(),
            tmpfs: Vec::new(),
            network: SandboxNetwork::default(),
            ports: Vec::new(),
            dns_servers: Vec::new(),
            host_aliases: BTreeMap::new(),
            read_only: false,
            persistent: false,
            auto_remove: true,
        }
    }
}

fn resolve_mounts(
    client: &A3sBoxClient,
    mounts: Vec<VolumeMount>,
) -> Result<(Vec<String>, Vec<String>)> {
    let mut runtime_mounts = Vec::with_capacity(mounts.len());
    let mut volume_names = Vec::new();
    for mount in mounts {
        validate_guest_path("volume target", &mount.target)?;
        let source = match mount.source {
            VolumeSource::Bind(path) => {
                if path.as_os_str().is_empty() {
                    return Err(ClientError::Validation(
                        "bind mount source cannot be empty".to_string(),
                    ));
                }
                path.to_string_lossy().into_owned()
            }
            VolumeSource::Named(name) => {
                let volume = client.get_volume(&name)?.ok_or_else(|| {
                    ClientError::Validation(format!(
                        "volume '{name}' does not exist; create it before starting the sandbox"
                    ))
                })?;
                if volume.mount_point.is_empty() {
                    return Err(ClientError::Validation(format!(
                        "volume '{name}' has no runtime mount point"
                    )));
                }
                if !volume_names.contains(&name) {
                    volume_names.push(name);
                }
                volume.mount_point
            }
        };
        let mode = if mount.read_only { ":ro" } else { ":rw" };
        runtime_mounts.push(format!("{source}:{}{mode}", mount.target));
    }
    Ok((runtime_mounts, volume_names))
}

fn validate_guest_path(label: &str, path: &str) -> Result<()> {
    if !path.starts_with('/') || path.contains('\0') {
        return Err(ClientError::Validation(format!(
            "{label} must be an absolute guest path without NUL bytes"
        )));
    }
    Ok(())
}
