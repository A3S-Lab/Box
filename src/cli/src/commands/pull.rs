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
}

pub async fn execute(args: PullArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = Arc::new(super::open_image_store()?);

    let puller = a3s_box_runtime::ImagePuller::new(
        store,
        a3s_box_runtime::RegistryAuth::from_env(),
    );

    if !args.quiet {
        println!("Pulling {}...", args.image);
    }
    let image = puller.pull(&args.image).await?;

    if args.quiet {
        println!("{}", image.root_dir().display());
    } else {
        println!(
            "Pulled: {} ({})",
            args.image,
            image.root_dir().display()
        );
    }

    Ok(())
}
