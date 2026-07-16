use std::path::{Path, PathBuf};
use std::sync::Arc;

use a3s_box_core::volume::VolumeConfig;
use a3s_box_runtime::VolumeStore;
use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuntimeVolume {
    pub name: String,
    pub mount_point: PathBuf,
    pub in_use_by: Vec<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RuntimeVolumeRemoveResult {
    Removed,
    NotFound,
}

#[derive(Debug, Error)]
pub enum RuntimeVolumeError {
    #[error("runtime volume is in use")]
    InUse,
    #[error("runtime volume store is unavailable: {0}")]
    Unavailable(String),
}

pub type RuntimeVolumeResult<T> = std::result::Result<T, RuntimeVolumeError>;

#[async_trait]
pub trait RuntimeVolumeStore: Send + Sync {
    async fn materialize(&self, name: &str) -> RuntimeVolumeResult<RuntimeVolume>;

    async fn get(&self, name: &str) -> RuntimeVolumeResult<Option<RuntimeVolume>>;

    async fn remove(&self, name: &str) -> RuntimeVolumeResult<RuntimeVolumeRemoveResult>;
}

#[derive(Debug, Clone)]
pub struct A3sRuntimeVolumeStore {
    store: Arc<VolumeStore>,
    volume_root: PathBuf,
}

impl A3sRuntimeVolumeStore {
    pub fn new(runtime_home: impl AsRef<Path>) -> Self {
        let runtime_home = runtime_home.as_ref();
        let volume_root = runtime_home.join("volumes");
        Self {
            store: Arc::new(VolumeStore::new(
                runtime_home.join("volumes.json"),
                &volume_root,
            )),
            volume_root,
        }
    }
}

#[async_trait]
impl RuntimeVolumeStore for A3sRuntimeVolumeStore {
    async fn materialize(&self, name: &str) -> RuntimeVolumeResult<RuntimeVolume> {
        let name = name.to_string();
        let store = self.store.clone();
        let volume_root = self.volume_root.clone();
        tokio::task::spawn_blocking(move || {
            let volume = store
                .get_or_create(VolumeConfig::new(&name, ""))
                .map_err(|error| unavailable("materialize volume", error))?;
            runtime_volume(volume, &volume_root)
        })
        .await
        .map_err(|error| RuntimeVolumeError::Unavailable(format!("volume task failed: {error}")))?
    }

    async fn get(&self, name: &str) -> RuntimeVolumeResult<Option<RuntimeVolume>> {
        let name = name.to_string();
        let store = self.store.clone();
        let volume_root = self.volume_root.clone();
        tokio::task::spawn_blocking(move || {
            store
                .get(&name)
                .map_err(|error| unavailable("load volume", error))?
                .map(|volume| runtime_volume(volume, &volume_root))
                .transpose()
        })
        .await
        .map_err(|error| RuntimeVolumeError::Unavailable(format!("volume task failed: {error}")))?
    }

    async fn remove(&self, name: &str) -> RuntimeVolumeResult<RuntimeVolumeRemoveResult> {
        let name = name.to_string();
        let store = self.store.clone();
        let volume_root = self.volume_root.clone();
        tokio::task::spawn_blocking(move || {
            let configured = store
                .get(&name)
                .map_err(|error| unavailable("load volume before removal", error))?;
            if configured.as_ref().is_some_and(VolumeConfig::is_in_use) {
                return Err(RuntimeVolumeError::InUse);
            }

            let data_path = volume_root.join(&name);
            let result = match configured {
                Some(_) => match store.remove(&name, false) {
                    Ok(_) => RuntimeVolumeRemoveResult::Removed,
                    Err(error) => {
                        let current = store
                            .get(&name)
                            .map_err(|load_error| unavailable("reload volume after removal", load_error))?;
                        if current.as_ref().is_some_and(VolumeConfig::is_in_use) {
                            return Err(RuntimeVolumeError::InUse);
                        }
                        return Err(unavailable("remove volume", error));
                    }
                },
                None => RuntimeVolumeRemoveResult::NotFound,
            };

            if data_path.exists() {
                std::fs::remove_dir_all(&data_path)
                    .map_err(|error| unavailable("remove volume data", error))?;
            }
            Ok(result)
        })
        .await
        .map_err(|error| RuntimeVolumeError::Unavailable(format!("volume task failed: {error}")))?
    }
}

fn runtime_volume(
    volume: VolumeConfig,
    volume_root: &Path,
) -> RuntimeVolumeResult<RuntimeVolume> {
    let canonical_root = volume_root
        .canonicalize()
        .map_err(|error| unavailable("canonicalize volume root", error))?;
    let mount_point = PathBuf::from(&volume.mount_point)
        .canonicalize()
        .map_err(|error| unavailable("canonicalize volume mount point", error))?;
    let metadata = std::fs::metadata(&mount_point)
        .map_err(|error| unavailable("inspect volume mount point", error))?;
    if !metadata.is_dir() || mount_point.parent() != Some(canonical_root.as_path()) {
        return Err(RuntimeVolumeError::Unavailable(format!(
            "volume '{}' resolved outside its managed root",
            volume.name
        )));
    }
    Ok(RuntimeVolume {
        name: volume.name,
        mount_point,
        in_use_by: volume.in_use_by,
    })
}

fn unavailable(context: &str, error: impl std::fmt::Display) -> RuntimeVolumeError {
    RuntimeVolumeError::Unavailable(format!("{context}: {error}"))
}
