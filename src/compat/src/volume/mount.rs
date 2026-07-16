use std::collections::BTreeSet;
use std::path::{Component, Path, PathBuf};

use async_trait::async_trait;
use serde::{Deserialize, Serialize};

use super::{VolumeServiceError, VolumeServiceResult};

const MAX_MOUNT_PATH_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct VolumeMount {
    pub name: String,
    pub path: String,
}

impl VolumeMount {
    pub fn new(name: impl Into<String>, path: impl Into<String>) -> VolumeServiceResult<Self> {
        let mount = Self {
            name: name.into(),
            path: path.into(),
        };
        mount.validate()?;
        Ok(mount)
    }

    pub fn validate(&self) -> VolumeServiceResult<()> {
        if !super::valid_volume_name(&self.name) {
            return Err(VolumeServiceError::InvalidRequest(
                "volume mount name is invalid".to_string(),
            ));
        }
        if self.path.len() > MAX_MOUNT_PATH_BYTES || self.path.contains(':') {
            return Err(VolumeServiceError::InvalidRequest(
                "volume mount path is invalid".to_string(),
            ));
        }
        let path = Path::new(&self.path);
        if !path.is_absolute() || path == Path::new("/") {
            return Err(VolumeServiceError::InvalidRequest(
                "volume mount path must be an absolute non-root path".to_string(),
            ));
        }
        for component in path.components() {
            if matches!(component, Component::ParentDir | Component::CurDir | Component::Prefix(_))
            {
                return Err(VolumeServiceError::InvalidRequest(
                    "volume mount path cannot contain traversal components".to_string(),
                ));
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ResolvedVolumeMount {
    pub public: VolumeMount,
    pub runtime_name: String,
    pub host_path: PathBuf,
}

impl ResolvedVolumeMount {
    pub fn runtime_spec(&self) -> String {
        format!("{}:{}:rw", self.host_path.display(), self.public.path)
    }
}

pub fn validate_mounts(mounts: &[VolumeMount]) -> VolumeServiceResult<()> {
    let mut paths = BTreeSet::new();
    for mount in mounts {
        mount.validate()?;
        if !paths.insert(mount.path.as_str()) {
            return Err(VolumeServiceError::InvalidRequest(
                "volume mount paths must be unique".to_string(),
            ));
        }
    }
    Ok(())
}

#[async_trait]
pub trait VolumeMountResolver: Send + Sync {
    async fn resolve_mounts(
        &self,
        owner_id: &str,
        mounts: &[VolumeMount],
    ) -> VolumeServiceResult<Vec<ResolvedVolumeMount>>;
}
