//! Production Box-owned bundle preparation for the native Linux OCI service.

use std::path::{Path, PathBuf};

use a3s_box_core::config::{ResourceConfig, TeeConfig};
use a3s_box_core::{
    BoxError, ExecutionBackend, ExecutionIsolation, ExecutionManagerError, ExecutionManagerResult,
    NetworkMode,
};
use a3s_oci_sdk::{CreateAttachments, IoMode, IsolationRequest, OciBundle, ProcessIo};
use async_trait::async_trait;

use super::{
    LocalExecutionResourcePlan, OciBundlePreparationContext, OciBundleProvider,
    OciPreparedExecution, VmLocalExecutionBackend,
};
use crate::sandbox::probe_sandbox_capabilities_for;
use crate::{BoxRecord, ManagedExecutionMetadata};

/// Prepares immutable bundles from Box image/rootfs policy while the separate
/// A3S OCI Runtime service owns container lifecycle and I/O.
#[derive(Clone)]
pub struct NativeLinuxOciBundleProvider {
    preparer: VmLocalExecutionBackend,
    runtime_path: PathBuf,
    agent_path: PathBuf,
}

/// Qualification-only producer for OCI Runtime's Windows dedicated-VM service.
#[derive(Clone)]
pub struct WindowsWhpxOciBundleProvider {
    preparer: VmLocalExecutionBackend,
    runtime_root: PathBuf,
}

impl WindowsWhpxOciBundleProvider {
    pub fn new(home_dir: impl Into<PathBuf>, runtime_root: impl Into<PathBuf>) -> Self {
        Self {
            preparer: VmLocalExecutionBackend::new(home_dir),
            runtime_root: runtime_root.into(),
        }
    }

    pub fn runtime_root(&self) -> &Path {
        &self.runtime_root
    }

    pub fn with_pull_progress_fn(mut self, pull_progress_fn: crate::PullProgressFn) -> Self {
        self.preparer = self.preparer.with_pull_progress_fn(pull_progress_fn);
        self
    }
}

impl NativeLinuxOciBundleProvider {
    pub fn new(
        home_dir: impl Into<PathBuf>,
        runtime_path: impl Into<PathBuf>,
        agent_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            preparer: VmLocalExecutionBackend::new(home_dir),
            runtime_path: runtime_path.into(),
            agent_path: agent_path.into(),
        }
    }

    pub fn runtime_path(&self) -> &Path {
        &self.runtime_path
    }

    pub fn agent_path(&self) -> &Path {
        &self.agent_path
    }

    pub fn with_pull_progress_fn(mut self, pull_progress_fn: crate::PullProgressFn) -> Self {
        self.preparer = self.preparer.with_pull_progress_fn(pull_progress_fn);
        self
    }
}

