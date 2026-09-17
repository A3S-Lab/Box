//! Compose `--isolation sandbox` lifecycle over `LocalExecutionManager`.
//!
//! MicroVM Compose keeps the legacy `VmManager::boot` path. This module only
//! covers the SandboxViaOci GA route so Compose shares create/start/remove with
//! CLI/SDK. Named bridges and static TCP published ports require keep-authority
//! opt-in (same DNAT surface as CLI/SDK); UDP and `host_port=0` stay refused.

use std::collections::BTreeMap;
use std::path::Path;

use a3s_box_core::config::BoxConfig;
use a3s_box_core::network::NetworkMode;
use a3s_box_core::{
    parse_port_mapping, sandbox_named_bridge_opt_in_enabled, CreateExecutionRequest, ExecutionId,
    ExecutionManager, ExecutionRecordPolicy, ExecutionRestartPolicy, KillExecutionOptions,
    OperationId, PortProtocol, OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV,
};
use a3s_box_runtime::{ComposeRuntimePlan, ManagedExecutionState};

use crate::state::{BoxRecord, StateFile};

use super::lifecycle::ServiceBox;
use super::secrets::ComposeSecretLease;

/// Reject Compose features that SandboxViaOci cannot honor yet.
pub(super) fn preflight_sandbox_compose(
    project: &ComposeRuntimePlan,
) -> Result<(), Box<dyn std::error::Error>> {
    let bridge_opt_in = sandbox_named_bridge_opt_in_enabled();
    if !project.config.networks.is_empty() && !bridge_opt_in {
        return Err(format!(
            "Compose --isolation sandbox named bridge networks require {OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV}=1 (matched root)"
        )
        .into());
    }
    for service_name in &project.service_order {
        let service = project.config.services.get(service_name).ok_or_else(|| {
            format!("Service '{service_name}' disappeared from the resolved Compose project")
        })?;
        if !service.networks.names().is_empty() && !bridge_opt_in {
            return Err(format!(
                "Compose --isolation sandbox named networks on service '{service_name}' require {OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV}=1 (matched root)"
            )
            .into());
        }
        if !service.ports.is_empty() {
            if !bridge_opt_in {
                return Err(format!(
                    "Compose --isolation sandbox published ports on service '{service_name}' require {OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV}=1 (matched root) with Bridge"
                )
                .into());
            }
            validate_sandbox_compose_published_ports(service_name, &service.ports)?;
        }
    }
    Ok(())
}

fn validate_sandbox_compose_published_ports(
    service_name: &str,
    ports: &[String],
) -> Result<(), Box<dyn std::error::Error>> {
    for entry in ports {
        let mapping = parse_port_mapping(entry).map_err(|error| {
            format!("Compose --isolation sandbox published port on service '{service_name}': {error}")
        })?;
        if mapping.protocol != PortProtocol::Tcp {
            return Err(format!(
                "Compose --isolation sandbox published ports on service '{service_name}' only support TCP; got '{entry}'"
            )
            .into());
        }
        if mapping.host_port == 0 {
            return Err(format!(
                "Compose --isolation sandbox published ports on service '{service_name}' reject host_port=0 auto-assign in '{entry}'"
            )
            .into());
        }
    }
    Ok(())
}

/// Apply SandboxViaOci Compose networking policy.
///
/// Loopback-only by default. Keep-authority opt-in preserves Bridge so prepare
/// can stage host netDevices and keeps static TCP `port_map` for DNAT.
pub(super) fn sandbox_box_config(mut config: BoxConfig) -> BoxConfig {
    let keep_bridge = sandbox_named_bridge_opt_in_enabled()
        && matches!(config.network, NetworkMode::Bridge { .. });
    if !keep_bridge {
        config.network = NetworkMode::None;
        config.port_map.clear();
    }
    config
}

/// Whether Compose sandbox should create/attach NetworkStore networks.
pub(super) fn sandbox_compose_creates_networks() -> bool {
    sandbox_named_bridge_opt_in_enabled()
}

/// Inputs for one Compose SandboxViaOci service create/start.
pub(super) struct SandboxServiceBootRequest<'a> {
    pub project_name: &'a str,
    pub svc_name: &'a str,
    pub box_config: BoxConfig,
    pub labels: BTreeMap<String, String>,
    pub restart_policy: ExecutionRestartPolicy,
    pub max_restart_count: u32,
    pub volume_names: Vec<String>,
    pub secret_root: Option<&'a Path>,
    pub health_check: Option<crate::state::HealthCheck>,
    pub healthcheck_disabled: bool,
}

