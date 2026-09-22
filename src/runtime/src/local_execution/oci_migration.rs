//! Explicit production composition for Sandbox migration to A3S OCI Runtime.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[cfg(target_os = "linux")]
use a3s_box_core::ExecutionBackend;
#[cfg(target_os = "linux")]
use a3s_box_core::{ExecutionIsolation, ExecutionState, KillOutcome};
use a3s_box_core::{ExecutionManagerError, ExecutionManagerResult};
#[cfg(target_os = "linux")]
use async_trait::async_trait;
#[cfg(target_os = "linux")]
use sha2::{Digest, Sha256};

#[cfg(target_os = "linux")]
mod isolation_split;

use super::LocalExecutionManager;
#[cfg(target_os = "linux")]
use super::{
    LinuxKvmOciBundleProvider, LocalExecutionBackend, LocalExecutionHandle,
    LocalExecutionObservation, NativeLinuxOciBundleProvider, OciLocalExecutionBackend,
    OciMigrationPolicy,
};
#[cfg(target_os = "linux")]
use crate::{BoxRecord, ManagedRuntimeRoute};

pub const OCI_MIGRATION_ENV: &str = "A3S_BOX_OCI_MIGRATION";
pub const OCI_HOST_ROOT_ENV: &str = "A3S_BOX_OCI_HOST_ROOT";
pub const OCI_RUNTIME_PATH_ENV: &str = "A3S_BOX_OCI_RUNTIME_PATH";
pub const OCI_AGENT_PATH_ENV: &str = "A3S_BOX_OCI_AGENT_PATH";
pub const OCI_WHPX_ENDPOINT_ENV: &str = "A3S_BOX_OCI_WHPX_ENDPOINT";
pub const OCI_KVM_ENDPOINT_ENV: &str = "A3S_BOX_OCI_KVM_ENDPOINT";
pub const OCI_KVM_BOX_OWNED_ENV: &str = "A3S_BOX_KVM_OCI_BOX_OWNED";
pub const OCI_KVM_SERVICE_ROOT_ENV: &str = "A3S_BOX_KVM_OCI_SERVICE_ROOT";
pub const OCI_KVM_SERVICE_BIN_ENV: &str = "A3S_BOX_KVM_OCI_SERVICE_BIN";
pub const OCI_KVM_SERVICE_SHIM_ENV: &str = "A3S_BOX_KVM_OCI_SERVICE_SHIM";
pub const OCI_KVM_SERVICE_MANIFEST_ENV: &str = "A3S_BOX_KVM_OCI_SERVICE_MANIFEST";
pub const OCI_WHPX_BOX_OWNED_ENV: &str = "A3S_BOX_WHPX_OCI_BOX_OWNED";
pub const OCI_WHPX_SERVICE_ROOT_ENV: &str = "A3S_BOX_WHPX_OCI_SERVICE_ROOT";
pub const OCI_WHPX_SERVICE_BIN_ENV: &str = "A3S_BOX_WHPX_OCI_SERVICE_BIN";
pub const OCI_WHPX_SERVICE_SHIM_ENV: &str = "A3S_BOX_WHPX_OCI_SERVICE_SHIM";
pub const OCI_WHPX_SERVICE_VM_ROOTFS_ENV: &str = "A3S_BOX_WHPX_OCI_SERVICE_VM_ROOTFS";
#[cfg(test)]
const DEFAULT_OCI_WHPX_ENDPOINT: &str = r"\\.\pipe\a3s-oci-box-qualification";

/// Explicit native-Linux owner and artifact selection for Sandbox migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeLinuxOciMigrationConfig {
    service_root: PathBuf,
    runtime_path: Option<PathBuf>,
    agent_path: Option<PathBuf>,
}

/// Explicit connection to an externally owned qualification-only WHPX service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsWhpxOciMigrationConfig {
    runtime_root: PathBuf,
    endpoint: super::OciRuntimeEndpoint,
    box_owned_owner: Option<WindowsWhpxBoxOwnedOwner>,
}

/// Optional Box-owned Host ensure inputs for Windows WHPX qualification.
///
/// When set, construction identity-fences and (re)spawns
/// `box-whpx-qualification-service` under `service_root`. External-only connect
/// remains available when this is absent so existing operator-launched Hosts
/// keep working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WindowsWhpxBoxOwnedOwner {
    service_root: PathBuf,
    runtime_path: PathBuf,
    shim_path: PathBuf,
    vm_rootfs: PathBuf,
}

impl WindowsWhpxBoxOwnedOwner {
    pub fn service_root(&self) -> &Path {
        &self.service_root
    }

    pub fn runtime_path(&self) -> &Path {
        &self.runtime_path
    }

    pub fn shim_path(&self) -> &Path {
        &self.shim_path
    }

    pub fn vm_rootfs(&self) -> &Path {
        &self.vm_rootfs
    }
}

/// Optional Box-owned Host ensure inputs for Linux KVM qualification.
///
/// When set, construction identity-fences and (re)spawns
/// `box-kvm-qualification-service` under `service_root`. External-only connect
/// remains available when this is absent so existing operator-launched Hosts
/// keep working.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxKvmBoxOwnedOwner {
    service_root: PathBuf,
    runtime_path: PathBuf,
    shim_path: PathBuf,
    system_image_manifest: PathBuf,
}

impl LinuxKvmBoxOwnedOwner {
    pub fn service_root(&self) -> &Path {
        &self.service_root
    }

    pub fn runtime_path(&self) -> &Path {
        &self.runtime_path
    }

    pub fn shim_path(&self) -> &Path {
        &self.shim_path
    }

    pub fn system_image_manifest(&self) -> &Path {
        &self.system_image_manifest
    }
}

/// Explicit connection to a qualification-only Linux KVM service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LinuxKvmOciMigrationConfig {
    runtime_root: PathBuf,
    endpoint: super::OciRuntimeEndpoint,
    box_owned_owner: Option<LinuxKvmBoxOwnedOwner>,
}

impl LinuxKvmOciMigrationConfig {
    pub fn new(
        runtime_root: impl Into<PathBuf>,
        endpoint: impl Into<PathBuf>,
    ) -> ExecutionManagerResult<Self> {
        let config = Self {
            runtime_root: runtime_root.into(),
            endpoint: super::OciRuntimeEndpoint::unix_socket(endpoint)?,
            box_owned_owner: None,
        };
        config.validate()?;
        Ok(config)
    }

    /// Attach Box-owned Host ensure/recovery for the qualification service.
    ///
    /// The configured endpoint must be `{service_root}/runtime.sock`. This does
    /// not select production MicroVM routing.
    pub fn with_box_owned_owner(
        mut self,
        service_root: impl Into<PathBuf>,
        runtime_path: impl Into<PathBuf>,
        shim_path: impl Into<PathBuf>,
        system_image_manifest: impl Into<PathBuf>,
    ) -> ExecutionManagerResult<Self> {
        self.box_owned_owner = Some(LinuxKvmBoxOwnedOwner {
            service_root: service_root.into(),
            runtime_path: runtime_path.into(),
            shim_path: shim_path.into(),
            system_image_manifest: system_image_manifest.into(),
        });
        self.validate()?;
        Ok(self)
    }

    pub fn runtime_root(&self) -> &Path {
        &self.runtime_root
    }

    pub fn endpoint(&self) -> &super::OciRuntimeEndpoint {
        &self.endpoint
    }

    pub fn box_owned_owner(&self) -> Option<&LinuxKvmBoxOwnedOwner> {
        self.box_owned_owner.as_ref()
    }