#[async_trait]
impl OciBundleProvider for NativeLinuxOciBundleProvider {
    async fn plan_create_resources(
        &self,
        record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionResourcePlan> {
        let metadata = native_linux_metadata(record)?;
        let mut manager = self.preparer.new_oci_preparation_manager(record)?;
        let anonymous_volumes =
            if let Some(snapshot_id) = metadata.request.rootfs_snapshot_id.as_ref() {
                let home_dir = self.preparer.home_dir().to_path_buf();
                let snapshot_id = snapshot_id.to_string();
                let expected_image = metadata.request.config.image.clone();
                let config = tokio::task::spawn_blocking(move || {
                    let store = crate::SnapshotStore::new(&home_dir.join("snapshots"))?;
                    let _snapshot_lock = store.acquire_exclusive_lock()?;
                    let rootfs = store.rootfs_path(&snapshot_id);
                    crate::resolved_image::load_snapshot_oci_config(&rootfs, &expected_image)
                })
                .await
                .map_err(|error| {
                    ExecutionManagerError::Internal(format!(
                        "native Linux OCI snapshot resource planning task failed: {error}"
                    ))
                })?
                .map_err(|error| preparation_error("plan snapshot-owned resources", error))?;
                manager
                    .plan_anonymous_volumes(&config)
                    .map(|plans| plans.into_iter().map(|plan| plan.name).collect())
                    .map_err(|error| preparation_error("plan snapshot-owned resources", error))?
            } else {
                manager
                    .plan_image_anonymous_volumes()
                    .await
                    .map_err(|error| preparation_error("plan image-owned resources", error))?
            };
        Ok(LocalExecutionResourcePlan { anonymous_volumes })
    }

    async fn prepare(
        &self,
        record: &BoxRecord,
        _context: &OciBundlePreparationContext,
    ) -> ExecutionManagerResult<OciPreparedExecution> {
        let metadata = native_linux_metadata(record)?;

        // Re-hash the exact configured artifacts immediately before rootfs or
        // bundle mutations. Owner startup and bundle evidence use this same pair.
        let capabilities = probe_sandbox_capabilities_for(
            ExecutionBackend::A3sOci,
            Some(&self.runtime_path),
            Some(&self.agent_path),
        );
        capabilities
            .require_ready()
            .map_err(|error| preparation_error("capability preflight", error))?;

        let mut manager = self.preparer.new_oci_preparation_manager(record)?;
        let prepared = manager
            .prepare_runtime_owned_sandbox_bundle(&metadata.plan, &capabilities)
            .await
            .map_err(|error| preparation_error("prepare bundle", error))?;
        let bundle = match OciBundle::load(&prepared.bundle_dir).await {
            Ok(bundle) => bundle,
            Err(error) => {
                let cleanup = manager.cleanup_runtime_owned_sandbox_bundle();
                return Err(match cleanup {
                    Ok(()) => ExecutionManagerError::Internal(format!(
                        "failed to load the generated OCI bundle: {error}"
                    )),
                    Err(cleanup) => ExecutionManagerError::Internal(format!(
                        "failed to load the generated OCI bundle: {error}; cleanup also failed: {cleanup}"
                    )),
                });
            }
        };
        let io = ProcessIo {
            stdin: if metadata.request.config.stdin_open {
                IoMode::Pipe
            } else {
                IoMode::Null
            },
            stdout: IoMode::Capture,
            stderr: IoMode::Capture,
            terminal_size: None,
        };
        let attachments = match CreateAttachments::from_bundle(&bundle, io) {
            Ok(attachments) => attachments,
            Err(error) => {
                return Err(cleanup_after_prepare_failure(
                    &manager,
                    format!("failed to derive generated OCI bundle attachments: {error}"),
                ));
            }
        };
        let mut result = match OciPreparedExecution::with_attachments(
            bundle,
            attachments,
            prepared.console_output,
        ) {
            Ok(result) => result,
            Err(error) => {
                return Err(cleanup_after_prepare_failure(
                    &manager,
                    format!("failed to validate generated OCI bundle attachments: {error}"),
                ));
            }
        };
        result.anonymous_volumes = prepared.anonymous_volumes;
        Ok(result)
    }

    async fn cleanup(&self, record: &BoxRecord) -> ExecutionManagerResult<()> {
        let manager = self.preparer.new_oci_preparation_manager(record)?;
        manager
            .cleanup_runtime_owned_sandbox_bundle()
            .map_err(|error| preparation_error("cleanup bundle", error))
    }

    async fn ensure_log_projection(
        &self,
        record: &BoxRecord,
        binding: &super::OciRuntimeBinding,
    ) -> ExecutionManagerResult<()> {
        super::oci_log_projection::ensure(record, binding).await
    }

    async fn wait_log_projection_drained(
        &self,
        record: &BoxRecord,
        binding: &super::OciRuntimeBinding,
    ) -> ExecutionManagerResult<()> {
        super::oci_log_projection::wait_drained(record, binding).await
    }

    async fn wait_log_projection_stopped_after_owner_loss(
        &self,
        record: &BoxRecord,
        binding: &super::OciRuntimeBinding,
    ) -> ExecutionManagerResult<()> {
        super::oci_log_projection::wait_stopped_after_owner_loss(record, binding).await
    }
}

#[async_trait]
impl OciBundleProvider for WindowsWhpxOciBundleProvider {
    fn preflight(
        &self,
        record: &BoxRecord,
        context: &OciBundlePreparationContext,
    ) -> ExecutionManagerResult<()> {
        if !cfg!(all(target_os = "windows", target_arch = "x86_64")) {
            return Err(ExecutionManagerError::Unavailable(
                "Box/WHPX OCI qualification requires Windows x86_64".to_string(),
            ));
        }
        if record.isolation != ExecutionIsolation::Microvm
            || !matches!(context.isolation(), IsolationRequest::DedicatedVm)
        {
            return Err(ExecutionManagerError::InvalidRequest(
                "Box/WHPX OCI qualification requires dedicated MicroVM isolation".to_string(),
            ));
        }
        let metadata = record.managed_execution.as_ref().ok_or_else(|| {
            ExecutionManagerError::Internal(format!(
                "execution {} has no managed lifecycle metadata",
                record.id
            ))
        })?;
        metadata
            .validate()
            .map_err(|error| ExecutionManagerError::Internal(error.to_string()))?;
        if metadata.plan.backend != ExecutionBackend::Krun {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "execution {} did not resolve to the MicroVM backend",
                record.id
            )));
        }
        validate_whpx_qualification(record)?;
        context.runtime_bundle_handoff_directory(&self.runtime_root)?;
        validate_runtime_root(&self.runtime_root)
    }

    async fn prepare(
        &self,
        record: &BoxRecord,
        context: &OciBundlePreparationContext,
    ) -> ExecutionManagerResult<OciPreparedExecution> {
        self.preflight(record, context)?;
        let metadata = record.managed_execution.as_ref().ok_or_else(|| {
            ExecutionManagerError::Internal(format!(
                "execution {} has no managed lifecycle metadata",
                record.id
            ))
        })?;
        let bundle_directory = context.runtime_bundle_handoff_directory(&self.runtime_root)?;
        let mut manager = self.preparer.new_oci_preparation_manager(record)?;
        let prepared = manager
            .prepare_runtime_owned_microvm_bundle(&metadata.plan, &bundle_directory)
            .await
            .map_err(|error| whpx_preparation_error("prepare bundle", error))?;
        let bundle = match OciBundle::load(&prepared.bundle_dir).await {
            Ok(bundle) => bundle,
            Err(error) => {
                return Err(cleanup_after_whpx_prepare_failure(
                    &manager,
                    &bundle_directory,
                    format!("failed to load the generated portable OCI bundle: {error}"),
                ));
            }
        };
        let io = ProcessIo {
            stdin: if metadata.request.config.stdin_open {
                IoMode::Pipe
            } else {
                IoMode::Null
            },
            stdout: IoMode::Capture,
            stderr: IoMode::Capture,
            terminal_size: None,
        };
        let attachments = match CreateAttachments::from_bundle(&bundle, io) {
            Ok(attachments) => attachments,
            Err(error) => {
                return Err(cleanup_after_whpx_prepare_failure(
                    &manager,
                    &bundle_directory,
                    format!("failed to derive portable OCI bundle attachments: {error}"),
                ));
            }
        };
        let mut result = match OciPreparedExecution::with_attachments(
            bundle,
            attachments,
            prepared.console_output,
        ) {
            Ok(result) => result,
            Err(error) => {
                return Err(cleanup_after_whpx_prepare_failure(
                    &manager,
                    &bundle_directory,
                    format!("failed to validate portable OCI bundle attachments: {error}"),
                ));
            }
        };
        result = match result.with_runtime_bundle_handoff(context, &self.runtime_root) {
            Ok(result) => result,
            Err(error) => {
                return Err(cleanup_after_whpx_prepare_failure(
                    &manager,
                    &bundle_directory,
                    format!("failed to bind portable OCI bundle handoff: {error}"),
                ));
            }
        };
        result.anonymous_volumes = prepared.anonymous_volumes;
        Ok(result)
    }

    async fn cleanup(&self, record: &BoxRecord) -> ExecutionManagerResult<()> {
        let manager = self.preparer.new_oci_preparation_manager(record)?;
        manager
            .cleanup_runtime_owned_microvm_bundle()
            .map_err(|error| whpx_preparation_error("cleanup bundle", error))
    }

    async fn ensure_log_projection(
        &self,
        record: &BoxRecord,
        binding: &super::OciRuntimeBinding,
    ) -> ExecutionManagerResult<()> {
        super::oci_log_projection::ensure(record, binding).await
    }

    async fn wait_log_projection_drained(
        &self,
        record: &BoxRecord,
        binding: &super::OciRuntimeBinding,
    ) -> ExecutionManagerResult<()> {
        super::oci_log_projection::wait_drained(record, binding).await
    }

    async fn wait_log_projection_stopped_after_owner_loss(
        &self,
        record: &BoxRecord,
        binding: &super::OciRuntimeBinding,
    ) -> ExecutionManagerResult<()> {
        super::oci_log_projection::wait_stopped_after_owner_loss(record, binding).await
    }
}

