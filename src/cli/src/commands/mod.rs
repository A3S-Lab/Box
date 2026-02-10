//! CLI command definitions and dispatch.

mod cp;
mod create;
mod exec;
mod attach;
mod image_inspect;
mod image_prune;
mod image_tag;
mod images;
mod info;
mod inspect;
mod kill;
mod logs;
mod pause;
mod port;
mod ps;
mod pull;
mod rename;
mod restart;
mod rm;
mod rmi;
mod run;
mod start;
mod stats;
mod stop;
mod system_prune;
mod top;
mod unpause;
mod version;
mod wait;

use std::path::PathBuf;

use clap::{Parser, Subcommand};

/// Environment variable to override the image cache size limit.
///
/// Accepts human-readable sizes: `500m`, `10g`, `1t`, etc.
const IMAGE_CACHE_SIZE_ENV: &str = "A3S_IMAGE_CACHE_SIZE";

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
    /// Start one or more stopped or created boxes
    Start(start::StartArgs),
    /// Gracefully stop one or more running boxes
    Stop(stop::StopArgs),
    /// Restart one or more boxes
    Restart(restart::RestartArgs),
    /// Remove one or more boxes
    Rm(rm::RmArgs),
    /// Force-kill one or more running boxes
    Kill(kill::KillArgs),
    /// Pause one or more running boxes
    Pause(pause::PauseArgs),
    /// Unpause one or more paused boxes
    Unpause(unpause::UnpauseArgs),
    /// List boxes
    Ps(ps::PsArgs),
    /// Display resource usage statistics
    Stats(stats::StatsArgs),
    /// View box logs
    Logs(logs::LogsArgs),
    /// Execute a command in a running box
    Exec(exec::ExecArgs),
    /// Display running processes in a box
    Top(top::TopArgs),
    /// Display detailed box information
    Inspect(inspect::InspectArgs),
    /// Attach to a running box's console output
    Attach(attach::AttachArgs),
    /// Block until one or more boxes stop
    Wait(wait::WaitArgs),
    /// Rename a box
    Rename(rename::RenameArgs),
    /// List port mappings for a box
    Port(port::PortArgs),
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
    /// Copy files between host and a running box
    Cp(cp::CpArgs),
    /// Remove all unused data (stopped boxes and unused images)
    SystemPrune(system_prune::SystemPruneArgs),
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
///
/// The cache size limit can be configured via the `A3S_IMAGE_CACHE_SIZE`
/// environment variable (e.g., `500m`, `20g`). Defaults to 10 GB.
pub(crate) fn open_image_store() -> Result<a3s_box_runtime::ImageStore, Box<dyn std::error::Error>> {
    let dir = images_dir();
    let max_size = match std::env::var(IMAGE_CACHE_SIZE_ENV) {
        Ok(val) => crate::output::parse_size_bytes(&val).map_err(|e| {
            format!(
                "Invalid {IMAGE_CACHE_SIZE_ENV}={val:?}: {e} (examples: 500m, 10g, 1t)"
            )
        })?,
        Err(_) => a3s_box_runtime::DEFAULT_IMAGE_CACHE_SIZE,
    };
    let store = a3s_box_runtime::ImageStore::new(&dir, max_size)?;
    Ok(store)
}

/// Tail a file, printing new content as it appears.
///
/// Waits for the file to exist, then continuously reads and prints new data.
/// Used by `run` (foreground mode) and `attach`.
pub(crate) async fn tail_file(path: &std::path::Path) {
    use tokio::io::AsyncReadExt;

    // Wait for file to exist
    loop {
        if path.exists() {
            break;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }

    let mut file = match tokio::fs::File::open(path).await {
        Ok(f) => f,
        Err(_) => return,
    };

    let mut buf = vec![0u8; 4096];
    loop {
        match file.read(&mut buf).await {
            Ok(0) => {
                tokio::time::sleep(tokio::time::Duration::from_millis(200)).await;
            }
            Ok(n) => {
                let text = String::from_utf8_lossy(&buf[..n]);
                print!("{text}");
            }
            Err(_) => break,
        }
    }
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
        Command::Pause(args) => pause::execute(args).await,
        Command::Unpause(args) => unpause::execute(args).await,
        Command::Ps(args) => ps::execute(args).await,
        Command::Stats(args) => stats::execute(args).await,
        Command::Logs(args) => logs::execute(args).await,
        Command::Exec(args) => exec::execute(args).await,
        Command::Top(args) => top::execute(args).await,
        Command::Inspect(args) => inspect::execute(args).await,
        Command::Attach(args) => attach::execute(args).await,
        Command::Wait(args) => wait::execute(args).await,
        Command::Rename(args) => rename::execute(args).await,
        Command::Port(args) => port::execute(args).await,
        Command::Images(args) => images::execute(args).await,
        Command::Pull(args) => pull::execute(args).await,
        Command::Rmi(args) => rmi::execute(args).await,
        Command::ImageInspect(args) => image_inspect::execute(args).await,
        Command::ImagePrune(args) => image_prune::execute(args).await,
        Command::Tag(args) => image_tag::execute(args).await,
        Command::Cp(args) => cp::execute(args).await,
        Command::SystemPrune(args) => system_prune::execute(args).await,
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