    pub fn from_environment(home_dir: &Path) -> ExecutionManagerResult<Option<Self>> {
        parse_linux_kvm_environment(
            LinuxKvmEnvironmentInputs {
                mode: std::env::var_os(OCI_MIGRATION_ENV),
                runtime_root: std::env::var_os(OCI_HOST_ROOT_ENV),
                endpoint: std::env::var_os(OCI_KVM_ENDPOINT_ENV),
                box_owned: std::env::var_os(OCI_KVM_BOX_OWNED_ENV),
                service_root: std::env::var_os(OCI_KVM_SERVICE_ROOT_ENV),
                service_bin: std::env::var_os(OCI_KVM_SERVICE_BIN_ENV),
                service_shim: std::env::var_os(OCI_KVM_SERVICE_SHIM_ENV),
                service_manifest: std::env::var_os(OCI_KVM_SERVICE_MANIFEST_ENV),
            },
            home_dir,
        )
    }

    fn validate(&self) -> ExecutionManagerResult<()> {
        validate_absolute_normalized(&self.runtime_root, "KVM OCI runtime root")?;
        match &self.endpoint {
            super::OciRuntimeEndpoint::UnixSocket { path } => {
                if let Some(owner) = &self.box_owned_owner {
                    validate_absolute_normalized(&owner.service_root, "KVM OCI service root")?;
                    validate_absolute_normalized(&owner.runtime_path, "KVM OCI runtime binary")?;
                    validate_absolute_normalized(&owner.shim_path, "KVM OCI shim")?;
                    validate_absolute_normalized(
                        &owner.system_image_manifest,
                        "KVM OCI system-image manifest",
                    )?;
                    let expected = owner.service_root.join("runtime.sock");
                    if path != &expected {
                        return Err(ExecutionManagerError::InvalidRequest(format!(
                            "Box-owned KVM OCI endpoint must be {} (got {})",
                            expected.display(),
                            path.display()
                        )));
                    }
                }
                Ok(())
            }
            super::OciRuntimeEndpoint::WindowsNamedPipe { .. } => {
                Err(ExecutionManagerError::InvalidRequest(
                    "KVM OCI qualification requires a Unix-domain socket endpoint".to_string(),
                ))
            }
        }
    }
}

impl WindowsWhpxOciMigrationConfig {
    pub fn new(
        runtime_root: impl Into<PathBuf>,
        endpoint: impl Into<String>,
    ) -> ExecutionManagerResult<Self> {
        let config = Self {
            runtime_root: runtime_root.into(),
            endpoint: super::OciRuntimeEndpoint::windows_named_pipe(endpoint)?,
            box_owned_owner: None,
        };
        config.validate()?;
        Ok(config)
    }

    /// Attach Box-owned Host ensure/recovery for the qualification service.
    ///
    /// The configured endpoint must be the deterministic pipe derived from
    /// `service_root`. This does not select production MicroVM routing.
    pub fn with_box_owned_owner(
        mut self,
        service_root: impl Into<PathBuf>,
        runtime_path: impl Into<PathBuf>,
        shim_path: impl Into<PathBuf>,
        vm_rootfs: impl Into<PathBuf>,
    ) -> ExecutionManagerResult<Self> {
        self.box_owned_owner = Some(WindowsWhpxBoxOwnedOwner {
            service_root: service_root.into(),
            runtime_path: runtime_path.into(),
            shim_path: shim_path.into(),
            vm_rootfs: vm_rootfs.into(),
        });
        self.validate()?;
        Ok(self)
    }

    pub fn runtime_root(&self) -> &Path {
        &self.runtime_root
    }

    pub fn endpoint(&self) -> &super::OciRuntimeEndpoint {
        &self.endpoint
    }

    pub fn box_owned_owner(&self) -> Option<&WindowsWhpxBoxOwnedOwner> {
        self.box_owned_owner.as_ref()
    }

    pub fn from_environment(home_dir: &Path) -> ExecutionManagerResult<Option<Self>> {
        parse_windows_environment(
            WindowsWhpxEnvironmentInputs {
                mode: std::env::var_os(OCI_MIGRATION_ENV),
                runtime_root: std::env::var_os(OCI_HOST_ROOT_ENV),
                endpoint: std::env::var_os(OCI_WHPX_ENDPOINT_ENV),
                box_owned: std::env::var_os(OCI_WHPX_BOX_OWNED_ENV),
                service_root: std::env::var_os(OCI_WHPX_SERVICE_ROOT_ENV),
                service_bin: std::env::var_os(OCI_WHPX_SERVICE_BIN_ENV),
                service_shim: std::env::var_os(OCI_WHPX_SERVICE_SHIM_ENV),
                service_vm_rootfs: std::env::var_os(OCI_WHPX_SERVICE_VM_ROOTFS_ENV),
            },
            home_dir,
        )
    }

    fn validate(&self) -> ExecutionManagerResult<()> {
        validate_absolute_normalized(&self.runtime_root, "WHPX OCI runtime root")?;
        match &self.endpoint {
            super::OciRuntimeEndpoint::WindowsNamedPipe { name } => {
                if let Some(owner) = &self.box_owned_owner {
                    validate_absolute_normalized(&owner.service_root, "WHPX OCI service root")?;
                    validate_absolute_normalized(&owner.runtime_path, "WHPX OCI runtime binary")?;
                    validate_absolute_normalized(&owner.shim_path, "WHPX OCI shim")?;
                    validate_absolute_normalized(&owner.vm_rootfs, "WHPX OCI vm-rootfs")?;
                    let expected = super::oci_whpx_owner::owned_pipe_name(&owner.service_root)?;
                    if name != &expected {
                        return Err(ExecutionManagerError::InvalidRequest(format!(
                            "Box-owned WHPX OCI endpoint must be {expected} (got {name})"
                        )));
                    }
                }
                Ok(())
            }
            super::OciRuntimeEndpoint::UnixSocket { .. } => {
                Err(ExecutionManagerError::InvalidRequest(
                    "WHPX OCI qualification requires a Windows named-pipe endpoint".to_string(),
                ))
            }
        }
    }
}

impl NativeLinuxOciMigrationConfig {
    pub fn new(service_root: impl Into<PathBuf>) -> ExecutionManagerResult<Self> {
        let config = Self {
            service_root: service_root.into(),
            runtime_path: None,
            agent_path: None,
        };
        config.validate()?;
        Ok(config)
    }

    pub fn with_artifacts(
        mut self,
        runtime_path: impl Into<PathBuf>,
        agent_path: impl Into<PathBuf>,
    ) -> ExecutionManagerResult<Self> {
        self.runtime_path = Some(runtime_path.into());
        self.agent_path = Some(agent_path.into());
        self.validate()?;
        Ok(self)
    }

    pub fn service_root(&self) -> &Path {
        &self.service_root
    }

    pub fn runtime_path(&self) -> Option<&Path> {
        self.runtime_path.as_deref()
    }

    pub fn agent_path(&self) -> Option<&Path> {
        self.agent_path.as_deref()
    }

    /// Parse the process-wide migration selection.
    ///
    /// On Linux, an absent value selects the Sandbox GA default (`SandboxViaOci`).
    /// Explicit `off`/`legacy` preserves the VM-only backend. Explicit
    /// `sandbox`/`on` selects the same composition but hard-fails construction
    /// when the OCI owner is not launch-ready.
    pub fn from_environment(home_dir: &Path) -> ExecutionManagerResult<Option<Self>> {
        Ok(parse_environment(
            std::env::var_os(OCI_MIGRATION_ENV),
            std::env::var_os(OCI_HOST_ROOT_ENV),
            std::env::var_os(OCI_RUNTIME_PATH_ENV),
            std::env::var_os(OCI_AGENT_PATH_ENV),
            home_dir,
        )?
        .map(|(_, config)| config))
    }

