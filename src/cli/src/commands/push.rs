//! `a3s-box push` command — Push a local image to a registry.

use std::sync::Arc;

use clap::Args;

#[derive(Args)]
pub struct PushArgs {
    /// Image reference (e.g., "ghcr.io/org/image:tag")
    pub image: String,

    /// Suppress progress output
    #[arg(short, long)]
    pub quiet: bool,
}

pub async fn execute(args: PushArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = Arc::new(super::open_image_store()?);

    // Parse the target reference
    let reference = a3s_box_runtime::ImageReference::parse(&args.image)?;

    // Look up the image in the local store
    let stored = store.get(&args.image).await.ok_or_else(|| {
        format!(
            "Image '{}' not found locally. Pull or build it first.",
            args.image
        )
    })?;

    if !args.quiet {
        println!("Pushing {}...", args.image);
    }

    // Load auth from credential store (falls back to env vars, then anonymous)
    let auth = a3s_box_runtime::RegistryAuth::from_credential_store(&reference.registry);
    let pusher = a3s_box_runtime::RegistryPusher::with_auth(auth);

    let result = pusher.push(&reference, &stored.path).await?;

    if args.quiet {
        println!("{}", result.manifest_url);
    } else {
        println!("Pushed: {} ({})", args.image, result.manifest_url);
    }

    Ok(())
}
