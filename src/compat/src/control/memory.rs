use std::collections::BTreeMap;
use std::num::NonZeroU32;
use std::sync::{RwLock, RwLockReadGuard, RwLockWriteGuard};

use async_trait::async_trait;
use chrono::{DateTime, Utc};

use super::{
    CompareAndSwapResult, LifecycleState, OnTimeoutAction, RepositoryError, RepositoryResult,
    SandboxCursor, SandboxGeneration, SandboxId, SandboxListFilter, SandboxPage, SandboxRecord,
    SandboxRepository,
};

/// Process-local repository used by protocol fixtures and focused tests.
///
/// It deliberately has no durability guarantees. The production compatibility
/// service uses the transactional repository introduced by the persistence
/// slice.
#[derive(Debug, Default)]
pub struct MemorySandboxRepository {
    records: RwLock<BTreeMap<SandboxId, SandboxRecord>>,
}

impl MemorySandboxRepository {
    fn read(&self) -> RepositoryResult<RwLockReadGuard<'_, BTreeMap<SandboxId, SandboxRecord>>> {
        self.records
            .read()
            .map_err(|_| RepositoryError::Unavailable("memory repository lock poisoned".into()))
    }

    fn write(&self) -> RepositoryResult<RwLockWriteGuard<'_, BTreeMap<SandboxId, SandboxRecord>>> {
        self.records
            .write()
            .map_err(|_| RepositoryError::Unavailable("memory repository lock poisoned".into()))
    }
}

#[async_trait]
impl SandboxRepository for MemorySandboxRepository {
    async fn insert(&self, record: SandboxRecord) -> RepositoryResult<()> {
        let mut records = self.write()?;
        if records.contains_key(record.sandbox_id()) {
            return Err(RepositoryError::Duplicate(record.sandbox_id().clone()));
        }
        records.insert(record.sandbox_id().clone(), record);
        Ok(())
    }

    async fn get(&self, sandbox_id: &SandboxId) -> RepositoryResult<Option<SandboxRecord>> {
        Ok(self.read()?.get(sandbox_id).cloned())
    }

    async fn list(&self, filter: &SandboxListFilter) -> RepositoryResult<SandboxPage> {
        let records = self.read()?;
        let mut matching = records
            .values()
            .filter(|record| record.owner_id() == filter.owner_id)
            .filter(|record| {
                record
                    .public_state()
                    .is_some_and(|state| filter.states.is_empty() || filter.states.contains(&state))
            })
            .filter(|record| {
                filter
                    .metadata
                    .iter()
                    .all(|(key, value)| record.metadata().get(key) == Some(value))
            })
            .filter(|record| {
                filter.after.as_ref().is_none_or(|cursor| {
                    (record.created_at(), record.sandbox_id())
                        > (cursor.created_at, &cursor.sandbox_id)
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        matching.sort_by(|left, right| {
            (left.created_at(), left.sandbox_id()).cmp(&(right.created_at(), right.sandbox_id()))
        });

        let limit = filter.limit.get() as usize;
        let has_more = matching.len() > limit;
        matching.truncate(limit);
        let next = has_more
            .then(|| matching.last())
            .flatten()
            .map(|last| super::SandboxCursor {
                created_at: last.created_at(),
                sandbox_id: last.sandbox_id().clone(),
            });
        Ok(SandboxPage {
            records: matching,
            next,
        })
    }

    async fn list_reconcilable(
        &self,
        after: Option<&SandboxCursor>,
        limit: NonZeroU32,
    ) -> RepositoryResult<SandboxPage> {
        let records = self.read()?;
        let mut matching = records
            .values()
            .filter(|record| !record.is_terminal())
            .filter(|record| {
                after.is_none_or(|cursor| {
                    (record.created_at(), record.sandbox_id())
                        > (cursor.created_at, &cursor.sandbox_id)
                })
            })
            .cloned()
            .collect::<Vec<_>>();
        matching.sort_by(|left, right| {
            (left.created_at(), left.sandbox_id()).cmp(&(right.created_at(), right.sandbox_id()))
        });

        let limit = limit.get() as usize;
        let has_more = matching.len() > limit;
        matching.truncate(limit);
        let next = has_more
            .then(|| matching.last())
            .flatten()
            .map(|last| SandboxCursor {
                created_at: last.created_at(),
                sandbox_id: last.sandbox_id().clone(),
            });
        Ok(SandboxPage {
            records: matching,
            next,
        })
    }

    async fn claim_expired(
        &self,
        deadline: DateTime<Utc>,
        limit: NonZeroU32,
    ) -> RepositoryResult<Vec<SandboxRecord>> {
        let mut records = self.write()?;
        let mut candidates = records
            .values()
            .filter(|record| record.expires_at() <= deadline)
            .filter(|record| {
                matches!(
                    (record.state(), record.lifecycle().on_timeout),
                    (
                        LifecycleState::Running,
                        OnTimeoutAction::Kill | OnTimeoutAction::Pause
                    ) | (LifecycleState::Paused, OnTimeoutAction::Kill)
                )
            })
            .map(|record| {
                (
                    record.expires_at(),
                    record.sandbox_id().clone(),
                    record.lifecycle().on_timeout,
                )
            })
            .collect::<Vec<_>>();
        candidates.sort_by(|left, right| (left.0, &left.1).cmp(&(right.0, &right.1)));
        candidates.truncate(limit.get() as usize);

        let mut claimed = Vec::with_capacity(candidates.len());
        for (_, sandbox_id, action) in candidates {
            let record = records.get_mut(&sandbox_id).ok_or_else(|| {
                RepositoryError::Corrupt(format!(
                    "expired memory record disappeared during claim: {sandbox_id}"
                ))
            })?;
            match action {
                OnTimeoutAction::Kill => record.begin_kill(),
                OnTimeoutAction::Pause => record.begin_pause(),
            }
            .map_err(|error| {
                RepositoryError::Corrupt(format!(
                    "cannot claim expired memory record {sandbox_id}: {error}"
                ))
            })?;
            claimed.push(record.clone());
        }
        Ok(claimed)
    }

    async fn compare_and_swap(
        &self,
        sandbox_id: &SandboxId,
        expected: SandboxGeneration,
        replacement: SandboxRecord,
    ) -> RepositoryResult<CompareAndSwapResult> {
        if replacement.sandbox_id() != sandbox_id || replacement.generation() <= expected {
            return Err(RepositoryError::Corrupt(
                "invalid compare-and-swap replacement".to_string(),
            ));
        }

        let mut records = self.write()?;
        let Some(current) = records.get(sandbox_id) else {
            return Ok(CompareAndSwapResult::NotFound);
        };
        if current.generation() != expected {
            return Ok(CompareAndSwapResult::Conflict {
                actual_generation: current.generation(),
            });
        }
        records.insert(sandbox_id.clone(), replacement);
        Ok(CompareAndSwapResult::Updated)
    }
}
