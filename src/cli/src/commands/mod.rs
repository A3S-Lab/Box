//! CLI command definitions and dispatch.

mod create;
mod exec;
mod images;
mod info;
mod inspect;
mod kill;
mod logs;
mod ps;
mod pull;
mod rm;
mod rmi;
mod run;
mod start;
mod stop;
mod version;

use clap::{Parser, Subcommand};

/// A3S Box — Docker-like MicroVM runtime.
#[derive(Parser)]
#[command(name = "a3s-box", version, about)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Command,
}

/// Available commands.
#[derive(Subcommand)]
pub enum Command {
    /// Create and start a new box from an image
    Run(run::RunArgs),
    /// Create a new box without starting it
    Create(create::CreateArgs),
    /// Start a stopped or created box
    Start(start::StartArgs),
    /// Gracefully stop a running box
    Stop(stop::StopArgs),
    /// Remove a box
    Rm(rm::RmArgs),
    /// Force-kill a running box
    Kill(kill::KillArgs),
    /// List boxes
    Ps(ps::PsArgs),
    /// View box logs
    Logs(logs::LogsArgs),
    /// Execute a command in a running box
    Exec(exec::ExecArgs),
    /// Display detailed box information
    Inspect(inspect::InspectArgs),
    /// List cached images
    Images(images::ImagesArgs),
    /// Pull an image from a registry
    Pull(pull::PullArgs),
    /// Remove a cached image
    Rmi(rmi::RmiArgs),
    /// Show version information
    Version(version::VersionArgs),
    /// Show system information
    Info(info::InfoArgs),
    /// Update a3s-box to the latest version
    Update,
}

/// Dispatch a parsed CLI to the appropriate command handler.
pub async fn dispatch(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Run(args) => run::execute(args).await,
        Command::Create(args) => create::execute(args).await,
        Command::Start(args) => start::execute(args).await,
        Command::Stop(args) => stop::execute(args).await,
        Command::Rm(args) => rm::execute(args).await,
        Command::Kill(args) => kill::execute(args).await,
        Command::Ps(args) => ps::execute(args).await,
        Command::Logs(args) => logs::execute(args).await,
        Command::Exec(args) => exec::execute(args).await,
        Command::Inspect(args) => inspect::execute(args).await,
        Command::Images(args) => images::execute(args).await,
        Command::Pull(args) => pull::execute(args).await,
        Command::Rmi(args) => rmi::execute(args).await,
        Command::Version(args) => version::execute(args).await,
        Command::Info(args) => info::execute(args).await,
        Command::Update => {
            a3s_updater::run_update(&a3s_updater::UpdateConfig {
                binary_name: "a3s-box",
                crate_name: "a3s-box-cli",
                current_version: env!("CARGO_PKG_VERSION"),
                github_owner: "a3s-lab",
                github_repo: "a3s",
            })
            .await
            .map_err(|e| e.into())
        }
    }
}
