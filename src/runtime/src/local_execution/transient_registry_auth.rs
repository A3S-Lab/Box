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
        key: &str,
        auth: RegistryAuth,
    ) -> ExecutionManagerResult<TransientRegistryAuthLease> {
        let token = uuid::Uuid::new_v4();
        match self.entries.entry(key.to_owned()) {
            Entry::Vacant(entry) => {
                entry.insert(PendingRegistryAuth { token, auth });
            }
            Entry::Occupied(_) => {
                return Err(ExecutionManagerError::Unavailable(format!(
                    "transient registry authorization for {key} is already pending"
                )));
            }
        }
        Ok(TransientRegistryAuthLease {
            broker: self.clone(),
            key: key.to_owned(),
            token,
        })
    }

    pub(crate) fn take(&self, key: &str) -> Option<RegistryAuth> {
        self.entries.remove(key).map(|(_, pending)| pending.auth)
    }

    /// Clone an authorization for a pre-reservation planning pass.
    ///
    /// Planning must be able to inspect a private image before the execution
    /// has an internal ID, but the same authorization still has to remain
    /// available for the subsequent boot pull. Keeping the value in this
    /// in-memory broker avoids writing caller credentials to the durable Box
    /// record or credential store.
    pub(crate) fn clone_auth(&self, key: &str) -> Option<RegistryAuth> {
        self.entries.get(key).map(|pending| pending.auth.clone())
    }

    /// Create a lease for an authorization that is already staged under `key`.
    ///
    /// The lease is intentionally independent of the entry's ownership. The
    /// boot path consumes the entry, after which dropping this lease is a safe
    /// no-op. If boot never claims it, dropping the lease removes the pending
    /// credential and closes the transient handoff.
    pub(crate) fn lease(&self, key: &str) -> Option<TransientRegistryAuthLease> {
        let token = self.entries.get(key)?.token;
        Some(TransientRegistryAuthLease {
            broker: self.clone(),
            key: key.to_owned(),
            token,
        })
    }

    /// Move a pre-reservation authorization to the durable execution key.
    ///
    /// The source lease remains valid and only releases the source key. The
    /// caller should retain the returned target lease until the execution has
    /// either claimed the authorization during boot or failed before boot.
    pub(crate) fn promote(
        &self,
        source: &str,
        target: &str,
    ) -> ExecutionManagerResult<TransientRegistryAuthLease> {
        if source == target {
            return self.lease(target).ok_or_else(|| {
                ExecutionManagerError::Unavailable(format!(
                    "transient registry authorization for {target} is no longer pending"
                ))
            });
        }

        let (_, pending) = self.entries.remove(source).ok_or_else(|| {
            ExecutionManagerError::Unavailable(format!(
                "transient registry authorization for {source} is no longer pending"
            ))
        })?;
        let token = pending.token;
        match self.entries.entry(target.to_owned()) {
            Entry::Vacant(entry) => {
                entry.insert(pending);
                Ok(TransientRegistryAuthLease {
                    broker: self.clone(),
                    key: target.to_owned(),
                    token,
                })
            }
            Entry::Occupied(_) => {
                // Preserve the source on a conflict so the caller's source
                // lease can still clean up the credential deterministically.
                self.entries.insert(source.to_owned(), pending);
                Err(ExecutionManagerError::Unavailable(format!(
                    "transient registry authorization for {target} is already pending"
                )))
            }
        }
    }

    fn release(&self, key: &str, token: uuid::Uuid) {
        if let Entry::Occupied(entry) = self.entries.entry(key.to_owned()) {
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
    key: String,
    token: uuid::Uuid,
}

impl Drop for TransientRegistryAuthLease {
    fn drop(&mut self) {
        self.broker.release(&self.key, self.token);
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

    #[test]
    fn planning_can_clone_and_promote_authorization_without_persisting_it() {
        let broker = TransientRegistryAuthBroker::default();
        let source = broker
            .bind("operation", RegistryAuth::basic("user", "password"))
            .unwrap();

        assert_eq!(
            broker
                .clone_auth("operation")
                .and_then(|auth| auth.basic_credentials()),
            Some(("user".into(), "password".into()))
        );

        let target = broker.promote("operation", "execution").unwrap();
        assert!(broker.clone_auth("operation").is_none());
        assert_eq!(
            broker
                .clone_auth("execution")
                .and_then(|auth| auth.basic_credentials()),
            Some(("user".into(), "password".into()))
        );

        drop(source);
        assert_eq!(broker.pending(), 1);
        drop(target);
        assert_eq!(broker.pending(), 0);
    }

    #[test]
    fn existing_authorization_can_be_leased_for_boot() {
        let broker = TransientRegistryAuthBroker::default();
        let staged = broker
            .bind("execution", RegistryAuth::basic("user", "password"))
            .unwrap();
        let boot = broker.lease("execution").unwrap();
        assert_eq!(broker.pending(), 1);
        assert!(broker.take("execution").is_some());
        drop(boot);
        drop(staged);
        assert_eq!(broker.pending(), 0);
    }
}
