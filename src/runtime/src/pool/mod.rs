//! Warm VM pool for cold start optimization.
//!
//! Pre-boots MicroVMs so that `acquire()` returns an already-ready VM
//! instead of waiting for the full boot sequence.

pub mod client;
mod daemon;
mod registry;
pub mod scaler;
mod serve;
pub mod warm_pool;

pub use client::{
    PoolClientLease, PoolClientOutput, PoolClientRun, PoolImageStat, PoolLeaseClient,
    PoolLeaseExec, PoolLeaseExecRequest, PoolLeaseReleaseRequest, PoolLeaseReleaseResponse,
    PoolLeaseRequest, PoolLeaseResponse, PoolRequest, PoolRunRequest, PoolRunResponse,
    PoolStatusResponse, PoolStopResponse,
};
pub use scaler::{PoolScaler, ScaleDecision};
pub use warm_pool::{PoolStats, WarmPool};

pub use daemon::{
    start_pool_daemon, validate_pool_daemon_config, PoolDaemon, PoolDaemonConfig, PoolDaemonReport,
};
pub use registry::{
    DEFAULT_POOL_BOOT_CONCURRENCY, DEFAULT_POOL_LEASE_TTL_SECS, DEFAULT_POOL_MEMORY,
    DEFAULT_POOL_MEMORY_MB, DEFAULT_POOL_VCPUS,
};
