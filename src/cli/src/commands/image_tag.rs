//! `a3s-box tag` command — create a tag that refers to an existing image.

use clap::Args;

#[derive(Args)]
pub struct ImageTagArgs {
    /// Source image reference
    pub source: String,

    /// Target image reference (new tag)
    pub target: String,
}

pub async fn execute(args: ImageTagArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = super::open_image_store()?;

    let source = store
        .get(&args.source)
        .await
        .ok_or_else(|| format!("Image not found: {}", args.source))?;

    // Store with new reference pointing to the same digest directory (no disk copy)
    store
        .put(&args.target, &source.digest, &source.path)
        .await?;

    println!("{}", args.target);
    Ok(())
}
