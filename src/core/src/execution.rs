//! Backend-neutral execution isolation resolution.

use serde::{Deserialize, Serialize};

use crate::config::{BoxConfig, ExecutionIsolation, TeeConfig};
use crate::error::{BoxError, Result};
use crate::network::NetworkMode;

/// Concrete backend selected for an execution request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum ExecutionBackend {
    /// libkrun-backed MicroVM execution.
    Krun,
    /// OCI execution through the certified crun runtime.
    Crun,
}

/// Security boundary provided by the resolved backend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum IsolationClass {
    /// A hardware-backed virtual-machine boundary.
    HardwareVm,
    /// Linux namespaces and controls sharing the host kernel.
    SharedKernel,
}

/// Deterministic result of resolving one execution request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResolvedExecutionPlan {
    /// Isolation requested by the caller or selected by the implicit default.
    pub requested_isolation: ExecutionIsolation,
    /// Concrete runtime backend.
    pub backend: ExecutionBackend,
    /// Effective security-boundary class.
    pub isolation_class: IsolationClass,
    /// Controls that the selected backend must prove before launch.
    pub required_controls: Vec<String>,
}

const SANDBOX_REQUIRED_CONTROLS: &[&str] = &[
    "user-namespace",
    "mount-namespace",
    "pid-namespace",
    "ipc-namespace",
    "uts-namespace",
    "network-namespace",
    "seccomp",
    "capability-bounding-set",
    "no-new-privileges",
    "cgroup-v2",
];

const SANDBOX_ALLOWED_ADDED_CAPABILITIES: &[&str] = &[
    "AUDIT_WRITE",
    "CHOWN",
    "DAC_OVERRIDE",
    "FOWNER",
    "FSETID",
    "KILL",
    "MKNOD",
    "NET_BIND_SERVICE",
    "SETFCAP",
    "SETGID",
    "SETPCAP",
    "SETUID",
    "SYS_CHROOT",
];

/// Resolve a box configuration without probing or mutating the host.
///
/// Host capabilities are checked separately immediately before preparation.
/// Keeping this function pure makes unsupported feature combinations fail
/// before image pulls, rootfs mounts, state changes, or runtime processes.
pub fn resolve_execution(config: &BoxConfig) -> Result<ResolvedExecutionPlan> {
    match config.isolation {
        ExecutionIsolation::Microvm => Ok(ResolvedExecutionPlan {
            requested_isolation: ExecutionIsolation::Microvm,
            backend: ExecutionBackend::Krun,
            isolation_class: IsolationClass::HardwareVm,
            required_controls: Vec::new(),
        }),
        ExecutionIsolation::Sandbox => {
            validate_sandbox_compatibility(config)?;
            Ok(ResolvedExecutionPlan {
                requested_isolation: ExecutionIsolation::Sandbox,
                backend: ExecutionBackend::Crun,
                isolation_class: IsolationClass::SharedKernel,
                required_controls: SANDBOX_REQUIRED_CONTROLS
                    .iter()
                    .map(|control| (*control).to_string())
                    .collect(),
            })
        }
    }
}