    fn validate(&self) -> ExecutionManagerResult<()> {
        validate_absolute_normalized(&self.service_root, "OCI host root")?;
        match (&self.runtime_path, &self.agent_path) {
            (Some(runtime), Some(agent)) => {
                validate_absolute_normalized(runtime, "OCI runtime path")?;
                validate_absolute_normalized(agent, "OCI agent path")?;
            }
            (None, None) => {}
            _ => {
                return Err(ExecutionManagerError::InvalidRequest(
                    "OCI runtime and agent paths must be supplied together".to_string(),
                ))
            }
        }
        Ok(())
    }
}

impl LocalExecutionManager {
    /// Compose the retained VM backend with the production native-Linux OCI
    /// owner and bundle provider. Only new Sandbox reservations use OCI.
    pub async fn with_native_linux_oci_migration(
        state_path: impl Into<PathBuf>,
        home_dir: impl Into<PathBuf>,
        config: NativeLinuxOciMigrationConfig,
    ) -> ExecutionManagerResult<Self> {
        Self::with_native_linux_oci_migration_and_pull_progress(
            state_path.into(),
            home_dir.into(),
            config,
            None,
        )
        .await
    }

    async fn with_native_linux_oci_migration_and_pull_progress(
        state_path: PathBuf,
        home_dir: PathBuf,
        config: NativeLinuxOciMigrationConfig,
        pull_progress_fn: Option<crate::PullProgressFn>,
    ) -> ExecutionManagerResult<Self> {
        config.validate()?;

        #[cfg(target_os = "linux")]
        {
            let oci =
                connect_native_linux_sandbox_backend(&home_dir, &config, pull_progress_fn.as_ref())
                    .await?;
            Ok(Self::with_oci_migration_backend_and_pull_progress(
                state_path,
                home_dir,
                oci,
                OciMigrationPolicy::SandboxViaOci,
                pull_progress_fn,
            ))
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = (state_path, home_dir, config, pull_progress_fn);
            Err(ExecutionManagerError::Unavailable(
                "native Linux OCI migration is supported only on Linux".to_string(),
            ))
        }
    }

    /// Compose the retained backend with the externally launched Box/WHPX OCI service.
    pub async fn with_windows_whpx_oci_qualification(
        state_path: impl Into<PathBuf>,
        home_dir: impl Into<PathBuf>,
        config: WindowsWhpxOciMigrationConfig,
    ) -> ExecutionManagerResult<Self> {
        Self::with_windows_whpx_oci_qualification_and_pull_progress(
            state_path.into(),
            home_dir.into(),
            config,
            None,
        )
        .await
    }

    async fn with_windows_whpx_oci_qualification_and_pull_progress(
        state_path: PathBuf,
        home_dir: PathBuf,
        config: WindowsWhpxOciMigrationConfig,
        pull_progress_fn: Option<crate::PullProgressFn>,
    ) -> ExecutionManagerResult<Self> {
        config.validate()?;

        #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
        {
            let mut provider =
                super::WindowsWhpxOciBundleProvider::new(home_dir.clone(), config.runtime_root());
            if let Some(progress) = pull_progress_fn.as_ref() {
                provider = provider.with_pull_progress_fn(progress.clone());
            }
            let provider = Arc::new(provider);
            let oci = if let Some(owner) = config.box_owned_owner() {
                let artifacts = super::oci_whpx_owner::WindowsWhpxOwnerArtifacts::certify(
                    owner.runtime_path.clone(),
                    owner.shim_path.clone(),
                    owner.vm_rootfs.clone(),
                )?;
                let endpoint = super::oci_whpx_owner::ensure_windows_whpx_oci_owner(
                    &owner.service_root,
                    &artifacts,
                )
                .await?;
                if &endpoint != config.endpoint() {
                    return Err(ExecutionManagerError::Internal(format!(
                        "Windows WHPX OCI owner ensure returned a different endpoint ({endpoint:?}) than configured ({:?})",
                        config.endpoint()
                    )));
                }
                Arc::new(
                    super::OciLocalExecutionBackend::connect(endpoint, provider)
                        .await?
                        .with_windows_whpx_owner_recovery(owner.service_root.clone(), artifacts),
                )
            } else {
                Arc::new(
                    super::OciLocalExecutionBackend::connect(config.endpoint().clone(), provider)
                        .await?,
                )
            };
            Ok(Self::with_oci_migration_backend_and_pull_progress(
                state_path,
                home_dir,
                oci,
                super::OciMigrationPolicy::MicrovmViaOci,
                pull_progress_fn,
            ))
        }

        #[cfg(not(all(target_os = "windows", target_arch = "x86_64")))]
        {
            let _ = (state_path, home_dir, config, pull_progress_fn);
            Err(ExecutionManagerError::Unavailable(
                "Box/WHPX OCI qualification requires Windows x86_64".to_string(),
            ))
        }
    }

    /// Compose the retained backend with the externally launched Box/KVM OCI service.
    pub async fn with_linux_kvm_oci_qualification(
        state_path: impl Into<PathBuf>,
        home_dir: impl Into<PathBuf>,
        config: LinuxKvmOciMigrationConfig,
    ) -> ExecutionManagerResult<Self> {
        Self::with_linux_kvm_oci_qualification_and_pull_progress(
            state_path.into(),
            home_dir.into(),
            config,
            None,
        )
        .await
    }

