use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};

use a3s_box_core::error::{BoxError, Result};
use sha2::{Digest, Sha256};

use super::ImageStore;

static REUSE_SEQ: AtomicU64 = AtomicU64::new(0);

impl ImageStore {
    /// Reuse a content-addressed blob from any indexed image layout.
    ///
    /// Candidates are selected only by canonical digest path, cloned or copied
    /// into a private staging file, and then revalidated by declared size and
    /// SHA-256 before an atomic no-clobber publication.
    pub(crate) async fn reuse_verified_blob(
        &self,
        digest: &str,
        declared_size: i64,
        dest: &Path,
    ) -> Result<bool> {
        let expected_hex = validate_digest(digest)?;
        let expected_size = u64::try_from(declared_size).map_err(|_| {
            BoxError::OciImageError(format!(
                "Cannot reuse blob {digest} with negative declared size {declared_size}"
            ))
        })?;
        let candidates = {
            let index = self.index.read().await;
            index
                .values()
                .map(|image| image.path.join("blobs").join("sha256").join(&expected_hex))
                .collect::<HashSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
        };
        if candidates.is_empty() {
            return Ok(false);
        }

        let dest = dest.to_path_buf();
        let digest = digest.to_string();
        tokio::task::spawn_blocking(move || {
            reuse_from_candidates(&candidates, &dest, &digest, expected_size)
        })
        .await
        .map_err(|error| {
            BoxError::OciImageError(format!("Verified blob reuse worker failed: {error}"))
        })?
    }
}

fn reuse_from_candidates(
    candidates: &[PathBuf],
    dest: &Path,
    digest: &str,
    expected_size: u64,
) -> Result<bool> {
    if dest.exists() {
        return verify_path(dest, digest, expected_size).map_err(|error| {
            BoxError::OciImageError(format!(
                "Failed to verify existing destination blob {}: {error}",
                dest.display()
            ))
        });
    }
    let parent = dest.parent().ok_or_else(|| {
        BoxError::OciImageError(format!(
            "Blob destination has no parent: {}",
            dest.display()
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(|error| {
        BoxError::OciImageError(format!(
            "Failed to create blob destination directory {}: {error}",
            parent.display()
        ))
    })?;

    for candidate in candidates {
        let metadata = match std::fs::symlink_metadata(candidate) {
            Ok(metadata)
                if metadata.file_type().is_file()
                    && !metadata.file_type().is_symlink()
                    && metadata.len() == expected_size =>
            {
                metadata
            }
            _ => continue,
        };
        if !metadata.file_type().is_file() {
            continue;
        }

        let seq = REUSE_SEQ.fetch_add(1, Ordering::Relaxed);
        let staging = parent.join(format!(
            ".blob-reuse-{}-{}-{seq}",
            std::process::id(),
            digest.strip_prefix("sha256:").unwrap_or("invalid")
        ));
        match clone_or_copy_verified(candidate, &staging, digest, expected_size) {
            Ok(method) => {
                let published = match std::fs::hard_link(&staging, dest) {
                    Ok(()) => true,
                    Err(error) if dest.exists() => match verify_path(dest, digest, expected_size) {
                        Ok(valid) => valid,
                        Err(verify_error) => {
                            let _ = std::fs::remove_file(&staging);
                            return Err(BoxError::OciImageError(format!(
                                "Concurrent blob publication failed verification after {error}: {verify_error}"
                            )));
                        }
                    },
                    Err(error) => {
                        let _ = std::fs::remove_file(&staging);
                        return Err(BoxError::OciImageError(format!(
                            "Failed to publish reused blob {}: {error}",
                            dest.display()
                        )));
                    }
                };
                let _ = std::fs::remove_file(&staging);
                if published {
                    tracing::debug!(
                        source = %candidate.display(),
                        destination = %dest.display(),
                        ?method,
                        "Published verified shared blob"
                    );
                    return Ok(true);
                }
            }
            Err(error) => {
                let _ = std::fs::remove_file(&staging);
                tracing::warn!(
                    source = %candidate.display(),
                    %digest,
                    %error,
                    "Ignoring invalid shared blob candidate"
                );
            }
        }
    }
    Ok(false)
}

#[derive(Debug, Clone, Copy)]
enum ReuseMethod {
    Reflink,
    Copy,
}

fn clone_or_copy_verified(
    source_path: &Path,
    staging: &Path,
    digest: &str,
    expected_size: u64,
) -> std::io::Result<ReuseMethod> {
    let mut source = open_source(source_path)?;
    let source_metadata = source.metadata()?;
    if !source_metadata.file_type().is_file() || source_metadata.len() != expected_size {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "shared blob candidate changed before reuse",
        ));
    }
    let mut destination = OpenOptions::new()
        .create_new(true)
        .read(true)
        .write(true)
        .open(staging)?;

    let method = if try_reflink(&source, &destination)? {
        ReuseMethod::Reflink
    } else {
        source.seek(std::io::SeekFrom::Start(0))?;
        destination.set_len(0)?;
        destination.seek(std::io::SeekFrom::Start(0))?;
        std::io::copy(&mut source, &mut destination)?;
        ReuseMethod::Copy
    };
    destination.flush()?;
    destination.sync_all()?;
    if !verify_file(&mut destination, digest, expected_size)? {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "shared blob candidate failed digest or size verification",
        ));
    }
    Ok(method)
}

