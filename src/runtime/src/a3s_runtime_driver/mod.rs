//! A3S Runtime provider adapter for Box isolation backends.

mod artifact;
mod exec;
mod health;
mod lifecycle;
mod logs;
mod mapping;
mod metadata;
mod secret;
mod service_endpoints;

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use a3s_box_core::ExecutionPortConnector;
use a3s_runtime::contract::{
    HealthCheckKind, IsolationLevel, MountKind, NetworkMode, ResourceControl, RuntimeActionRequest,
    RuntimeCapabilities, RuntimeExecRequest, RuntimeExecResult, RuntimeFeature, RuntimeInspection,
    RuntimeLogChunk, RuntimeLogQuery, RuntimeObservation, RuntimeRemoval, RuntimeUnitClass,
    RuntimeUnitSpec,
};
use a3s_runtime::{ProviderId, RuntimeDriver, RuntimeError, RuntimeResult, RuntimeUnitRecord};
use async_trait::async_trait;
use tokio::sync::OnceCell;

use crate::local_execution::TransientRegistryAuthBroker;
use crate::{ExecutionIsolation, LocalExecutionManager, VmLocalExecutionBackend};

use self::artifact::ArtifactStorageOwner;
use self::secret::SecretMaterializationOwner;
use self::service_endpoints::ServiceEndpointOwner;

pub use self::artifact::{BoxArtifactPort, BoxArtifactPortError};
pub use self::secret::{
    BoxRegistryCredential, BoxSecretMaterial, BoxSecretMaterializationError, BoxSecretMaterializer,
};

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
    /// Existing private Linux tmpfs mount used only for transient Runtime
    /// Secret files. The provider never creates or silently downgrades this
    /// mount to disk-backed storage.
    pub secret_root: PathBuf,
}

