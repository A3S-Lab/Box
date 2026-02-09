//! CLI command definitions and dispatch.

mod create;
mod exec;
mod image_inspect;
mod image_prune;
mod image_tag;
mod images;
mod info;
mod inspect;
mod kill;
mod logs;
mod ps;
mod pull;
mod restart;
mod rm;
mod rmi;
mod run;
mod start;
mod stats;
mod stop;
mod version;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Default maximum image store size: 10 GB.
const IMAGE_STORE_MAX_SIZE: u64 = 10 * 1024 * 1024 * 1024;

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
    /// Restart a running box
    Restart(restart::RestartArgs),
    /// Remove a box
    Rm(rm::RmArgs),
    /// Force-kill a running box
    Kill(kill::KillArgs),
    /// List boxes
    Ps(ps::PsArgs),
    /// Display resource usage statistics
    Stats(stats::StatsArgs),
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
    /// Remove one or more cached images
    Rmi(rmi::RmiArgs),
    /// Display detailed image information as JSON
    ImageInspect(image_inspect::ImageInspectArgs),
    /// Remove unused images
    ImagePrune(image_prune::ImagePruneArgs),
    /// Create a tag that refers to an existing image
    Tag(image_tag::ImageTagArgs),
    /// Show version information
    Version(version::VersionArgs),
    /// Show system information
    Info(info::InfoArgs),
    /// Update a3s-box to the latest version
    Update,
}

/// Return the path to the image store directory (~/.a3s/images).
pub(crate) fn images_dir() -> PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".a3s"))
        .unwrap_or_else(|| PathBuf::from(".a3s"))
        .join("images")
}

/// Open the shared image store.
pub(crate) fn open_image_store() -> Result<a3s_box_runtime::ImageStore, Box<dyn std::error::Error>> {
    let dir = images_dir();
    let store = a3s_box_runtime::ImageStore::new(&dir, IMAGE_STORE_MAX_SIZE)?;
    Ok(store)
}

/// Dispatch a parsed CLI to the appropriate command handler.
pub async fn dispatch(cli: Cli) -> Result<(), Box<dyn std::error::Error>> {
    match cli.command {
        Command::Run(args) => run::execute(args).await,
        Command::Create(args) => create::execute(args).await,
        Command::Start(args) => start::execute(args).await,
        Command::Stop(args) => stop::execute(args).await,
        Command::Restart(args) => restart::execute(args).await,
        Command::Rm(args) => rm::execute(args).await,
        Command::Kill(args) => kill::execute(args).await,
        Command::Ps(args) => ps::execute(args).await,
        Command::Stats(args) => stats::execute(args).await,
        Command::Logs(args) => logs::execute(args).await,
        Command::Exec(args) => exec::execute(args).await,
        Command::Inspect(args) => inspect::execute(args).await,
        Command::Images(args) => images::execute(args).await,
        Command::Pull(args) => pull::execute(args).await,
        Command::Rmi(args) => rmi::execute(args).await,
        Command::ImageInspect(args) => image_inspect::execute(args).await,
        Command::ImagePrune(args) => image_prune::execute(args).await,
        Command::Tag(args) => image_tag::execute(args).await,
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
