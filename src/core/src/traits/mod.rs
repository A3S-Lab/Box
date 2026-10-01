//! Core trait abstractions for pluggable backends.
//!
//! These traits define the extension points of A3S Box that have more than
//! one real implementation or active cross-crate consumers today: the
//! `ExecutionManager` execution seam and the `ExecutionSessionManager`
//! process-session seam, plus the `CacheBackend` cache seam. Traits with a
//! single implementation and no consumers are deliberately kept out of this
//! module; introduce a trait only when a second implementation exists.

pub mod cache;
pub mod execution;
pub mod session;
pub mod store;

pub use cache::{CacheBackend, CacheEntry, CacheStats};
pub use execution::{
    CreateExecutionRequest, ExecutionCpuStats, ExecutionEventBatch, ExecutionEventKind,
    ExecutionEventsRequest, ExecutionGeneration, ExecutionHealthCheck, ExecutionId, ExecutionLease,
    ExecutionManager, ExecutionManagerError, ExecutionManagerResult, ExecutionMemoryStats,
    ExecutionPortConnector, ExecutionPortIo, ExecutionPortStream, ExecutionProcessInfo,
    ExecutionProcessInventory, ExecutionRecordPolicy, ExecutionReservation,
    ExecutionResourceUpdate, ExecutionRestartPolicy, ExecutionRuntimeEvent, ExecutionSnapshot,
    ExecutionSnapshotId, ExecutionState, ExecutionStats, ExecutionStatus, ExecutionUdpPort,
    ExecutionUdpPortIo, KillExecutionOptions, KillOutcome, OperationId, ReconcileOutcome,
    RestartExecutionOptions, MAX_EXECUTION_EVENT_BATCH_ITEMS,
};
pub use session::{
    ExecutionProcess, ExecutionProcessInput, ExecutionProcessSignal, ExecutionProcessStream,
    ExecutionSessionManager,
};
pub use store::StoredImage;
