//! `a3s-box load` command — Load an image from a tar archive.

use clap::Args;

mod layout;

#[derive(Args)]
pub struct LoadArgs {
    /// Input tar file path
    #[arg(short, long)]
    pub input: String,

    /// Tag to assign to the loaded image
    #[arg(short, long)]
    pub tag: Option<String>,

    /// Select the Linux platform from an indexed OCI archive (defaults to the host architecture)
    #[arg(long, value_name = "OS/ARCH[/VARIANT]")]
    pub platform: Option<String>,
}

pub async fn execute(args: LoadArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = super::open_image_store()?;

    // Extract tar to a temporary directory
    let tmp_dir =
        tempfile::tempdir().map_err(|e| format!("Failed to create temp directory: {e}"))?;

    let file = std::fs::File::open(&args.input)
        .map_err(|e| format!("Failed to open {}: {e}", args.input))?;
    let mut archive = tar::Archive::new(file);
    archive
        .unpack(tmp_dir.path())
        .map_err(|e| format!("Failed to extract archive: {e}"))?;

    // Resolve a direct manifest or a nested multi-platform index before the
    // layout becomes visible in the persistent store. Every downstream image
    // consumer expects index.json to point at an image manifest.
    let prepared = layout::prepare(
        tmp_dir.path(),
        args.platform.as_deref(),
        args.tag.as_deref(),
    )?;

    let stored = store
        .put(&prepared.reference, &prepared.digest, tmp_dir.path())
        .await?;

    println!(
        "Loaded image: {} ({})",
        stored.reference,
        crate::output::format_bytes(stored.size_bytes)
    );
    Ok(())
}
