//! `a3s-box pull` command.

use std::sync::Arc;

use clap::Args;

#[derive(Args)]
pub struct PullArgs {
    /// Image reference (e.g., "alpine:latest", "ghcr.io/org/image:tag")
    pub image: String,

    /// Suppress progress output
    #[arg(short, long)]
    pub quiet: bool,

    /// Set target platform (e.g., "linux/amd64", "linux/arm64")
    #[arg(long)]
    pub platform: Option<String>,
}

pub async fn execute(args: PullArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = Arc::new(super::open_image_store()?);

    // Parse reference to determine registry for credential lookup
    let reference = a3s_box_runtime::ImageReference::parse(&args.image)?;
    let auth = a3s_box_runtime::RegistryAuth::from_credential_store(&reference.registry);

    let puller = a3s_box_runtime::ImagePuller::new(store, auth);

    if !args.quiet {
        println!("Pulling {}...", args.image);
    }
    let image = puller.pull(&args.image).await?;

    if args.quiet {
        println!("{}", image.root_dir().display());
    } else {
        println!("Pulled: {} ({})", args.image, image.root_dir().display());
    }

    Ok(())
}
