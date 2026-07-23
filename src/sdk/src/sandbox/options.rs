use std::collections::BTreeMap;

use a3s_box_core::config::ResourceConfig;
use a3s_box_core::{
    resolve_execution, BoxConfig, CreateExecutionRequest, ExecutionIsolation,
    ExecutionRecordPolicy, OperationId,
};

use crate::{ClientError, Result};

/// Default OCI image used by all native local SDKs.
pub const DEFAULT_SANDBOX_IMAGE: &str = "alpine:3.20";

/// Default lifetime of a locally created Sandbox.
pub const DEFAULT_SANDBOX_TIMEOUT_SECONDS: u64 = 3_600;

const KEEPALIVE_COMMAND: &[&str] = &["/bin/sh", "-c", "while :; do sleep 3600; done"];

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

    pub(crate) fn into_runtime_request(self) -> Result<(CreateExecutionRequest, OperationId)> {
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
            resources,
            cmd: KEEPALIVE_COMMAND
                .iter()
                .map(|part| (*part).to_string())
                .collect(),
            extra_env: self.envs.into_iter().collect(),
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
                    auto_remove: true,
                    ..ExecutionRecordPolicy::default()
                },
                rootfs_snapshot_id: None,
            },
            operation,
        ))
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
        }
    }
}
