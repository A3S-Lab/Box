//! Shared stored-image type for the Box image domain.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Metadata for a stored OCI image.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StoredImage {
    /// OCI image reference (e.g., `docker.io/library/ubuntu:22.04`)
    pub reference: String,
    /// Content-addressable digest
    pub digest: String,
    /// Size on disk in bytes
    pub size_bytes: u64,
    /// When the image was first pulled
    pub pulled_at: DateTime<Utc>,
    /// When the image was last used (for LRU eviction)
    pub last_used: DateTime<Utc>,
    /// Path to the unpacked OCI image layout on disk
    pub path: PathBuf,
}