    async fn with_linux_kvm_oci_qualification_and_pull_progress(
        state_path: PathBuf,
        home_dir: PathBuf,
        config: LinuxKvmOciMigrationConfig,
        pull_progress_fn: Option<crate::PullProgressFn>,
    ) -> ExecutionManagerResult<Self> {
        config.validate()?;

        #[cfg(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        ))]
        {
            let mut provider =
                LinuxKvmOciBundleProvider::new(home_dir.clone(), config.runtime_root());
            if let Some(progress) = pull_progress_fn.as_ref() {
                provider = provider.with_pull_progress_fn(progress.clone());
            }
            let provider = Arc::new(provider);
            let oci = if let Some(owner) = config.box_owned_owner() {
                let artifacts = super::oci_kvm_owner::LinuxKvmOwnerArtifacts::certify(
                    owner.runtime_path.clone(),
                    owner.shim_path.clone(),
                    owner.system_image_manifest.clone(),
                )?;
                // Fresh KVM qualification construction: reclaiming a dead Host
                // must tear down Live-survivable session-owner/shim orphans.
                // Retained-manager Live reopen keeps the default (no reap).
                let endpoint = super::oci_kvm_owner::ensure_linux_kvm_oci_owner_with_options(
                    &owner.service_root,
                    &artifacts,
                    super::oci_kvm_owner::EnsureLinuxKvmOwnerOptions {
                        reap_orphaned_session_children: true,
                        runtime_root: Some(config.runtime_root().to_path_buf()),
                    },
                )
                .await?;
                if &endpoint != config.endpoint() {
                    return Err(ExecutionManagerError::Internal(format!(
                        "Linux KVM OCI owner ensure returned a different endpoint ({endpoint:?}) than configured ({:?})",
                        config.endpoint()
                    )));
                }
                Arc::new(
                    OciLocalExecutionBackend::connect(endpoint, provider)
                        .await?
                        .with_linux_kvm_owner_recovery(owner.service_root.clone(), artifacts),
                )
            } else {
                Arc::new(
                    OciLocalExecutionBackend::connect(config.endpoint().clone(), provider).await?,
                )
            };
            Ok(Self::with_oci_migration_backend_and_pull_progress(
                state_path,
                home_dir.clone(),
                Arc::new(isolation_split::IsolationSplitBackend::new(
                    sandbox_backend_beside_kvm_qualification(&home_dir, pull_progress_fn.as_ref())
                        .await?,
                    oci,
                )),
                OciMigrationPolicy::AllViaOci,
                pull_progress_fn,
            ))
        }

        #[cfg(not(all(
            target_os = "linux",
            any(target_arch = "x86_64", target_arch = "aarch64")
        )))]
        {
            let _ = (state_path, home_dir, config, pull_progress_fn);
            Err(ExecutionManagerError::Unavailable(
                "Box/KVM OCI qualification requires Linux x86_64 or aarch64".to_string(),
            ))
        }
    }

    /// Select the production Sandbox migration composition.
    ///
    /// On Linux, an absent `A3S_BOX_OCI_MIGRATION` defaults to `SandboxViaOci`.
    /// Explicit `off` keeps the legacy VM backend. Explicit `sandbox` hard-fails
    /// when the OCI owner is not launch-ready; the Linux default soft-composes a
    /// fail-closed unavailable OCI backend so MicroVM continue to work without
    /// OCI host prep.
    pub async fn with_configured_backend(
        state_path: impl Into<PathBuf>,
        home_dir: impl Into<PathBuf>,
    ) -> ExecutionManagerResult<Self> {
        Self::with_configured_backend_and_pull_progress(state_path, home_dir, None).await
    }

    /// Configured construction retaining the CLI's image-pull progress hook on
    /// both the legacy and migrated preparation paths.
    pub async fn with_configured_backend_and_pull_progress(
        state_path: impl Into<PathBuf>,
        home_dir: impl Into<PathBuf>,
        pull_progress_fn: Option<crate::PullProgressFn>,
    ) -> ExecutionManagerResult<Self> {
        let state_path = state_path.into();
        let home_dir = home_dir.into();

        #[cfg(all(target_os = "windows", target_arch = "x86_64"))]
        {
            match WindowsWhpxOciMigrationConfig::from_environment(&home_dir)? {
                Some(config) => {
                    Self::with_windows_whpx_oci_qualification_and_pull_progress(
                        state_path,
                        home_dir,
                        config,
                        pull_progress_fn,
                    )
                    .await
                }
                None => legacy_backend(state_path, home_dir, pull_progress_fn),
            }
        }

        #[cfg(target_os = "linux")]
        {
            if let Some(config) = LinuxKvmOciMigrationConfig::from_environment(&home_dir)? {
                return Self::with_linux_kvm_oci_qualification_and_pull_progress(
                    state_path,
                    home_dir,
                    config,
                    pull_progress_fn,
                )
                .await;
            }
            match parse_environment(
                std::env::var_os(OCI_MIGRATION_ENV),
                std::env::var_os(OCI_HOST_ROOT_ENV),
                std::env::var_os(OCI_RUNTIME_PATH_ENV),
                std::env::var_os(OCI_AGENT_PATH_ENV),
                &home_dir,
            )? {
                Some((selection, config)) => {
                    match Self::with_native_linux_oci_migration_and_pull_progress(
                        state_path.clone(),
                        home_dir.clone(),
                        config,
                        pull_progress_fn.clone(),
                    )
                    .await
                    {
                        Ok(manager) => Ok(manager),
                        Err(error)
                            if matches!(
                                selection,
                                NativeLinuxMigrationSelection::DefaultSandbox
                            ) =>
                        {
                            Ok(Self::with_oci_migration_backend_and_pull_progress(
                                state_path,
                                home_dir,
                                Arc::new(UnavailableOciMigrationBackend::new(error)),
                                OciMigrationPolicy::SandboxViaOci,
                                pull_progress_fn,
                            ))
                        }
                        Err(error) => Err(error),
                    }
                }
                None => legacy_backend(state_path, home_dir, pull_progress_fn),
            }
        }

        #[cfg(not(any(
            target_os = "linux",
            all(target_os = "windows", target_arch = "x86_64")
        )))]
        {
            match NativeLinuxOciMigrationConfig::from_environment(&home_dir)? {
                Some(config) => {
                    Self::with_native_linux_oci_migration_and_pull_progress(
                        state_path,
                        home_dir,
                        config,
                        pull_progress_fn,
                    )
                    .await
                }
                None => legacy_backend(state_path, home_dir, pull_progress_fn),
            }
        }
    }
}

fn legacy_backend(
    state_path: PathBuf,
    home_dir: PathBuf,
    pull_progress_fn: Option<crate::PullProgressFn>,
) -> ExecutionManagerResult<LocalExecutionManager> {
    let mut backend = crate::local_execution::VmLocalExecutionBackend::new(&home_dir);
    if let Some(progress) = pull_progress_fn {
        backend = backend.with_pull_progress_fn(progress);
    }
    Ok(LocalExecutionManager::new(
        state_path,
        home_dir,
        Arc::new(backend),
    ))
}

/// How Linux selected the native Sandbox OCI composition.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[allow(dead_code)] // DefaultSandbox is selected only on Linux host paths.
enum NativeLinuxMigrationSelection {
    /// Env absent on Linux: default SandboxViaOci; soft if owner not ready.
    DefaultSandbox,
    /// Explicit sandbox/on: hard-fail when the owner is not launch-ready.
    ExplicitSandbox,
}

/// Fail-closed OCI side of `SandboxViaOci` when default activation cannot start
/// the owner. MicroVM continues on the legacy backend; Sandbox preflight fails.
#[cfg(target_os = "linux")]
#[derive(Debug, Clone)]
struct UnavailableOciMigrationBackend {
    message: String,
}

#[cfg(target_os = "linux")]
impl UnavailableOciMigrationBackend {
    fn new(error: ExecutionManagerError) -> Self {
        Self {
            message: format!(
                "Linux Sandbox OCI owner is not launch-ready ({error}); install the A3S OCI Runtime package or set {OCI_MIGRATION_ENV}=off for MicroVM-only hosts"
            ),
        }
    }

    fn unavailable(&self) -> ExecutionManagerError {
        ExecutionManagerError::Unavailable(self.message.clone())
    }
}

#[cfg(target_os = "linux")]
#[async_trait]
impl LocalExecutionBackend for UnavailableOciMigrationBackend {
    async fn preflight_isolation(
        &self,
        isolation: ExecutionIsolation,
    ) -> ExecutionManagerResult<()> {
        if isolation.is_sandbox() {
            Err(self.unavailable())
        } else {
            Ok(())
        }
    }

    fn route_for_create(&self, _record: &BoxRecord) -> ExecutionManagerResult<ManagedRuntimeRoute> {
        Ok(ManagedRuntimeRoute::OciSdk)
    }

    async fn preflight(&self, _record: &BoxRecord) -> ExecutionManagerResult<()> {
        Err(self.unavailable())
    }

    async fn start(&self, _record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        Err(self.unavailable())
    }

    async fn inspect(
        &self,
        _record: &BoxRecord,
    ) -> ExecutionManagerResult<LocalExecutionObservation> {
        Ok(LocalExecutionObservation {
            state: ExecutionState::Created,
            handle: None,
            exit_code: None,
        })
    }

    async fn pause(
        &self,
        _record: &BoxRecord,
        _keep_memory: bool,
    ) -> ExecutionManagerResult<LocalExecutionHandle> {
        Err(self.unavailable())
    }

