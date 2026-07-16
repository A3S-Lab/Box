use async_trait::async_trait;
use thiserror::Error;

use super::{SnapshotId, SnapshotRecord, SnapshotState};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SnapshotReplaceResult {
    Updated,
    NotFound,
    Conflict,
}

#[derive(Debug, Error)]
pub enum SnapshotRepositoryError {
    #[error("snapshot already exists")]
    Duplicate,
    #[error("snapshot repository is unavailable: {0}")]
    Unavailable(String),
    #[error("snapshot repository contains invalid data: {0}")]
    Corrupt(String),
}

pub type SnapshotRepositoryResult<T> = std::result::Result<T, SnapshotRepositoryError>;

#[async_trait]
pub trait SnapshotRepository: Send + Sync {
    async fn insert(&self, record: SnapshotRecord) -> SnapshotRepositoryResult<()>;

    async fn get(&self, snapshot_id: &SnapshotId)
        -> SnapshotRepositoryResult<Option<SnapshotRecord>>;

    async fn get_by_reference(
        &self,
        owner_id: &str,
        reference: &str,
    ) -> SnapshotRepositoryResult<Option<SnapshotRecord>>;

    async fn list(&self, owner_id: &str) -> SnapshotRepositoryResult<Vec<SnapshotRecord>>;

    async fn list_in_state(
        &self,
        state: SnapshotState,
    ) -> SnapshotRepositoryResult<Vec<SnapshotRecord>>;

    async fn replace(
        &self,
        expected: SnapshotState,
        replacement: SnapshotRecord,
    ) -> SnapshotRepositoryResult<SnapshotReplaceResult>;

    async fn delete(
        &self,
        snapshot_id: &SnapshotId,
        expected: SnapshotState,
    ) -> SnapshotRepositoryResult<SnapshotReplaceResult>;
}
