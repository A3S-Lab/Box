use std::collections::HashSet;
use std::sync::{Arc, Mutex, MutexGuard};

use super::SnapshotId;

#[derive(Debug, Default)]
pub(super) struct SnapshotOperations {
    claimed: Mutex<HashSet<SnapshotId>>,
}

impl SnapshotOperations {
    pub(super) fn try_claim(
        self: &Arc<Self>,
        snapshot_id: &SnapshotId,
    ) -> Option<SnapshotOperationClaim> {
        let mut claimed = lock_recovering_poison(&self.claimed);
        if !claimed.insert(snapshot_id.clone()) {
            return None;
        }
        Some(SnapshotOperationClaim {
            operations: self.clone(),
            snapshot_id: snapshot_id.clone(),
        })
    }
}

#[derive(Debug)]
pub(super) struct SnapshotOperationClaim {
    operations: Arc<SnapshotOperations>,
    snapshot_id: SnapshotId,
}

impl Drop for SnapshotOperationClaim {
    fn drop(&mut self) {
        lock_recovering_poison(&self.operations.claimed).remove(&self.snapshot_id);
    }
}

fn lock_recovering_poison<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}
