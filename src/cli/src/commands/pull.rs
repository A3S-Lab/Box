//! `a3s-box pull` command.

use std::sync::Arc;

use clap::Args;

#[derive(Args)]
pub struct PullArgs {
    /// Image reference (e.g., "alpine:latest", "ghcr.io/org/image:tag")
    pub image: String,

    /// Suppress progress output
    #[arg(short, long)]
    pub quiet: bool,

    /// Set target platform (e.g., "linux/amd64", "linux/arm64")
    #[arg(long)]
    pub platform: Option<String>,

    /// Verify image signature with a cosign public key file
    #[arg(long, value_name = "KEY_FILE")]
    pub verify_key: Option<String>,

    /// Verify image signature with keyless cosign (issuer and identity)
    #[arg(long, value_name = "ISSUER", requires = "verify_identity")]
    pub verify_issuer: Option<String>,

    /// Identity (email/URI) for keyless signature verification
    #[arg(long, value_name = "IDENTITY")]
    pub verify_identity: Option<String>,
}

pub async fn execute(args: PullArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = Arc::new(super::open_image_store()?);

    // Parse reference to determine registry for credential lookup
    let reference = a3s_box_runtime::ImageReference::parse(&args.image)?;
    let auth = a3s_box_runtime::RegistryAuth::from_credential_store(&reference.registry);

    // Honor `--platform` (e.g. "linux/arm64") so multi-arch image indexes
    // resolve to the requested architecture instead of the host's.
    let mut puller =
        a3s_box_runtime::ImagePuller::with_platform(store, auth, args.platform.clone());

    puller = puller.with_signature_policy(signature_policy_from_args(&args));

    if !args.quiet {
        println!("Pulling {}...", args.image);
        puller = puller.with_progress_event_fn(std::sync::Arc::new(|progress| {
            println!("{}", format_pull_progress_line(&progress));
        }));
    }
    let image = puller.pull(&args.image).await?;
    crate::audit::record(
        a3s_box_core::audit::AuditAction::ImagePull,
        a3s_box_core::audit::AuditOutcome::Success,
        &args.image,
        &format!("pulled image {}", args.image),
    );

    if args.quiet {
        println!("{}", image.root_dir().display());
    } else {
        println!("Pulled: {} ({})", args.image, image.root_dir().display());
    }

    Ok(())
}

fn signature_policy_from_args(args: &PullArgs) -> a3s_box_runtime::SignaturePolicy {
    if let Some(ref key_path) = args.verify_key {
        a3s_box_runtime::SignaturePolicy::CosignKey {
            public_key: key_path.clone(),
        }
    } else if let (Some(ref issuer), Some(ref identity)) =
        (&args.verify_issuer, &args.verify_identity)
    {
        a3s_box_runtime::SignaturePolicy::CosignKeyless {
            issuer: issuer.clone(),
            identity: identity.clone(),
        }
    } else {
        a3s_box_runtime::SignaturePolicy::Skip
    }
}

fn format_pull_progress_line(progress: &a3s_box_runtime::PullProgress) -> String {
    use a3s_box_runtime::PullProgressState;

    let digest = &progress.digest;
    let short = &digest[digest.len().saturating_sub(12)..];
    let prefix = format!(
        "  [{}/{}] {short}",
        progress.current_layer, progress.total_layers
    );
    match progress.state {
        PullProgressState::Downloading => format!(
            "{prefix}: {} / {} downloaded",
            format_layer_size(progress.downloaded_bytes),
            format_layer_size(progress.total_bytes)
        ),
        PullProgressState::Retrying => format!(
            "{prefix}: retry {}/{} from {} / {} after {} ms",
            progress.attempt,
            progress.max_attempts,
            format_layer_size(progress.downloaded_bytes),
            format_layer_size(progress.total_bytes),
            progress.retry_delay_ms.unwrap_or(0)
        ),
        PullProgressState::Reused => {
            format!(
                "{prefix}: {} reused ✓",
                format_layer_size(progress.total_bytes)
            )
        }
        PullProgressState::Complete => format!(
            "{prefix}: {} / {} downloaded ✓",
            format_layer_size(progress.downloaded_bytes),
            format_layer_size(progress.total_bytes)
        ),
    }
}

fn format_layer_size(size: u64) -> String {
    if size >= 1_048_576 {
        format!("{:.1} MB", size as f64 / 1_048_576.0)
    } else if size >= 1024 {
        format!("{:.1} KB", size as f64 / 1024.0)
    } else {
        format!("{} B", size)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args() -> PullArgs {
        PullArgs {
            image: "docker.io/library/alpine:latest".to_string(),
            quiet: false,
            platform: None,
            verify_key: None,
            verify_issuer: None,
            verify_identity: None,
        }
    }

    #[test]
    fn signature_policy_defaults_to_skip() {
        assert_eq!(
            signature_policy_from_args(&args()),
            a3s_box_runtime::SignaturePolicy::Skip
        );
    }

    #[test]
    fn signature_policy_uses_key_before_keyless_options() {
        let mut args = args();
        args.verify_key = Some("/tmp/cosign.pub".to_string());
        args.verify_issuer = Some("https://issuer.example".to_string());
        args.verify_identity = Some("builder@example.com".to_string());

        assert_eq!(
            signature_policy_from_args(&args),
            a3s_box_runtime::SignaturePolicy::CosignKey {
                public_key: "/tmp/cosign.pub".to_string()
            }
        );
    }

    #[test]
    fn signature_policy_uses_keyless_when_issuer_and_identity_are_present() {
        let mut args = args();
        args.verify_issuer = Some("https://issuer.example".to_string());
        args.verify_identity = Some("builder@example.com".to_string());

        assert_eq!(
            signature_policy_from_args(&args),
            a3s_box_runtime::SignaturePolicy::CosignKeyless {
                issuer: "https://issuer.example".to_string(),
                identity: "builder@example.com".to_string(),
            }
        );
    }

    #[test]
    fn signature_policy_skips_incomplete_keyless_options() {
        let mut args = args();
        args.verify_issuer = Some("https://issuer.example".to_string());

        assert_eq!(
            signature_policy_from_args(&args),
            a3s_box_runtime::SignaturePolicy::Skip
        );
    }

    #[test]
    fn format_layer_size_selects_bytes_kilobytes_and_megabytes() {
        assert_eq!(format_layer_size(999), "999 B");
        assert_eq!(format_layer_size(1536), "1.5 KB");
        assert_eq!(format_layer_size(1_572_864), "1.5 MB");
    }

    #[test]
    fn format_pull_progress_line_truncates_digest_and_marks_completion() {
        assert_eq!(
            format_pull_progress_line(&a3s_box_runtime::PullProgress {
                current_layer: 2,
                total_layers: 5,
                digest: "sha256:0123456789abcdef".to_string(),
                downloaded_bytes: 2048,
                total_bytes: 4096,
                attempt: 1,
                max_attempts: 4,
                retry_delay_ms: None,
                state: a3s_box_runtime::PullProgressState::Downloading,
            }),
            "  [2/5] 456789abcdef: 2.0 KB / 4.0 KB downloaded"
        );
        assert_eq!(
            format_pull_progress_line(&a3s_box_runtime::PullProgress {
                current_layer: 2,
                total_layers: 5,
                digest: "abc".to_string(),
                downloaded_bytes: 512,
                total_bytes: 512,
                attempt: 2,
                max_attempts: 4,
                retry_delay_ms: None,
                state: a3s_box_runtime::PullProgressState::Complete,
            }),
            "  [2/5] abc: 512 B / 512 B downloaded ✓"
        );
    }
}
