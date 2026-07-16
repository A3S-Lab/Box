use std::sync::Arc;

use a3s_box_runtime::sandbox::{
    map_container_gid, map_container_uid, probe_sandbox_capabilities, unmap_host_gid,
    unmap_host_uid, UserNamespaceEvidence,
};
use chrono::{DateTime, Utc};
use serde::Serialize;
use thiserror::Error;

#[cfg(unix)]
mod unix;
#[cfg(unix)]
mod unix_ops;
#[cfg(not(unix))]
mod unsupported;

#[cfg(unix)]
pub use unix::{PendingVolumeWrite, VolumeFilesystem};
#[cfg(not(unix))]
pub use unsupported::{PendingVolumeWrite, VolumeFilesystem};

pub const MAX_DIRECTORY_DEPTH: u32 = 64;

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct VolumeMetadataUpdate {
    pub uid: Option<u32>,
    pub gid: Option<u32>,
    pub mode: Option<u32>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum VolumeEntryType {
    Unknown,
    File,
    Directory,
    Symlink,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct VolumeEntry {
    pub name: String,
    #[serde(rename = "type")]
    pub entry_type: VolumeEntryType,
    pub path: String,
    pub size: i64,
    pub mode: u32,
    pub uid: u32,
    pub gid: u32,
    pub atime: DateTime<Utc>,
    pub mtime: DateTime<Utc>,
    pub ctime: DateTime<Utc>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub target: Option<String>,
}

#[derive(Debug, Error)]
pub enum VolumeContentError {
    #[error("invalid volume path: {0}")]
    InvalidPath(String),
    #[error("volume path was not found")]
    NotFound,
    #[error("volume path conflicts with an existing or active entry")]
    Conflict,
    #[error("volume path operation is not permitted")]
    PermissionDenied,
    #[error("volume content operations are unsupported: {0}")]
    Unsupported(String),
    #[error("volume content storage is unavailable: {0}")]
    Unavailable(String),
}

pub type VolumeContentResult<T> = std::result::Result<T, VolumeContentError>;

pub trait VolumeIdMapper: Send + Sync {
    fn host_uid(&self, container_uid: u32) -> VolumeContentResult<u32>;
    fn host_gid(&self, container_gid: u32) -> VolumeContentResult<u32>;
    fn container_uid(&self, host_uid: u32) -> VolumeContentResult<u32>;
    fn container_gid(&self, host_gid: u32) -> VolumeContentResult<u32>;
}

#[derive(Debug, Clone)]
pub struct SandboxVolumeIdMapper {
    evidence: UserNamespaceEvidence,
}

impl SandboxVolumeIdMapper {
    pub fn new(evidence: UserNamespaceEvidence) -> Self {
        Self { evidence }
    }
}

impl VolumeIdMapper for SandboxVolumeIdMapper {
    fn host_uid(&self, container_uid: u32) -> VolumeContentResult<u32> {
        map_container_uid(&self.evidence, container_uid).map_err(mapping_error)
    }

    fn host_gid(&self, container_gid: u32) -> VolumeContentResult<u32> {
        map_container_gid(&self.evidence, container_gid).map_err(mapping_error)
    }

    fn container_uid(&self, host_uid: u32) -> VolumeContentResult<u32> {
        unmap_host_uid(&self.evidence, host_uid).map_err(mapping_error)
    }

    fn container_gid(&self, host_gid: u32) -> VolumeContentResult<u32> {
        unmap_host_gid(&self.evidence, host_gid).map_err(mapping_error)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct IdentityVolumeIdMapper {
    effective_uid: u32,
    effective_gid: u32,
}

impl IdentityVolumeIdMapper {
    pub fn current() -> Self {
        #[cfg(unix)]
        {
            Self {
                effective_uid: unsafe { libc::geteuid() } as u32,
                effective_gid: unsafe { libc::getegid() } as u32,
            }
        }

        #[cfg(not(unix))]
        {
            Self {
                effective_uid: 0,
                effective_gid: 0,
            }
        }
    }
}

impl Default for IdentityVolumeIdMapper {
    fn default() -> Self {
        Self::current()
    }
}

impl VolumeIdMapper for IdentityVolumeIdMapper {
    fn host_uid(&self, container_uid: u32) -> VolumeContentResult<u32> {
        Ok(if container_uid == 0 {
            self.effective_uid
        } else {
            container_uid
        })
    }

    fn host_gid(&self, container_gid: u32) -> VolumeContentResult<u32> {
        Ok(if container_gid == 0 {
            self.effective_gid
        } else {
            container_gid
        })
    }

    fn container_uid(&self, host_uid: u32) -> VolumeContentResult<u32> {
        Ok(if host_uid == self.effective_uid {
            0
        } else {
            host_uid
        })
    }

    fn container_gid(&self, host_gid: u32) -> VolumeContentResult<u32> {
        Ok(if host_gid == self.effective_gid {
            0
        } else {
            host_gid
        })
    }
}

pub fn current_volume_id_mapper() -> VolumeContentResult<Arc<dyn VolumeIdMapper>> {
    #[cfg(target_os = "linux")]
    {
        let snapshot = probe_sandbox_capabilities(None);
        let evidence = snapshot.user_namespace.ok_or_else(|| {
            VolumeContentError::Unsupported(
                "Sandbox user-namespace identity mappings are unavailable".to_string(),
            )
        })?;
        Ok(Arc::new(SandboxVolumeIdMapper::new(evidence)))
    }

    #[cfg(not(target_os = "linux"))]
    {
        Ok(Arc::new(IdentityVolumeIdMapper::current()))
    }
}

fn mapping_error(error: impl std::fmt::Display) -> VolumeContentError {
    VolumeContentError::InvalidPath(format!("volume ownership is outside Sandbox mappings: {error}"))
}