fn validate_whpx_qualification(record: &BoxRecord) -> ExecutionManagerResult<()> {
    let metadata = record.managed_execution.as_ref().ok_or_else(|| {
        ExecutionManagerError::Internal(format!(
            "execution {} has no managed lifecycle metadata",
            record.id
        ))
    })?;
    let config = &metadata.request.config;
    let defaults = ResourceConfig::default();
    if config.resources.vcpus != 1 || config.resources.memory_mb != 512 {
        return Err(unqualified(
            "the fixed WHPX profile requires exactly 1 vCPU and 512 MiB of memory",
        ));
    }
    if config.resources.disk_mb != defaults.disk_mb || config.resources.timeout != defaults.timeout
    {
        return Err(unqualified(
            "custom disk size or lifetime timeout is not qualified for the WHPX OCI profile",
        ));
    }
    if config.tee != TeeConfig::None {
        return Err(unqualified("TEE is not qualified for the WHPX OCI profile"));
    }
    if !config.workspace.as_os_str().is_empty()
        || !config.volumes.is_empty()
        || config.virtiofs_cache.is_some()
        || !metadata.request.policy.volume_names.is_empty()
        || metadata.request.policy.managed_secret_root.is_some()
    {
        return Err(unqualified(
            "workspace, bind, named, and secret mounts are not qualified for the WHPX OCI profile",
        ));
    }
    if !matches!(config.network, NetworkMode::None)
        || !matches!(record.network_mode, NetworkMode::None)
        || !config.port_map.is_empty()
        || !config.dns.is_empty()
        || !config.add_hosts.is_empty()
    {
        return Err(unqualified(
            "the WHPX OCI profile requires network=none and no network customization",
        ));
    }
    if config.pool.enabled
        || config.pool.snapshot_fork
        || config.deferred_main
        || config.ksm
        || config.snapshot_mem_file.is_some()
        || config.snapshot_sock.is_some()
        || config.restore_from.is_some()
        || metadata.request.rootfs_snapshot_id.is_some()
    {
        return Err(unqualified(
            "pool, deferred-main, KSM, and Snapshot modes are not qualified for the WHPX OCI profile",
        ));
    }
    if !config.tmpfs.is_empty()
        || config.resource_limits != Default::default()
        || !config.cap_add.is_empty()
        || !config.cap_drop.is_empty()
        || !config.security_opt.is_empty()
        || !config.sysctls.is_empty()
        || config.privileged
        || config.read_only
        || config.sidecar.is_some()
        || config.persistent
    {
        return Err(unqualified(
            "custom mounts, controls, privileges, sidecars, and persistence are not qualified for the WHPX OCI profile",
        ));
    }
    let policy = &metadata.request.policy;
    if policy.init
        || !policy.devices.is_empty()
        || policy.gpus.is_some()
        || policy.shm_size.is_some()
        || policy.oom_kill_disable
        || policy.oom_score_adj.is_some()
    {
        return Err(unqualified(
            "init, device, GPU, shared-memory, and OOM overrides are not qualified for the WHPX OCI profile",
        ));
    }
    if policy.platform.as_deref().is_some_and(|platform| {
        !matches!(
            platform.trim().to_ascii_lowercase().as_str(),
            "linux/amd64" | "linux/x86_64"
        )
    }) {
        return Err(unqualified(
            "the WHPX OCI profile supports only Linux amd64 images",
        ));
    }
    if record.cpus != 1 || record.memory_mb != 512 {
        return Err(ExecutionManagerError::Internal(
            "Box record resources drifted from the fixed WHPX qualification profile".to_string(),
        ));
    }
    Ok(())
}

