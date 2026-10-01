//! `a3s-box load` command — Load an image from a tar archive.

use a3s_box_core::error::BoxError;
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

pub async fn execute(args: LoadArgs) -> Result<(), BoxError> {
    let store = super::open_image_store()?;

    // Extract tar to a temporary directory
    let tmp_dir =
        tempfile::tempdir().map_err(|e| super::io_error("Failed to create temp directory", e))?;

    let file = open_load_archive(std::path::Path::new(&args.input))?;
    let mut archive = tar::Archive::new(file);
    archive
        .unpack(tmp_dir.path())
        .map_err(|e| super::io_error("Failed to extract archive", e))?;

    // Resolve a direct manifest or a nested multi-platform index before the
    // layout becomes visible in the persistent store. Every downstream image
    // consumer expects index.json to point at an image manifest.
    let prepared = layout::prepare(
        tmp_dir.path(),
        args.platform.as_deref(),
        args.tag.as_deref(),
    )
    .map_err(BoxError::OciImageError)?;

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

fn open_load_archive(path: &std::path::Path) -> Result<std::fs::File, BoxError> {
    super::commit::refuse_archive_ancestor_reparse(path)?;
    std::fs::File::open(path)
        .map_err(|error| super::io_error(format!("Failed to open {}", path.display()), error))
}

#[cfg(test)]
mod tests {
    use super::open_load_archive;

    #[cfg(windows)]
    #[test]
    fn open_load_archive_does_not_follow_an_ancestor_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = tempfile::TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("image.tar"), b"secret").unwrap();
        let parent = tmp.path().join("parent");
        std::fs::create_dir_all(&parent).unwrap();
        let link = parent.join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let error = open_load_archive(&link.join("image.tar"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("junction"),
            "image archive was opened through an ancestor junction: {error}"
        );
        assert_eq!(std::fs::read(outside.join("image.tar")).unwrap(), b"secret");
    }
}
