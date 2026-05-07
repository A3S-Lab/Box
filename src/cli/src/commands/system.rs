//! `a3s-box system` subcommands — Docker-compatible system command namespace.

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct SystemArgs {
    #[command(subcommand)]
    pub command: SystemCommand,
}

#[derive(Subcommand)]
pub enum SystemCommand {
    /// Show disk usage
    Df(super::df::DfArgs),
    /// Remove all unused data
    Prune(super::system_prune::SystemPruneArgs),
    /// Show system information
    Info(super::info::InfoArgs),
    /// Stream real-time system events
    Events(super::events::EventsArgs),
}

pub async fn execute(args: SystemArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        SystemCommand::Df(a) => super::df::execute(a).await,
        SystemCommand::Prune(a) => super::system_prune::execute(a).await,
        SystemCommand::Info(a) => super::info::execute(a).await,
        SystemCommand::Events(a) => super::events::execute(a).await,
    }
}
