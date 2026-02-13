//! Layer creation utilities for image building.
//!
//! Provides filesystem snapshotting, diffing, and tar.gz layer creation
//! for producing OCI image layers from build steps.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use sha2::{Digest, Sha256};

/// Metadata for a single file in a snapshot.
#[derive(Debug, Clone, PartialEq)]
pub struct FileEntry {
    /// Relative path from rootfs root
    pub path: PathBuf,
    /// File size in bytes
    pub size: u64,
    /// Modification time (seconds since epoch)
    pub mtime: i64,
    /// Whether this is a directory
    pub is_dir: bool,
}

/// A snapshot of a directory's file state.
#[derive(Debug, Clone)]
pub struct DirSnapshot {
    /// Map of relative path → file entry
    pub entries: HashMap<PathBuf, FileEntry>,
}

impl DirSnapshot {
    /// Take a snapshot of a directory, recording all files and their metadata.
    pub fn capture(root: &Path) -> Result<Self> {
        let mut entries = HashMap::new();
        walk_dir(root, root, &mut entries)?;
        Ok(DirSnapshot { entries })
    }

    /// Compute the diff between this snapshot (before) and another (after).
    ///
    /// Returns paths of files that were added or modified.
    pub fn diff(&self, after: &DirSnapshot) -> Vec<PathBuf> {
        let mut changed = Vec::new();

        for (path, after_entry) in &after.entries {
            match self.entries.get(path) {
                None => {
                    // New file
                    changed.push(path.clone());
                }
                Some(before_entry) => {
                    // Modified: size or mtime changed
                    if before_entry.size != after_entry.size
                        || before_entry.mtime != after_entry.mtime
                    {
                        changed.push(path.clone());
                    }
                }
            }
        }

        // Sort for deterministic output
        changed.sort();
        changed
    }
}

/// Recursively walk a directory and collect file entries.
fn walk_dir(root: &Path, current: &Path, entries: &mut HashMap<PathBuf, FileEntry>) -> Result<()> {
    let read_dir = std::fs::read_dir(current).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to read directory {}: {}",
            current.display(),
            e
        ))
    })?;

    for entry in read_dir {
        let entry = entry
            .map_err(|e| BoxError::BuildError(format!("Failed to read directory entry: {}", e)))?;

        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to compute relative path for {}: {}",
                    path.display(),
                    e
                ))
            })?
            .to_path_buf();

        let metadata = entry.metadata().map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to read metadata for {}: {}",
                path.display(),
                e
            ))
        })?;

        let mtime = metadata
            .modified()
            .map(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs() as i64
            })
            .unwrap_or(0);

        entries.insert(
            relative.clone(),
            FileEntry {
                path: relative,
                size: metadata.len(),
                mtime,
                is_dir: metadata.is_dir(),
            },
        );

        if metadata.is_dir() {
            walk_dir(root, &path, entries)?;
        }
    }

    Ok(())
}

/// Create a tar.gz layer from a list of changed files in a rootfs.
///
/// Returns the path to the created layer file and its SHA256 digest.
pub fn create_layer(
    rootfs: &Path,
    changed_files: &[PathBuf],
    output_path: &Path,
) -> Result<LayerInfo> {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let file = std::fs::File::create(output_path).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to create layer file {}: {}",
            output_path.display(),
            e
        ))
    })?;

    let encoder = GzEncoder::new(file, Compression::default());
    let mut builder = tar::Builder::new(encoder);

    for relative_path in changed_files {
        let full_path = rootfs.join(relative_path);
        if !full_path.exists() {
            continue;
        }

        if full_path.is_dir() {
            builder.append_dir(relative_path, &full_path).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to add directory {} to layer: {}",
                    relative_path.display(),
                    e
                ))
            })?;
        } else {
            builder
                .append_path_with_name(&full_path, relative_path)
                .map_err(|e| {
                    BoxError::BuildError(format!(
                        "Failed to add file {} to layer: {}",
                        relative_path.display(),
                        e
                    ))
                })?;
        }
    }

    builder
        .finish()
        .map_err(|e| BoxError::BuildError(format!("Failed to finalize layer: {}", e)))?;

    // Compute SHA256 digest of the layer file
    let digest = sha256_file(output_path)?;
    let size = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);

    Ok(LayerInfo {
        path: output_path.to_path_buf(),
        digest,
        size,
    })
}

