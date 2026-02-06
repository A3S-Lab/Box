//! `a3s-box images` command.

use clap::Args;

use crate::output;

#[derive(Args)]
pub struct ImagesArgs;

pub async fn execute(_args: ImagesArgs) -> Result<(), Box<dyn std::error::Error>> {
    let home = dirs::home_dir()
        .map(|h| h.join(".a3s"))
        .unwrap_or_else(|| std::path::PathBuf::from(".a3s"));

    let images_dir = home.join("images");
    if !images_dir.exists() {
        let table = output::new_table(&["REPOSITORY", "TAG", "DIGEST", "SIZE", "PULLED"]);
        println!("{table}");
        return Ok(());
    }

    let store = a3s_box_runtime::ImageStore::new(&images_dir, 10 * 1024 * 1024 * 1024)?;
    let images = store.list().await;

    let mut table = output::new_table(&["REPOSITORY", "TAG", "DIGEST", "SIZE", "PULLED"]);

    for image in &images {
        // Parse reference into repository and tag
        let (repo, tag) = match image.reference.rsplit_once(':') {
            Some((r, t)) => (r, t),
            None => (image.reference.as_str(), "latest"),
        };

        // Truncate digest for display
        let short_digest = if image.digest.len() > 19 {
            &image.digest[..19]
        } else {
            &image.digest
        };

        table.add_row(&[
            repo,
            tag,
            short_digest,
            &output::format_bytes(image.size_bytes),
            &output::format_ago(&image.pulled_at),
        ]);
    }

    println!("{table}");
    Ok(())
}