/// Create and start one Compose service through the configured local execution manager.
pub(super) async fn boot_sandbox_service(
    request: SandboxServiceBootRequest<'_>,
) -> Result<BoxRecord, Box<dyn std::error::Error>> {
    let SandboxServiceBootRequest {
        project_name,
        svc_name,
        box_config,
        labels,
        restart_policy,
        max_restart_count,
        volume_names,
        secret_root,
        health_check,
        healthcheck_disabled,
    } = request;
    let home = a3s_box_core::dirs_home();
    let manager = super::super::configured_local_execution_manager(&home).await?;
    manager.preflight_isolation(box_config.isolation).await?;

    let box_name = format!("{project_name}-{svc_name}");
    let operation_id = OperationId::new(format!("compose-sandbox-{}", uuid::Uuid::new_v4()))?;
    let create_request = CreateExecutionRequest {
        external_sandbox_id: operation_id.as_str().to_string(),
        config: box_config,
        labels,
        policy: ExecutionRecordPolicy {
            name: Some(box_name),
            auto_remove: false,
            restart_policy,
            max_restart_count,
            health_check,
            healthcheck_disabled,
            log_config: Default::default(),
            volume_names,
            platform: None,
            init: false,
            devices: Vec::new(),
            gpus: None,
            shm_size: None,
            stop_signal: None,
            stop_timeout: None,
            oom_kill_disable: false,
            oom_score_adj: None,
            managed_secret_root: secret_root.map(Path::to_path_buf),
        },
        rootfs_snapshot_id: None,
    };

    let reservation = manager.create(create_request, &operation_id).await?;
    let execution_id = reservation.execution_id.clone();
    if let Err(error) = manager.start(&execution_id, reservation.generation).await {
        let _ = manager
            .remove_execution(&execution_id, reservation.generation)
            .await;
        return Err(
            format!("Failed to start Compose sandbox service '{svc_name}': {error}").into(),
        );
    }

    let box_id = execution_id.to_string();
    StateFile::load_readonly()?
        .find_by_id(&box_id)
        .cloned()
        .ok_or_else(|| {
            format!("managed Compose sandbox service {box_id} disappeared after startup").into()
        })
}

/// Tear down a managed Compose service through LocalExecutionManager when possible.
pub(super) async fn teardown_managed_service(
    service: &ServiceBox,
) -> Result<bool, Box<dyn std::error::Error>> {
    let Some(record) = StateFile::load_readonly()?
        .find_by_id(&service.box_id)
        .cloned()
    else {
        return Ok(false);
    };
    let Some(metadata) = record.managed_execution.as_ref() else {
        return Ok(false);
    };
    let generation = metadata.generation;
    let state = record
        .managed_state()?
        .ok_or_else(|| format!("Box {} lost managed lifecycle metadata", record.name))?;
    let home = a3s_box_core::dirs_home();
    let manager = super::super::configured_local_execution_manager(&home).await?;
    let execution_id = ExecutionId::new(record.id.clone())?;
    let terminate = matches!(
        state,
        ManagedExecutionState::Running
            | ManagedExecutionState::Paused
            | ManagedExecutionState::Killing
            | ManagedExecutionState::Creating
            | ManagedExecutionState::Starting
    );
    if terminate {
        manager
            .kill_with_options(
                &execution_id,
                generation,
                KillExecutionOptions {
                    signal: Some(9),
                    timeout_secs: Some(0),
                },
            )
            .await?;
    }
    manager.remove_execution(&execution_id, generation).await?;
    Ok(true)
}

pub(super) fn execution_restart_policy(
    policy: &str,
) -> Result<ExecutionRestartPolicy, Box<dyn std::error::Error>> {
    match policy {
        "no" => Ok(ExecutionRestartPolicy::No),
        "always" => Ok(ExecutionRestartPolicy::Always),
        "on-failure" => Ok(ExecutionRestartPolicy::OnFailure),
        "unless-stopped" => Ok(ExecutionRestartPolicy::UnlessStopped),
        other => Err(format!("Invalid normalized restart policy: {other}").into()),
    }
}

