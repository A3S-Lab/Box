//! `a3s-box save` command — Save an image to a tar archive.
//!
//! Creates a tar archive of the OCI image layout directory, suitable for
//! transferring to another machine and loading with `a3s-box load`.

use a3s_box_core::error::BoxError;
use clap::Args;

use crate::image_usage;

#[derive(Args)]
pub struct SaveArgs {
    /// Image reference to save
    pub image: String,

    /// Output file path (e.g., "nginx.tar")
    #[arg(short, long)]
    pub output: String,
}

pub async fn execute(args: SaveArgs) -> Result<(), BoxError> {
    let store = super::open_image_store()?;

    let images = store.list().await;
    let stored = image_usage::resolve_required_stored_image(&images, &args.image)
        .map_err(BoxError::OciImageError)?;

    // Stage the layout in a temp dir and stamp the image reference into the
    // index.json `org.opencontainers.image.ref.name` annotation, so the tag
    // round-trips through `load` (the stored layout carries no annotation).
    let staging =
        tempfile::TempDir::new().map_err(|e| super::io_error("Failed to create staging dir", e))?;
    copy_layout(&stored.path, staging.path())?;
    stamp_ref_annotation(&staging.path().join("index.json"), &stored.reference)?;

    // Create tar archive of the OCI layout directory
    create_tar_archive(staging.path(), &args.output)?;

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

/// Recursively copy an OCI layout directory into `dst`.
fn copy_layout(src: &std::path::Path, dst: &std::path::Path) -> Result<(), BoxError> {
    super::commit::refuse_archive_ancestor_reparse(src)?;
    super::commit::refuse_archive_ancestor_reparse(dst)?;
    #[cfg(windows)]
    {
        super::commit::refuse_directory_reparse(src)?;
        super::commit::refuse_directory_reparse(dst)?;
    }
    std::fs::create_dir_all(dst)
        .map_err(|e| super::io_error(format!("Failed to create {}", dst.display()), e))?;
    for entry in std::fs::read_dir(src).map_err(|e| super::io_error("Failed to read layout", e))? {
        let entry = entry.map_err(|e| super::io_error("Failed to read layout entry", e))?;
        let from = entry.path();
        let to = dst.join(entry.file_name());
        if entry
            .file_type()
            .map_err(|e| super::io_error("stat failed", e))?
            .is_dir()
        {
            copy_layout(&from, &to)?;
        } else {
            std::fs::copy(&from, &to)
                .map_err(|e| super::io_error(format!("Failed to copy {}", from.display()), e))?;
        }
    }
    Ok(())
}

/// Stamp `org.opencontainers.image.ref.name = reference` onto the first
/// manifest descriptor in `index.json` so `load` can restore the tag.
fn stamp_ref_annotation(index_path: &std::path::Path, reference: &str) -> Result<(), BoxError> {
    let bytes =
        std::fs::read(index_path).map_err(|e| super::io_error("Failed to read index.json", e))?;
    let mut index: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|e| BoxError::SerializationError(format!("Invalid index.json: {e}")))?;
    if let Some(manifest) = index
        .get_mut("manifests")
        .and_then(|m| m.get_mut(0))
        .and_then(|m| m.as_object_mut())
    {
        let annotations = manifest
            .entry("annotations")
            .or_insert_with(|| serde_json::json!({}));
        if let Some(map) = annotations.as_object_mut() {
            map.insert(
                "org.opencontainers.image.ref.name".to_string(),
                serde_json::Value::String(reference.to_string()),
            );
        }
    }
    let out = serde_json::to_vec_pretty(&index)
        .map_err(|e| BoxError::SerializationError(format!("Failed to encode index.json: {e}")))?;
    std::fs::write(index_path, out)
        .map_err(|e| super::io_error("Failed to write index.json", e))?;
    Ok(())
}

