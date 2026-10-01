//! Standalone machine-facing scale authority for A3S Gateway.

use std::{
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::PathBuf,
    time::Duration,
};

use a3s_box_core::error::BoxError;
use a3s_box_runtime::{
    serve_scale_api, DurableScaleAuthority, LocalScaleReconciler, ScaleApiState,
    ScaleAuthorityError, ScaleCatalogError, ScaleEndpointConfig, ScaleEndpointConfigError,
    ScaleServiceCatalog,
};
use clap::Args;

use super::common::{resolve_isolation, IsolationArg};

#[derive(Args)]
pub struct ScaleApiArgs {
    /// Address exposed to the trusted Gateway control plane.
    #[arg(long, default_value = "127.0.0.1:9090")]
    address: SocketAddr,

    /// Durable operation/revision journal.
    #[arg(long)]
    state: Option<PathBuf>,

    /// Compose ACL file containing Box-owned stateless service templates.
    #[arg(
        long,
        value_name = "COMPOSE.ACL",
        required_unless_present = "desired_state_only"
    )]
    services: Option<PathBuf>,

    /// Persist desired state without starting workloads (diagnostic/migration use only).
    #[arg(long, conflicts_with = "services")]
    desired_state_only: bool,

    /// Use shared-kernel Sandbox isolation for generated replicas.
    #[arg(long, value_enum)]
    isolation: Option<IsolationArg>,

    /// Address used for runtime-owned replica endpoint listeners.
    #[arg(long, default_value_t = IpAddr::V4(Ipv4Addr::LOCALHOST))]
    endpoint_bind_address: IpAddr,

    /// Host or IP advertised to Gateway for replica traffic.
    #[arg(long, value_name = "HOST", requires = "services")]
    endpoint_advertise_host: Option<String>,

    /// Seconds to drain existing replica relay connections after endpoint withdrawal.
    #[arg(long, default_value_t = 3, value_parser = clap::value_parser!(u64).range(1..=300))]
    endpoint_drain_timeout_secs: u64,

    /// Maximum aggregate desired replicas accepted by this authority.
    #[arg(long, default_value_t = 1000)]
    max_instances: u32,
}

pub async fn execute(args: ScaleApiArgs) -> Result<(), BoxError> {
    let state = args
        .state
        .unwrap_or_else(|| a3s_box_core::dirs_home().join("scale-authority.json"));
    let authority =
        DurableScaleAuthority::open(state, args.max_instances).map_err(scale_authority_error)?;
    let authority = if let Some(services) = args.services {
        let catalog = ScaleServiceCatalog::from_acl_file(
            &services,
            "gateway-scale",
            resolve_isolation(args.isolation),
        )
        .map_err(scale_catalog_error)?;
        let home = a3s_box_core::dirs_home();
        let manager = super::configured_local_execution_manager(&home).await?;
        let advertise_host =
            required_advertise_host(args.endpoint_advertise_host, args.endpoint_bind_address)?;
        let endpoint_config = ScaleEndpointConfig::new(args.endpoint_bind_address, advertise_host)
            .map_err(scale_endpoint_config_error)?
            .with_drain_timeout(Duration::from_secs(args.endpoint_drain_timeout_secs));
        ScaleApiState::with_reconciler(
            authority,
            LocalScaleReconciler::with_endpoint_config(manager, catalog, endpoint_config),
        )
    } else {
        tracing::warn!(
            "Starting scale authority without workload reconciliation; desired state only"
        );
        ScaleApiState::authority_only(authority)
    };
    tracing::info!(address = %args.address, "starting Gateway scale authority");
    serve_scale_api(args.address, authority).await?;
    Ok(())
}

fn required_advertise_host(
    configured: Option<String>,
    bind_address: std::net::IpAddr,
) -> Result<String, BoxError> {
    match configured {
        Some(host) => Ok(host),
        None if !bind_address.is_unspecified() => Ok(bind_address.to_string()),
        None => Err(BoxError::ConfigError(
            "--endpoint-advertise-host is required when the bind address is unspecified".into(),
        )),
    }
}

