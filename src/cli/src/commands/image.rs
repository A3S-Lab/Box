//! `a3s-box image` subcommands — Docker-compatible image command namespace.

use clap::{Args, Subcommand};

#[derive(Args)]
pub struct ImageArgs {
    #[command(subcommand)]
    pub command: ImageCommand,
}

#[derive(Subcommand)]
pub enum ImageCommand {
    /// List cached images
    #[command(alias = "list")]
    Ls(super::images::ImagesArgs),
    /// Pull an image from a registry
    Pull(super::pull::PullArgs),
    /// Push an image to a registry
    Push(super::push::PushArgs),
    /// Remove one or more cached images
    #[command(alias = "remove")]
    Rm(super::rmi::RmiArgs),
    /// Display detailed image information as JSON
    Inspect(super::image_inspect::ImageInspectArgs),
    /// Show image layer history
    History(super::history::HistoryArgs),
    /// Remove unused images
    Prune(super::image_prune::ImagePruneArgs),
    /// Create a tag that refers to an existing image
    Tag(super::image_tag::ImageTagArgs),
    /// Save an image to a tar archive
    Save(super::save::SaveArgs),
    /// Load an image from a tar archive
    Load(super::load::LoadArgs),
}

pub async fn execute(args: ImageArgs) -> Result<(), Box<dyn std::error::Error>> {
    match args.command {
        ImageCommand::Ls(a) => super::images::execute(a).await,
        ImageCommand::Pull(a) => super::pull::execute(a).await,
        ImageCommand::Push(a) => super::push::execute(a).await,
        ImageCommand::Rm(a) => super::rmi::execute(a).await,
        ImageCommand::Inspect(a) => super::image_inspect::execute(a).await,
        ImageCommand::History(a) => super::history::execute(a).await,
        ImageCommand::Prune(a) => super::image_prune::execute(a).await,
        ImageCommand::Tag(a) => super::image_tag::execute(a).await,
        ImageCommand::Save(a) => super::save::execute(a).await,
        ImageCommand::Load(a) => super::load::execute(a).await,
    }
}
