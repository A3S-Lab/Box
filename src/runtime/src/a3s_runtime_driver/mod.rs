//! A3S Runtime provider adapter for Box isolation backends.

mod artifact;
mod attestation;
mod exec;
mod health;
mod lifecycle;
mod logs;
mod mapping;
mod metadata;
mod secret;
mod service_endpoints;
mod volume_storage;

use std::future::Future;
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use a3s_box_core::config::SevSnpGeneration;
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
use crate::tee::AttestationPolicy;
#[cfg(any(test, feature = "runtime-provider-qualification"))]
use crate::LocalExecutionBackend;
use crate::{BoxRecord, ExecutionIsolation, LocalExecutionManager, VmLocalExecutionBackend};

use self::artifact::ArtifactStorageOwner;
use self::attestation::AttestationArtifactOwner;
use self::secret::SecretMaterializationOwner;
use self::service_endpoints::ServiceEndpointOwner;

pub use self::artifact::{BoxArtifactPort, BoxArtifactPortError};
pub use self::secret::{
    BoxRegistryCredential, BoxSecretEnvironmentProjection, BoxSecretMaterial,
    BoxSecretMaterializationError, BoxSecretMaterializer, BoxTransientSecretStore,
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

/// Explicit AMD SEV-SNP policy for a confidential Box Runtime provider.
///
/// Hardware mode is the secure default. Simulation is advertised distinctly
/// and is accepted only when the caller opts in with `simulate: true`.
#[derive(Debug, Clone, Default)]
pub struct BoxRuntimeSevSnpConfig {
    /// CPU generation used to build the guest firmware configuration.
    pub generation: SevSnpGeneration,
    /// Accept a simulated SNP report instead of requiring genuine hardware.
    pub simulate: bool,
    /// Policy enforced during the RA-TLS attestation handshake.
    pub attestation_policy: AttestationPolicy,
}

/// Concrete A3S Runtime driver backed by a configured Box isolation backend.
pub struct BoxRuntimeDriver {
    provider_id: ProviderId,
    pub(super) config: BoxRuntimeDriverConfig,
    pub(super) manager: LocalExecutionManager,
    port_connector: Arc<dyn ExecutionPortConnector>,
    service_endpoints: ServiceEndpointOwner,
    execution_isolation: ExecutionIsolation,
    /// Whether this concrete provider instance can enforce a byte-precise
    /// Sandbox writable-layer quota. Qualification backends deliberately do
    /// not inherit the production host probe.
    supports_ephemeral_storage: bool,
    sev_snp: Option<BoxRuntimeSevSnpConfig>,
    attestation: AttestationArtifactOwner,
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

    /// Create a Runtime driver that explicitly supports AMD SEV-SNP
    /// confidential MicroVMs in addition to ordinary MicroVM sandboxes.
    pub fn new_confidential(
        config: BoxRuntimeDriverConfig,
        sev_snp: BoxRuntimeSevSnpConfig,
    ) -> RuntimeResult<Self> {
        validate_sev_snp_config(&sev_snp)?;
        let mut driver = Self::new(config)?;
        driver.sev_snp = Some(sev_snp);
        Ok(driver)
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
            execution_isolation == ExecutionIsolation::Sandbox
                && crate::rootfs::writable_layer_quota_supported(),
        )
    }

    /// Construct the Box Runtime driver over a caller-owned qualification
    /// backend and generation-fenced data-plane connector.
    ///
    /// This seam exists only for downstream product qualification. It lets a
    /// host exercise the production Box Runtime mapping, durable lifecycle,
    /// health, endpoint, stop, and removal code against real child processes
    /// without requiring a nested hypervisor or privileged OCI runtime on a
    /// general CI runner. Release builds do not expose this constructor unless
    /// the explicit `runtime-provider-qualification` feature is enabled.
    ///
    /// The supplied provider build is immutable evidence for the complete
    /// driver instance. Callers must not use this constructor as a production
    /// capability-probe bypass.
    #[cfg(any(test, feature = "runtime-provider-qualification"))]
    pub fn new_for_runtime_provider_qualification(
        config: BoxRuntimeDriverConfig,
        backend: Arc<dyn LocalExecutionBackend>,
        port_connector: Arc<dyn ExecutionPortConnector>,
        execution_isolation: ExecutionIsolation,
        provider_build: impl Into<String>,
    ) -> RuntimeResult<Self> {
        let provider_build = provider_build.into();
        validate_qualification_provider_build(&provider_build)?;
        let manager = LocalExecutionManager::new(
            config.home_dir.join("boxes.json"),
            &config.home_dir,
            backend,
        );
        let driver = Self::with_manager_connector_and_materializer(
            config,
            manager,
            port_connector,
            execution_isolation,
            None,
            None,
            Some(TransientRegistryAuthBroker::default()),
            false,
        )?;
        driver.provider_build.set(provider_build).map_err(|_| {
            RuntimeError::Protocol(
                "Box Runtime qualification provider build was initialized twice".into(),
            )
        })?;
        Ok(driver)
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

    #[allow(clippy::too_many_arguments)]
    fn with_manager_connector_and_materializer(
        config: BoxRuntimeDriverConfig,
        manager: LocalExecutionManager,
        connector: Arc<dyn ExecutionPortConnector>,
        execution_isolation: ExecutionIsolation,
        materializer: Option<Arc<dyn BoxSecretMaterializer>>,
        artifact_port: Option<Arc<dyn BoxArtifactPort>>,
        transient_registry_auth: Option<TransientRegistryAuthBroker>,
        supports_ephemeral_storage: bool,
    ) -> RuntimeResult<Self> {
        validate_config(&config)?;
        let endpoint_connector = Arc::clone(&connector);
        let secret_materialization =
            SecretMaterializationOwner::new(config.secret_root.clone(), materializer);
        let artifact_storage = ArtifactStorageOwner::new(config.home_dir.clone(), artifact_port);
        Ok(Self {
            provider_id: ProviderId::parse("a3s-box")?,
            config,
            manager,
            port_connector: connector,
            service_endpoints: ServiceEndpointOwner::new(endpoint_connector),
            execution_isolation,
            supports_ephemeral_storage,
            sev_snp: None,
            attestation: AttestationArtifactOwner::default(),
            artifact_storage,
            secret_materialization,
            transient_registry_auth,
            provider_build: OnceCell::new(),
        })
    }

    /// Concrete Box isolation backend selected for this provider instance.
    pub const fn execution_isolation(&self) -> ExecutionIsolation {
        self.execution_isolation
    }

    pub(super) fn sev_snp_config(&self) -> Option<&BoxRuntimeSevSnpConfig> {
        self.sev_snp.as_ref()
    }

    #[cfg(test)]
    fn with_attestation_transport(
        mut self,
        transport: Arc<dyn self::attestation::BoxAttestationTransport>,
    ) -> Self {
        self.attestation = AttestationArtifactOwner::with_transport(transport);
        self
    }

    #[cfg(test)]
    fn with_attested_main_starter(
        mut self,
        starter: Arc<dyn self::attestation::BoxAttestedMainStarter>,
    ) -> Self {
        self.attestation = self.attestation.with_main_starter(starter);
        self
    }

    pub(super) async fn provider_build(&self) -> RuntimeResult<String> {
        self.provider_build
            .get_or_try_init(|| async {
                let execution_isolation = self.execution_isolation;
                let sev_snp_simulated = self.sev_snp.as_ref().map(|config| config.simulate);
                let provider_build = tokio::time::timeout(
                    self.config.control_timeout,
                    tokio::task::spawn_blocking(move || {
                        probe_provider_build(execution_isolation, sev_snp_simulated)
                    }),
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

    /// Reserves the complete caller-declared graceful shutdown interval plus
    /// the ordinary provider-control budget. A provider-local timeout must not
    /// truncate a valid Runtime lifecycle policy; an outer request deadline
    /// may still bound the complete public operation.
    pub(super) async fn bounded_lifecycle<T, F>(
        &self,
        spec: &RuntimeUnitSpec,
        operation: &'static str,
        future: F,
    ) -> RuntimeResult<T>
    where
        F: Future<Output = RuntimeResult<T>>,
    {
        let timeout = self.lifecycle_control_timeout(spec);
        tokio::time::timeout(timeout, future).await.map_err(|_| {
            RuntimeError::ProviderUnavailable(format!(
                "Box {operation} exceeded the lifecycle-aware control timeout"
            ))
        })?
    }

    fn lifecycle_control_timeout(&self, spec: &RuntimeUnitSpec) -> Duration {
        let graceful_shutdown_seconds = spec
            .service_lifecycle
            .as_ref()
            .map_or(0, |lifecycle| u64::from(lifecycle.shutdown_grace_seconds));
        self.control_timeout_with_grace(graceful_shutdown_seconds)
    }

    pub(super) async fn bounded_record_lifecycle<T, F>(
        &self,
        record: &BoxRecord,
        operation: &'static str,
        future: F,
    ) -> RuntimeResult<T>
    where
        F: Future<Output = RuntimeResult<T>>,
    {
        let timeout = self.control_timeout_with_grace(record.stop_timeout.unwrap_or(0));
        tokio::time::timeout(timeout, future).await.map_err(|_| {
            RuntimeError::ProviderUnavailable(format!(
                "Box {operation} exceeded the lifecycle-aware control timeout"
            ))
        })?
    }

    fn control_timeout_with_grace(&self, graceful_shutdown_seconds: u64) -> Duration {
        self.config
            .control_timeout
            .saturating_add(Duration::from_secs(graceful_shutdown_seconds))
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

fn probe_provider_build(
    execution_isolation: ExecutionIsolation,
    sev_snp_simulated: Option<bool>,
) -> Result<String, String> {
    let provider_build = match execution_isolation {
        ExecutionIsolation::Microvm => {
            let support = crate::host_check::check_virtualization_support()
                .map_err(|error| format!("microVM unavailable: {error}"))?;
            if sev_snp_simulated == Some(false) {
                let tee = crate::tee::check_sev_snp_support()
                    .map_err(|error| format!("SEV-SNP capability probe failed: {error}"))?;
                if !tee.available {
                    return Err(format!(
                        "SEV-SNP unavailable: {}",
                        tee.reason.unwrap_or_else(|| "unknown reason".into())
                    ));
                }
            }
            format!(
                "a3s-box/{} isolation/microvm hypervisor/{}",
                env!("CARGO_PKG_VERSION"),
                support.backend
            )
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
            format!(
                "a3s-box/{} isolation/sandbox a3s-oci/sha256:{} agent/sha256:{}",
                env!("CARGO_PKG_VERSION"),
                &runtime.runtime_sha256[..16],
                &runtime.agent_sha256[..16]
            )
        }
    };
    Ok(match sev_snp_simulated {
        Some(true) => format!("{provider_build} tee/sev-snp-simulated"),
        Some(false) => format!("{provider_build} tee/sev-snp-hardware"),
        None => provider_build,
    })
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

#[cfg(any(test, feature = "runtime-provider-qualification"))]
fn validate_qualification_provider_build(provider_build: &str) -> RuntimeResult<()> {
    if provider_build.is_empty()
        || provider_build.len() > 255
        || provider_build.trim() != provider_build
        || provider_build.bytes().any(|byte| byte.is_ascii_control())
    {
        return Err(RuntimeError::InvalidRequest(
            "Box Runtime qualification provider build must be a trimmed non-empty string of at most 255 bytes"
                .into(),
        ));
    }
    Ok(())
}

fn validate_sev_snp_config(config: &BoxRuntimeSevSnpConfig) -> RuntimeResult<()> {
    if config
        .attestation_policy
        .expected_measurement
        .as_ref()
        .is_some_and(|measurement| {
            measurement.len() != 96
                || !measurement
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
        })
    {
        return Err(RuntimeError::InvalidRequest(
            "Box Runtime SEV-SNP measurement must be a canonical lowercase SHA-384 hex value"
                .into(),
        ));
    }
    if config.attestation_policy.max_report_age_secs.is_some() {
        return Err(RuntimeError::InvalidRequest(
            "Box Runtime SEV-SNP RA-TLS artifacts do not support report-age policy".into(),
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
            RuntimeFeature::ServiceLifecycle,
        ];
        if self.secret_materialization.configured() {
            features.push(RuntimeFeature::SecretReferences);
        }
        if self.artifact_storage.artifact_configured() {
            features.push(RuntimeFeature::OutputArtifacts);
        }
        if self.sev_snp.is_some() {
            features.push(RuntimeFeature::Attestation);
            features.push(RuntimeFeature::IdentityAttachment);
        }
        let mut mount_kinds = vec![MountKind::Volume, MountKind::Tmpfs];
        if self.artifact_storage.artifact_configured() {
            mount_kinds.insert(0, MountKind::Artifact);
        }
        let mut isolation_levels = vec![IsolationLevel::Sandbox];
        if self.sev_snp.is_some() {
            isolation_levels.push(IsolationLevel::Confidential);
        }
        let mut resource_controls = vec![
            ResourceControl::Cpu,
            ResourceControl::Memory,
            ResourceControl::Pids,
        ];
        if self.supports_ephemeral_storage {
            resource_controls.push(ResourceControl::EphemeralStorage);
        }
        resource_controls.push(ResourceControl::ExecutionTimeout);
        let capabilities = RuntimeCapabilities {
            schema: RuntimeCapabilities::SCHEMA.into(),
            provider_id: self.provider_id.clone(),
            provider_build: self.provider_build().await?,
            unit_classes: vec![RuntimeUnitClass::Task, RuntimeUnitClass::Service],
            artifact_media_types: vec![OCI_IMAGE_MANIFEST.into(), OCI_IMAGE_INDEX.into()],
            // Runtime uses `Sandbox` as the provider-neutral isolation
            // class. `execution_isolation` selects Box's concrete backend.
            isolation_levels,
            network_modes: vec![NetworkMode::None, NetworkMode::Service],
            mount_kinds,
            health_check_kinds: vec![
                HealthCheckKind::Http,
                HealthCheckKind::Tcp,
                HealthCheckKind::Command,
            ],
            resource_controls,
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
        self.bounded_lifecycle(&unit.spec, "inspection", self.inspect_unit(unit))
            .await
    }

    async fn stop(
        &self,
        unit: &RuntimeUnitRecord,
        request: &RuntimeActionRequest,
    ) -> RuntimeResult<RuntimeObservation> {
        self.bounded_lifecycle(&unit.spec, "stop", self.stop_unit(unit, request))
            .await
    }

    async fn remove(
        &self,
        unit: &RuntimeUnitRecord,
        request: &RuntimeActionRequest,
    ) -> RuntimeResult<RuntimeRemoval> {
        self.bounded_lifecycle(&unit.spec, "remove", self.remove_unit(unit, request))
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
#[cfg(all(test, unix))]
mod service_lifecycle_tests;
#[cfg(test)]
mod test_support;
#[cfg(test)]
mod tests;
