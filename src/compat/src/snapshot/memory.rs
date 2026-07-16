use std::collections::BTreeMap;
use std::sync::{Mutex, MutexGuard};

use async_trait::async_trait;

use super::{
    SnapshotId, SnapshotRecord, SnapshotReplaceResult, SnapshotRepository,
    SnapshotRepositoryError, SnapshotRepositoryResult, SnapshotState,
};

#[derive(Debug, Default)]
pub struct MemorySnapshotRepository {
    records: Mutex<BTreeMap<SnapshotId, SnapshotRecord>>,
}

impl MemorySnapshotRepository {
    fn records(
        &self,
    ) -> SnapshotRepositoryResult<MutexGuard<'_, BTreeMap<SnapshotId, SnapshotRecord>>> {
        self.records.lock().map_err(|_| {
            SnapshotRepositoryError::Unavailable("snapshot repository lock poisoned".to_string())
        })
    }
}

#[async_trait]
impl SnapshotRepository for MemorySnapshotRepository {
    async fn insert(&self, record: SnapshotRecord) -> SnapshotRepositoryResult<()> {
        record
            .validate()
            .map_err(|error| SnapshotRepositoryError::Corrupt(error.to_string()))?;
        let mut records = self.records()?;
        if records.contains_key(record.snapshot_id())
            || records.values().any(|existing| {
                existing.owner_id() == record.owner_id()
                    && existing.reference() == record.reference()
            })
        {
            return Err(SnapshotRepositoryError::Duplicate);
        }
        records.insert(record.snapshot_id().clone(), record);
        Ok(())
    }

    async fn get(
        &self,
        snapshot_id: &SnapshotId,
    ) -> SnapshotRepositoryResult<Option<SnapshotRecord>> {
        Ok(self.records()?.get(snapshot_id).cloned())
    }

    async fn get_by_reference(
        &self,
        owner_id: &str,
        reference: &str,
    ) -> SnapshotRepositoryResult<Option<SnapshotRecord>> {
        Ok(self
            .records()?
            .values()
            .find(|record| record.owner_id() == owner_id && record.reference() == reference)
            .cloned())
    }

    async fn list(&self, owner_id: &str) -> SnapshotRepositoryResult<Vec<SnapshotRecord>> {
        let mut records = self
            .records()?
            .values()
            .filter(|record| {
                record.owner_id() == owner_id && record.state() == SnapshotState::Active
            })
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by_key(|record| (record.created_at(), record.snapshot_id().clone()));
        Ok(records)
    }

    async fn list_in_state(
        &self,
        state: SnapshotState,
    ) -> SnapshotRepositoryResult<Vec<SnapshotRecord>> {
        let mut records = self
            .records()?
            .values()
            .filter(|record| record.state() == state)
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by_key(|record| (record.created_at(), record.snapshot_id().clone()));
        Ok(records)
    }

    async fn replace(
        &self,
        expected: SnapshotState,
        replacement: SnapshotRecord,
    ) -> SnapshotRepositoryResult<SnapshotReplaceResult> {
        replacement
            .validate()
            .map_err(|error| SnapshotRepositoryError::Corrupt(error.to_string()))?;
        let mut records = self.records()?;
        let Some(current) = records.get(replacement.snapshot_id()) else {
            return Ok(SnapshotReplaceResult::NotFound);
        };
        if current.state() != expected {
            return Ok(SnapshotReplaceResult::Conflict);
        }
        records.insert(replacement.snapshot_id().clone(), replacement);
        Ok(SnapshotReplaceResult::Updated)
    }

    async fn delete(
        &self,
        snapshot_id: &SnapshotId,
        expected: SnapshotState,
    ) -> SnapshotRepositoryResult<SnapshotReplaceResult> {
        let mut records = self.records()?;
        let Some(current) = records.get(snapshot_id) else {
            return Ok(SnapshotReplaceResult::NotFound);
        };
        if current.state() != expected {
            return Ok(SnapshotReplaceResult::Conflict);
        }
        records.remove(snapshot_id);
        Ok(SnapshotReplaceResult::Updated)
    }
}
