//! `a3s-box images` command.

use clap::Args;

use crate::output;

use super::images_dir;

#[derive(Args)]
pub struct ImagesArgs {
    /// Only show image references (one per line)
    #[arg(short, long)]
    pub quiet: bool,

    /// Filter output using Docker-style KEY=VALUE filters
    ///
    /// Supported keys: reference, digest, dangling.
    #[arg(short = 'f', long = "filter", value_name = "KEY=VALUE")]
    pub filters: Vec<String>,

    /// Format output using placeholders: {{.Repository}}, {{.Tag}}, {{.Digest}},
    /// {{.Size}}, {{.Pulled}}, {{.Reference}}
    #[arg(long)]
    pub format: Option<String>,
}

pub async fn execute(args: ImagesArgs) -> Result<(), Box<dyn std::error::Error>> {
    let filters = parse_image_filters(&args.filters)?;
    let images_dir = images_dir();
    if !images_dir.exists() {
        if !args.quiet && args.format.is_none() {
            let table = output::new_table(&["REPOSITORY", "TAG", "DIGEST", "SIZE", "PULLED"]);
            println!("{table}");
        }
        return Ok(());
    }

    let store = super::open_image_store()?;
    let images: Vec<_> = store
        .list()
        .await
        .into_iter()
        .filter(|image| image_matches_filters(image, &filters))
        .collect();

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

#[derive(Debug, Clone, PartialEq, Eq)]
enum ImageFilter {
    Reference(String),
    Digest(String),
    Dangling(bool),
}

fn parse_image_filters(filters: &[String]) -> Result<Vec<ImageFilter>, String> {
    filters
        .iter()
        .map(|filter| {
            let (key, value) = filter
                .split_once('=')
                .ok_or_else(|| format!("Invalid image filter '{filter}': expected KEY=VALUE"))?;
            match key {
                "reference" => Ok(ImageFilter::Reference(value.to_string())),
                "digest" => Ok(ImageFilter::Digest(value.to_string())),
                "dangling" => match value {
                    "true" => Ok(ImageFilter::Dangling(true)),
                    "false" => Ok(ImageFilter::Dangling(false)),
                    _ => Err(format!(
                        "Invalid dangling filter value '{value}': expected true or false"
                    )),
                },
                _ => Err(format!(
                    "Unsupported image filter '{key}': supported filters are reference, digest, dangling"
                )),
            }
        })
        .collect()
}

fn image_matches_filters(image: &a3s_box_runtime::StoredImage, filters: &[ImageFilter]) -> bool {
    if filters.is_empty() {
        return true;
    }

    let reference_filters: Vec<&str> = filters
        .iter()
        .filter_map(|filter| match filter {
            ImageFilter::Reference(value) => Some(value.as_str()),
            _ => None,
        })
        .collect();
    if !reference_filters.is_empty()
        && !reference_filters
            .iter()
            .any(|pattern| image_matches_reference_filter(image, pattern))
    {
        return false;
    }

    let digest_filters: Vec<&str> = filters
        .iter()
        .filter_map(|filter| match filter {
            ImageFilter::Digest(value) => Some(value.as_str()),
            _ => None,
        })
        .collect();
    if !digest_filters.is_empty()
        && !digest_filters
            .iter()
            .any(|digest| image.digest == *digest || image.digest.starts_with(*digest))
    {
        return false;
    }

    let dangling_filters: Vec<bool> = filters
        .iter()
        .filter_map(|filter| match filter {
            ImageFilter::Dangling(value) => Some(*value),
            _ => None,
        })
        .collect();
    if !dangling_filters.is_empty()
        && !dangling_filters
            .iter()
            .any(|expected| image_is_dangling(image) == *expected)
    {
        return false;
    }

    true
}

fn image_matches_reference_filter(image: &a3s_box_runtime::StoredImage, pattern: &str) -> bool {
    image_reference_candidates(image)
        .iter()
        .any(|candidate| wildcard_match(pattern, candidate))
}

fn image_reference_candidates(image: &a3s_box_runtime::StoredImage) -> Vec<String> {
    let mut candidates = vec![
        image.reference.clone(),
        image.digest.clone(),
        format!("{}@{}", image.reference, image.digest),
    ];

    if let Ok(reference) = a3s_box_runtime::ImageReference::parse(&image.reference) {
        let full = reference.full_reference();
        let repository = format!("{}/{}", reference.registry, reference.repository);
        let short_repository = reference
            .repository
            .strip_prefix("library/")
            .unwrap_or(&reference.repository);

        candidates.push(full.clone());
        candidates.push(repository.clone());
        candidates.push(reference.repository.clone());
        candidates.push(short_repository.to_string());
        candidates.push(format!("{}@{}", full, image.digest));
        candidates.push(format!("{}@{}", repository, image.digest));
        candidates.push(format!("{}@{}", reference.repository, image.digest));
        candidates.push(format!("{}@{}", short_repository, image.digest));

        if let Some(tag) = reference.tag {
            candidates.push(format!("{repository}:{tag}"));
            candidates.push(format!("{}:{tag}", reference.repository));
            candidates.push(format!("{short_repository}:{tag}"));
        }
    }

    candidates.sort();
    candidates.dedup();
    candidates
}

fn image_is_dangling(image: &a3s_box_runtime::StoredImage) -> bool {
    image.reference == "<none>"
        || image.reference.contains("<none>:<none>")
        || a3s_box_runtime::ImageReference::parse(&image.reference)
            .map(|reference| reference.tag.is_none() && reference.digest.is_none())
            .unwrap_or(false)
}

fn wildcard_match(pattern: &str, value: &str) -> bool {
    if !pattern.contains('*') && !pattern.contains('?') {
        return value == pattern;
    }
    wildcard_match_inner(pattern.as_bytes(), value.as_bytes())
}

fn wildcard_match_inner(pattern: &[u8], value: &[u8]) -> bool {
    match pattern.split_first() {
        None => value.is_empty(),
        Some((b'*', rest)) => {
            wildcard_match_inner(rest, value)
                || value
                    .split_first()
                    .is_some_and(|(_, value_rest)| wildcard_match_inner(pattern, value_rest))
        }
        Some((b'?', rest)) => value
            .split_first()
            .is_some_and(|(_, value_rest)| wildcard_match_inner(rest, value_rest)),
        Some((expected, rest)) => value.split_first().is_some_and(|(actual, value_rest)| {
            actual == expected && wildcard_match_inner(rest, value_rest)
        }),
    }
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

    // --- Image filter tests ---

    #[test]
    fn test_parse_image_filter_reference() {
        let filters = parse_image_filters(&["reference=nginx".to_string()]).unwrap();
        assert_eq!(filters, vec![ImageFilter::Reference("nginx".to_string())]);
    }

    #[test]
    fn test_parse_image_filter_rejects_invalid_form() {
        let err = parse_image_filters(&["reference".to_string()]).unwrap_err();
        assert!(err.contains("expected KEY=VALUE"));
    }

    #[test]
    fn test_parse_image_filter_rejects_unsupported_key() {
        let err = parse_image_filters(&["label=app".to_string()]).unwrap_err();
        assert!(err.contains("Unsupported image filter"));
    }

    #[test]
    fn test_reference_filter_matches_docker_short_name() {
        let stored = sample_stored(
            "docker.io/library/nginx:latest",
            "sha256:abcdef1234567890",
            100,
        );

        assert!(image_matches_filters(
            &stored,
            &[ImageFilter::Reference("nginx".to_string())]
        ));
        assert!(image_matches_filters(
            &stored,
            &[ImageFilter::Reference("nginx:latest".to_string())]
        ));
        assert!(!image_matches_filters(
            &stored,
            &[ImageFilter::Reference("redis".to_string())]
        ));
    }

    #[test]
    fn test_reference_filter_supports_globs() {
        let stored = sample_stored("ghcr.io/acme/api:v1", "sha256:abcdef1234567890", 100);

        assert!(image_matches_filters(
            &stored,
            &[ImageFilter::Reference("ghcr.io/acme/*:v1".to_string())]
        ));
        assert!(image_matches_filters(
            &stored,
            &[ImageFilter::Reference("*/api:v?".to_string())]
        ));
    }

    #[test]
    fn test_repeated_reference_filters_are_or_matched() {
        let stored = sample_stored("nginx:latest", "sha256:abcdef1234567890", 100);

        assert!(image_matches_filters(
            &stored,
            &[
                ImageFilter::Reference("redis".to_string()),
                ImageFilter::Reference("nginx".to_string())
            ]
        ));
    }

    #[test]
    fn test_digest_filter_matches_prefix() {
        let stored = sample_stored("nginx:latest", "sha256:abcdef1234567890", 100);

        assert!(image_matches_filters(
            &stored,
            &[ImageFilter::Digest("sha256:abcdef".to_string())]
        ));
        assert!(!image_matches_filters(
            &stored,
            &[ImageFilter::Digest("sha256:999999".to_string())]
        ));
    }

    #[test]
    fn test_dangling_filter_matches_none_reference() {
        let dangling = sample_stored("<none>:<none>", "sha256:abcdef1234567890", 100);
        let tagged = sample_stored("nginx:latest", "sha256:abcdef1234567890", 100);

        assert!(image_matches_filters(
            &dangling,
            &[ImageFilter::Dangling(true)]
        ));
        assert!(image_matches_filters(
            &tagged,
            &[ImageFilter::Dangling(false)]
        ));
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
