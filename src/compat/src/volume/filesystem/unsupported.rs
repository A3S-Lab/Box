use std::path::Path;
use std::sync::Arc;

use super::{
    VolumeContentError, VolumeContentResult, VolumeEntry, VolumeIdMapper, VolumeMetadataUpdate,
};

#[derive(Clone)]
pub struct VolumeFilesystem {
    _ids: Arc<dyn VolumeIdMapper>,
}

impl VolumeFilesystem {
    pub fn new(ids: Arc<dyn VolumeIdMapper>) -> Self {
        Self { _ids: ids }
    }

    pub async fn initialize_root(&self, root: &Path) -> VolumeContentResult<()> {
        let metadata = tokio::fs::symlink_metadata(root).await.map_err(|error| {
            VolumeContentError::Unavailable(format!("inspect volume root: {error}"))
        })?;
        if metadata.file_type().is_symlink() || !metadata.is_dir() {
            return Err(VolumeContentError::InvalidPath(
                "volume root must be an existing non-symlink directory".to_string(),
            ));
        }
        Ok(())
    }

    pub async fn stat(&self, _root: &Path, _path: &str) -> VolumeContentResult<VolumeEntry> {
        Err(unsupported())
    }

    pub async fn list(
        &self,
        _root: &Path,
        _path: &str,
        _depth: u32,
    ) -> VolumeContentResult<Vec<VolumeEntry>> {
        Err(unsupported())
    }

    pub async fn make_dir(
        &self,
        _root: &Path,
        _path: &str,
        _metadata: VolumeMetadataUpdate,
        _force: bool,
    ) -> VolumeContentResult<VolumeEntry> {
        Err(unsupported())
    }

    pub async fn update_metadata(
        &self,
        _root: &Path,
        _path: &str,
        _metadata: VolumeMetadataUpdate,
    ) -> VolumeContentResult<VolumeEntry> {
        Err(unsupported())
    }

    pub async fn remove(&self, _root: &Path, _path: &str) -> VolumeContentResult<()> {
        Err(unsupported())
    }

    pub async fn open_file(
        &self,
        _root: &Path,
        _path: &str,
    ) -> VolumeContentResult<tokio::fs::File> {
        Err(unsupported())
    }

    pub async fn begin_write(
        &self,
        _root: &Path,
        _path: &str,
        _metadata: VolumeMetadataUpdate,
        _force: bool,
    ) -> VolumeContentResult<PendingVolumeWrite> {
        Err(unsupported())
    }
}

pub struct PendingVolumeWrite;

impl PendingVolumeWrite {
    pub async fn write_all(&mut self, _bytes: &[u8]) -> VolumeContentResult<()> {
        Err(unsupported())
    }

    pub async fn finish(self) -> VolumeContentResult<VolumeEntry> {
        Err(unsupported())
    }
}

fn unsupported() -> VolumeContentError {
    VolumeContentError::Unsupported("descriptor-relative filesystem APIs require Unix".to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::volume::IdentityVolumeIdMapper;

    #[tokio::test]
    async fn control_plane_initialization_works_but_content_access_fails_closed() {
        let directory = tempfile::tempdir().unwrap();
        let filesystem = VolumeFilesystem::new(Arc::new(IdentityVolumeIdMapper::current()));

        filesystem.initialize_root(directory.path()).await.unwrap();
        assert!(matches!(
            filesystem.stat(directory.path(), "/").await,
            Err(VolumeContentError::Unsupported(_))
        ));
    }
}
