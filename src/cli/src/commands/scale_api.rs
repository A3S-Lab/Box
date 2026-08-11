//! Standalone machine-facing scale authority for A3S Gateway.

use std::{net::SocketAddr, path::PathBuf, sync::Arc};

use a3s_box_runtime::{serve_scale_api, DurableScaleAuthority};
use clap::Args;
use tokio::sync::Mutex;

#[derive(Args)]
pub struct ScaleApiArgs {
    /// Address exposed to the trusted Gateway control plane.
    #[arg(long, default_value = "127.0.0.1:9090")]
    address: SocketAddr,

    /// Durable operation/revision journal.
    #[arg(long)]
    state: Option<PathBuf>,

    /// Maximum aggregate desired replicas accepted by this authority.
    #[arg(long, default_value_t = 1000)]
    max_instances: u32,
}

pub async fn execute(args: ScaleApiArgs) -> Result<(), Box<dyn std::error::Error>> {
    let state = args
        .state
        .unwrap_or_else(|| a3s_box_core::dirs_home().join("scale-authority.json"));
    let authority = Arc::new(Mutex::new(DurableScaleAuthority::open(
        state,
        args.max_instances,
    )?));
    tracing::info!(address = %args.address, "starting Gateway scale authority");
    serve_scale_api(args.address, authority).await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use clap::Parser;

    #[test]
    fn scale_api_command_has_safe_loopback_defaults() {
        let cli = super::super::Cli::try_parse_from(["a3s-box", "scale-api"]).unwrap();
        let super::super::Command::ScaleApi(args) = cli.command else {
            panic!("expected scale-api command");
        };
        assert_eq!(args.address, "127.0.0.1:9090".parse().unwrap());
        assert_eq!(args.max_instances, 1000);
        assert!(args.state.is_none());
    }
}
