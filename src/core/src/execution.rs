//! Backend-neutral execution isolation resolution.

#[cfg(unix)]
use std::net::IpAddr;

use serde::{Deserialize, Serialize};

use crate::config::{BoxConfig, ExecutionIsolation, TeeConfig};
use crate::error::{BoxError, Result};
use crate::host_mount_policy::ResolvedHostMount;
use crate::network::NetworkMode;
#[cfg(unix)]
use crate::security_policy::EgressProtocol;
use crate::security_policy::{EgressPolicy, HostMountPolicyMode, ResolvedSandboxSecurityPolicy};

/// Concrete backend selected for an execution request.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum ExecutionBackend {
    /// libkrun-backed MicroVM execution.
    Krun,
    /// Shared-kernel execution through A3S OCI Runtime.
    A3sOci,
}

impl ExecutionBackend {
    /// Whether this backend provides shared-kernel Sandbox execution.
    pub const fn is_sandbox(self) -> bool {
        matches!(self, Self::A3sOci)
    }
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
    /// Canonical optional policy bound to this execution.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_policy: Option<ResolvedSandboxSecurityPolicy>,
    /// SHA-256 digest of the canonical optional policy.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub security_policy_digest: Option<String>,
    /// Canonical host binds and their planning-time policy evidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub host_mounts: Vec<ResolvedHostMount>,
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
    resolve_execution_internal(config, None)
}

/// Resolve an execution after the runtime has classified every host bind.
///
/// Supplying this evidence is distinct from an empty list: an enabled host
/// mount policy with no external binds still requires the runtime to prove it
/// completed mount provenance resolution.
pub fn resolve_execution_with_host_mounts(
    config: &BoxConfig,
    host_mounts: Vec<ResolvedHostMount>,
) -> Result<ResolvedExecutionPlan> {
    resolve_execution_internal(config, Some(host_mounts))
}

fn resolve_execution_internal(
    config: &BoxConfig,
    host_mounts: Option<Vec<ResolvedHostMount>>,
) -> Result<ResolvedExecutionPlan> {
    let host_mount_evidence_supplied = host_mounts.is_some();
    let host_mounts = host_mounts.unwrap_or_default();
    let security_policy = config
        .security_policy
        .as_ref()
        .map(|policy| policy.resolve())
        .transpose()?;
    let security_policy_digest = security_policy
        .as_ref()
        .map(ResolvedSandboxSecurityPolicy::digest)
        .transpose()?;
    if security_policy.is_some() && config.pool.enabled {
        return Err(BoxError::ConfigError(
            "optional security policies are not supported by warm-pool templates until per-lease policy state is reset and revalidated"
                .to_string(),
        ));
    }

    let plan = match config.isolation {
        ExecutionIsolation::Microvm => {
            validate_microvm_compatibility(config)?;
            ResolvedExecutionPlan {
                requested_isolation: ExecutionIsolation::Microvm,
                backend: ExecutionBackend::Krun,
                isolation_class: IsolationClass::HardwareVm,
                required_controls: Vec::new(),
                security_policy,
                security_policy_digest,
                host_mounts,
            }
        }
        ExecutionIsolation::Sandbox => {
            validate_sandbox_compatibility(config)?;
            ResolvedExecutionPlan {
                requested_isolation: ExecutionIsolation::Sandbox,
                backend: ExecutionBackend::A3sOci,
                isolation_class: IsolationClass::SharedKernel,
                required_controls: SANDBOX_REQUIRED_CONTROLS
                    .iter()
                    .map(|control| (*control).to_string())
                    .collect(),
                security_policy,
                security_policy_digest,
                host_mounts,
            }
        }
    };

    validate_optional_security_policy_support(plan, host_mount_evidence_supplied, config)
}