    async fn resume(&self, _record: &BoxRecord) -> ExecutionManagerResult<LocalExecutionHandle> {
        Err(self.unavailable())
    }

    async fn kill(&self, _record: &BoxRecord) -> ExecutionManagerResult<KillOutcome> {
        Err(self.unavailable())
    }
}

#[cfg(target_os = "linux")]
async fn connect_native_linux_sandbox_backend(
    home_dir: &Path,
    config: &NativeLinuxOciMigrationConfig,
    pull_progress_fn: Option<&crate::PullProgressFn>,
) -> ExecutionManagerResult<Arc<dyn LocalExecutionBackend>> {
    config.validate()?;
    let capabilities = crate::sandbox::probe_sandbox_capabilities_for(
        ExecutionBackend::A3sOci,
        config.runtime_path(),
        config.agent_path(),
    );
    capabilities.require_ready().map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "native Linux OCI migration preflight failed: {error}"
        ))
    })?;
    let artifacts = capabilities.a3s_oci.as_ref().ok_or_else(|| {
        ExecutionManagerError::Unavailable(
            "native Linux OCI migration preflight returned no runtime artifacts".to_string(),
        )
    })?;
    // Fresh SandboxViaOci construction: reclaiming a dead Host must tear down
    // Live-survivable supervised orphans so stopped-only reconcile sees a
    // tombstone. Retained-manager Live reopen keeps the default (no reap).
    let endpoint = super::oci_owner::ensure_native_linux_oci_owner_with_options(
        config.service_root(),
        artifacts,
        super::oci_owner::EnsureNativeLinuxOwnerOptions {
            reap_orphaned_supervised_sessions: true,
        },
    )
    .await?;
    let mut provider = NativeLinuxOciBundleProvider::new(
        home_dir.to_path_buf(),
        artifacts.runtime_path.clone(),
        artifacts.agent_path.clone(),
    );
    if let Some(progress) = pull_progress_fn {
        provider = provider.with_pull_progress_fn(progress.clone());
    }
    let backend = OciLocalExecutionBackend::connect(endpoint, Arc::new(provider))
        .await?
        .with_native_linux_owner_recovery(config.service_root(), artifacts.clone());
    Ok(Arc::new(backend))
}

/// `all` asks for both isolations on OCI, so a missing Sandbox owner fails the
/// process. `microvm`/`kvm` keep qualification alive and fail Sandbox closed
/// through [`UnavailableOciMigrationBackend`].
#[cfg(target_os = "linux")]
fn migration_mode_requires_ready_sandbox() -> bool {
    std::env::var_os(OCI_MIGRATION_ENV)
        .and_then(|value| value.into_string().ok())
        .is_some_and(|value| {
            matches!(
                value.trim().to_ascii_lowercase().as_str(),
                "all" | "all-via-oci"
            )
        })
}

#[cfg(target_os = "linux")]
async fn sandbox_backend_beside_kvm_qualification(
    home_dir: &Path,
    pull_progress_fn: Option<&crate::PullProgressFn>,
) -> ExecutionManagerResult<Arc<dyn LocalExecutionBackend>> {
    let Some((_, config)) = parse_environment(
        None,
        std::env::var_os(OCI_HOST_ROOT_ENV),
        std::env::var_os(OCI_RUNTIME_PATH_ENV),
        std::env::var_os(OCI_AGENT_PATH_ENV),
        home_dir,
    )?
    else {
        return Err(ExecutionManagerError::Internal(
            "Linux default Sandbox OCI config was not selected beside KVM qualification"
                .to_string(),
        ));
    };
    match connect_native_linux_sandbox_backend(home_dir, &config, pull_progress_fn).await {
        Ok(backend) => Ok(backend),
        Err(error) if !migration_mode_requires_ready_sandbox() => {
            Ok(Arc::new(UnavailableOciMigrationBackend::new(error)))
        }
        Err(error) => Err(error),
    }
}

fn parse_environment(
    mode: Option<OsString>,
    service_root: Option<OsString>,
    runtime_path: Option<OsString>,
    agent_path: Option<OsString>,
    home_dir: &Path,
) -> ExecutionManagerResult<Option<(NativeLinuxMigrationSelection, NativeLinuxOciMigrationConfig)>>
{
    let selection = match mode.filter(|value| !value.is_empty()) {
        None => {
            #[cfg(target_os = "linux")]
            {
                NativeLinuxMigrationSelection::DefaultSandbox
            }
            #[cfg(not(target_os = "linux"))]
            {
                return Ok(None);
            }
        }
        Some(mode) => {
            let mode = mode.to_str().ok_or_else(|| {
                ExecutionManagerError::InvalidRequest(format!(
                    "{OCI_MIGRATION_ENV} must contain UTF-8 text"
                ))
            })?;
            match mode.trim().to_ascii_lowercase().as_str() {
                "" | "0" | "false" | "off" | "disabled" | "legacy" => return Ok(None),
                "1" | "true" | "on" | "sandbox" | "sandbox-via-oci" => {
                    NativeLinuxMigrationSelection::ExplicitSandbox
                }
                "all" | "all-via-oci" | "microvm" | "microvm-via-oci" | "kvm" => {
                    return Err(ExecutionManagerError::InvalidRequest(format!(
                        "MicroVM OCI qualification uses {OCI_KVM_ENDPOINT_ENV} with LinuxKvmOciMigrationConfig; NativeLinuxOciMigrationConfig accepts only sandbox"
                    )))
                }
                value => {
                    return Err(ExecutionManagerError::InvalidRequest(format!(
                        "unsupported {OCI_MIGRATION_ENV} value {value:?}; expected off or sandbox"
                    )))
                }
            }
        }
    };

    let root = service_root
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_service_root(home_dir));
    let mut config = NativeLinuxOciMigrationConfig::new(root)?;
    match (
        runtime_path.filter(|value| !value.is_empty()),
        agent_path.filter(|value| !value.is_empty()),
    ) {
        (Some(runtime), Some(agent)) => {
            config = config.with_artifacts(PathBuf::from(runtime), PathBuf::from(agent))?;
        }
        (None, None) => {}
        _ => {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "{OCI_RUNTIME_PATH_ENV} and {OCI_AGENT_PATH_ENV} must be set together"
            )))
        }
    }
    Ok(Some((selection, config)))
}

/// Explicit on/off flag for qualification Host ownership.
///
/// Absent or empty stays off. Unknown text fails closed so a typo cannot
/// silently select the external qualification endpoint.
fn parse_explicit_bool(env_name: &str, value: Option<OsString>) -> ExecutionManagerResult<bool> {
    let Some(value) = value.filter(|value| !value.is_empty()) else {
        return Ok(false);
    };
    let value = value.to_str().ok_or_else(|| {
        ExecutionManagerError::InvalidRequest(format!("{env_name} must contain UTF-8 text"))
    })?;
    match value.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "off" | "no" => Ok(false),
        "1" | "true" | "on" | "yes" | "box-owned" => Ok(true),
        other => Err(ExecutionManagerError::InvalidRequest(format!(
            "unsupported {env_name} value {other:?}; expected 0/1, false/true, off/on, no/yes, or box-owned"
        ))),
    }
}

