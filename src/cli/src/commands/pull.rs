//! `a3s-box pull` command.

use std::sync::Arc;

use clap::Args;

#[derive(Args)]
pub struct PullArgs {
    /// Image reference (e.g., "alpine:latest", "ghcr.io/org/image:tag")
    pub image: String,
}

pub async fn execute(args: PullArgs) -> Result<(), Box<dyn std::error::Error>> {
    let home = dirs::home_dir()
        .map(|h| h.join(".a3s"))
        .unwrap_or_else(|| std::path::PathBuf::from(".a3s"));

    let images_dir = home.join("images");
    let store = Arc::new(a3s_box_runtime::ImageStore::new(
        &images_dir,
        10 * 1024 * 1024 * 1024,
    )?);

    let puller = a3s_box_runtime::ImagePuller::new(
        store,
        a3s_box_runtime::RegistryAuth::from_env(),
    );

    println!("Pulling {}...", args.image);
    let image = puller.pull(&args.image).await?;

    println!(
        "Pulled: {} ({})",
        args.image,
        image.root_dir().display()
    );

    Ok(())
}
