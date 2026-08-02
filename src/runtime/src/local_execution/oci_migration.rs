//! Explicit production composition for Sandbox migration to A3S OCI Runtime.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

#[cfg(target_os = "linux")]
use a3s_box_core::ExecutionBackend;
use a3s_box_core::{ExecutionManagerError, ExecutionManagerResult};
#[cfg(target_os = "linux")]
use sha2::{Digest, Sha256};

use super::LocalExecutionManager;
#[cfg(target_os = "linux")]
use super::{NativeLinuxOciBundleProvider, OciLocalExecutionBackend, OciMigrationPolicy};

pub const OCI_MIGRATION_ENV: &str = "A3S_BOX_OCI_MIGRATION";
pub const OCI_HOST_ROOT_ENV: &str = "A3S_BOX_OCI_HOST_ROOT";
pub const OCI_RUNTIME_PATH_ENV: &str = "A3S_BOX_OCI_RUNTIME_PATH";
pub const OCI_AGENT_PATH_ENV: &str = "A3S_BOX_OCI_AGENT_PATH";

/// Explicit native-Linux owner and artifact selection for Sandbox migration.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct NativeLinuxOciMigrationConfig {
    service_root: PathBuf,
    runtime_path: Option<PathBuf>,
    agent_path: Option<PathBuf>,
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

    /// Parse the process-wide opt-in. An absent/off value preserves the
    /// existing VM backend without probing or starting an OCI owner.
    pub fn from_environment(home_dir: &Path) -> ExecutionManagerResult<Option<Self>> {
        parse_environment(
            std::env::var_os(OCI_MIGRATION_ENV),
            std::env::var_os(OCI_HOST_ROOT_ENV),
            std::env::var_os(OCI_RUNTIME_PATH_ENV),
            std::env::var_os(OCI_AGENT_PATH_ENV),
            home_dir,
        )
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
                    "native Linux OCI migration preflight returned no runtime artifacts"
                        .to_string(),
                )
            })?;
            let endpoint =
                super::oci_owner::ensure_native_linux_oci_owner(config.service_root(), artifacts)
                    .await?;
            let mut provider = NativeLinuxOciBundleProvider::new(
                home_dir.clone(),
                artifacts.runtime_path.clone(),
                artifacts.agent_path.clone(),
            );
            if let Some(progress) = pull_progress_fn.as_ref() {
                provider = provider.with_pull_progress_fn(progress.clone());
            }
            let provider = Arc::new(provider);
            let oci = Arc::new(OciLocalExecutionBackend::connect(endpoint, provider).await?);
            return Ok(Self::with_oci_migration_backend_and_pull_progress(
                state_path,
                home_dir,
                oci,
                OciMigrationPolicy::SandboxViaOci,
                pull_progress_fn,
            ));
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = (state_path, home_dir, config, pull_progress_fn);
            Err(ExecutionManagerError::Unavailable(
                "native Linux OCI migration is supported only on Linux".to_string(),
            ))
        }
    }

    /// Select the production migration composition only when explicitly opted
    /// in through `A3S_BOX_OCI_MIGRATION=sandbox`.
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
            None => {
                let mut backend = crate::local_execution::VmLocalExecutionBackend::new(&home_dir);
                if let Some(progress) = pull_progress_fn {
                    backend = backend.with_pull_progress_fn(progress);
                }
                Ok(Self::new(state_path, home_dir, Arc::new(backend)))
            }
        }
    }
}

fn parse_environment(
    mode: Option<OsString>,
    service_root: Option<OsString>,
    runtime_path: Option<OsString>,
    agent_path: Option<OsString>,
    home_dir: &Path,
) -> ExecutionManagerResult<Option<NativeLinuxOciMigrationConfig>> {
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
        "1" | "true" | "on" | "sandbox" | "sandbox-via-oci" => {}
        "all" | "all-via-oci" => {
            return Err(ExecutionManagerError::InvalidRequest(
                "all-via-OCI migration is not qualified yet; use sandbox".to_string(),
            ))
        }
        value => {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "unsupported {OCI_MIGRATION_ENV} value {value:?}; expected off or sandbox"
            )))
        }
    }

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
    Ok(Some(config))
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
        return std::env::temp_dir()
            .join(format!("a3s-box-oci-{uid}-{}", hex::encode(&digest[..6])));
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
    fn environment_is_opt_in_and_rejects_unqualified_all_policy() {
        let home = absolute("a3s-oci-config-home");
        assert_eq!(
            parse_environment(None, None, None, None, &home).unwrap(),
            None
        );
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
        let config = parse_environment(
            Some(OsString::from("sandbox")),
            Some(absolute("a3s-oci-root").into_os_string()),
            Some(runtime.clone().into_os_string()),
            Some(agent.clone().into_os_string()),
            &home,
        )
        .unwrap()
        .unwrap();
        assert_eq!(config.runtime_path(), Some(runtime.as_path()));
        assert_eq!(config.agent_path(), Some(agent.as_path()));
    }
}
