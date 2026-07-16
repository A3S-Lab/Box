use std::path::{Path, PathBuf};
use std::sync::Arc;

use tokio::io::AsyncWriteExt;

use super::unix_ops;
use super::{
    VolumeContentError, VolumeContentResult, VolumeEntry, VolumeIdMapper, VolumeMetadataUpdate,
};

#[derive(Clone)]
pub struct VolumeFilesystem {
    ids: Arc<dyn VolumeIdMapper>,
}

impl VolumeFilesystem {
    pub fn new(ids: Arc<dyn VolumeIdMapper>) -> Self {
        Self { ids }
    }

    pub async fn initialize_root(&self, root: &Path) -> VolumeContentResult<()> {
        let root = root.to_path_buf();
        let ids = self.ids.clone();
        blocking("initialize volume root", move || {
            unix_ops::initialize_root(&root, ids.as_ref())
        })
        .await
    }

    pub async fn stat(&self, root: &Path, path: &str) -> VolumeContentResult<VolumeEntry> {
        let root = root.to_path_buf();
        let path = path.to_string();
        let ids = self.ids.clone();
        blocking("stat volume path", move || {
            unix_ops::stat_path(&root, &path, ids.as_ref())
        })
        .await
    }

    pub async fn list(
        &self,
        root: &Path,
        path: &str,
        depth: u32,
    ) -> VolumeContentResult<Vec<VolumeEntry>> {
        let root = root.to_path_buf();
        let path = path.to_string();
        let ids = self.ids.clone();
        blocking("list volume directory", move || {
            unix_ops::list_path(&root, &path, depth, ids.as_ref())
        })
        .await
    }

    pub async fn make_dir(
        &self,
        root: &Path,
        path: &str,
        metadata: VolumeMetadataUpdate,
        force: bool,
    ) -> VolumeContentResult<VolumeEntry> {
        let root = root.to_path_buf();
        let path = path.to_string();
        let ids = self.ids.clone();
        blocking("create volume directory", move || {
            unix_ops::make_dir(&root, &path, metadata, force, ids.as_ref())
        })
        .await
    }

    pub async fn update_metadata(
        &self,
        root: &Path,
        path: &str,
        metadata: VolumeMetadataUpdate,
    ) -> VolumeContentResult<VolumeEntry> {
        let root = root.to_path_buf();
        let path = path.to_string();
        let ids = self.ids.clone();
        blocking("update volume metadata", move || {
            unix_ops::update_metadata(&root, &path, metadata, ids.as_ref())
        })
        .await
    }

    pub async fn remove(&self, root: &Path, path: &str) -> VolumeContentResult<()> {
        let root = root.to_path_buf();
        let path = path.to_string();
        blocking("remove volume path", move || {
            unix_ops::remove_path(&root, &path)
        })
        .await
    }

    pub async fn open_file(&self, root: &Path, path: &str) -> VolumeContentResult<tokio::fs::File> {
        let root = root.to_path_buf();
        let path = path.to_string();
        let file = blocking("open volume file", move || {
            unix_ops::open_file(&root, &path)
        })
        .await?;
        Ok(tokio::fs::File::from_std(file))
    }

    pub async fn begin_write(
        &self,
        root: &Path,
        path: &str,
        metadata: VolumeMetadataUpdate,
        force: bool,
    ) -> VolumeContentResult<PendingVolumeWrite> {
        let root = root.to_path_buf();
        let path = path.to_string();
        let ids = self.ids.clone();
        let prepared_root = root.clone();
        let prepared_path = path.clone();
        let (prepared, file) = blocking("prepare volume upload", move || {
            unix_ops::prepare_upload(
                &prepared_root,
                &prepared_path,
                metadata,
                force,
                ids.as_ref(),
            )
        })
        .await?;
        Ok(PendingVolumeWrite {
            filesystem: self.clone(),
            root,
            path,
            prepared: Some(prepared),
            file: Some(tokio::fs::File::from_std(file)),
        })
    }
}

pub struct PendingVolumeWrite {
    filesystem: VolumeFilesystem,
    root: PathBuf,
    path: String,
    prepared: Option<unix_ops::PreparedUpload>,
    file: Option<tokio::fs::File>,
}

impl PendingVolumeWrite {
    pub async fn write_all(&mut self, bytes: &[u8]) -> VolumeContentResult<()> {
        self.file
            .as_mut()
            .ok_or_else(|| VolumeContentError::Unavailable("upload is already complete".into()))?
            .write_all(bytes)
            .await
            .map_err(|error| VolumeContentError::Unavailable(format!("write upload: {error}")))
    }

    pub async fn finish(mut self) -> VolumeContentResult<VolumeEntry> {
        let mut file = self
            .file
            .take()
            .ok_or_else(|| VolumeContentError::Unavailable("upload file is missing".into()))?;
        file.flush()
            .await
            .map_err(|error| VolumeContentError::Unavailable(format!("flush upload: {error}")))?;
        file.sync_all()
            .await
            .map_err(|error| VolumeContentError::Unavailable(format!("sync upload: {error}")))?;
        let file = file.into_std().await;
        let prepared = self.prepared.take().ok_or_else(|| {
            VolumeContentError::Unavailable("upload transaction is missing".into())
        })?;
        blocking("commit volume upload", move || {
            unix_ops::finish_upload(prepared, file)
        })
        .await?;
        self.filesystem.stat(&self.root, &self.path).await
    }
}

async fn blocking<T, F>(operation: &'static str, function: F) -> VolumeContentResult<T>
where
    T: Send + 'static,
    F: FnOnce() -> VolumeContentResult<T> + Send + 'static,
{
    tokio::task::spawn_blocking(function)
        .await
        .map_err(|error| {
            VolumeContentError::Unavailable(format!("{operation} task failed: {error}"))
        })?
}