/// Reject policy controls until the selected backend has a complete
/// enforcement implementation. Explicit unrestricted egress is safe to
/// persist because it does not claim an additional runtime control.
fn validate_optional_security_policy_support(
    mut plan: ResolvedExecutionPlan,
    host_mount_evidence_supplied: bool,
    config: &BoxConfig,
) -> Result<ResolvedExecutionPlan> {
    let Some(policy) = &plan.security_policy else {
        if host_mount_evidence_supplied || !plan.host_mounts.is_empty() {
            return Err(BoxError::ConfigError(
                "host mount evidence requires an enabled host mount policy".to_string(),
            ));
        }
        return Ok(plan);
    };

    let mut unsupported = Vec::new();
    if policy.host_mounts.is_some() && !host_mount_evidence_supplied {
        unsupported.push("host mount admission");
    }
    if let Some(egress) = policy
        .egress
        .as_ref()
        .filter(|egress| !matches!(egress, EgressPolicy::Unrestricted))
    {
        match plan.backend {
            ExecutionBackend::Krun => validate_microvm_egress_support(config, egress)?,
            ExecutionBackend::A3sOci => unsupported.push("restricted egress"),
        }
    }
    if !unsupported.is_empty() {
        let backend = match plan.backend {
            ExecutionBackend::Krun => "libkrun MicroVM",
            ExecutionBackend::A3sOci => "A3S OCI Sandbox",
        };
        return Err(BoxError::ConfigError(format!(
            "optional security policy is not implemented by the selected {backend} backend: {}",
            unsupported.join(", ")
        )));
    }

    if let Some(host_policy) = &policy.host_mounts {
        for mount in &plan.host_mounts {
            mount.assessment.ensure_allowed()?;
        }
        plan.required_controls.push(
            match host_policy.mode {
                HostMountPolicyMode::Audit => "host-mount-policy-audit",
                HostMountPolicyMode::Enforce => "host-mount-policy-enforce",
            }
            .to_string(),
        );
    } else if host_mount_evidence_supplied || !plan.host_mounts.is_empty() {
        return Err(BoxError::ConfigError(
            "host mount evidence requires an enabled host mount policy".to_string(),
        ));
    }

    if policy.receipt.is_some() {
        plan.required_controls
            .push("security-receipt-v1".to_string());
    }

    if policy
        .egress
        .as_ref()
        .is_some_and(|egress| !matches!(egress, EgressPolicy::Unrestricted))
    {
        plan.required_controls
            .push("microvm-egress-policy-v1".to_string());
    }

    Ok(plan)
}

fn validate_microvm_egress_support(config: &BoxConfig, policy: &EgressPolicy) -> Result<()> {
    #[cfg(not(unix))]
    {
        let _ = (config, policy);
        return Err(BoxError::ConfigError(
            "restricted MicroVM egress requires a Unix host policy channel".to_string(),
        ));
    }

    #[cfg(unix)]
    {
        if !matches!(config.network, NetworkMode::Tsi) {
            return Err(BoxError::ConfigError(
                "restricted MicroVM egress requires TSI network mode; Bridge and None do not have a qualified enforcement boundary"
                    .to_string(),
            ));
        }
        if !config.port_map.is_empty() {
            return Err(BoxError::ConfigError(
                "restricted MicroVM egress does not support TSI published ports".to_string(),
            ));
        }
        if config.sidecar.is_some() {
            return Err(BoxError::ConfigError(
                "restricted MicroVM egress does not support vsock sidecars".to_string(),
            ));
        }
        if config.pool.enabled || config.pool.snapshot_fork {
            return Err(BoxError::ConfigError(
                "restricted MicroVM egress does not support warm pools or snapshot-fork"
                    .to_string(),
            ));
        }
        if config.snapshot_mem_file.is_some()
            || config.snapshot_sock.is_some()
            || config.restore_from.is_some()
        {
            return Err(BoxError::ConfigError(
                "restricted MicroVM egress does not support VM snapshots or restore".to_string(),
            ));
        }

        if let EgressPolicy::Allowlist(rules) = policy {
            for rule in &rules.ip_rules {
                if rule.protocol != EgressProtocol::Tcp {
                    return Err(BoxError::ConfigError(
                        "restricted MicroVM egress currently supports raw TCP rules only; UDP is rejected"
                            .to_string(),
                    ));
                }
                let address = rule
                    .range
                    .split_once('/')
                    .map_or(rule.range.as_str(), |(address, _)| address)
                    .parse::<IpAddr>()
                    .map_err(|error| {
                        BoxError::ConfigError(format!(
                            "restricted MicroVM egress contains an invalid IP range '{}': {error}",
                            rule.range
                        ))
                    })?;
                if !address.is_ipv4() {
                    return Err(BoxError::ConfigError(
                        "restricted MicroVM egress currently supports raw IPv4 rules only; IPv6 is rejected"
                            .to_string(),
                    ));
                }
            }
        }

        Ok(())
    }
}

