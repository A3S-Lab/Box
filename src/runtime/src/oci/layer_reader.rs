//! Shared no-follow compression boundary for OCI filesystem layers.

use std::fs::File;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

use a3s_box_core::error::{BoxError, Result};
use flate2::read::GzDecoder;
use sha2::{Digest, Sha256};

use super::image::{
    canonical_sha256_digest_hex, open_regular_file_no_follow, MAX_OCI_LAYER_BLOB_BYTES,
};

pub(crate) fn open(path: &Path) -> Result<Box<dyn Read>> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| {
        oci_error(format!(
            "Failed to inspect OCI layer {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() || metadata.file_type().is_symlink() {
        return Err(oci_error(format!(
            "OCI layer is not a plain file: {}",
            path.display()
        )));
    }

    let file = open_regular_file_no_follow(path, "OCI layer")?;
    let opened = file.metadata().map_err(BoxError::IoError)?;
    if !opened.is_file() || opened.len() != metadata.len() {
        return Err(oci_error(format!(
            "OCI layer changed while opening: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        if opened.dev() != metadata.dev() || opened.ino() != metadata.ino() {
            return Err(oci_error(format!(
                "OCI layer identity changed while opening: {}",
                path.display()
            )));
        }
    }

    decode(file, path)
}

/// Snapshot and authenticate one compressed OCI layer before decoding it.
///
/// Hashing and later decoding different opens of a mutable blob would leave a
/// time-of-check/time-of-use gap. The anonymous spool is therefore the exact
/// byte sequence whose descriptor was verified and the only source decoded by
/// direct ext4 assembly.
pub(crate) fn open_verified(
    path: &Path,
    digest: &str,
    expected_size: u64,
    spool_parent: &Path,
) -> Result<Box<dyn Read>> {
    let expected_hex = canonical_sha256_digest_hex(digest)?;
    if expected_size > MAX_OCI_LAYER_BLOB_BYTES {
        return Err(oci_error(format!(
            "refusing OCI layer {digest}: descriptor size {expected_size} exceeds the {MAX_OCI_LAYER_BLOB_BYTES}-byte limit"
        )));
    }

    let mut source = open_regular_file_no_follow(path, "OCI layer")?;
    let opened_size = source.metadata().map_err(BoxError::IoError)?.len();
    if opened_size != expected_size {
        return Err(oci_error(format!(
            "refusing OCI layer {digest}: descriptor size {expected_size} does not match actual size {opened_size}"
        )));
    }
    let mut spool = tempfile::tempfile_in(spool_parent).map_err(|error| {
        oci_error(format!(
            "Failed to create authenticated OCI layer spool in {}: {error}",
            spool_parent.display()
        ))
    })?;
    let mut hasher = Sha256::new();
    let mut total = 0u64;
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = source.read(&mut buffer).map_err(|error| {
            oci_error(format!(
                "Failed to read OCI layer {}: {error}",
                path.display()
            ))
        })?;
        if read == 0 {
            break;
        }
        total = total
            .checked_add(read as u64)
            .ok_or_else(|| oci_error("OCI layer byte count overflow"))?;
        if total > expected_size {
            return Err(oci_error(format!(
                "refusing OCI layer {digest}: content grew beyond its descriptor size {expected_size} while reading"
            )));
        }
        hasher.update(&buffer[..read]);
        spool
            .write_all(&buffer[..read])
            .map_err(BoxError::IoError)?;
    }
    if total != expected_size {
        return Err(oci_error(format!(
            "refusing OCI layer {digest}: descriptor size {expected_size} does not match bytes read {total}"
        )));
    }
    let actual_hex = format!("{:x}", hasher.finalize());
    if actual_hex != expected_hex {
        return Err(oci_error(format!(
            "refusing OCI layer {digest}: descriptor digest does not match actual bytes (sha256:{actual_hex})"
        )));
    }
    spool.flush().map_err(BoxError::IoError)?;
    spool.seek(SeekFrom::Start(0)).map_err(BoxError::IoError)?;
    decode(spool, path)
}

fn decode(mut file: File, path: &Path) -> Result<Box<dyn Read>> {
    let mut magic = [0; 4];
    let read = file.read(&mut magic).map_err(|error| {
        oci_error(format!(
            "Failed to read OCI layer {}: {error}",
            path.display()
        ))
    })?;
    file.seek(SeekFrom::Start(0)).map_err(|error| {
        oci_error(format!(
            "Failed to rewind OCI layer {}: {error}",
            path.display()
        ))
    })?;
    if read >= 2 && magic[..2] == [0x1f, 0x8b] {
        Ok(Box::new(GzDecoder::new(file)))
    } else if read >= 4 && magic == [0x28, 0xb5, 0x2f, 0xfd] {
        let decoder = zstd::stream::read::Decoder::new(file).map_err(|error| {
            oci_error(format!(
                "Failed to initialize zstd layer {}: {error}",
                path.display()
            ))
        })?;
        Ok(Box::new(decoder))
    } else {
        Ok(Box::new(file))
    }
}

fn oci_error(message: impl Into<String>) -> BoxError {
    BoxError::OciImageError(message.into())
}