/// Validate features that cannot be represented safely by the sandbox MVP.
pub fn validate_sandbox_compatibility(config: &BoxConfig) -> Result<()> {
    if !config.isolation.is_sandbox() {
        return Ok(());
    }

    let mut unsupported = Vec::new();

    if !matches!(config.tee, TeeConfig::None) {
        unsupported.push("TEE and attestation");
    }
    if config.pool.enabled || config.pool.snapshot_fork {
        unsupported.push("warm pools and snapshot-fork");
    }
    if config.deferred_main {
        unsupported.push("deferred main execution");
    }
    if config.ksm {
        unsupported.push("KSM");
    }
    if config.snapshot_mem_file.is_some()
        || config.snapshot_sock.is_some()
        || config.restore_from.is_some()
    {
        unsupported.push("VM snapshots and restore");
    }
    if config.privileged {
        unsupported.push("privileged mode");
    }
    if config.sidecar.is_some() {
        unsupported.push("vsock sidecars");
    }
    if !config.port_map.is_empty() {
        unsupported.push("published ports");
    }
    if matches!(config.network, NetworkMode::Bridge { .. }) {
        unsupported.push("named bridge networking");
    }
    if !config.sysctls.is_empty() {
        unsupported.push("custom sysctls");
    }
    if config
        .security_opt
        .iter()
        .any(|option| option.trim().eq_ignore_ascii_case("seccomp=unconfined"))
    {
        unsupported.push("unconfined seccomp");
    }

    let disallowed_capabilities: Vec<String> = config
        .cap_add
        .iter()
        .map(|capability| normalize_capability(capability))
        .filter(|capability| !SANDBOX_ALLOWED_ADDED_CAPABILITIES.contains(&capability.as_str()))
        .collect();
    if !disallowed_capabilities.is_empty() {
        return Err(BoxError::ConfigError(format!(
            "sandbox isolation rejects added capabilities outside its allowlist: {}",
            disallowed_capabilities.join(", ")
        )));
    }

    if unsupported.is_empty() {
        Ok(())
    } else {
        Err(BoxError::ConfigError(format!(
            "sandbox isolation does not support: {}",
            unsupported.join(", ")
        )))
    }
}

fn normalize_capability(capability: &str) -> String {
    let normalized = capability.trim().to_ascii_uppercase();
    normalized
        .strip_prefix("CAP_")
        .unwrap_or(&normalized)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{PoolConfig, SidecarConfig};

    fn sandbox_config() -> BoxConfig {
        BoxConfig {
            isolation: ExecutionIsolation::Sandbox,
            ..Default::default()
        }
    }

    #[test]
    fn default_resolves_only_to_krun_hardware_vm() {
        let plan = resolve_execution(&BoxConfig::default()).unwrap();
        assert_eq!(plan.backend, ExecutionBackend::Krun);
        assert_eq!(plan.isolation_class, IsolationClass::HardwareVm);
        assert!(plan.required_controls.is_empty());
    }

    #[test]
    fn sandbox_resolves_to_crun_shared_kernel_with_mandatory_controls() {
        let plan = resolve_execution(&sandbox_config()).unwrap();
        assert_eq!(plan.backend, ExecutionBackend::Crun);
        assert_eq!(plan.isolation_class, IsolationClass::SharedKernel);
        for required in SANDBOX_REQUIRED_CONTROLS {
            assert!(plan.required_controls.iter().any(|value| value == required));
        }
    }

    #[test]
    fn sandbox_rejects_vm_only_features_together() {
        let config = BoxConfig {
            isolation: ExecutionIsolation::Sandbox,
            tee: TeeConfig::Tdx {
                workload_id: "test".to_string(),
                simulate: true,
            },
            pool: PoolConfig {
                enabled: true,
                ..Default::default()
            },
            sidecar: Some(SidecarConfig::default()),
            port_map: vec!["8080:80".to_string()],
            privileged: true,
            ..Default::default()
        };

        let error = resolve_execution(&config).unwrap_err().to_string();
        assert!(error.contains("TEE and attestation"));
        assert!(error.contains("warm pools"));
        assert!(error.contains("vsock sidecars"));
        assert!(error.contains("published ports"));
        assert!(error.contains("privileged mode"));
    }

    #[test]
    fn sandbox_rejects_unconfined_seccomp() {
        let config = BoxConfig {
            security_opt: vec!["seccomp=unconfined".to_string()],
            ..sandbox_config()
        };
        assert!(resolve_execution(&config)
            .unwrap_err()
            .to_string()
            .contains("unconfined seccomp"));
    }

    #[test]
    fn sandbox_normalizes_and_allows_baseline_capabilities() {
        let config = BoxConfig {
            cap_add: vec!["cap_chown".to_string(), "NET_BIND_SERVICE".to_string()],
            ..sandbox_config()
        };
        assert!(resolve_execution(&config).is_ok());
    }

    #[test]
    fn sandbox_rejects_powerful_added_capability() {
        let config = BoxConfig {
            cap_add: vec!["CAP_SYS_ADMIN".to_string()],
            ..sandbox_config()
        };
        let error = resolve_execution(&config).unwrap_err().to_string();
        assert!(error.contains("SYS_ADMIN"));
    }
}