/// Validate features that cannot be represented safely by the MicroVM backend.
pub fn validate_microvm_compatibility(config: &BoxConfig) -> Result<()> {
    if config.isolation != ExecutionIsolation::Microvm {
        return Ok(());
    }

    validate_security_options(config, SecurityBackend::Microvm)
}

/// Validate features that cannot be represented safely by the sandbox MVP.
pub fn validate_sandbox_compatibility(config: &BoxConfig) -> Result<()> {
    if !config.isolation.is_sandbox() {
        return Ok(());
    }

    validate_security_options(config, SecurityBackend::Sandbox)?;

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

#[derive(Debug, Clone, Copy)]
enum SecurityBackend {
    Microvm,
    Sandbox,
}

impl SecurityBackend {
    fn label(self) -> &'static str {
        match self {
            Self::Microvm => "microVM",
            Self::Sandbox => "sandbox",
        }
    }
}

fn validate_security_options(config: &BoxConfig, backend: SecurityBackend) -> Result<()> {
    for raw_option in &config.security_opt {
        let option = raw_option.trim();
        if option.is_empty() {
            return Err(BoxError::ConfigError(format!(
                "{} isolation does not accept an empty security option",
                backend.label()
            )));
        }

        if option.eq_ignore_ascii_case("no-new-privileges") {
            continue;
        }

        let Some((key, value)) = option.split_once('=') else {
            return Err(unsupported_security_option(backend, option));
        };
        let key = key.trim();
        let value = value.trim();

        if key.eq_ignore_ascii_case("seccomp") {
            if value.eq_ignore_ascii_case("default") {
                continue;
            }
            if value.eq_ignore_ascii_case("unconfined") {
                if matches!(backend, SecurityBackend::Microvm) {
                    continue;
                }
                return Err(BoxError::ConfigError(
                    "sandbox isolation does not support unconfined seccomp".to_string(),
                ));
            }
            if value.is_empty() {
                return Err(BoxError::ConfigError(format!(
                    "{} isolation requires a seccomp mode",
                    backend.label()
                )));
            }
            return Err(BoxError::ConfigError(format!(
                "{} isolation does not support custom seccomp profile '{}'",
                backend.label(),
                value
            )));
        }

        if key.eq_ignore_ascii_case("no-new-privileges") {
            if value.eq_ignore_ascii_case("true") {
                continue;
            }
            if value.eq_ignore_ascii_case("false") {
                if matches!(backend, SecurityBackend::Microvm) {
                    continue;
                }
                return Err(BoxError::ConfigError(
                    "sandbox isolation requires no-new-privileges and cannot disable it"
                        .to_string(),
                ));
            }
            return Err(BoxError::ConfigError(format!(
                "{} isolation requires no-new-privileges to be true or false, got '{}'",
                backend.label(),
                value
            )));
        }

        if key.eq_ignore_ascii_case("apparmor") {
            return Err(BoxError::ConfigError(format!(
                "{} isolation does not support AppArmor security option '{}'",
                backend.label(),
                option
            )));
        }

        if key.eq_ignore_ascii_case("label") {
            return Err(BoxError::ConfigError(format!(
                "{} isolation does not support SELinux label security option '{}'",
                backend.label(),
                option
            )));
        }

        return Err(unsupported_security_option(backend, option));
    }

    Ok(())
}