fn parse_windows_environment(
    inputs: WindowsWhpxEnvironmentInputs,
    home_dir: &Path,
) -> ExecutionManagerResult<Option<WindowsWhpxOciMigrationConfig>> {
    let WindowsWhpxEnvironmentInputs {
        mode,
        runtime_root,
        endpoint,
        box_owned,
        service_root,
        service_bin,
        service_shim,
        service_vm_rootfs,
    } = inputs;
    let Some(mode) = mode.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let mode = mode.to_str().ok_or_else(|| {
        ExecutionManagerError::InvalidRequest(format!(
            "{OCI_MIGRATION_ENV} must contain UTF-8 text"
        ))
    })?;
    match mode.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "off" | "disabled" | "legacy" => return Ok(None),
        "all" | "all-via-oci" | "microvm" | "microvm-via-oci" | "whpx" => {}
        "1" | "true" | "on" | "sandbox" | "sandbox-via-oci" => {
            return Err(ExecutionManagerError::InvalidRequest(
                "Windows OCI qualification supports only MicroVM/WHPX routing; use all or microvm"
                    .to_string(),
            ))
        }
        value => {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "unsupported {OCI_MIGRATION_ENV} value {value:?}; expected off, microvm, or all"
            )))
        }
    }

    let runtime_root = runtime_root
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_service_root(home_dir));
    let box_owned = parse_explicit_bool(OCI_WHPX_BOX_OWNED_ENV, box_owned)?;

    if box_owned {
        let service_root = service_root
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                ExecutionManagerError::InvalidRequest(format!(
                    "{OCI_WHPX_SERVICE_ROOT_ENV} must be set when {OCI_WHPX_BOX_OWNED_ENV} is enabled"
                ))
            })?;
        let service_bin = service_bin
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                ExecutionManagerError::InvalidRequest(format!(
                    "{OCI_WHPX_SERVICE_BIN_ENV} must be set when {OCI_WHPX_BOX_OWNED_ENV} is enabled"
                ))
            })?;
        let service_shim = service_shim
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                ExecutionManagerError::InvalidRequest(format!(
                    "{OCI_WHPX_SERVICE_SHIM_ENV} must be set when {OCI_WHPX_BOX_OWNED_ENV} is enabled"
                ))
            })?;
        let service_vm_rootfs = service_vm_rootfs
            .filter(|value| !value.is_empty())
            .map(PathBuf::from)
            .ok_or_else(|| {
                ExecutionManagerError::InvalidRequest(format!(
                    "{OCI_WHPX_SERVICE_VM_ROOTFS_ENV} must be set when {OCI_WHPX_BOX_OWNED_ENV} is enabled"
                ))
            })?;
        let endpoint = match endpoint.filter(|value| !value.is_empty()) {
            Some(value) => value.into_string().map_err(|_| {
                ExecutionManagerError::InvalidRequest(format!(
                    "{OCI_WHPX_ENDPOINT_ENV} must contain UTF-8 text"
                ))
            })?,
            None => super::oci_whpx_owner::owned_pipe_name(&service_root)?,
        };
        return WindowsWhpxOciMigrationConfig::new(runtime_root, endpoint)?
            .with_box_owned_owner(service_root, service_bin, service_shim, service_vm_rootfs)
            .map(Some);
    }

    let endpoint = endpoint
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ExecutionManagerError::InvalidRequest(format!(
            "{OCI_WHPX_ENDPOINT_ENV} must be set explicitly for the qualification-only WHPX service"
        ))
        })?
        .into_string()
        .map_err(|_| {
            ExecutionManagerError::InvalidRequest(format!(
                "{OCI_WHPX_ENDPOINT_ENV} must contain UTF-8 text"
            ))
        })?;
    WindowsWhpxOciMigrationConfig::new(runtime_root, endpoint).map(Some)
}

#[derive(Default)]
struct WindowsWhpxEnvironmentInputs {
    mode: Option<OsString>,
    runtime_root: Option<OsString>,
    endpoint: Option<OsString>,
    box_owned: Option<OsString>,
    service_root: Option<OsString>,
    service_bin: Option<OsString>,
    service_shim: Option<OsString>,
    service_vm_rootfs: Option<OsString>,
}

#[derive(Default)]
struct LinuxKvmEnvironmentInputs {
    mode: Option<OsString>,
    runtime_root: Option<OsString>,
    endpoint: Option<OsString>,
    box_owned: Option<OsString>,
    service_root: Option<OsString>,
    service_bin: Option<OsString>,
    service_shim: Option<OsString>,
    service_manifest: Option<OsString>,
}

fn parse_linux_kvm_environment(
    inputs: LinuxKvmEnvironmentInputs,
    home_dir: &Path,
) -> ExecutionManagerResult<Option<LinuxKvmOciMigrationConfig>> {
    let LinuxKvmEnvironmentInputs {
        mode,
        runtime_root,
        endpoint,
        box_owned,
        service_root,
        service_bin,
        service_shim,
        service_manifest,
    } = inputs;
    let Some(mode) = mode.filter(|value| !value.is_empty()) else {
        return Ok(None);
    };
    let mode = mode.to_str().ok_or_else(|| {
        ExecutionManagerError::InvalidRequest(format!(
            "{OCI_MIGRATION_ENV} must contain UTF-8 text"
        ))
    })?;
    match mode.trim().to_ascii_lowercase().as_str() {
        "" | "0" | "false" | "off" | "disabled" | "legacy" | "1" | "true" | "on" | "sandbox"
        | "sandbox-via-oci" => return Ok(None),
        "all" | "all-via-oci" | "microvm" | "microvm-via-oci" | "kvm" => {}
        value => {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "unsupported {OCI_MIGRATION_ENV} value {value:?}; expected off, sandbox, microvm, or all"
            )))
        }
    }

    let host_root_override = runtime_root
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let box_owned = parse_explicit_bool(OCI_KVM_BOX_OWNED_ENV, box_owned)?;

    if box_owned {
        return parse_linux_kvm_explicit_box_owned(
            home_dir,
            host_root_override,
            endpoint,
            service_root,
            service_bin,
            service_shim,
            service_manifest,
        )
        .map(Some);
    }

    if let Some(endpoint) = endpoint.filter(|value| !value.is_empty()) {
        let runtime_root = host_root_override.unwrap_or_else(|| default_service_root(home_dir));
        return LinuxKvmOciMigrationConfig::new(runtime_root, PathBuf::from(endpoint)).map(Some);
    }

    // Gate 1+2: opt-in microvm|all without qualification endpoint → packaged
    // Box-owned Host. Default omit-isolation stays Box-libkrun until gate 5.
    #[cfg(target_os = "linux")]
    {
        return parse_linux_kvm_packaged_box_owned(
            home_dir,
            host_root_override,
            service_root,
            service_bin,
            service_shim,
            service_manifest,
        )
        .map(Some);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (service_root, service_bin, service_shim, service_manifest);
        Err(ExecutionManagerError::InvalidRequest(format!(
            "{OCI_KVM_ENDPOINT_ENV} must be set explicitly for the qualification-only KVM service"
        )))
    }
}

fn parse_linux_kvm_explicit_box_owned(
    _home_dir: &Path,
    host_root_override: Option<PathBuf>,
    endpoint: Option<OsString>,
    service_root: Option<OsString>,
    service_bin: Option<OsString>,
    service_shim: Option<OsString>,
    service_manifest: Option<OsString>,
) -> ExecutionManagerResult<LinuxKvmOciMigrationConfig> {
    let service_root = service_root
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .ok_or_else(|| {
            ExecutionManagerError::InvalidRequest(format!(
                "{OCI_KVM_SERVICE_ROOT_ENV} must be set when {OCI_KVM_BOX_OWNED_ENV} is enabled"
            ))
        })?;
    let (service_bin, service_shim, service_manifest) =
        resolve_linux_kvm_owner_artifacts(service_bin, service_shim, service_manifest)?;
    let runtime_root = host_root_override.unwrap_or_else(|| service_root.join("runtime"));
    let endpoint = endpoint
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| service_root.join("runtime.sock"));
    LinuxKvmOciMigrationConfig::new(runtime_root, endpoint)?.with_box_owned_owner(
        service_root,
        service_bin,
        service_shim,
        service_manifest,
    )
}

