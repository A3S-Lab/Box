//! Guest-captured filesystem metadata used by stopped-box commit.

use serde::{Deserialize, Serialize};

/// Location written inside a persistent rootfs before guest shutdown.
pub const ROOTFS_METADATA_PATH: &str = "/.a3s_rootfs_metadata_v1.json";
/// Location used to carry OCI header ownership across a rootless host extraction.
pub const IMAGE_ROOTFS_METADATA_PATH: &str = "/.a3s_image_metadata_v1.json";
/// Stable manifest schema identifier.
pub const ROOTFS_METADATA_SCHEMA: &str = "a3s.box.rootfs-metadata.v1";

/// Metadata kind supported by OCI rootfs archives.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RootfsEntryKind {
    Directory,
    Regular,
    Symlink,
}

/// One guest-visible filesystem entry. Paths are base64-encoded raw Unix bytes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootfsMetadataEntry {
    pub path_base64: String,
    pub kind: RootfsEntryKind,
    pub mode: u32,
    pub uid: u64,
    pub gid: u64,
    pub mtime: u64,
    pub size: u64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub link_target_base64: Option<String>,
}

/// Complete terminal metadata snapshot for one rootfs generation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RootfsMetadataManifest {
    pub schema: String,
    pub entries: Vec<RootfsMetadataEntry>,
}

impl RootfsMetadataManifest {
    pub fn new(entries: Vec<RootfsMetadataEntry>) -> Self {
        Self {
            schema: ROOTFS_METADATA_SCHEMA.to_string(),
            entries,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != ROOTFS_METADATA_SCHEMA {
            return Err(format!(
                "unsupported rootfs metadata schema: {}",
                self.schema
            ));
        }
        Ok(())
    }
}
