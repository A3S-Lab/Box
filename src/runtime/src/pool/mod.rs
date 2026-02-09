//! Warm VM pool for cold start optimization.
//!
//! Pre-boots MicroVMs so that `acquire()` returns an already-ready VM
//! instead of waiting for the full boot sequence.

pub mod warm_pool;

pub use warm_pool::{PoolStats, WarmPool};