#[cfg(target_os = "linux")]
fn parse_linux_kvm_packaged_box_owned(
    home_dir: &Path,
    host_root_override: Option<PathBuf>,
    service_root: Option<OsString>,
    service_bin: Option<OsString>,
    service_shim: Option<OsString>,
    service_manifest: Option<OsString>,
) -> ExecutionManagerResult<LinuxKvmOciMigrationConfig> {
    let (service_bin, service_shim, service_manifest) =
        resolve_linux_kvm_owner_artifacts(service_bin, service_shim, service_manifest)?;
    let service_root = service_root
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| default_service_root(home_dir));
    let runtime_root = host_root_override.unwrap_or_else(|| service_root.join("runtime"));
    let endpoint = service_root.join("runtime.sock");
    LinuxKvmOciMigrationConfig::new(runtime_root, endpoint)?.with_box_owned_owner(
        service_root,
        service_bin,
        service_shim,
        service_manifest,
    )
}

fn resolve_linux_kvm_owner_artifacts(
    service_bin: Option<OsString>,
    service_shim: Option<OsString>,
    service_manifest: Option<OsString>,
) -> ExecutionManagerResult<(PathBuf, PathBuf, PathBuf)> {
    let override_bin = service_bin
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let override_shim = service_shim
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);
    let override_manifest = service_manifest
        .filter(|value| !value.is_empty())
        .map(PathBuf::from);

    #[cfg(target_os = "linux")]
    {
        if override_bin.is_some() && override_shim.is_some() && override_manifest.is_some() {
            return Ok((
                override_bin.expect("checked"),
                override_shim.expect("checked"),
                override_manifest.expect("checked"),
            ));
        }
        let discovered = super::oci_kvm_packaged::discover_packaged_linux_kvm_artifacts(
            super::oci_kvm_packaged::PackagedLinuxKvmOverrides {
                runtime_path: override_bin,
                shim_path: override_shim,
                system_image_manifest: override_manifest,
            },
        )?;
        Ok((
            discovered.runtime_path,
            discovered.shim_path,
            discovered.system_image_manifest,
        ))
    }

    #[cfg(not(target_os = "linux"))]
    {
        let service_bin = override_bin.ok_or_else(|| {
            ExecutionManagerError::InvalidRequest(format!(
                "{OCI_KVM_SERVICE_BIN_ENV} must be set when {OCI_KVM_BOX_OWNED_ENV} is enabled"
            ))
        })?;
        let service_shim = override_shim.ok_or_else(|| {
            ExecutionManagerError::InvalidRequest(format!(
                "{OCI_KVM_SERVICE_SHIM_ENV} must be set when {OCI_KVM_BOX_OWNED_ENV} is enabled"
            ))
        })?;
        let service_manifest = override_manifest.ok_or_else(|| {
            ExecutionManagerError::InvalidRequest(format!(
                "{OCI_KVM_SERVICE_MANIFEST_ENV} must be set when {OCI_KVM_BOX_OWNED_ENV} is enabled"
            ))
        })?;
        Ok((service_bin, service_shim, service_manifest))
    }
}

fn default_service_root(home_dir: &Path) -> PathBuf {
    #[cfg(target_os = "linux")]
    {
        use std::os::unix::ffi::OsStrExt as _;

        // Keep runtime.sock below the small sockaddr_un limit while separating
        // different A3S homes owned by the same UID.
        let digest = Sha256::digest(home_dir.as_os_str().as_bytes());
        // SAFETY: geteuid has no preconditions or failure result.
        let uid = unsafe { libc::geteuid() };
        std::env::temp_dir().join(format!("a3s-box-oci-{uid}-{}", hex::encode(&digest[..6])))
    }

    #[cfg(not(target_os = "linux"))]
    home_dir.join("run").join("oci-host")
}