fn unsupported_security_option(backend: SecurityBackend, option: &str) -> BoxError {
    BoxError::ConfigError(format!(
        "{} isolation does not support security option '{}'",
        backend.label(),
        option
    ))
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
    use crate::host_mount_policy::{
        HostBindMount, HostMountOutcome, HostMountPolicyEvaluator, ResolvedHostMount,
    };
    use crate::security_policy::{
        EgressHttpRule, EgressIpRule, EgressPolicy, HostMountPolicy, ReceiptPolicy,
        SandboxSecurityPolicy,
    };

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
        assert!(plan.security_policy.is_none());
        assert!(plan.security_policy_digest.is_none());
        assert!(plan.host_mounts.is_empty());

        let value = serde_json::to_value(plan).unwrap();
        assert!(value.get("security_policy").is_none());
        assert!(value.get("security_policy_digest").is_none());
        assert!(value.get("host_mounts").is_none());
    }

    #[test]
    fn legacy_resolved_plan_defaults_without_security_policy() {
        let plan: ResolvedExecutionPlan = serde_json::from_value(serde_json::json!({
            "requested_isolation": "microvm",
            "backend": "krun",
            "isolation_class": "hardware-vm",
            "required_controls": []
        }))
        .unwrap();

        assert!(plan.security_policy.is_none());
        assert!(plan.security_policy_digest.is_none());
        assert!(plan.host_mounts.is_empty());
        assert_eq!(
            serde_json::to_value(plan).unwrap(),
            serde_json::json!({
                "requested_isolation": "microvm",
                "backend": "krun",
                "isolation_class": "hardware-vm",
                "required_controls": []
            })
        );
    }

    #[test]
    fn explicit_unrestricted_policy_is_normalized_and_bound_to_plan() {
        let config = BoxConfig {
            security_policy: Some(SandboxSecurityPolicy::new().egress(EgressPolicy::Unrestricted)),
            ..Default::default()
        };

        let plan = resolve_execution(&config).unwrap();
        let policy = plan.security_policy.as_ref().unwrap();
        assert_eq!(policy.egress, Some(EgressPolicy::Unrestricted));
        assert_eq!(plan.security_policy_digest, Some(policy.digest().unwrap()));
    }

    #[test]
    fn warm_pool_rejects_optional_policy_instead_of_reusing_lease_state() {
        let config = BoxConfig {
            pool: PoolConfig {
                enabled: true,
                ..PoolConfig::default()
            },
            security_policy: Some(SandboxSecurityPolicy::new().egress(EgressPolicy::Unrestricted)),
            ..BoxConfig::default()
        };

        let error = resolve_execution(&config).unwrap_err().to_string();

        assert!(error.contains("warm-pool"));
        assert!(error.contains("per-lease policy state"));
    }

    #[test]
    fn host_mounts_without_runtime_evidence_fail_closed_for_every_backend() {
        for isolation in [ExecutionIsolation::Microvm, ExecutionIsolation::Sandbox] {
            let config = BoxConfig {
                isolation,
                security_policy: Some(
                    SandboxSecurityPolicy::new().host_mounts(HostMountPolicy::agent_safe()),
                ),
                ..Default::default()
            };
            let error = resolve_execution(&config).unwrap_err().to_string();
            assert!(
                error.contains("not implemented"),
                "unexpected error: {error}"
            );
            assert!(
                error.contains("host mount admission"),
                "unexpected error: {error}"
            );
        }
    }

    #[cfg(unix)]
    #[test]
    fn microvm_accepts_only_current_restricted_egress_rule_shapes() {
        let policies = [
            EgressPolicy::DenyAll,
            EgressPolicy::allow_domains(["api.example.com"]),
            EgressPolicy::allowlist(
                [EgressHttpRule::http("updates.example.com")],
                [EgressIpRule::tcp("192.0.2.0/24", 443)],
            ),
        ];

        for egress in policies {
            let config = BoxConfig {
                security_policy: Some(SandboxSecurityPolicy::new().egress(egress)),
                ..BoxConfig::default()
            };
            let plan = resolve_execution(&config).unwrap();
            assert!(plan
                .required_controls
                .contains(&"microvm-egress-policy-v1".to_string()));
        }
    }

    #[test]
    fn sandbox_rejects_restricted_egress_until_its_boundary_exists() {
        let config = BoxConfig {
            isolation: ExecutionIsolation::Sandbox,
            security_policy: Some(SandboxSecurityPolicy::new().egress(EgressPolicy::DenyAll)),
            ..BoxConfig::default()
        };

        let error = resolve_execution(&config).unwrap_err().to_string();

        assert!(
            error.contains("not implemented"),
            "unexpected error: {error}"
        );
        assert!(
            error.contains("restricted egress"),
            "unexpected error: {error}"
        );
    }

    #[cfg(unix)]
    #[test]
    fn restricted_microvm_egress_rejects_networks_without_its_boundary() {
        for network in [
            NetworkMode::None,
            NetworkMode::Bridge {
                network: "backend".to_string(),
            },
        ] {
            let config = BoxConfig {
                network,
                security_policy: Some(SandboxSecurityPolicy::new().egress(EgressPolicy::DenyAll)),
                ..BoxConfig::default()
            };

            let error = resolve_execution(&config).unwrap_err().to_string();
            assert!(error.contains("requires TSI"), "unexpected error: {error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn restricted_microvm_egress_rejects_unimplemented_raw_transports() {
        let policies = [
            (
                EgressPolicy::allowlist(
                    std::iter::empty::<EgressHttpRule>(),
                    [EgressIpRule::udp("192.0.2.1/32", 53)],
                ),
                "UDP",
            ),
            (
                EgressPolicy::allowlist(
                    std::iter::empty::<EgressHttpRule>(),
                    [EgressIpRule::tcp("2001:db8::/32", 443)],
                ),
                "IPv6",
            ),
        ];

        for (egress, expected) in policies {
            let config = BoxConfig {
                security_policy: Some(SandboxSecurityPolicy::new().egress(egress)),
                ..BoxConfig::default()
            };
            let error = resolve_execution(&config).unwrap_err().to_string();
            assert!(error.contains(expected), "unexpected error: {error}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn restricted_microvm_egress_rejects_state_that_cannot_be_generation_fenced() {
        let cases = [
            (
                BoxConfig {
                    port_map: vec!["8080:80".to_string()],
                    ..BoxConfig::default()
                },
                "published ports",
            ),
            (
                BoxConfig {
                    sidecar: Some(SidecarConfig::default()),
                    ..BoxConfig::default()
                },
                "sidecars",
            ),
            (
                BoxConfig {
                    snapshot_mem_file: Some("memory".to_string()),
                    snapshot_sock: Some("snapshot.sock".to_string()),
                    ..BoxConfig::default()
                },
                "snapshots",
            ),
            (
                BoxConfig {
                    pool: PoolConfig {
                        snapshot_fork: true,
                        ..PoolConfig::default()
                    },
                    ..BoxConfig::default()
                },
                "snapshot-fork",
            ),
        ];

        for (mut config, expected) in cases {
            config.security_policy =
                Some(SandboxSecurityPolicy::new().egress(EgressPolicy::DenyAll));
            let error = resolve_execution(&config).unwrap_err().to_string();
            assert!(error.contains(expected), "unexpected error: {error}");
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn restricted_microvm_egress_rejects_hosts_without_policy_channel() {
        let config = BoxConfig {
            security_policy: Some(SandboxSecurityPolicy::new().egress(EgressPolicy::DenyAll)),
            ..BoxConfig::default()
        };

        let error = resolve_execution(&config).unwrap_err().to_string();

        assert!(error.contains("Unix host"), "unexpected error: {error}");
    }

    #[test]
    fn required_receipts_are_bound_to_every_backend_plan() {
        for isolation in [ExecutionIsolation::Microvm, ExecutionIsolation::Sandbox] {
            let config = BoxConfig {
                isolation,
                security_policy: Some(
                    SandboxSecurityPolicy::new().receipt(ReceiptPolicy::Required),
                ),
                ..Default::default()
            };

            let plan = resolve_execution(&config).unwrap();

            assert!(plan
                .required_controls
                .contains(&"security-receipt-v1".to_string()));
            assert_eq!(
                plan.security_policy.as_ref().unwrap().receipt,
                Some(ReceiptPolicy::Required)
            );
        }
    }

    #[test]
    fn classified_host_mounts_are_bound_to_the_resolved_plan() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("home");
        let source = fixture.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        let host_policy = HostMountPolicy::agent_safe();
        let evaluator =
            HostMountPolicyEvaluator::with_home_dir(&host_policy, Some(home.clone())).unwrap();
        let assessment = evaluator.evaluate(&source).unwrap();
        let resolved = ResolvedHostMount::new(
            HostBindMount {
                source: source.clone(),
                destination: "/workspace".to_string(),
                read_only: false,
            },
            assessment,
        )
        .unwrap();
        let config = BoxConfig {
            volumes: vec![format!("{}:/workspace:rw", source.display())],
            security_policy: Some(SandboxSecurityPolicy::new().host_mounts(host_policy)),
            ..Default::default()
        };

        let plan = resolve_execution_with_host_mounts(&config, vec![resolved.clone()]).unwrap();

        assert_eq!(plan.host_mounts, vec![resolved]);
        assert!(plan
            .required_controls
            .contains(&"host-mount-policy-enforce".to_string()));
    }

    #[test]
    fn audit_mount_evidence_is_not_mislabeled_as_enforcement() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("home");
        let source = fixture.path().join("workspace");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::write(source.join(".env"), b"TOKEN=secret").unwrap();
        let host_policy = HostMountPolicy::agent_safe().audit_only();
        let evaluator = HostMountPolicyEvaluator::with_home_dir(&host_policy, Some(home)).unwrap();
        let assessment = evaluator.evaluate(&source).unwrap();
        assert_eq!(assessment.outcome, HostMountOutcome::Audit);
        let resolved = ResolvedHostMount::new(
            HostBindMount {
                source,
                destination: "/workspace".to_string(),
                read_only: true,
            },
            assessment,
        )
        .unwrap();
        let config = BoxConfig {
            security_policy: Some(SandboxSecurityPolicy::new().host_mounts(host_policy)),
            ..Default::default()
        };

        let plan = resolve_execution_with_host_mounts(&config, vec![resolved]).unwrap();

        assert!(plan
            .required_controls
            .contains(&"host-mount-policy-audit".to_string()));
        assert!(!plan
            .required_controls
            .contains(&"host-mount-policy-enforce".to_string()));
    }

    #[test]
    fn host_mount_evidence_without_policy_is_rejected() {
        let error = resolve_execution_with_host_mounts(&BoxConfig::default(), Vec::new())
            .unwrap_err()
            .to_string();
        assert!(error.contains("requires an enabled host mount policy"));
    }

    #[test]
    fn microvm_rejects_host_kernel_and_custom_security_profiles() {
        for (option, expected) in [
            ("apparmor=runtime/default", "AppArmor"),
            ("label=type:container_t", "SELinux"),
            ("seccomp=/profiles/restricted.json", "custom seccomp"),
            ("systempaths=unconfined", "security option"),
        ] {
            let config = BoxConfig {
                security_opt: vec![option.to_string()],
                ..Default::default()
            };
            let error = resolve_execution(&config).unwrap_err().to_string();
            assert!(
                error.contains(expected),
                "expected {option:?} rejection to mention {expected:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn microvm_accepts_guest_enforceable_security_options() {
        let config = BoxConfig {
            security_opt: vec![
                " SECCOMP=DEFAULT ".to_string(),
                "seccomp=unconfined".to_string(),
                "no-new-privileges".to_string(),
                "no-new-privileges=false".to_string(),
            ],
            cap_add: vec!["NET_ADMIN".to_string()],
            cap_drop: vec!["NET_RAW".to_string()],
            privileged: true,
            ..Default::default()
        };

        assert!(resolve_execution(&config).is_ok());
    }

    #[test]
    fn sandbox_resolves_to_a3s_oci_shared_kernel_with_mandatory_controls() {
        let plan = resolve_execution(&sandbox_config()).unwrap();
        assert_eq!(plan.backend, ExecutionBackend::A3sOci);
        assert!(plan.backend.is_sandbox());
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
    fn sandbox_rejects_security_options_not_wired_to_oci() {
        for (option, expected) in [
            ("apparmor=runtime/default", "AppArmor"),
            ("label=type:container_t", "SELinux"),
            ("seccomp=/profiles/restricted.json", "custom seccomp"),
            ("no-new-privileges=false", "requires no-new-privileges"),
        ] {
            let config = BoxConfig {
                security_opt: vec![option.to_string()],
                ..sandbox_config()
            };
            let error = resolve_execution(&config).unwrap_err().to_string();
            assert!(
                error.contains(expected),
                "expected {option:?} rejection to mention {expected:?}, got {error:?}"
            );
        }
    }

    #[test]
    fn sandbox_accepts_security_options_compiled_by_oci_backend() {
        let config = BoxConfig {
            security_opt: vec![
                "seccomp=default".to_string(),
                "no-new-privileges".to_string(),
                "no-new-privileges=true".to_string(),
            ],
            cap_add: vec!["cap_chown".to_string()],
            cap_drop: vec!["NET_RAW".to_string()],
            ..sandbox_config()
        };

        assert!(resolve_execution(&config).is_ok());
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