/// Create a tar.gz layer from an entire directory (used for COPY).
///
/// All files under `src_dir` are added to the layer with paths relative
/// to `target_prefix` (the destination path inside the image).
pub fn create_layer_from_dir(
    src_dir: &Path,
    target_prefix: &Path,
    output_path: &Path,
) -> Result<LayerInfo> {
    use flate2::write::GzEncoder;
    use flate2::Compression;

    let file = std::fs::File::create(output_path).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to create layer file {}: {}",
            output_path.display(),
            e
        ))
    })?;

    let encoder = GzEncoder::new(file, Compression::default());
    let mut builder = tar::Builder::new(encoder);

    add_dir_to_tar(&mut builder, src_dir, src_dir, target_prefix)?;

    builder
        .finish()
        .map_err(|e| BoxError::BuildError(format!("Failed to finalize layer: {}", e)))?;

    let digest = sha256_file(output_path)?;
    let size = std::fs::metadata(output_path).map(|m| m.len()).unwrap_or(0);

    Ok(LayerInfo {
        path: output_path.to_path_buf(),
        digest,
        size,
    })
}

/// Recursively add a directory's contents to a tar builder.
fn add_dir_to_tar<W: std::io::Write>(
    builder: &mut tar::Builder<W>,
    root: &Path,
    current: &Path,
    target_prefix: &Path,
) -> Result<()> {
    let entries = std::fs::read_dir(current).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to read directory {}: {}",
            current.display(),
            e
        ))
    })?;

    for entry in entries {
        let entry =
            entry.map_err(|e| BoxError::BuildError(format!("Failed to read entry: {}", e)))?;

        let path = entry.path();
        let relative = path
            .strip_prefix(root)
            .map_err(|e| BoxError::BuildError(format!("Failed to strip prefix: {}", e)))?;
        let tar_path = target_prefix.join(relative);

        if path.is_dir() {
            builder.append_dir(&tar_path, &path).map_err(|e| {
                BoxError::BuildError(format!("Failed to add directory to layer: {}", e))
            })?;
            add_dir_to_tar(builder, root, &path, target_prefix)?;
        } else {
            builder
                .append_path_with_name(&path, &tar_path)
                .map_err(|e| BoxError::BuildError(format!("Failed to add file to layer: {}", e)))?;
        }
    }

    Ok(())
}

/// Information about a created layer.
#[derive(Debug, Clone)]
pub struct LayerInfo {
    /// Path to the layer tar.gz file
    pub path: PathBuf,
    /// SHA256 digest (hex string, without "sha256:" prefix)
    pub digest: String,
    /// Size in bytes
    pub size: u64,
}

impl LayerInfo {
    /// Get the digest with "sha256:" prefix.
    pub fn prefixed_digest(&self) -> String {
        format!("sha256:{}", self.digest)
    }
}

/// Compute SHA256 digest of a file.
pub fn sha256_file(path: &Path) -> Result<String> {
    let data = std::fs::read(path).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to read file for hashing {}: {}",
            path.display(),
            e
        ))
    })?;

    let mut hasher = Sha256::new();
    hasher.update(&data);
    let hash = hasher.finalize();
    Ok(hex::encode(hash))
}

