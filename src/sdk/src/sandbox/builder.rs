use std::path::PathBuf;

use a3s_box_core::{ExecutionIsolation, ExecutionSnapshotId, PortMapping};

use super::{Sandbox, SandboxCreateOptions, SandboxNetwork, TmpfsMount, VolumeMount};
use crate::{A3sBoxClient, Result};

/// Fluent builder for a local Sandbox.
#[derive(Debug, Clone)]
pub struct SandboxBuilder {
    client: A3sBoxClient,
    options: SandboxCreateOptions,
}

impl SandboxBuilder {
    pub(crate) fn new(client: A3sBoxClient, image: impl Into<String>) -> Self {
        Self {
            client,
            options: SandboxCreateOptions::new(image),
        }
    }

    pub const fn timeout_seconds(mut self, timeout_seconds: u64) -> Self {
        self.options.timeout_seconds = timeout_seconds;
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.options.envs.insert(key.into(), value.into());
        self
    }

    pub fn metadata(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.options.metadata.insert(key.into(), value.into());
        self
    }

    pub fn name(mut self, name: impl Into<String>) -> Self {
        self.options.name = Some(name.into());
        self
    }

    pub const fn cpus(mut self, cpus: u32) -> Self {
        self.options.cpus = Some(cpus);
        self
    }

    pub const fn memory_mb(mut self, memory_mb: u32) -> Self {
        self.options.memory_mb = Some(memory_mb);
        self
    }

    pub const fn isolation(mut self, isolation: ExecutionIsolation) -> Self {
        self.options.isolation = isolation;
        self
    }

    pub fn filesystem_snapshot(mut self, snapshot_id: ExecutionSnapshotId) -> Self {
        self.options.rootfs_snapshot_id = Some(snapshot_id);
        self
    }

    pub fn workspace(mut self, path: impl Into<PathBuf>) -> Self {
        self.options.workspace = Some(path.into());
        self
    }

    pub fn workdir(mut self, path: impl Into<String>) -> Self {
        self.options.workdir = Some(path.into());
        self
    }

    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.options.user = Some(user.into());
        self
    }

    pub fn hostname(mut self, hostname: impl Into<String>) -> Self {
        self.options.hostname = Some(hostname.into());
        self
    }

    pub fn mount(mut self, mount: VolumeMount) -> Self {
        self.options.mounts.push(mount);
        self
    }

    pub fn mount_bind(self, source: impl Into<PathBuf>, target: impl Into<String>) -> Self {
        self.mount(VolumeMount::bind(source, target))
    }

    pub fn mount_named(self, name: impl Into<String>, target: impl Into<String>) -> Self {
        self.mount(VolumeMount::named(name, target))
    }

    pub fn tmpfs(mut self, mount: TmpfsMount) -> Self {
        self.options.tmpfs.push(mount);
        self
    }

    pub fn network(mut self, network: SandboxNetwork) -> Self {
        self.options.network = network;
        self
    }

    pub fn publish_port(mut self, port: PortMapping) -> Self {
        self.options.ports.push(port);
        self
    }

    pub fn publish_tcp(mut self, host_port: u16, guest_port: u16) -> Self {
        self.options.ports.push(PortMapping {
            host_port,
            guest_port,
            protocol: a3s_box_core::PortProtocol::Tcp,
        });
        self
    }

    pub fn dns_server(mut self, server: impl Into<String>) -> Self {
        self.options.dns_servers.push(server.into());
        self
    }

    pub fn host_alias(mut self, host: impl Into<String>, ip: impl Into<String>) -> Self {
        self.options.host_aliases.insert(host.into(), ip.into());
        self
    }

    pub const fn read_only(mut self, read_only: bool) -> Self {
        self.options.read_only = read_only;
        self
    }

    pub const fn persistent(mut self, persistent: bool) -> Self {
        self.options.persistent = persistent;
        self
    }

    pub const fn auto_remove(mut self, auto_remove: bool) -> Self {
        self.options.auto_remove = auto_remove;
        self
    }

    pub async fn start(self) -> Result<Sandbox> {
        Sandbox::create_with_client(self.client, self.options).await
    }

    /// Return the typed request value without starting a Sandbox.
    pub fn options(&self) -> &SandboxCreateOptions {
        &self.options
    }
}

impl A3sBoxClient {
    /// Start a fluent builder for a local Sandbox.
    pub fn sandbox(&self, image: impl Into<String>) -> SandboxBuilder {
        SandboxBuilder::new(self.clone(), image)
    }
}