fn unqualified(message: &str) -> ExecutionManagerError {
    ExecutionManagerError::InvalidRequest(format!("Box/WHPX OCI qualification rejected: {message}"))
}

fn validate_runtime_root(path: &Path) -> ExecutionManagerResult<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to inspect WHPX runtime root {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_dir() || metadata_is_reparse_point(&metadata) {
        return Err(ExecutionManagerError::InvalidRequest(format!(
            "WHPX runtime root is not a plain directory: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(windows)]
fn metadata_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    use std::os::windows::fs::MetadataExt as _;
    const FILE_ATTRIBUTE_REPARSE_POINT: u32 = 0x400;
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT != 0
}

#[cfg(not(windows))]
fn metadata_is_reparse_point(metadata: &std::fs::Metadata) -> bool {
    metadata.file_type().is_symlink()
}

fn whpx_preparation_error(action: &str, error: BoxError) -> ExecutionManagerError {
    match error {
        BoxError::ConfigError(message) => ExecutionManagerError::InvalidRequest(message),
        error => {
            ExecutionManagerError::Unavailable(format!("Box/WHPX OCI {action} failed: {error}"))
        }
    }
}

fn cleanup_after_whpx_prepare_failure(
    manager: &crate::VmManager,
    bundle_directory: &Path,
    message: String,
) -> ExecutionManagerError {
    let bundle_cleanup = remove_scoped_bundle(bundle_directory);
    let rootfs_cleanup = manager.cleanup_runtime_owned_microvm_bundle();
    match (bundle_cleanup, rootfs_cleanup) {
        (Ok(()), Ok(())) => ExecutionManagerError::Internal(message),
        (bundle, rootfs) => ExecutionManagerError::Internal(format!(
            "{message}; cleanup also failed: handoff={bundle:?}, rootfs={rootfs:?}"
        )),
    }
}

fn remove_scoped_bundle(bundle_directory: &Path) -> std::io::Result<()> {
    if bundle_directory.file_name().and_then(|name| name.to_str()) != Some("bundle")
        || bundle_directory.parent().and_then(Path::parent).is_none()
    {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            format!(
                "refusing to remove an unscoped bundle: {}",
                bundle_directory.display()
            ),
        ));
    }
    let metadata = match std::fs::symlink_metadata(bundle_directory) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    if !metadata.is_dir() || metadata_is_reparse_point(&metadata) {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!(
                "bundle is not a plain directory: {}",
                bundle_directory.display()
            ),
        ));
    }
    std::fs::remove_dir_all(bundle_directory)
}

