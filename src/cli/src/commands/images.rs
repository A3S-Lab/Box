//! `a3s-box images` command.

use clap::Args;

use crate::output;

use super::images_dir;

#[derive(Args)]
pub struct ImagesArgs {
    /// Only show image references (one per line)
    #[arg(short, long)]
    pub quiet: bool,
}

pub async fn execute(args: ImagesArgs) -> Result<(), Box<dyn std::error::Error>> {
    let images_dir = images_dir();
    if !images_dir.exists() {
        if !args.quiet {
            let table = output::new_table(&["REPOSITORY", "TAG", "DIGEST", "SIZE", "PULLED"]);
            println!("{table}");
        }
        return Ok(());
    }

    let store = super::open_image_store()?;
    let images = store.list().await;

    if args.quiet {
        for image in &images {
            println!("{}", image.reference);
        }
        return Ok(());
    }

    let mut table = output::new_table(&["REPOSITORY", "TAG", "DIGEST", "SIZE", "PULLED"]);

    for image in &images {
        // Parse reference with ImageReference for proper repo/tag splitting
        let (repo, tag) = match a3s_box_runtime::ImageReference::parse(&image.reference) {
            Ok(r) => {
                let repo = format!("{}/{}", r.registry, r.repository);
                let tag = r.tag.unwrap_or_else(|| "<none>".to_string());
                (repo, tag)
            }
            Err(_) => (image.reference.clone(), "<none>".to_string()),
        };

        // Format digest: "sha256:" prefix + first 12 hex chars
        let short_digest = if let Some(hex) = image.digest.strip_prefix("sha256:") {
            let truncated = if hex.len() > 12 { &hex[..12] } else { hex };
            format!("sha256:{truncated}")
        } else {
            let truncated = if image.digest.len() > 12 {
                &image.digest[..12]
            } else {
                &image.digest
            };
            truncated.to_string()
        };

        table.add_row(&[
            &repo,
            &tag,
            &short_digest,
            &output::format_bytes(image.size_bytes),
            &output::format_ago(&image.pulled_at),
        ]);
    }

    println!("{table}");
    Ok(())
}
