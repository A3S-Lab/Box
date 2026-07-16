use std::collections::BTreeMap;
use std::sync::Mutex;

use async_trait::async_trait;

use super::{
    VolumeId, VolumeRecord, VolumeReplaceResult, VolumeRepository, VolumeRepositoryError,
    VolumeRepositoryResult, VolumeState,
};

#[derive(Debug, Default)]
pub struct MemoryVolumeRepository {
    records: Mutex<BTreeMap<VolumeId, VolumeRecord>>,
}

#[async_trait]
impl VolumeRepository for MemoryVolumeRepository {
    async fn insert(&self, record: VolumeRecord) -> VolumeRepositoryResult<()> {
        record
            .validate()
            .map_err(|error| VolumeRepositoryError::Corrupt(error.to_string()))?;
        let mut records = self.records.lock().map_err(lock_error)?;
        if records.contains_key(record.volume_id())
            || records.values().any(|existing| {
                existing.owner_id() == record.owner_id() && existing.name() == record.name()
            })
        {
            return Err(VolumeRepositoryError::Duplicate);
        }
        records.insert(record.volume_id().clone(), record);
        Ok(())
    }

    async fn get(&self, volume_id: &VolumeId) -> VolumeRepositoryResult<Option<VolumeRecord>> {
        Ok(self
            .records
            .lock()
            .map_err(lock_error)?
            .get(volume_id)
            .cloned())
    }

    async fn get_by_owner_name(
        &self,
        owner_id: &str,
        name: &str,
    ) -> VolumeRepositoryResult<Option<VolumeRecord>> {
        Ok(self
            .records
            .lock()
            .map_err(lock_error)?
            .values()
            .find(|record| record.owner_id() == owner_id && record.name() == name)
            .cloned())
    }

    async fn list(&self, owner_id: &str) -> VolumeRepositoryResult<Vec<VolumeRecord>> {
        let mut records = self
            .records
            .lock()
            .map_err(lock_error)?
            .values()
            .filter(|record| {
                record.owner_id() == owner_id && record.state() == VolumeState::Active
            })
            .cloned()
            .collect::<Vec<_>>();
        records.sort_by(|left, right| {
            left.created_at()
                .cmp(&right.created_at())
                .then_with(|| left.volume_id().cmp(right.volume_id()))
        });
        Ok(records)
    }

    async fn list_in_state(
        &self,
        state: VolumeState,
    ) -> VolumeRepositoryResult<Vec<VolumeRecord>> {
        Ok(self
            .records
            .lock()
            .map_err(lock_error)?
            .values()
            .filter(|record| record.state() == state)
            .cloned()
            .collect())
    }

    async fn replace(
        &self,
        expected: VolumeState,
        replacement: VolumeRecord,
    ) -> VolumeRepositoryResult<VolumeReplaceResult> {
        replacement
            .validate()
            .map_err(|error| VolumeRepositoryError::Corrupt(error.to_string()))?;
        let mut records = self.records.lock().map_err(lock_error)?;
        let Some(current) = records.get(replacement.volume_id()) else {
            return Ok(VolumeReplaceResult::NotFound);
        };
        if current.state() != expected {
            return Ok(VolumeReplaceResult::Conflict);
        }
        records.insert(replacement.volume_id().clone(), replacement);
        Ok(VolumeReplaceResult::Updated)
    }

    async fn delete(
        &self,
        volume_id: &VolumeId,
        expected: VolumeState,
    ) -> VolumeRepositoryResult<VolumeReplaceResult> {
        let mut records = self.records.lock().map_err(lock_error)?;
        let Some(current) = records.get(volume_id) else {
            return Ok(VolumeReplaceResult::NotFound);
        };
        if current.state() != expected {
            return Ok(VolumeReplaceResult::Conflict);
        }
        records.remove(volume_id);
        Ok(VolumeReplaceResult::Updated)
    }
}

fn lock_error<T>(_: std::sync::PoisonError<T>) -> VolumeRepositoryError {
    VolumeRepositoryError::Unavailable("memory volume repository lock is poisoned".to_string())
}
