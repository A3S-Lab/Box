//! Warm VM pool for cold start optimization.
//!
//! Pre-boots MicroVMs so that `acquire()` returns an already-ready VM
//! instead of waiting for the full boot sequence.

pub mod scaler;
pub mod warm_pool;

pub use scaler::{PoolScaler, ScaleDecision};
pub use warm_pool::{PoolStats, WarmPool};