impl Default for BoxRuntimeDriverConfig {
    fn default() -> Self {
        let home_dir = a3s_box_core::dirs_home();
        Self {
            secret_root: home_dir.join("runtime-secrets"),
            home_dir,
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
    port_connector: Arc<dyn ExecutionPortConnector>,
    service_endpoints: ServiceEndpointOwner,
    execution_isolation: ExecutionIsolation,
    artifact_storage: ArtifactStorageOwner,
    secret_materialization: SecretMaterializationOwner,
    transient_registry_auth: Option<TransientRegistryAuthBroker>,
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
        let (manager, broker) = production_manager(&config);
        let connector: Arc<dyn ExecutionPortConnector> = Arc::new(manager.clone());
        Self::with_manager_connector_and_materializer(
            config,
            manager,
            connector,
            execution_isolation,
            None,
            None,
            Some(broker),
        )
    }

    /// Compose the shared Box driver with one caller-owned Secret resolver.
    ///
    /// The resolver is normally backed by the node agent's existing
    /// authenticated control channel. It is not a second lifecycle or Secret
    /// store, and Box never persists the returned bytes.
    pub fn with_secret_materializer(
        mut self,
        materializer: Arc<dyn BoxSecretMaterializer>,
    ) -> Self {
        self.secret_materialization =
            SecretMaterializationOwner::new(self.config.secret_root.clone(), Some(materializer));
        self
    }

    /// Compose the shared Box driver with one caller-owned Artifact boundary.
    ///
    /// The caller keeps authenticated transport and Artifact admission. Box
    /// reuses its existing VolumeStore and lifecycle records for mount wiring,
    /// Task-output staging, generation fencing, and cleanup.
    pub fn with_artifact_port(mut self, port: Arc<dyn BoxArtifactPort>) -> Self {
        self.artifact_storage = ArtifactStorageOwner::new(self.config.home_dir.clone(), Some(port));
        self
    }

    fn with_manager_connector_and_materializer(
        config: BoxRuntimeDriverConfig,
        manager: LocalExecutionManager,
        connector: Arc<dyn ExecutionPortConnector>,
        execution_isolation: ExecutionIsolation,
        materializer: Option<Arc<dyn BoxSecretMaterializer>>,
        artifact_port: Option<Arc<dyn BoxArtifactPort>>,
        transient_registry_auth: Option<TransientRegistryAuthBroker>,
    ) -> RuntimeResult<Self> {
        validate_config(&config)?;
        let endpoint_connector = Arc::clone(&connector);
        let secret_materialization =
            SecretMaterializationOwner::new(config.secret_root.clone(), materializer);
        Ok(Self {
            provider_id: ProviderId::parse("a3s-box")?,
            config,
            manager,
            port_connector: connector,
            service_endpoints: ServiceEndpointOwner::new(endpoint_connector),
            execution_isolation,
            artifact_storage: ArtifactStorageOwner::new(config.home_dir.clone(), artifact_port),
            secret_materialization,
            transient_registry_auth,
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

fn production_manager(
    config: &BoxRuntimeDriverConfig,
) -> (LocalExecutionManager, TransientRegistryAuthBroker) {
    let broker = TransientRegistryAuthBroker::default();
    let backend =
        VmLocalExecutionBackend::new(&config.home_dir).with_transient_registry_auth(broker.clone());
    let manager = LocalExecutionManager::new(
        config.home_dir.join("boxes.json"),
        &config.home_dir,
        Arc::new(backend),
    );
    (manager, broker)
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
    let secret_root = config.secret_root.to_str().ok_or_else(|| {
        RuntimeError::InvalidRequest(
            "Box Runtime Secret root must be an encodable UTF-8 Linux path".into(),
        )
    })?;
    let normalized_secret_root = secret_root.strip_prefix('/').is_some_and(|relative| {
        !relative.is_empty()
            && relative
                .split('/')
                .all(|segment| !segment.is_empty() && !matches!(segment, "." | ".."))
    });
    if !normalized_secret_root
        || secret_root.contains([':', '\0'])
        || secret_root.bytes().any(|byte| byte.is_ascii_control())
        || !config.secret_root.is_absolute()
        || config.secret_root.parent().is_none()
        || config.secret_root.components().any(|component| {
            matches!(
                component,
                std::path::Component::CurDir
                    | std::path::Component::ParentDir
                    | std::path::Component::Prefix(_)
            )
        })
    {
        return Err(RuntimeError::InvalidRequest(
            "Box Runtime Secret root must be an encodable absolute normalized non-root Linux path"
                .into(),
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
        if self.secret_materialization.configured() {
            self.secret_materialization.require_ready().await?;
        }
        let mut features = vec![
            RuntimeFeature::DurableIdentity,
            RuntimeFeature::Stop,
            RuntimeFeature::Remove,
            RuntimeFeature::ServiceTcp,
            RuntimeFeature::Logs,
            RuntimeFeature::Exec,
        ];
        if self.secret_materialization.configured() {
            features.push(RuntimeFeature::SecretReferences);
        }
        if self.artifact_storage.artifact_configured() {
            features.push(RuntimeFeature::OutputArtifacts);
        }
        let mut mount_kinds = vec![MountKind::Volume, MountKind::Tmpfs];
        if self.artifact_storage.artifact_configured() {
            mount_kinds.insert(0, MountKind::Artifact);
        }
        let capabilities = RuntimeCapabilities {
            schema: RuntimeCapabilities::SCHEMA.into(),
            provider_id: self.provider_id.clone(),
            provider_build: self.provider_build().await?,
            unit_classes: vec![RuntimeUnitClass::Task, RuntimeUnitClass::Service],
            artifact_media_types: vec![OCI_IMAGE_MANIFEST.into(), OCI_IMAGE_INDEX.into()],
            // Runtime 0.2 uses `Sandbox` as the provider-neutral isolation
            // class. `execution_isolation` selects Box's concrete backend.
            isolation_levels: vec![IsolationLevel::Sandbox],
            network_modes: vec![NetworkMode::None, NetworkMode::Service],
            mount_kinds,
            health_check_kinds: vec![
                HealthCheckKind::Http,
                HealthCheckKind::Tcp,
                HealthCheckKind::Command,
            ],
            resource_controls: vec![
                ResourceControl::Cpu,
                ResourceControl::Memory,
                ResourceControl::Pids,
                ResourceControl::ExecutionTimeout,
            ],
            features,
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
mod artifact_tests;
#[cfg(test)]
mod conformance_tests;
#[cfg(test)]
mod exec_integration_tests;
#[cfg(test)]
mod lifecycle_tests;
#[cfg(test)]
mod service_endpoint_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
