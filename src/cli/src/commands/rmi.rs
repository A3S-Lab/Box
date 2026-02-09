//! `a3s-box rmi` command — remove one or more cached images.

use clap::Args;

#[derive(Args)]
pub struct RmiArgs {
    /// Image references to remove
    #[arg(required = true)]
    pub images: Vec<String>,

    /// Force removal (ignore not-found errors)
    #[arg(short, long)]
    pub force: bool,
}

pub async fn execute(args: RmiArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = super::open_image_store()?;

    let mut errors: Vec<String> = Vec::new();

    for reference in &args.images {
        match store.remove(reference).await {
            Ok(()) => {
                println!("Removed: {reference}");
            }
            Err(e) => {
                if args.force {
                    // Silently skip not-found errors in force mode
                    continue;
                }
                errors.push(format!("{reference}: {e}"));
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        let msg = errors.join("\n");
        Err(format!("Failed to remove image(s):\n{msg}").into())
    }
}
