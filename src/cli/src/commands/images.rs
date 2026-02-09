//! `a3s-box images` command.

use clap::Args;

use crate::output;

use super::images_dir;

#[derive(Args)]
pub struct ImagesArgs {
    /// Only show image references (one per line)
    #[arg(short, long)]
    pub quiet: bool,

    /// Format output using placeholders: {{.Repository}}, {{.Tag}}, {{.Digest}},
    /// {{.Size}}, {{.Pulled}}, {{.Reference}}
    #[arg(long)]
    pub format: Option<String>,
}

pub async fn execute(args: ImagesArgs) -> Result<(), Box<dyn std::error::Error>> {
    let images_dir = images_dir();
    if !images_dir.exists() {
        if !args.quiet && args.format.is_none() {
            let table = output::new_table(&["REPOSITORY", "TAG", "DIGEST", "SIZE", "PULLED"]);
            println!("{table}");
        }
        return Ok(());
    }

    let store = super::open_image_store()?;
    let images = store.list().await;

    // --quiet: print only references
    if args.quiet {
        for image in &images {
            println!("{}", image.reference);
        }
        return Ok(());
    }

    // Pre-compute display fields for each image
    let rows: Vec<ImageRow> = images.iter().map(ImageRow::from_stored).collect();

    // --format: custom template output
    if let Some(ref fmt) = args.format {
        for row in &rows {
            println!("{}", row.apply_format(fmt));
        }
        return Ok(());
    }

    // Default: table output
    let mut table = output::new_table(&["REPOSITORY", "TAG", "DIGEST", "SIZE", "PULLED"]);
    for row in &rows {
        table.add_row(&[
            &row.repository,
            &row.tag,
            &row.digest,
            &row.size,
            &row.pulled,
        ]);
    }

    println!("{table}");
    Ok(())
}

/// Pre-computed display fields for a single image row.
struct ImageRow {
    reference: String,
    repository: String,
    tag: String,
    digest: String,
    size: String,
    pulled: String,
}

impl ImageRow {
    fn from_stored(image: &a3s_box_runtime::StoredImage) -> Self {
        let (repository, tag) = match a3s_box_runtime::ImageReference::parse(&image.reference) {
            Ok(r) => {
                let repo = format!("{}/{}", r.registry, r.repository);
                let tag = r.tag.unwrap_or_else(|| "<none>".to_string());
                (repo, tag)
            }
            Err(_) => (image.reference.clone(), "<none>".to_string()),
        };

        // Format digest: "sha256:" prefix + first 12 hex chars
        let digest = if let Some(hex) = image.digest.strip_prefix("sha256:") {
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

        Self {
            reference: image.reference.clone(),
            repository,
            tag,
            digest,
            size: output::format_bytes(image.size_bytes),
            pulled: output::format_ago(&image.pulled_at),
        }
    }

    /// Apply a format template, replacing `{{.Field}}` placeholders.
    fn apply_format(&self, fmt: &str) -> String {
        fmt.replace("{{.Repository}}", &self.repository)
            .replace("{{.Tag}}", &self.tag)
            .replace("{{.Digest}}", &self.digest)
            .replace("{{.Size}}", &self.size)
            .replace("{{.Pulled}}", &self.pulled)
            .replace("{{.Reference}}", &self.reference)
    }
}
