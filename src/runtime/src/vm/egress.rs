//! Generation-scoped MicroVM egress preparation and cleanup.

use std::net::{Ipv4Addr, SocketAddr, SocketAddrV4};

use a3s_box_core::vmm::{NetworkEgressConfig, NetworkHttpProxyRoute};
use a3s_box_core::{
    BoxError, CompiledEgressPolicy, EgressPolicy, ExecutionId, NetworkMode, ResolvedExecutionPlan,
};

use crate::egress_proxy::{EgressProxyConfig, EgressProxyHandle};

use super::VmManager;

pub(crate) const RESTRICTED_GUEST_IP: Ipv4Addr = Ipv4Addr::new(10, 90, 0, 2);
pub(crate) const RESTRICTED_GATEWAY_IP: Ipv4Addr = Ipv4Addr::new(10, 90, 0, 1);
pub(crate) const RESTRICTED_PREFIX_LEN: u8 = 24;
pub(crate) const RESTRICTED_HTTP_PROXY_PORT: u16 = 3128;

pub(crate) struct PreparedMicrovmEgress {
    handle: EgressProxyHandle,
    network: NetworkEgressConfig,
    authenticated_proxy_url: String,
}

pub(crate) fn requires_generation_context(plan: &ResolvedExecutionPlan) -> bool {
    restricted_policy(plan).is_some()
}

impl VmManager {
    pub(super) async fn prepare_microvm_egress(
        &mut self,
        plan: &ResolvedExecutionPlan,
    ) -> a3s_box_core::Result<()> {
        let Some(policy) = restricted_policy(plan).cloned() else {
            return Ok(());
        };
        if self.prepared_egress.is_some() {
            return Err(BoxError::StateError(
                "restricted MicroVM egress was prepared more than once".to_string(),
            ));
        }
        if !matches!(&self.config.network, NetworkMode::Tsi) {
            return Err(BoxError::ConfigError(
                "restricted MicroVM egress requires the TSI network mode; bridge enforcement is not available"
                    .to_string(),
            ));
        }

        #[cfg(not(unix))]
        {
            let _ = policy;
            return Err(BoxError::ConfigError(
                "restricted MicroVM egress is not available on this host platform".to_string(),
            ));
        }

        #[cfg(unix)]
        {
            let context = self.security_context.as_ref().ok_or_else(|| {
                BoxError::StateError(
                    "restricted MicroVM egress has no managed execution generation".to_string(),
                )
            })?;
            let execution_id = ExecutionId::new(self.box_id.clone())
                .map_err(|error| BoxError::StateError(error.to_string()))?;
            let limits = CompiledEgressPolicy::compile(&policy)?.limits();
            let generation_dir = self
                .home_dir
                .join("boxes")
                .join(&self.box_id)
                .join("security")
                .join("egress")
                .join(format!("generation-{}", context.generation.get()));
            // Unix-domain socket path limits are small (104 bytes on macOS).
            // Keep the durable decision log under the execution directory, but
            // place the transient generation socket in the bounded runtime
            // socket tree used by the other VM control channels.
            let policy_socket_path = self.socket_dir().join(format!(
                "egress-generation-{}.sock",
                context.generation.get()
            ));
            let proxy = EgressProxyHandle::start(
                EgressProxyConfig::new(
                    execution_id,
                    context.generation,
                    policy,
                    generation_dir.join("decisions.jsonl"),
                )
                .with_policy_socket_path(&policy_socket_path),
            )
            .await
            .map_err(|error| {
                BoxError::NetworkError(format!(
                    "failed to prepare mandatory MicroVM egress proxy: {error}"
                ))
            })?;
            if !proxy.is_running() {
                return Err(BoxError::NetworkError(
                    "mandatory MicroVM egress proxy terminated during preparation".to_string(),
                ));
            }

            let guest_proxy_address = SocketAddr::V4(SocketAddrV4::new(
                RESTRICTED_GATEWAY_IP,
                RESTRICTED_HTTP_PROXY_PORT,
            ));
            let authenticated_proxy_url = proxy.authenticated_proxy_url(guest_proxy_address);
            let network = NetworkEgressConfig {
                policy_socket_path,
                http_proxy: Some(NetworkHttpProxyRoute {
                    guest_port: RESTRICTED_HTTP_PROXY_PORT,
                    host_address: proxy.local_address(),
                }),
                limits,
            };
            self.prepared_egress = Some(PreparedMicrovmEgress {
                handle: proxy,
                network,
                authenticated_proxy_url,
            });
            Ok(())
        }
    }

