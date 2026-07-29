//! A3S Runtime provider adapter for Box isolation backends.

mod exec;
mod lifecycle;
mod logs;
mod mapping;
mod metadata;

use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use a3s_runtime::contract::{
    IsolationLevel, MountKind, NetworkMode, ResourceControl, RuntimeActionRequest,
    RuntimeCapabilities, RuntimeExecRequest, RuntimeExecResult, RuntimeFeature, RuntimeInspection,
    RuntimeLogChunk, RuntimeLogQuery, RuntimeObservation, RuntimeRemoval, RuntimeUnitClass,
    RuntimeUnitSpec,
};
use a3s_runtime::{ProviderId, RuntimeDriver, RuntimeError, RuntimeResult, RuntimeUnitRecord};
use async_trait::async_trait;
use tokio::sync::OnceCell;

use crate::{ExecutionIsolation, LocalExecutionManager};

pub(super) const OCI_IMAGE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub(super) const OCI_IMAGE_INDEX: &str = "application/vnd.oci.image.index.v1+json";

/// Host paths and bounds for one Box Runtime provider instance.
#[derive(Debug, Clone)]
pub struct BoxRuntimeDriverConfig {
    /// Private A3S Box state root. Runtime records share its canonical
    /// `boxes.json` store with CLI-created records but never adopt them.
    pub home_dir: PathBuf,
    /// Independent bound for one provider control-plane operation.
    pub control_timeout: Duration,
    /// Poll cadence while waiting for a finite Task to reach a terminal state.
    pub task_poll_interval: Duration,
}

impl Default for BoxRuntimeDriverConfig {
    fn default() -> Self {
        Self {
            home_dir: a3s_box_core::dirs_home(),
            control_timeout: Duration::from_secs(60),
            task_poll_interval: Duration::from_millis(50),
        }
    }
}

/// Concrete A3S Runtime driver backed by a configured Box isolation backend.
pub struct BoxRuntimeDriver {
    provider_id: ProviderId,
    pub(super) config: BoxRuntimeDriverConfig,
    pub(super) manager: LocalExecutionManager,
    execution_isolation: ExecutionIsolation,
    provider_build: OnceCell<String>,
}

impl BoxRuntimeDriver {
    /// Create a Runtime driver backed by Box MicroVM isolation.
    ///
    /// Shared-kernel execution is never selected as an automatic fallback.
    pub fn new(config: BoxRuntimeDriverConfig) -> RuntimeResult<Self> {
        Self::new_with_isolation(config, ExecutionIsolation::Microvm)
    }

    /// Create a Runtime driver with an explicit concrete Box isolation
    /// backend for Runtime's provider-neutral `IsolationLevel::Sandbox`.
    pub fn new_with_isolation(
        config: BoxRuntimeDriverConfig,
        execution_isolation: ExecutionIsolation,
    ) -> RuntimeResult<Self> {
        validate_config(&config)?;
        let manager = LocalExecutionManager::with_vm_backend(
            config.home_dir.join("boxes.json"),
            &config.home_dir,
        );
        Self::with_manager(config, manager, execution_isolation)
    }

    fn with_manager(
        config: BoxRuntimeDriverConfig,
        manager: LocalExecutionManager,
        execution_isolation: ExecutionIsolation,
    ) -> RuntimeResult<Self> {
        validate_config(&config)?;
        Ok(Self {
            provider_id: ProviderId::parse("a3s-box")?,
            config,
            manager,
            execution_isolation,
            provider_build: OnceCell::new(),
        })
    }

    /// Concrete Box isolation backend selected for this provider instance.
    pub const fn execution_isolation(&self) -> ExecutionIsolation {
        self.execution_isolation
    }

    pub(super) async fn provider_build(&self) -> RuntimeResult<String> {
        self.provider_build
            .get_or_try_init(|| async {
                let execution_isolation = self.execution_isolation;
                let provider_build = tokio::time::timeout(
                    self.config.control_timeout,
                    tokio::task::spawn_blocking(move || probe_provider_build(execution_isolation)),
                )
                .await
                .map_err(|_| {
                    RuntimeError::ProviderUnavailable(
                        "Box provider capability probe exceeded the control timeout".into(),
                    )
                })?
                .map_err(|error| {
                    RuntimeError::ProviderUnavailable(format!(
                        "Box provider capability probe failed: {error}"
                    ))
                })?
                .map_err(RuntimeError::ProviderUnavailable)?;
                Ok::<String, RuntimeError>(provider_build)
            })
            .await
            .cloned()
    }

