//! `a3s-box save` command — Save an image to a tar archive.
//!
//! Creates a tar archive of the OCI image layout directory, suitable for
//! transferring to another machine and loading with `a3s-box load`.

use clap::Args;

#[derive(Args)]
pub struct SaveArgs {
    /// Image reference to save
    pub image: String,

    /// Output file path (e.g., "nginx.tar")
    #[arg(short, long)]
    pub output: String,
}

pub async fn execute(args: SaveArgs) -> Result<(), Box<dyn std::error::Error>> {
    let store = super::open_image_store()?;

    let stored = store
        .get(&args.image)
        .await
        .ok_or_else(|| format!("Image not found: {}", args.image))?;

    // Create tar archive of the OCI layout directory
    create_tar_archive(&stored.path, &args.output)?;

    let size = std::fs::metadata(&args.output)
        .map(|m| m.len())
        .unwrap_or(0);

    println!(
        "Saved {} to {} ({})",
        args.image,
        args.output,
        crate::output::format_bytes(size)
    );
    Ok(())
}

/// Create a tar archive from a directory.
fn create_tar_archive(
    src_dir: &std::path::Path,
    output_path: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let file = std::fs::File::create(output_path)
        .map_err(|e| format!("Failed to create {output_path}: {e}"))?;

    let mut builder = tar::Builder::new(file);
    builder
        .append_dir_all(".", src_dir)
        .map_err(|e| format!("Failed to archive image: {e}"))?;
    builder
        .finish()
        .map_err(|e| format!("Failed to finalize archive: {e}"))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_create_tar_archive() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let output = dst.path().join("test.tar");

        // Create test content
        fs::write(src.path().join("file1.txt"), "hello").unwrap();
        fs::create_dir(src.path().join("subdir")).unwrap();
        fs::write(src.path().join("subdir").join("file2.txt"), "world").unwrap();

        create_tar_archive(src.path(), output.to_str().unwrap()).unwrap();

        assert!(output.exists());
        assert!(output.metadata().unwrap().len() > 0);
    }

    #[test]
    fn test_create_tar_archive_invalid_output() {
        let src = TempDir::new().unwrap();
        let result = create_tar_archive(src.path(), "/nonexistent/dir/test.tar");
        assert!(result.is_err());
    }

    #[test]
    fn test_create_tar_archive_roundtrip() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        let extract = TempDir::new().unwrap();
        let output = dst.path().join("test.tar");

        // Create test content
        fs::write(src.path().join("data.txt"), "test content").unwrap();

        // Archive
        create_tar_archive(src.path(), output.to_str().unwrap()).unwrap();

        // Extract and verify
        let file = fs::File::open(&output).unwrap();
        let mut archive = tar::Archive::new(file);
        archive.unpack(extract.path()).unwrap();

        let content = fs::read_to_string(extract.path().join("data.txt")).unwrap();
        assert_eq!(content, "test content");
    }
}