    pub(super) fn apply_microvm_egress_environment(&self, environment: &mut Vec<(String, String)>) {
        let Some(prepared) = self.prepared_egress.as_ref() else {
            return;
        };
        environment.retain(|(name, _)| {
            !matches!(
                name.as_str(),
                "HTTP_PROXY"
                    | "http_proxy"
                    | "HTTPS_PROXY"
                    | "https_proxy"
                    | "ALL_PROXY"
                    | "all_proxy"
                    | "NO_PROXY"
                    | "no_proxy"
            )
        });
        for name in ["HTTP_PROXY", "http_proxy", "HTTPS_PROXY", "https_proxy"] {
            environment.push((name.to_string(), prepared.authenticated_proxy_url.clone()));
        }
        for name in ["NO_PROXY", "no_proxy"] {
            environment.push((name.to_string(), "localhost,127.0.0.1,::1".to_string()));
        }
    }

    pub(crate) fn managed_session_environment(
        &self,
    ) -> a3s_box_core::Result<Vec<(String, String)>> {
        let required = self
            .resolved_execution_plan
            .as_ref()
            .is_some_and(requires_generation_context);
        if required && !self.prepared_egress_is_running() {
            return Err(BoxError::NetworkError(
                "restricted MicroVM egress proxy is unavailable".to_string(),
            ));
        }
        let mut environment = Vec::new();
        self.apply_microvm_egress_environment(&mut environment);
        Ok(environment)
    }

    pub(super) fn prepared_egress_network(&self) -> Option<NetworkEgressConfig> {
        self.prepared_egress
            .as_ref()
            .map(|prepared| prepared.network.clone())
    }

    pub(super) fn prepared_egress_is_running(&self) -> bool {
        let required = self
            .resolved_execution_plan
            .as_ref()
            .is_some_and(requires_generation_context);
        match self.prepared_egress.as_ref() {
            Some(prepared) => {
                prepared.handle.is_running()
                    && policy_socket_is_live(&prepared.network.policy_socket_path)
            }
            None => !required,
        }
    }

    pub(super) async fn stop_prepared_egress(&mut self) -> a3s_box_core::Result<()> {
        let Some(prepared) = self.prepared_egress.take() else {
            return Ok(());
        };
        prepared.handle.stop().await.map_err(|error| {
            BoxError::NetworkError(format!("failed to stop MicroVM egress proxy: {error}"))
        })
    }
}

fn restricted_policy(plan: &ResolvedExecutionPlan) -> Option<&EgressPolicy> {
    plan.security_policy
        .as_ref()
        .and_then(|policy| policy.egress.as_ref())
        .filter(|policy| !matches!(policy, EgressPolicy::Unrestricted))
}

#[cfg(unix)]
fn policy_socket_is_live(path: &std::path::Path) -> bool {
    use std::os::unix::fs::{FileTypeExt, MetadataExt, PermissionsExt};

    let Ok(metadata) = std::fs::symlink_metadata(path) else {
        return false;
    };
    // SAFETY: querying the effective process UID has no preconditions.
    let effective_uid = unsafe { libc::geteuid() };
    metadata.file_type().is_socket()
        && !metadata.file_type().is_symlink()
        && metadata.uid() == effective_uid
        && metadata.permissions().mode() & 0o777 == 0o600
}

#[cfg(not(unix))]
fn policy_socket_is_live(_path: &std::path::Path) -> bool {
    false
}