fn open_source(path: &Path) -> std::io::Result<File> {
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.custom_flags(libc::O_CLOEXEC | libc::O_NOFOLLOW);
    }
    options.open(path)
}

#[cfg(target_os = "linux")]
fn try_reflink(source: &File, destination: &File) -> std::io::Result<bool> {
    use std::os::fd::AsRawFd;

    let result = unsafe {
        libc::ioctl(
            destination.as_raw_fd(),
            libc::FICLONE as _,
            source.as_raw_fd(),
        )
    };
    if result == 0 {
        Ok(true)
    } else {
        let error = std::io::Error::last_os_error();
        match error.raw_os_error() {
            Some(libc::EOPNOTSUPP | libc::ENOTTY | libc::EXDEV | libc::EINVAL) => Ok(false),
            _ => Err(error),
        }
    }
}

#[cfg(not(target_os = "linux"))]
fn try_reflink(_source: &File, _destination: &File) -> std::io::Result<bool> {
    Ok(false)
}

fn verify_path(path: &Path, digest: &str, expected_size: u64) -> std::io::Result<bool> {
    let metadata = std::fs::symlink_metadata(path)?;
    if metadata.file_type().is_symlink()
        || !metadata.file_type().is_file()
        || metadata.len() != expected_size
    {
        return Ok(false);
    }
    let mut file = open_source(path)?;
    verify_file(&mut file, digest, expected_size)
}

fn verify_file(file: &mut File, digest: &str, expected_size: u64) -> std::io::Result<bool> {
    if file.metadata()?.len() != expected_size {
        return Ok(false);
    }
    file.seek(std::io::SeekFrom::Start(0))?;
    let mut hasher = Sha256::new();
    let mut actual_size = 0_u64;
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        actual_size = actual_size.saturating_add(read as u64);
    }
    let actual = format!("sha256:{:x}", hasher.finalize());
    Ok(actual_size == expected_size && actual == digest)
}

fn validate_digest(digest: &str) -> Result<String> {
    digest
        .strip_prefix("sha256:")
        .filter(|hex| {
            hex.len() == 64
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        })
        .map(str::to_string)
        .ok_or_else(|| {
            BoxError::OciImageError(format!(
                "Cannot reuse malformed blob digest {digest:?}; expected sha256:<64 lowercase hex>"
            ))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn digest(bytes: &[u8]) -> String {
        format!("sha256:{:x}", Sha256::digest(bytes))
    }

    async fn seeded_store(bytes: &[u8]) -> (tempfile::TempDir, ImageStore, String) {
        let root = tempfile::tempdir().unwrap();
        let source = root.path().join("source");
        let blob_digest = digest(bytes);
        let blob_hex = blob_digest.strip_prefix("sha256:").unwrap();
        std::fs::create_dir_all(source.join("blobs/sha256")).unwrap();
        std::fs::write(
            source.join("oci-layout"),
            r#"{"imageLayoutVersion":"1.0.0"}"#,
        )
        .unwrap();
        std::fs::write(source.join("index.json"), r#"{"manifests":[]}"#).unwrap();
        std::fs::write(source.join("blobs/sha256").join(blob_hex), bytes).unwrap();
        let store = ImageStore::new(&root.path().join("images"), u64::MAX).unwrap();
        store
            .put(
                "seed:latest",
                &format!("sha256:{}", "a".repeat(64)),
                &source,
            )
            .await
            .unwrap();
        (root, store, blob_digest)
    }

    #[tokio::test]
    async fn reuses_only_size_and_digest_verified_content() {
        let (root, store, blob_digest) = seeded_store(b"shared-layer").await;
        let dest = root.path().join("target/blobs/sha256/blob");

        assert!(store
            .reuse_verified_blob(&blob_digest, 12, &dest)
            .await
            .unwrap());
        assert_eq!(std::fs::read(dest).unwrap(), b"shared-layer");
    }

    #[tokio::test]
    async fn corrupt_shared_candidate_is_not_reused() {
        let (root, store, blob_digest) = seeded_store(b"shared-layer").await;
        let blob_hex = blob_digest.strip_prefix("sha256:").unwrap();
        let stored = store.get("seed:latest").await.unwrap();
        std::fs::write(
            stored.path.join("blobs/sha256").join(blob_hex),
            b"corrupt-data",
        )
        .unwrap();
        let dest = root.path().join("target/blobs/sha256/blob");

        assert!(!store
            .reuse_verified_blob(&blob_digest, 12, &dest)
            .await
            .unwrap());
        assert!(!dest.exists());
    }
}
