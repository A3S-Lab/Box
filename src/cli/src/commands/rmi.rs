//! `a3s-box rmi` command.

use clap::Args;

#[derive(Args)]
pub struct RmiArgs {
    /// Image reference to remove
    pub image: String,
}

pub async fn execute(args: RmiArgs) -> Result<(), Box<dyn std::error::Error>> {
    let home = dirs::home_dir()
        .map(|h| h.join(".a3s"))
        .unwrap_or_else(|| std::path::PathBuf::from(".a3s"));

    let images_dir = home.join("images");
    let store = a3s_box_runtime::ImageStore::new(&images_dir, 10 * 1024 * 1024 * 1024)?;

    store.remove(&args.image).await?;
    println!("Removed: {}", args.image);

    Ok(())
}
