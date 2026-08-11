//! Standalone machine-facing scale authority for A3S Gateway.

use std::{net::SocketAddr, path::PathBuf};

use a3s_box_runtime::{
    serve_scale_api, DurableScaleAuthority, LocalScaleReconciler, ScaleApiState,
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

    /// Maximum aggregate desired replicas accepted by this authority.
    #[arg(long, default_value_t = 1000)]
    max_instances: u32,
}

pub async fn execute(args: ScaleApiArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = args
        .state
        .unwrap_or_else(|| a3s_box_core::dirs_home().join("scale-authority.json"));
    let authority = DurableScaleAuthority::open(state, args.max_instances)?;
    let authority = if let Some(services) = args.services {
        let catalog = ScaleServiceCatalog::from_acl_file(
            &services,
            "gateway-scale",
            resolve_isolation(args.isolation),
        )?;
        let home = a3s_box_core::dirs_home();
        let manager = super::configured_local_execution_manager(&home).await?;
        ScaleApiState::with_reconciler(authority, LocalScaleReconciler::new(manager, catalog))
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

#[cfg(test)]
mod tests {
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
    }

    #[test]
    fn scale_api_requires_templates_unless_authority_only_is_explicit() {
        let error = super::super::Cli::try_parse_from(["a3s-box", "scale-api"])
            .err()
            .expect("missing service catalog must be rejected");
        assert!(error.to_string().contains("--services"));
    }
}
