//! In-memory handoff for one execution's registry pull credentials.

use std::sync::Arc;

use a3s_box_core::{ExecutionManagerError, ExecutionManagerResult};
use dashmap::mapref::entry::Entry;
use dashmap::DashMap;

use crate::oci::RegistryAuth;

#[derive(Clone, Default)]
pub(crate) struct TransientRegistryAuthBroker {
    entries: Arc<DashMap<String, PendingRegistryAuth>>,
}

struct PendingRegistryAuth {
    token: uuid::Uuid,
    auth: RegistryAuth,
}

impl TransientRegistryAuthBroker {
    pub(crate) fn bind(
        &self,
        execution_id: &str,
        auth: RegistryAuth,
    ) -> ExecutionManagerResult<TransientRegistryAuthLease> {
        let token = uuid::Uuid::new_v4();
        match self.entries.entry(execution_id.to_owned()) {
            Entry::Vacant(entry) => {
                entry.insert(PendingRegistryAuth { token, auth });
            }
            Entry::Occupied(_) => {
                return Err(ExecutionManagerError::Unavailable(format!(
                    "execution {execution_id} already has a pending transient registry credential"
                )));
            }
        }
        Ok(TransientRegistryAuthLease {
            broker: self.clone(),
            execution_id: execution_id.to_owned(),
            token,
        })
    }

    pub(crate) fn take(&self, execution_id: &str) -> Option<RegistryAuth> {
        self.entries
            .remove(execution_id)
            .map(|(_, pending)| pending.auth)
    }

    fn release(&self, execution_id: &str, token: uuid::Uuid) {
        if let Entry::Occupied(entry) = self.entries.entry(execution_id.to_owned()) {
            if entry.get().token == token {
                entry.remove();
            }
        }
    }

    #[cfg(test)]
    pub(crate) fn pending(&self) -> usize {
        self.entries.len()
    }
}

pub(crate) struct TransientRegistryAuthLease {
    broker: TransientRegistryAuthBroker,
    execution_id: String,
    token: uuid::Uuid,
}

impl Drop for TransientRegistryAuthLease {
    fn drop(&mut self) {
        self.broker.release(&self.execution_id, self.token);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_does_not_remove_a_newer_handoff_after_consumption() {
        let broker = TransientRegistryAuthBroker::default();
        let first = broker
            .bind("execution", RegistryAuth::basic("first", "credential"))
            .unwrap();
        assert!(broker.take("execution").is_some());

        let second = broker
            .bind("execution", RegistryAuth::basic("second", "credential"))
            .unwrap();
        drop(first);

        assert_eq!(broker.pending(), 1);
        drop(second);
        assert_eq!(broker.pending(), 0);
    }
}
