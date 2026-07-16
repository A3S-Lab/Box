mod memory;
mod model;
mod repository;
mod service;
mod sqlite;
mod template;

pub use memory::MemorySnapshotRepository;
pub use model::{
    validate_snapshot_name, SnapshotId, SnapshotModelError, SnapshotRecord, SnapshotState,
};
pub use repository::{
    SnapshotReplaceResult, SnapshotRepository, SnapshotRepositoryError, SnapshotRepositoryResult,
};
pub use service::{
    PendingSnapshot, SnapshotCursor, SnapshotPage, SnapshotReconciliationReport,
    SnapshotService, SnapshotServiceDependencies, SnapshotServiceError, SnapshotServiceResult,
};
pub use sqlite::SqliteSnapshotRepository;
pub use template::SnapshotTemplateProvider;

#[cfg(test)]
mod tests;