fn validate_absolute_normalized(path: &Path, label: &str) -> ExecutionManagerResult<()> {
    if !path.is_absolute()
        || path.parent().is_none()
        || path
            .components()
            .any(|component| matches!(component, Component::CurDir | Component::ParentDir))
    {
        return Err(ExecutionManagerError::InvalidRequest(format!(
            "{label} must be an absolute normalized non-root path: {}",
            path.display()
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn absolute(name: &str) -> PathBuf {
        std::env::temp_dir().join(name)
    }

    #[test]
    fn environment_defaults_on_linux_and_rejects_unqualified_all_policy() {
        let home = absolute("a3s-oci-config-home");
        #[cfg(target_os = "linux")]
        {
            let (selection, config) = parse_environment(None, None, None, None, &home)
                .unwrap()
                .unwrap();
            assert_eq!(selection, NativeLinuxMigrationSelection::DefaultSandbox);
            assert!(config.runtime_path().is_none());
        }
        #[cfg(not(target_os = "linux"))]
        {
            assert_eq!(
                parse_environment(None, None, None, None, &home).unwrap(),
                None
            );
        }
        assert_eq!(
            parse_environment(Some(OsString::from("off")), None, None, None, &home).unwrap(),
            None
        );
        assert!(parse_environment(Some(OsString::from("all")), None, None, None, &home).is_err());
    }

    #[test]
    fn environment_requires_artifact_pair_and_accepts_sandbox() {
        let home = absolute("a3s-oci-config-home");
        let runtime = absolute("a3s-oci");
        let agent = absolute("a3s-oci-agent");
        assert!(parse_environment(
            Some(OsString::from("sandbox")),
            None,
            Some(runtime.clone().into_os_string()),
            None,
            &home
        )
        .is_err());
        let (selection, config) = parse_environment(
            Some(OsString::from("sandbox")),
            Some(absolute("a3s-oci-root").into_os_string()),
            Some(runtime.clone().into_os_string()),
            Some(agent.clone().into_os_string()),
            &home,
        )
        .unwrap()
        .unwrap();
        assert_eq!(selection, NativeLinuxMigrationSelection::ExplicitSandbox);
        assert_eq!(config.runtime_path(), Some(runtime.as_path()));
        assert_eq!(config.agent_path(), Some(agent.as_path()));
    }

    #[test]
    fn windows_environment_requires_explicit_pipe_and_accepts_microvm() {
        let home = absolute("a3s-oci-config-home");
        assert!(parse_windows_environment(
            WindowsWhpxEnvironmentInputs {
                mode: Some(OsString::from("all")),
                ..Default::default()
            },
            &home,
        )
        .is_err());

        let config = parse_windows_environment(
            WindowsWhpxEnvironmentInputs {
                mode: Some(OsString::from("microvm")),
                runtime_root: Some(absolute("a3s-oci-whpx-root").into_os_string()),
                endpoint: Some(OsString::from(DEFAULT_OCI_WHPX_ENDPOINT)),
                ..Default::default()
            },
            &home,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            config.endpoint(),
            &crate::local_execution::OciRuntimeEndpoint::windows_named_pipe(
                DEFAULT_OCI_WHPX_ENDPOINT,
            )
            .unwrap()
        );
        assert!(config.box_owned_owner().is_none());
    }

    #[test]
    fn windows_box_owned_environment_derives_pipe_from_service_root() {
        let home = absolute("a3s-oci-config-home");
        let service_root = absolute("a3s-whpx-service");
        let expected =
            super::super::oci_whpx_owner::owned_pipe_name(&service_root).expect("derive pipe");
        let config = parse_windows_environment(
            WindowsWhpxEnvironmentInputs {
                mode: Some(OsString::from("microvm")),
                runtime_root: Some(absolute("a3s-oci-whpx-root").into_os_string()),
                box_owned: Some(OsString::from("1")),
                service_root: Some(service_root.clone().into_os_string()),
                service_bin: Some(absolute("a3s-oci.exe").into_os_string()),
                service_shim: Some(absolute("a3s-oci-krun-shim.exe").into_os_string()),
                service_vm_rootfs: Some(absolute("system").into_os_string()),
                ..Default::default()
            },
            &home,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            config.endpoint(),
            &crate::local_execution::OciRuntimeEndpoint::windows_named_pipe(expected).unwrap()
        );
        let owner = config.box_owned_owner().expect("box-owned owner");
        assert_eq!(owner.service_root(), service_root.as_path());
    }

    #[test]
    fn linux_kvm_environment_requires_explicit_socket_and_accepts_microvm() {
        let home = absolute("a3s-oci-config-home");
        let endpoint = absolute("a3s-oci-kvm-box-runtime.sock");
        assert!(parse_linux_kvm_environment(
            LinuxKvmEnvironmentInputs {
                mode: Some(OsString::from("microvm")),
                ..Default::default()
            },
            &home
        )
        .is_err());
        assert_eq!(
            parse_linux_kvm_environment(
                LinuxKvmEnvironmentInputs {
                    mode: Some(OsString::from("sandbox")),
                    ..Default::default()
                },
                &home
            )
            .unwrap(),
            None
        );

        let config = parse_linux_kvm_environment(
            LinuxKvmEnvironmentInputs {
                mode: Some(OsString::from("microvm")),
                runtime_root: Some(absolute("a3s-oci-kvm-runtime").into_os_string()),
                endpoint: Some(endpoint.clone().into_os_string()),
                ..Default::default()
            },
            &home,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            config.endpoint(),
            &crate::local_execution::OciRuntimeEndpoint::unix_socket(&endpoint).unwrap()
        );
        assert_eq!(
            config.runtime_root(),
            absolute("a3s-oci-kvm-runtime").as_path()
        );
        assert!(config.box_owned_owner().is_none());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_kvm_packaged_opt_in_builds_box_owned_without_endpoint() {
        let home = absolute("a3s-oci-kvm-packaged-home");
        let service_root = absolute("a3s-oci-kvm-packaged-service");
        let runtime = absolute("a3s-oci-packaged-runtime-bin");
        let shim = absolute("a3s-oci-packaged-shim-bin");
        let manifest = absolute("a3s-oci-packaged-system-image.json");

        let config = parse_linux_kvm_environment(
            LinuxKvmEnvironmentInputs {
                mode: Some(OsString::from("microvm")),
                service_root: Some(service_root.clone().into_os_string()),
                service_bin: Some(runtime.clone().into_os_string()),
                service_shim: Some(shim.clone().into_os_string()),
                service_manifest: Some(manifest.clone().into_os_string()),
                ..Default::default()
            },
            &home,
        )
        .unwrap()
        .unwrap();

        assert_eq!(
            config.endpoint(),
            &crate::local_execution::OciRuntimeEndpoint::unix_socket(
                service_root.join("runtime.sock")
            )
            .unwrap()
        );
        assert_eq!(
            config.runtime_root(),
            service_root.join("runtime").as_path()
        );
        let owner = config
            .box_owned_owner()
            .expect("packaged path is Box-owned");
        assert_eq!(owner.service_root(), service_root.as_path());
        assert_eq!(owner.runtime_path(), runtime.as_path());
        assert_eq!(owner.shim_path(), shim.as_path());
        assert_eq!(owner.system_image_manifest(), manifest.as_path());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn linux_kvm_packaged_opt_in_all_mode_matches_microvm() {
        let home = absolute("a3s-oci-kvm-packaged-all-home");
        let service_root = absolute("a3s-oci-kvm-packaged-all-service");
        let config = parse_linux_kvm_environment(
            LinuxKvmEnvironmentInputs {
                mode: Some(OsString::from("all")),
                service_root: Some(service_root.clone().into_os_string()),
                service_bin: Some(absolute("a3s-oci-all").into_os_string()),
                service_shim: Some(absolute("a3s-oci-krun-shim-all").into_os_string()),
                service_manifest: Some(absolute("system-image-all.json").into_os_string()),
                ..Default::default()
            },
            &home,
        )
        .unwrap()
        .unwrap();
        assert!(config.box_owned_owner().is_some());
        assert_eq!(
            config.endpoint(),
            &crate::local_execution::OciRuntimeEndpoint::unix_socket(
                service_root.join("runtime.sock")
            )
            .unwrap()
        );
    }

    #[test]
    fn linux_kvm_box_owned_requires_service_artifacts_and_fences_endpoint() {
        let home = absolute("a3s-oci-config-home");
        let service_root = absolute("a3s-oci-kvm-service");
        let endpoint = service_root.join("runtime.sock");
        assert!(parse_linux_kvm_environment(
            LinuxKvmEnvironmentInputs {
                mode: Some(OsString::from("microvm")),
                runtime_root: Some(absolute("a3s-oci-kvm-runtime").into_os_string()),
                endpoint: Some(endpoint.clone().into_os_string()),
                box_owned: Some(OsString::from("1")),
                service_root: Some(service_root.clone().into_os_string()),
                ..Default::default()
            },
            &home,
        )
        .is_err());

        let config = parse_linux_kvm_environment(
            LinuxKvmEnvironmentInputs {
                mode: Some(OsString::from("kvm")),
                runtime_root: Some(absolute("a3s-oci-kvm-runtime").into_os_string()),
                box_owned: Some(OsString::from("true")),
                service_root: Some(service_root.clone().into_os_string()),
                service_bin: Some(absolute("a3s-oci").into_os_string()),
                service_shim: Some(absolute("a3s-oci-kvm-shim").into_os_string()),
                service_manifest: Some(absolute("system-image.json").into_os_string()),
                ..Default::default()
            },
            &home,
        )
        .unwrap()
        .unwrap();
        assert_eq!(
            config.endpoint(),
            &crate::local_execution::OciRuntimeEndpoint::unix_socket(&endpoint).unwrap()
        );
        let owner = config.box_owned_owner().expect("box-owned owner");
        assert_eq!(owner.service_root(), service_root.as_path());
        assert_eq!(owner.runtime_path(), absolute("a3s-oci").as_path());
        assert_eq!(owner.shim_path(), absolute("a3s-oci-kvm-shim").as_path());
        assert_eq!(
            owner.system_image_manifest(),
            absolute("system-image.json").as_path()
        );
    }

    #[test]
    fn kvm_box_owned_flag_rejects_unknown_values() {
        let home = absolute("a3s-oci-kvm-flag-home");
        let error = parse_linux_kvm_environment(
            LinuxKvmEnvironmentInputs {
                mode: Some(OsString::from("microvm")),
                endpoint: Some(absolute("runtime.sock").into_os_string()),
                box_owned: Some(OsString::from("maybe")),
                ..Default::default()
            },
            &home,
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains(OCI_KVM_BOX_OWNED_ENV),
            "unexpected error: {message}"
        );
        assert!(message.contains("maybe"), "unexpected error: {message}");
    }
}
