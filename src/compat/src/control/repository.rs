use std::collections::{BTreeMap, BTreeSet};
use std::num::NonZeroU32;

use async_trait::async_trait;
use chrono::{DateTime, Utc};
use thiserror::Error;

use super::{PublicSandboxState, SandboxGeneration, SandboxId, SandboxRecord};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxCursor {
    pub created_at: DateTime<Utc>,
    pub sandbox_id: SandboxId,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SandboxListFilter {
    pub owner_id: String,
    pub metadata: BTreeMap<String, String>,
    pub states: BTreeSet<PublicSandboxState>,
    pub limit: NonZeroU32,
    pub after: Option<SandboxCursor>,
}

#[derive(Debug, Clone)]
pub struct SandboxPage {
    pub records: Vec<SandboxRecord>,
    pub next: Option<SandboxCursor>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CompareAndSwapResult {
    Updated,
    NotFound,
    Conflict {
        actual_generation: SandboxGeneration,
    },
}

#[derive(Debug, Error)]
pub enum RepositoryError {
    #[error("sandbox already exists: {0}")]
    Duplicate(SandboxId),
    #[error("sandbox repository unavailable: {0}")]
    Unavailable(String),
    #[error("sandbox repository contains invalid data: {0}")]
    Corrupt(String),
}

pub type RepositoryResult<T> = std::result::Result<T, RepositoryError>;

/// Transactional persistence boundary for compatibility lifecycle records.
#[async_trait]
pub trait SandboxRepository: Send + Sync {
    async fn insert(&self, record: SandboxRecord) -> RepositoryResult<()>;

    async fn get(&self, sandbox_id: &SandboxId) -> RepositoryResult<Option<SandboxRecord>>;

    async fn list(&self, filter: &SandboxListFilter) -> RepositoryResult<SandboxPage>;

    /// Page through every non-terminal record that startup reconciliation must inspect.
    async fn list_reconcilable(
        &self,
        after: Option<&SandboxCursor>,
        limit: NonZeroU32,
    ) -> RepositoryResult<SandboxPage>;

    /// Atomically claim actionable records whose expiry is at or before `deadline`.
    ///
    /// Returned records have already advanced to `pausing` or `killing`. A
    /// concurrent timeout replacement must therefore either commit before the
    /// claim and make the record ineligible, or observe a generation conflict.
    async fn claim_expired(
        &self,
        deadline: DateTime<Utc>,
        limit: NonZeroU32,
    ) -> RepositoryResult<Vec<SandboxRecord>>;

    /// Replace one record only when its persisted generation equals `expected`.
    ///
    /// Implementations must reject a replacement with a different sandbox ID
    /// or a generation that does not advance `expected`.
    async fn compare_and_swap(
        &self,
        sandbox_id: &SandboxId,
        expected: SandboxGeneration,
        replacement: SandboxRecord,
    ) -> RepositoryResult<CompareAndSwapResult>;
}
