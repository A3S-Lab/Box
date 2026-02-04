//! OCI layer extraction utilities.
//!
//! Handles extraction of OCI image layers (tar.gz format) to filesystem.

use a3s_box_core::error::{BoxError, Result};
use flate2::read::GzDecoder;
use std::fs::File;
use std::path::Path;
use tar::Archive;

/// Extract a single OCI layer (tar.gz) to target directory.
///
/// # Arguments
///
/// * `layer_path` - Path to the layer tarball (*.tar.gz)
/// * `target_dir` - Directory to extract files into
///
/// # Errors
///
/// Returns error if:
/// - Layer file doesn't exist
/// - Decompression fails
/// - Extraction fails
/// - Target directory cannot be created
pub fn extract_layer(layer_path: &Path, target_dir: &Path) -> Result<()> {
    // Validate layer exists
    if !layer_path.exists() {
        return Err(BoxError::Other(format!(
            "Layer file not found: {}",
            layer_path.display()
        )));
    }

    // Create target directory
    std::fs::create_dir_all(target_dir).map_err(|e| {
        BoxError::Other(format!(
            "Failed to create target directory {}: {}",
            target_dir.display(),
            e
        ))
    })?;

    // Open layer file
    let file = File::open(layer_path).map_err(|e| {
        BoxError::Other(format!(
            "Failed to open layer file {}: {}",
            layer_path.display(),
            e
        ))
    })?;

    // Decompress gzip
    let decoder = GzDecoder::new(file);

    // Extract tar archive
    let mut archive = Archive::new(decoder);
    archive.unpack(target_dir).map_err(|e| {
        BoxError::Other(format!(
            "Failed to extract layer to {}: {}",
            target_dir.display(),
            e
        ))
    })?;

    tracing::debug!(
        layer = %layer_path.display(),
        target = %target_dir.display(),
        "Extracted OCI layer"
    );

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    #[test]
    fn test_extract_layer_creates_target_directory() {
        let temp_dir = TempDir::new().unwrap();
        let layer_path = temp_dir.path().join("layer.tar.gz");
        let target_dir = temp_dir.path().join("extracted");

        // Create a minimal tar.gz file
        create_test_layer(&layer_path, &[("test.txt", b"hello")]);

        // Extract layer
        extract_layer(&layer_path, &target_dir).unwrap();

        // Verify target directory was created
        assert!(target_dir.exists());
        assert!(target_dir.is_dir());
    }

    #[test]
    fn test_extract_layer_extracts_files() {
        let temp_dir = TempDir::new().unwrap();
        let layer_path = temp_dir.path().join("layer.tar.gz");
        let target_dir = temp_dir.path().join("extracted");

        // Create layer with test files
        create_test_layer(
            &layer_path,
            &[("file1.txt", b"content1"), ("dir/file2.txt", b"content2")],
        );

        // Extract layer
        extract_layer(&layer_path, &target_dir).unwrap();

        // Verify files were extracted
        assert!(target_dir.join("file1.txt").exists());
        assert!(target_dir.join("dir/file2.txt").exists());

        // Verify content
        let content1 = fs::read_to_string(target_dir.join("file1.txt")).unwrap();
        assert_eq!(content1, "content1");

        let content2 = fs::read_to_string(target_dir.join("dir/file2.txt")).unwrap();
        assert_eq!(content2, "content2");
    }

    #[test]
    fn test_extract_layer_nonexistent_file() {
        let temp_dir = TempDir::new().unwrap();
        let layer_path = temp_dir.path().join("nonexistent.tar.gz");
        let target_dir = temp_dir.path().join("extracted");

        // Try to extract non-existent layer
        let result = extract_layer(&layer_path, &target_dir);

        assert!(result.is_err());
        assert!(result
            .unwrap_err()
            .to_string()
            .contains("Layer file not found"));
    }

    #[test]
    fn test_extract_layer_multiple_layers_to_same_target() {
        let temp_dir = TempDir::new().unwrap();
        let layer1_path = temp_dir.path().join("layer1.tar.gz");
        let layer2_path = temp_dir.path().join("layer2.tar.gz");
        let target_dir = temp_dir.path().join("extracted");

        // Create two layers
        create_test_layer(&layer1_path, &[("base.txt", b"base content")]);
        create_test_layer(&layer2_path, &[("app.txt", b"app content")]);

        // Extract both layers to same target
        extract_layer(&layer1_path, &target_dir).unwrap();
        extract_layer(&layer2_path, &target_dir).unwrap();

        // Verify both files exist
        assert!(target_dir.join("base.txt").exists());
        assert!(target_dir.join("app.txt").exists());
    }

    #[test]
    fn test_extract_layer_overwrites_existing_files() {
        let temp_dir = TempDir::new().unwrap();
        let layer1_path = temp_dir.path().join("layer1.tar.gz");
        let layer2_path = temp_dir.path().join("layer2.tar.gz");
        let target_dir = temp_dir.path().join("extracted");

        // Create two layers with same filename
        create_test_layer(&layer1_path, &[("file.txt", b"version 1")]);
        create_test_layer(&layer2_path, &[("file.txt", b"version 2")]);

        // Extract first layer
        extract_layer(&layer1_path, &target_dir).unwrap();
        let content1 = fs::read_to_string(target_dir.join("file.txt")).unwrap();
        assert_eq!(content1, "version 1");

        // Extract second layer (should overwrite)
        extract_layer(&layer2_path, &target_dir).unwrap();
        let content2 = fs::read_to_string(target_dir.join("file.txt")).unwrap();
        assert_eq!(content2, "version 2");
    }

    // Helper function to create a test tar.gz layer
    fn create_test_layer(path: &Path, files: &[(&str, &[u8])]) {
        use flate2::write::GzEncoder;
        use flate2::Compression;
        use tar::Builder;

        let file = File::create(path).unwrap();
        let encoder = GzEncoder::new(file, Compression::default());
        let mut builder = Builder::new(encoder);

        for (name, content) in files {
            let mut header = tar::Header::new_gnu();
            header.set_size(content.len() as u64);
            header.set_mode(0o644);
            header.set_cksum();

            builder.append_data(&mut header, name, *content).unwrap();
        }

        builder.finish().unwrap();
    }
}