fn scale_authority_error(error: ScaleAuthorityError) -> BoxError {
    match error {
        ScaleAuthorityError::Conflict(message, _) => {
            BoxError::StateError(format!("scale operation conflict: {message}"))
        }
        ScaleAuthorityError::State(message) => {
            BoxError::StateError(format!("scale authority state error: {message}"))
        }
    }
}

fn scale_catalog_error(error: ScaleCatalogError) -> BoxError {
    match error {
        ScaleCatalogError::Read { path, source } => super::io_error(
            format!("failed to read scale service catalog {path}"),
            source,
        ),
        ScaleCatalogError::Invalid(message) => {
            BoxError::ConfigError(format!("invalid scale service catalog: {message}"))
        }
    }
}

fn scale_endpoint_config_error(error: ScaleEndpointConfigError) -> BoxError {
    BoxError::ConfigError(error.to_string())
}

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr};

    use clap::Parser;

    #[test]
    fn scale_api_command_has_safe_loopback_defaults() {
        let cli =
            super::super::Cli::try_parse_from(["a3s-box", "scale-api", "--desired-state-only"])
                .unwrap();
        let super::super::Command::ScaleApi(args) = cli.command else {
            panic!("expected scale-api command");
        };
        assert_eq!(args.address, "127.0.0.1:9090".parse().unwrap());
        assert_eq!(args.max_instances, 1000);
        assert!(args.state.is_none());
        assert!(args.services.is_none());
        assert!(args.desired_state_only);
        assert_eq!(args.endpoint_bind_address, IpAddr::V4(Ipv4Addr::LOCALHOST));
        assert!(args.endpoint_advertise_host.is_none());
        assert_eq!(args.endpoint_drain_timeout_secs, 3);
    }

    #[test]
    fn scale_api_requires_templates_unless_authority_only_is_explicit() {
        let error = super::super::Cli::try_parse_from(["a3s-box", "scale-api"])
            .err()
            .expect("missing service catalog must be rejected");
        assert!(error.to_string().contains("--services"));
    }

    #[test]
    fn scale_api_parses_private_endpoint_publication_policy() {
        let cli = super::super::Cli::try_parse_from([
            "a3s-box",
            "scale-api",
            "--services",
            "services.acl",
            "--endpoint-bind-address",
            "10.0.0.7",
            "--endpoint-advertise-host",
            "box.internal",
            "--endpoint-drain-timeout-secs",
            "4",
        ])
        .unwrap();
        let super::super::Command::ScaleApi(args) = cli.command else {
            panic!("expected scale-api command");
        };
        assert_eq!(
            args.endpoint_bind_address,
            "10.0.0.7".parse::<IpAddr>().unwrap()
        );
        assert_eq!(
            args.endpoint_advertise_host.as_deref(),
            Some("box.internal")
        );
        assert_eq!(args.endpoint_drain_timeout_secs, 4);
        assert_eq!(
            args.services.as_deref(),
            Some(std::path::Path::new("services.acl"))
        );
    }

    #[test]
    fn unspecified_bind_without_advertise_host_is_a_configuration_error() {
        match super::required_advertise_host(None, IpAddr::V4(Ipv4Addr::UNSPECIFIED)) {
            Err(a3s_box_core::error::BoxError::ConfigError(message)) => {
                assert!(message.contains("--endpoint-advertise-host"), "{message}");
            }
            other => panic!("expected ConfigError, got {other:?}"),
        }
    }

    #[test]
    fn non_acl_scale_catalog_is_a_configuration_error() {
        let error = a3s_box_runtime::ScaleServiceCatalog::from_acl_file(
            std::path::Path::new("services.txt"),
            "gateway-scale",
            super::resolve_isolation(None),
        )
        .expect_err("a non-acl catalog is rejected before it is read");
        match super::scale_catalog_error(error) {
            a3s_box_core::error::BoxError::ConfigError(message) => {
                assert!(message.contains(".acl"), "{message}");
                assert!(message.contains("services.txt"), "{message}");
            }
            other => panic!("expected ConfigError, got {other:?}"),
        }
    }
}