/// Secret root path for managed create policy, when a lease is present.
pub(super) fn lease_secret_root(lease: Option<&ComposeSecretLease>) -> Option<&Path> {
    lease.map(ComposeSecretLease::secret_root)
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::compose::{ComposeConfig, ServiceConfig, StringOrList};
    use std::collections::HashMap;
    use std::sync::Mutex;

    static ENV_LOCK: Mutex<()> = Mutex::new(());

    #[test]
    fn preflight_rejects_published_ports_without_keep_authority() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV);
        let mut services = HashMap::new();
        services.insert(
            "web".to_string(),
            ServiceConfig {
                image: Some("alpine:latest".to_string()),
                ports: vec!["8080:80".to_string()],
                ..Default::default()
            },
        );
        let config = ComposeConfig {
            version: None,
            services,
            volumes: HashMap::new(),
            networks: HashMap::new(),
        };
        let project = ComposeRuntimePlan::new("demo", config).unwrap();
        let error = preflight_sandbox_compose(&project).unwrap_err();
        assert!(
            error
                .to_string()
                .contains(OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV),
            "{error}"
        );
    }

    #[test]
    fn keep_authority_admits_static_tcp_published_ports() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var(OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV, "1");
        let mut services = HashMap::new();
        services.insert(
            "web".to_string(),
            ServiceConfig {
                image: Some("alpine:latest".to_string()),
                ports: vec!["8080:80".to_string()],
                ..Default::default()
            },
        );
        let config = ComposeConfig {
            version: None,
            services,
            volumes: HashMap::new(),
            networks: HashMap::new(),
        };
        let project = ComposeRuntimePlan::new("demo", config).unwrap();
        preflight_sandbox_compose(&project).unwrap();
        let config = sandbox_box_config(
            project
                .build_box_config("web", Some(&project.default_network_name()))
                .unwrap(),
        );
        assert!(matches!(config.network, NetworkMode::Bridge { .. }));
        assert_eq!(config.port_map, vec!["8080:80".to_string()]);
        std::env::remove_var(OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV);
    }

    #[test]
    fn keep_authority_rejects_host_port_zero_published_ports() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var(OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV, "1");
        let mut services = HashMap::new();
        services.insert(
            "web".to_string(),
            ServiceConfig {
                image: Some("alpine:latest".to_string()),
                ports: vec!["0:80".to_string()],
                ..Default::default()
            },
        );
        let config = ComposeConfig {
            version: None,
            services,
            volumes: HashMap::new(),
            networks: HashMap::new(),
        };
        let project = ComposeRuntimePlan::new("demo", config).unwrap();
        let error = preflight_sandbox_compose(&project).unwrap_err();
        assert!(error.to_string().contains("host_port=0"), "{error}");
        std::env::remove_var(OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV);
    }

    #[test]
    fn preflight_accepts_loopback_only_service() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV);
        let mut services = HashMap::new();
        services.insert(
            "web".to_string(),
            ServiceConfig {
                image: Some("alpine:latest".to_string()),
                command: Some(StringOrList::List(vec![
                    "sleep".to_string(),
                    "60".to_string(),
                ])),
                ..Default::default()
            },
        );
        let config = ComposeConfig {
            version: None,
            services,
            volumes: HashMap::new(),
            networks: HashMap::new(),
        };
        let project = ComposeRuntimePlan::new("demo", config).unwrap();
        preflight_sandbox_compose(&project).unwrap();
        let config = sandbox_box_config(
            project
                .build_box_config("web", Some(&project.default_network_name()))
                .unwrap(),
        );
        assert!(matches!(config.network, NetworkMode::None));
        assert!(config.port_map.is_empty());
    }

    #[test]
    fn preflight_rejects_named_networks_without_keep_authority() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::remove_var(OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV);
        let mut services = HashMap::new();
        services.insert(
            "web".to_string(),
            ServiceConfig {
                image: Some("alpine:latest".to_string()),
                ..Default::default()
            },
        );
        let mut networks = HashMap::new();
        networks.insert("frontend".to_string(), Default::default());
        let config = ComposeConfig {
            version: None,
            services,
            volumes: HashMap::new(),
            networks,
        };
        let project = ComposeRuntimePlan::new("demo", config).unwrap();
        let error = preflight_sandbox_compose(&project).unwrap_err();
        assert!(
            error
                .to_string()
                .contains(OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV),
            "{error}"
        );
    }

    #[test]
    fn keep_authority_preserves_bridge_in_sandbox_box_config() {
        let _guard = ENV_LOCK.lock().unwrap();
        std::env::set_var(OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV, "1");
        let config = sandbox_box_config(BoxConfig {
            network: NetworkMode::Bridge {
                network: "frontend".into(),
            },
            port_map: vec!["8080:80".into()],
            ..Default::default()
        });
        assert_eq!(
            config.network,
            NetworkMode::Bridge {
                network: "frontend".into(),
            }
        );
        assert_eq!(config.port_map, vec!["8080:80".to_string()]);
        assert!(sandbox_compose_creates_networks());
        std::env::remove_var(OCI_NATIVE_KEEP_NETWORK_DEVICE_AUTHORITY_ENV);
        assert!(!sandbox_compose_creates_networks());
    }
}
