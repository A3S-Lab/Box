use async_trait::async_trait;
use thiserror::Error;

use super::{VolumeId, VolumeRecord, VolumeState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum VolumeReplaceResult {
    Updated,
    NotFound,
    Conflict,
}

#[derive(Debug, Error)]
pub enum VolumeRepositoryError {
    #[error("volume already exists")]
    Duplicate,
    #[error("volume repository is unavailable: {0}")]
    Unavailable(String),
    #[error("volume repository contains invalid data: {0}")]
    Corrupt(String),
}

pub type VolumeRepositoryResult<T> = std::result::Result<T, VolumeRepositoryError>;

#[async_trait]
pub trait VolumeRepository: Send + Sync {
    async fn insert(&self, record: VolumeRecord) -> VolumeRepositoryResult<()>;

    async fn get(&self, volume_id: &VolumeId) -> VolumeRepositoryResult<Option<VolumeRecord>>;

    async fn get_by_owner_name(
        &self,
        owner_id: &str,
        name: &str,
    ) -> VolumeRepositoryResult<Option<VolumeRecord>>;

    async fn list(&self, owner_id: &str) -> VolumeRepositoryResult<Vec<VolumeRecord>>;

    async fn list_in_state(
        &self,
        state: VolumeState,
    ) -> VolumeRepositoryResult<Vec<VolumeRecord>>;

    async fn replace(
        &self,
        expected: VolumeState,
        replacement: VolumeRecord,
    ) -> VolumeRepositoryResult<VolumeReplaceResult>;

    async fn delete(
        &self,
        volume_id: &VolumeId,
        expected: VolumeState,
    ) -> VolumeRepositoryResult<VolumeReplaceResult>;
}
