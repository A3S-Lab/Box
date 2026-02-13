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
        table.add_row([
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

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_runtime::StoredImage;
    use chrono::Utc;
    use std::path::PathBuf;

    fn sample_stored(reference: &str, digest: &str, size: u64) -> StoredImage {
        StoredImage {
            reference: reference.to_string(),
            digest: digest.to_string(),
            size_bytes: size,
            pulled_at: Utc::now(),
            last_used: Utc::now(),
            path: PathBuf::from("/tmp/test"),
        }
    }

    // --- ImageRow::from_stored tests ---

    #[test]
    fn test_from_stored_simple_name() {
        let stored = sample_stored("nginx:1.25", "sha256:abcdef1234567890abcdef", 1024);
        let row = ImageRow::from_stored(&stored);

        assert_eq!(row.repository, "docker.io/library/nginx");
        assert_eq!(row.tag, "1.25");
        assert_eq!(row.digest, "sha256:abcdef123456");
        assert_eq!(row.reference, "nginx:1.25");
    }

    #[test]
    fn test_from_stored_custom_registry() {
        let stored = sample_stored(
            "ghcr.io/a3s-box/code:v0.1.0",
            "sha256:aabbccdd11223344aabbccdd",
            2048,
        );
        let row = ImageRow::from_stored(&stored);

        assert_eq!(row.repository, "ghcr.io/a3s-box/code");
        assert_eq!(row.tag, "v0.1.0");
        assert_eq!(row.digest, "sha256:aabbccdd1122");
    }

    #[test]
    fn test_from_stored_no_tag_defaults_latest() {
        let stored = sample_stored("alpine", "sha256:1234567890ab", 512);
        let row = ImageRow::from_stored(&stored);

        assert_eq!(row.repository, "docker.io/library/alpine");
        assert_eq!(row.tag, "latest");
    }

    #[test]
    fn test_from_stored_digest_short() {
        let stored = sample_stored("nginx:latest", "sha256:abcd", 100);
        let row = ImageRow::from_stored(&stored);

        // Short digest should not be truncated
        assert_eq!(row.digest, "sha256:abcd");
    }

    #[test]
    fn test_from_stored_digest_no_prefix() {
        let stored = sample_stored("nginx:latest", "abcdef1234567890", 100);
        let row = ImageRow::from_stored(&stored);

        // Without sha256: prefix, truncate to 12 chars
        assert_eq!(row.digest, "abcdef123456");
    }

    #[test]
    fn test_from_stored_digest_exactly_12() {
        let stored = sample_stored("nginx:latest", "sha256:abcdef123456", 100);
        let row = ImageRow::from_stored(&stored);

        assert_eq!(row.digest, "sha256:abcdef123456");
    }

    #[test]
    fn test_from_stored_invalid_reference_fallback() {
        // Empty reference should fail to parse, falling back to raw reference
        let stored = sample_stored("", "sha256:abc", 100);
        let row = ImageRow::from_stored(&stored);

        assert_eq!(row.repository, "");
        assert_eq!(row.tag, "<none>");
    }

    // --- ImageRow::apply_format tests ---

    #[test]
    fn test_apply_format_repository() {
        let row = ImageRow {
            reference: "nginx:1.25".to_string(),
            repository: "docker.io/library/nginx".to_string(),
            tag: "1.25".to_string(),
            digest: "sha256:abcdef123456".to_string(),
            size: "1.0 KB".to_string(),
            pulled: "5 minutes ago".to_string(),
        };

        assert_eq!(
            row.apply_format("{{.Repository}}:{{.Tag}}"),
            "docker.io/library/nginx:1.25"
        );
    }

    #[test]
    fn test_apply_format_all_fields() {
        let row = ImageRow {
            reference: "nginx:1.25".to_string(),
            repository: "docker.io/library/nginx".to_string(),
            tag: "1.25".to_string(),
            digest: "sha256:abcdef123456".to_string(),
            size: "1.0 KB".to_string(),
            pulled: "5 minutes ago".to_string(),
        };

        let result = row.apply_format("{{.Reference}} {{.Digest}} {{.Size}} {{.Pulled}}");
        assert_eq!(
            result,
            "nginx:1.25 sha256:abcdef123456 1.0 KB 5 minutes ago"
        );
    }

    #[test]
    fn test_apply_format_no_placeholders() {
        let row = ImageRow {
            reference: "nginx:1.25".to_string(),
            repository: "docker.io/library/nginx".to_string(),
            tag: "1.25".to_string(),
            digest: "sha256:abcdef123456".to_string(),
            size: "1.0 KB".to_string(),
            pulled: "5 minutes ago".to_string(),
        };

        assert_eq!(row.apply_format("plain text"), "plain text");
    }

    #[test]
    fn test_apply_format_repeated_placeholder() {
        let row = ImageRow {
            reference: "nginx:1.25".to_string(),
            repository: "docker.io/library/nginx".to_string(),
            tag: "1.25".to_string(),
            digest: "sha256:abcdef123456".to_string(),
            size: "1.0 KB".to_string(),
            pulled: "5 minutes ago".to_string(),
        };

        assert_eq!(row.apply_format("{{.Tag}}-{{.Tag}}"), "1.25-1.25");
    }
}