fn preparation_error(action: &str, error: BoxError) -> ExecutionManagerError {
    match error {
        BoxError::ConfigError(message) => ExecutionManagerError::InvalidRequest(message),
        error => {
            ExecutionManagerError::Unavailable(format!("native Linux OCI {action} failed: {error}"))
        }
    }
}

fn native_linux_metadata(record: &BoxRecord) -> ExecutionManagerResult<&ManagedExecutionMetadata> {
    if record.isolation != ExecutionIsolation::Sandbox {
        return Err(ExecutionManagerError::InvalidRequest(format!(
            "native Linux OCI migration only prepares Sandbox executions, got {:?}",
            record.isolation
        )));
    }
    let metadata = record.managed_execution.as_ref().ok_or_else(|| {
        ExecutionManagerError::Internal(format!(
            "execution {} has no managed lifecycle metadata",
            record.id
        ))
    })?;
    metadata
        .validate()
        .map_err(|error| ExecutionManagerError::Internal(error.to_string()))?;
    if metadata.plan.backend != ExecutionBackend::A3sOci {
        return Err(ExecutionManagerError::InvalidRequest(format!(
            "execution {} did not resolve to the A3S OCI Sandbox backend",
            record.id
        )));
    }
    Ok(metadata)
}

fn cleanup_after_prepare_failure(
    manager: &crate::VmManager,
    message: String,
) -> ExecutionManagerError {
    match manager.cleanup_runtime_owned_sandbox_bundle() {
        Ok(()) => ExecutionManagerError::Internal(message),
        Err(cleanup) => {
            ExecutionManagerError::Internal(format!("{message}; cleanup also failed: {cleanup}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use a3s_box_core::{
        BoxConfig, CreateExecutionRequest, ExecutionId, ExecutionIsolation, ExecutionSnapshotId,
        NetworkMode, OperationId, SnapshotImageConfig, SnapshotMetadata,
    };

    use super::*;

    #[tokio::test]
    async fn native_resource_plan_uses_snapshot_metadata_without_resolving_a_moved_tag() {
        let temporary = tempfile::tempdir().unwrap();
        let home = temporary.path().join("home");
        let snapshot_id = "anonymous-volume-snapshot";
        let image = "example.invalid/moved:latest";
        let source = temporary.path().join("snapshot-rootfs");
        std::fs::create_dir_all(&source).unwrap();
        let mut snapshot = SnapshotMetadata::new(
            snapshot_id.to_string(),
            snapshot_id.to_string(),
            "source-execution".to_string(),
            image.to_string(),
        );
        snapshot.image_config = Some(SnapshotImageConfig {
            volumes: vec!["/snapshot-data".to_string()],
            ..Default::default()
        });
        crate::SnapshotStore::new(&home.join("snapshots"))
            .unwrap()
            .save(snapshot, &source)
            .unwrap();

        let execution_id =
            ExecutionId::new("12345678-0000-0000-0000-000000000001".to_string()).unwrap();
        let request = CreateExecutionRequest {
            external_sandbox_id: "snapshot-plan".to_string(),
            config: BoxConfig {
                image: image.to_string(),
                isolation: ExecutionIsolation::Sandbox,
                network: NetworkMode::None,
                ..Default::default()
            },
            labels: BTreeMap::new(),
            policy: Default::default(),
            rootfs_snapshot_id: Some(ExecutionSnapshotId::new(snapshot_id).unwrap()),
        };
        let mut record = crate::local_execution::record::build_managed_record(
            &home,
            &execution_id,
            OperationId::new("snapshot-resource-plan").unwrap(),
            request,
            chrono::Utc::now(),
        )
        .unwrap();
        record.managed_execution.as_mut().unwrap().runtime_route =
            crate::ManagedRuntimeRoute::OciSdk;
        let provider = NativeLinuxOciBundleProvider::new(&home, "/runtime", "/agent");

        let plan = provider.plan_create_resources(&record).await.unwrap();

        assert_eq!(plan.anonymous_volumes.len(), 1);
        assert!(plan.anonymous_volumes[0].starts_with("anon_12345678_"));
        assert!(!home.join("images").exists());
        assert!(!record.box_dir.exists());
        assert!(!home.join("volumes").exists());
    }
}