/// Compute SHA256 digest of raw bytes.
pub fn sha256_bytes(data: &[u8]) -> String {
    let mut hasher = Sha256::new();
    hasher.update(data);
    let hash = hasher.finalize();
    hex::encode(hash)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use tempfile::TempDir;

    // --- DirSnapshot ---

    #[test]
    fn test_snapshot_empty_dir() {
        let tmp = TempDir::new().unwrap();
        let snap = DirSnapshot::capture(tmp.path()).unwrap();
        assert!(snap.entries.is_empty());
    }

    #[test]
    fn test_snapshot_with_files() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "hello").unwrap();
        fs::create_dir(tmp.path().join("sub")).unwrap();
        fs::write(tmp.path().join("sub").join("b.txt"), "world").unwrap();

        let snap = DirSnapshot::capture(tmp.path()).unwrap();
        assert!(snap.entries.contains_key(&PathBuf::from("a.txt")));
        assert!(snap.entries.contains_key(&PathBuf::from("sub")));
        assert!(snap.entries.contains_key(&PathBuf::from("sub/b.txt")));
    }

    #[test]
    fn test_snapshot_diff_new_file() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "hello").unwrap();

        let before = DirSnapshot::capture(tmp.path()).unwrap();

        fs::write(tmp.path().join("b.txt"), "world").unwrap();

        let after = DirSnapshot::capture(tmp.path()).unwrap();

        let diff = before.diff(&after);
        assert_eq!(diff, vec![PathBuf::from("b.txt")]);
    }

    #[test]
    fn test_snapshot_diff_modified_file() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "hello").unwrap();

        let before = DirSnapshot::capture(tmp.path()).unwrap();

        // Modify the file (change size)
        fs::write(tmp.path().join("a.txt"), "hello world").unwrap();

        let after = DirSnapshot::capture(tmp.path()).unwrap();

        let diff = before.diff(&after);
        assert_eq!(diff, vec![PathBuf::from("a.txt")]);
    }

    #[test]
    fn test_snapshot_diff_no_changes() {
        let tmp = TempDir::new().unwrap();
        fs::write(tmp.path().join("a.txt"), "hello").unwrap();

        let before = DirSnapshot::capture(tmp.path()).unwrap();
        let after = DirSnapshot::capture(tmp.path()).unwrap();

        let diff = before.diff(&after);
        assert!(diff.is_empty());
    }

    // --- create_layer ---

    #[test]
    fn test_create_layer_from_files() {
        let rootfs = TempDir::new().unwrap();
        let output_dir = TempDir::new().unwrap();

        fs::write(rootfs.path().join("hello.txt"), "hello").unwrap();
        fs::write(rootfs.path().join("world.txt"), "world").unwrap();

        let output_path = output_dir.path().join("layer.tar.gz");
        let changed = vec![PathBuf::from("hello.txt"), PathBuf::from("world.txt")];

        let info = create_layer(rootfs.path(), &changed, &output_path).unwrap();

        assert!(info.path.exists());
        assert!(info.size > 0);
        assert!(!info.digest.is_empty());
        assert_eq!(info.digest.len(), 64); // SHA256 hex
    }

    #[test]
    fn test_create_layer_empty() {
        let rootfs = TempDir::new().unwrap();
        let output_dir = TempDir::new().unwrap();
        let output_path = output_dir.path().join("layer.tar.gz");

        let info = create_layer(rootfs.path(), &[], &output_path).unwrap();
        assert!(info.path.exists());
    }

    // --- create_layer_from_dir ---

    #[test]
    fn test_create_layer_from_dir() {
        let src = TempDir::new().unwrap();
        let output_dir = TempDir::new().unwrap();

        fs::write(src.path().join("app.py"), "print('hi')").unwrap();
        fs::create_dir(src.path().join("lib")).unwrap();
        fs::write(src.path().join("lib").join("util.py"), "pass").unwrap();

        let output_path = output_dir.path().join("layer.tar.gz");
        let info = create_layer_from_dir(src.path(), Path::new("workspace"), &output_path).unwrap();

        assert!(info.path.exists());
        assert!(info.size > 0);

        // Verify the tar contains files under workspace/
        let file = fs::File::open(&info.path).unwrap();
        let decoder = flate2::read::GzDecoder::new(file);
        let mut archive = tar::Archive::new(decoder);
        let paths: Vec<String> = archive
            .entries()
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.path().unwrap().to_string_lossy().to_string())
            .collect();

        assert!(paths.iter().any(|p| p.contains("workspace/app.py")));
        assert!(paths.iter().any(|p| p.contains("workspace/lib")));
    }

    // --- sha256 ---

    #[test]
    fn test_sha256_file() {
        let tmp = TempDir::new().unwrap();
        let path = tmp.path().join("test.txt");
        fs::write(&path, "hello").unwrap();

        let digest = sha256_file(&path).unwrap();
        assert_eq!(digest.len(), 64);
        // Known SHA256 of "hello"
        assert_eq!(
            digest,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_sha256_bytes() {
        let digest = sha256_bytes(b"hello");
        assert_eq!(
            digest,
            "2cf24dba5fb0a30e26e83b2ac5b9e29e1b161e5c1fa7425e73043362938b9824"
        );
    }

    #[test]
    fn test_layer_info_prefixed_digest() {
        let info = LayerInfo {
            path: PathBuf::from("/tmp/layer.tar.gz"),
            digest: "abc123".to_string(),
            size: 100,
        };
        assert_eq!(info.prefixed_digest(), "sha256:abc123");
    }
}
