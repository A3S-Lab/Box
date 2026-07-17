//! A3S Runtime provider adapter for the certified Box Sandbox backend.

mod exec;
mod lifecycle;
mod logs;
mod mapping;
mod metadata;

use std::future::Future;
use std::path::PathBuf;
use std::time::Duration;

use a3s_runtime::contract::{
    IsolationLevel, NetworkMode, ResourceControl, RuntimeActionRequest, RuntimeCapabilities,
    RuntimeExecRequest, RuntimeExecResult, RuntimeFeature, RuntimeInspection, RuntimeLogChunk,
    RuntimeLogQuery, RuntimeObservation, RuntimeRemoval, RuntimeUnitClass, RuntimeUnitSpec,
};
use a3s_runtime::{ProviderId, RuntimeDriver, RuntimeError, RuntimeResult, RuntimeUnitRecord};
use async_trait::async_trait;
use tokio::sync::OnceCell;

use crate::LocalExecutionManager;

pub(super) const OCI_IMAGE_MANIFEST: &str = "application/vnd.oci.image.manifest.v1+json";
pub(super) const DOCKER_IMAGE_MANIFEST: &str =
    "application/vnd.docker.distribution.manifest.v2+json";

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

/// Concrete A3S Runtime driver backed only by Box's shared-kernel Sandbox.
pub struct BoxRuntimeDriver {
    provider_id: ProviderId,
    pub(super) config: BoxRuntimeDriverConfig,
    pub(super) manager: LocalExecutionManager,
    provider_build: OnceCell<String>,
}

impl BoxRuntimeDriver {
    pub fn new(config: BoxRuntimeDriverConfig) -> RuntimeResult<Self> {
        validate_config(&config)?;
        let manager = LocalExecutionManager::with_vm_backend(
            config.home_dir.join("boxes.json"),
            &config.home_dir,
        );
        Self::with_manager(config, manager)
    }

    fn with_manager(
        config: BoxRuntimeDriverConfig,
        manager: LocalExecutionManager,
    ) -> RuntimeResult<Self> {
        validate_config(&config)?;
        Ok(Self {
            provider_id: ProviderId::parse("a3s-box")?,
            config,
            manager,
            provider_build: OnceCell::new(),
        })
    }

    pub(super) async fn provider_build(&self) -> RuntimeResult<String> {
        self.provider_build
            .get_or_try_init(|| async {
                let snapshot = tokio::time::timeout(
                    self.config.control_timeout,
                    tokio::task::spawn_blocking(|| {
                        crate::sandbox::probe_sandbox_capabilities(None)
                    }),
                )
                .await
                .map_err(|_| {
                    RuntimeError::ProviderUnavailable(
                        "Box Sandbox capability probe exceeded the control timeout".into(),
                    )
                })?
                .map_err(|error| {
                    RuntimeError::ProviderUnavailable(format!(
                        "Box Sandbox capability probe failed: {error}"
                    ))
                })?;
                snapshot
                    .require_ready()
                    .map_err(|error| RuntimeError::ProviderUnavailable(error.to_string()))?;
                let runtime = snapshot.runtime.ok_or_else(|| {
                    RuntimeError::ProviderUnavailable(
                        "Box Sandbox capability probe returned no certified crun runtime".into(),
                    )
                })?;
                Ok::<String, RuntimeError>(format!(
                    "a3s-box/{} crun/{} sha256:{}",
                    env!("CARGO_PKG_VERSION"),
                    runtime.version,
                    &runtime.sha256[..16]
                ))
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
            artifact_media_types: vec![OCI_IMAGE_MANIFEST.into(), DOCKER_IMAGE_MANIFEST.into()],
            isolation_levels: vec![IsolationLevel::Sandbox],
            network_modes: vec![NetworkMode::None],
            mount_kinds: Vec::new(),
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
