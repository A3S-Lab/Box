//! Durable implementation of the backend-neutral local execution lifecycle.

mod api;
mod backend;
mod create;
mod operations;
mod record;
mod recovery;
mod store;
mod support;

use std::path::PathBuf;
use std::sync::Arc;

pub use backend::{LocalExecutionBackend, LocalExecutionHandle, LocalExecutionObservation};
use record::{build_managed_record, status_from_record};
use store::RuntimeUpdate;

use crate::{BoxRecord, ManagedExecutionOperation, ManagedExecutionState, ManagedExecutionStore};

/// Local lifecycle facade shared by service, CLI, and SDK adapters.
#[derive(Clone)]
pub struct LocalExecutionManager {
    store: ManagedExecutionStore,
    home_dir: PathBuf,
    backend: Arc<dyn LocalExecutionBackend>,
}

impl LocalExecutionManager {
    pub fn new(
        state_path: impl Into<PathBuf>,
        home_dir: impl Into<PathBuf>,
        backend: Arc<dyn LocalExecutionBackend>,
    ) -> Self {
        Self {
            store: ManagedExecutionStore::new(state_path),
            home_dir: home_dir.into(),
            backend,
        }
    }

    pub fn state_path(&self) -> &std::path::Path {
        self.store.path()
    }
}

#[cfg(test)]
mod tests;