    pub(super) async fn bounded<T, F>(&self, operation: &'static str, future: F) -> RuntimeResult<T>
    where
        F: Future<Output = RuntimeResult<T>>,
    {
        tokio::time::timeout(self.config.control_timeout, future)
            .await
            .map_err(|_| {
                RuntimeError::ProviderUnavailable(format!(
                    "Box {operation} exceeded the configured control timeout"
                ))
            })?
    }
}

fn probe_provider_build(execution_isolation: ExecutionIsolation) -> Result<String, String> {
    match execution_isolation {
        ExecutionIsolation::Microvm => {
            let support = crate::host_check::check_virtualization_support()
                .map_err(|error| format!("microVM unavailable: {error}"))?;
            Ok(format!(
                "a3s-box/{} isolation/microvm hypervisor/{}",
                env!("CARGO_PKG_VERSION"),
                support.backend
            ))
        }
        ExecutionIsolation::Sandbox => {
            let snapshot = crate::sandbox::probe_sandbox_capabilities_for(
                a3s_box_core::ExecutionBackend::A3sOci,
                None,
                None,
            );
            snapshot
                .require_ready()
                .map_err(|error| format!("shared-kernel backend unavailable: {error}"))?;
            let runtime = snapshot.a3s_oci.ok_or_else(|| {
                "shared-kernel capability probe returned no A3S OCI artifacts".to_string()
            })?;
            Ok(format!(
                "a3s-box/{} isolation/sandbox a3s-oci/sha256:{} agent/sha256:{}",
                env!("CARGO_PKG_VERSION"),
                &runtime.runtime_sha256[..16],
                &runtime.agent_sha256[..16]
            ))
        }
    }
}

fn validate_config(config: &BoxRuntimeDriverConfig) -> RuntimeResult<()> {
    if !config.home_dir.is_absolute() {
        return Err(RuntimeError::InvalidRequest(
            "Box Runtime home directory must be absolute".into(),
        ));
    }
    if config.control_timeout.is_zero() || config.task_poll_interval.is_zero() {
        return Err(RuntimeError::InvalidRequest(
            "Box Runtime timeout and poll interval must be positive".into(),
        ));
    }
    Ok(())
}

#[async_trait]
impl RuntimeDriver for BoxRuntimeDriver {
    fn provider_id(&self) -> &ProviderId {
        &self.provider_id
    }

    async fn capabilities(&self) -> RuntimeResult<RuntimeCapabilities> {
        let capabilities = RuntimeCapabilities {
            schema: RuntimeCapabilities::SCHEMA.into(),
            provider_id: self.provider_id.clone(),
            provider_build: self.provider_build().await?,
            unit_classes: vec![RuntimeUnitClass::Task, RuntimeUnitClass::Service],
            artifact_media_types: vec![OCI_IMAGE_MANIFEST.into(), OCI_IMAGE_INDEX.into()],
            // Runtime 0.2 uses `Sandbox` as the provider-neutral isolation
            // class. `execution_isolation` selects Box's concrete backend.
            isolation_levels: vec![IsolationLevel::Sandbox],
            network_modes: vec![NetworkMode::None],
            mount_kinds: vec![MountKind::Tmpfs],
            health_check_kinds: Vec::new(),
            resource_controls: vec![
                ResourceControl::Cpu,
                ResourceControl::Memory,
                ResourceControl::Pids,
                ResourceControl::ExecutionTimeout,
            ],
            features: vec![
                RuntimeFeature::DurableIdentity,
                RuntimeFeature::Stop,
                RuntimeFeature::Remove,
                RuntimeFeature::Logs,
                RuntimeFeature::Exec,
            ],
        };
        capabilities.validate().map_err(RuntimeError::Protocol)?;
        Ok(capabilities)
    }

    async fn apply(
        &self,
        spec: &RuntimeUnitSpec,
        current: &RuntimeObservation,
    ) -> RuntimeResult<RuntimeObservation> {
        self.apply_unit(spec, current).await
    }

    async fn inspect(&self, unit: &RuntimeUnitRecord) -> RuntimeResult<RuntimeInspection> {
        self.bounded("inspection", self.inspect_unit(unit)).await
    }

    async fn stop(
        &self,
        unit: &RuntimeUnitRecord,
        request: &RuntimeActionRequest,
    ) -> RuntimeResult<RuntimeObservation> {
        self.bounded("stop", self.stop_unit(unit, request)).await
    }

    async fn remove(
        &self,
        unit: &RuntimeUnitRecord,
        request: &RuntimeActionRequest,
    ) -> RuntimeResult<RuntimeRemoval> {
        self.bounded("remove", self.remove_unit(unit, request))
            .await
    }

    async fn logs(
        &self,
        unit: &RuntimeUnitRecord,
        query: &RuntimeLogQuery,
    ) -> RuntimeResult<Vec<RuntimeLogChunk>> {
        self.bounded("log read", self.read_runtime_logs(unit, query))
            .await
    }

    async fn exec(
        &self,
        unit: &RuntimeUnitRecord,
        request: &RuntimeExecRequest,
    ) -> RuntimeResult<RuntimeExecResult> {
        self.execute_runtime_command(unit, request).await
    }
}

#[cfg(test)]
mod conformance_tests;
#[cfg(test)]
mod exec_integration_tests;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