/// Create a tar archive from a directory.
fn create_tar_archive(src_dir: &std::path::Path, output_path: &str) -> Result<(), BoxError> {
    let output = std::path::Path::new(output_path);
    super::commit::refuse_archive_ancestor_reparse(output)?;
    #[cfg(windows)]
    super::commit::refuse_nested_directory_reparse(src_dir)?;
    let file = std::fs::File::create(output)
        .map_err(|e| super::io_error(format!("Failed to create {output_path}"), e))?;

    let mut builder = tar::Builder::new(file);
    builder
        .append_dir_all(".", src_dir)
        .map_err(|e| super::io_error("Failed to archive image", e))?;
    builder
        .finish()
        .map_err(|e| super::io_error("Failed to finalize archive", e))?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_runtime::StoredImage;
    use chrono::Utc;
    use std::fs;
    use std::path::PathBuf;
    use tempfile::TempDir;

    #[test]
    fn test_stamp_ref_annotation_adds_ref_name() {
        let dir = TempDir::new().unwrap();
        let index = dir.path().join("index.json");
        // An index.json with a manifest descriptor but no annotations.
        fs::write(
            &index,
            r#"{"schemaVersion":2,"manifests":[{"mediaType":"application/vnd.oci.image.manifest.v1+json","digest":"sha256:abc","size":1}]}"#,
        )
        .unwrap();

        stamp_ref_annotation(&index, "docker.io/library/rt:9").unwrap();

        let v: serde_json::Value = serde_json::from_slice(&fs::read(&index).unwrap()).unwrap();
        assert_eq!(
            v["manifests"][0]["annotations"]["org.opencontainers.image.ref.name"],
            serde_json::json!("docker.io/library/rt:9")
        );
    }

    #[test]
    fn test_copy_layout_round_trips_files() {
        let src = TempDir::new().unwrap();
        let dst = TempDir::new().unwrap();
        fs::write(src.path().join("index.json"), "{}").unwrap();
        fs::create_dir_all(src.path().join("blobs/sha256")).unwrap();
        fs::write(src.path().join("blobs/sha256/abc"), "blob").unwrap();

        copy_layout(src.path(), dst.path()).unwrap();

        assert_eq!(
            fs::read_to_string(dst.path().join("index.json")).unwrap(),
            "{}"
        );
        assert_eq!(
            fs::read_to_string(dst.path().join("blobs/sha256/abc")).unwrap(),
            "blob"
        );
    }

    #[cfg(windows)]
    #[test]
    fn copy_layout_does_not_follow_an_ancestor_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        let layout = outside.join("layout");
        fs::create_dir_all(&layout).unwrap();
        fs::write(layout.join("secret.txt"), b"secret").unwrap();
        let parent = tmp.path().join("parent");
        fs::create_dir_all(&parent).unwrap();
        let link = parent.join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());
        let dst = tmp.path().join("staging");
        fs::create_dir_all(&dst).unwrap();

        let error = copy_layout(&link.join("layout"), &dst)
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("junction"),
            "image layout was copied through an ancestor junction: {error}"
        );
        assert!(!dst.join("secret.txt").exists());
        assert_eq!(fs::read(layout.join("secret.txt")).unwrap(), b"secret");
    }

    #[cfg(windows)]
    #[test]
    fn copy_layout_does_not_read_through_a_directory_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), b"secret").unwrap();
        let link = tmp.path().join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());
        let dst = tmp.path().join("staging");
        fs::create_dir_all(&dst).unwrap();

        let error = copy_layout(&link, &dst).unwrap_err().to_string();
        assert!(
            error.contains("junction"),
            "image layout was copied through a directory junction: {error}"
        );
        assert!(!dst.join("secret.txt").exists());
        assert_eq!(fs::read(outside.join("secret.txt")).unwrap(), b"secret");
    }

    #[cfg(windows)]
    #[test]
    fn copy_layout_does_not_write_through_a_directory_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("layout");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("index.json"), b"{}").unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), b"secret").unwrap();
        let link = tmp.path().join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());

        let error = copy_layout(&src, &link).unwrap_err().to_string();
        assert!(
            error.contains("junction"),
            "image layout was written through a directory junction: {error}"
        );
        assert!(!outside.join("index.json").exists());
        assert_eq!(fs::read(outside.join("secret.txt")).unwrap(), b"secret");
    }

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

    #[cfg(windows)]
    #[test]
    fn create_tar_archive_does_not_follow_a_child_directory_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = TempDir::new().unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("secret.txt"), b"secret").unwrap();
        let src = tmp.path().join("layout");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("index.json"), b"{}").unwrap();
        let link = src.join("nested");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());
        let output = tmp.path().join("image.tar");

        let archived = create_tar_archive(&src, output.to_str().unwrap());
        let contains_secret = output.exists()
            && fs::read(&output)
                .unwrap_or_default()
                .windows(6)
                .any(|window| window == b"secret");
        assert!(
            !contains_secret,
            "image archive followed a child directory junction: {archived:?}"
        );
        assert_eq!(fs::read(outside.join("secret.txt")).unwrap(), b"secret");
    }

    #[cfg(windows)]
    #[test]
    fn create_tar_archive_does_not_create_through_an_ancestor_junction() {
        use std::os::windows::process::CommandExt;

        let tmp = TempDir::new().unwrap();
        let src = tmp.path().join("layout");
        fs::create_dir_all(&src).unwrap();
        fs::write(src.join("index.json"), b"{}").unwrap();
        let outside = tmp.path().join("outside");
        fs::create_dir_all(&outside).unwrap();
        let parent = tmp.path().join("parent");
        fs::create_dir_all(&parent).unwrap();
        let link = parent.join("link");
        let mut command = std::process::Command::new("cmd");
        command.raw_arg(format!(
            "/C mklink /J \"{}\" \"{}\"",
            link.display(),
            outside.display()
        ));
        assert!(command.status().expect("mklink").success());
        let output = link.join("image.tar");

        let error = create_tar_archive(&src, output.to_str().expect("utf-8"))
            .unwrap_err()
            .to_string();
        assert!(
            error.contains("junction"),
            "image archive was created through an ancestor junction: {error}"
        );
        assert!(!outside.join("image.tar").exists());
        assert_eq!(fs::read(src.join("index.json")).unwrap(), b"{}");
    }

    #[test]
    fn test_create_tar_archive_invalid_output() {
        let src = TempDir::new().unwrap();
        let result = create_tar_archive(src.path(), "/nonexistent/dir/test.tar");
        assert!(matches!(result, Err(BoxError::IoError(_))));
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

    #[test]
    fn test_save_resolution_accepts_digest() {
        let images = vec![StoredImage {
            reference: "example.com/app:latest".to_string(),
            digest: "sha256:abc".to_string(),
            size_bytes: 1024,
            pulled_at: Utc::now(),
            last_used: Utc::now(),
            path: PathBuf::from("/tmp/image"),
        }];

        let stored = image_usage::resolve_required_stored_image(&images, "sha256:abc").unwrap();

        assert_eq!(stored.reference, "example.com/app:latest");
    }
}
