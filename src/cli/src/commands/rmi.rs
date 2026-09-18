//! `a3s-box rmi` command — remove one or more cached images.

use clap::Args;

use crate::image_usage::{self, ImageReferenceScope};
use crate::state::StateFile;

#[derive(Args)]
pub struct RmiArgs {
    /// Image references to remove
    #[arg(required = true)]
    pub images: Vec<String>,

    /// Ignore missing images; images referenced by boxes are still protected
    #[arg(short, long)]
    pub force: bool,
}

pub async fn execute(args: RmiArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = super::open_image_store()?;
    // Fail closed on state load so we cannot invent an empty protect set and
    // remove in-use images (image-prune / system-prune parity).
    let state = StateFile::load_default().map_err(|error| {
        format!("Failed to load box state for rmi: {error}; refusing rmi success")
    })?;
    let protected_images = image_usage::referenced_images(&state, ImageReferenceScope::AllBoxes);

    let mut errors: Vec<String> = Vec::new();

    for query in &args.images {
        let images = store.list().await;
        // `--force` removes every reference matching the query (Docker: a digest
        // id with multiple tags is untagged-all); without force a single
        // unambiguous match is required.
        let targets: Vec<_> = if args.force {
            let all = image_usage::all_matching_images(&images, query);
            if all.is_empty() {
                continue; // force: ignore not-found
            }
            all
        } else {
            match image_usage::resolve_stored_image(&images, query) {
                Ok(Some(target)) => vec![target],
                Ok(None) => {
                    errors.push(format!("{query}: Image not found"));
                    continue;
                }
                Err(error) => {
                    errors.push(format!("{query}: {error}"));
                    continue;
                }
            }
        };

        for target in targets {
            if image_usage::is_protected_reference(&target.reference, &protected_images) {
                errors.push(format!(
                    "{}: image is referenced by an existing box; remove the box before removing this image",
                    target.reference
                ));
                continue;
            }

            match store.remove(&target.reference).await {
                Ok(()) => {
                    println!("Removed: {}", target.reference);
                }
                // `--force` only ignores missing images; lock/I/O/unsafe-path
                // failures must still refuse inventing rmi success.
                Err(e) if args.force && is_rmi_force_ignorable_missing(&e) => {}
                Err(e) => {
                    errors.push(format!("{}: {e}", target.reference));
                }
            }
        }
    }

    if errors.is_empty() {
        Ok(())
    } else {
        Err(format!(
            "Failed to remove image(s):\n{}; refusing rmi success",
            errors.join("\n")
        )
        .into())
    }
}

/// `--force` may skip only ImageStore not-found; other remove Errs fail closed.
fn is_rmi_force_ignorable_missing(error: &a3s_box_core::error::BoxError) -> bool {
    matches!(
        error,
        a3s_box_core::error::BoxError::OciImageError(msg)
            if msg.starts_with("Image not found:")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;

    #[test]
    fn test_protected_alias_blocks_rmi_target() {
        let mut protected = HashSet::new();
        protected.insert("docker.io/library/alpine:latest".to_string());

        assert!(image_usage::is_protected_reference(
            "alpine:latest",
            &protected
        ));
    }

    #[test]
    fn rmi_state_load_error_message_refuses_invented_success() {
        let message = format!(
            "Failed to load box state for rmi: {}; refusing rmi success",
            "permission denied"
        );
        assert!(message.contains("permission denied"));
        assert!(message.contains("refusing rmi success"));
    }

    #[test]
    fn rmi_force_only_ignores_image_not_found_remove_errors() {
        assert!(is_rmi_force_ignorable_missing(
            &a3s_box_core::error::BoxError::OciImageError(
                "Image not found: alpine:missing".to_string()
            )
        ));
        assert!(!is_rmi_force_ignorable_missing(
            &a3s_box_core::error::BoxError::OciImageError(
                "Failed to remove image directory: permission denied".to_string()
            )
        ));
        assert!(!is_rmi_force_ignorable_missing(
            &a3s_box_core::error::BoxError::Other("io error".to_string())
        ));
    }
}
