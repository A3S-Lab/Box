//! State management for box instances.
//!
//! Persists box metadata to `~/.a3s/boxes.json` with atomic writes.
//! On every load, dead active PIDs are reconciled to mark boxes as dead.

mod file;
mod lock;
pub(crate) mod policy;
#[cfg(test)]
mod tests;

pub use a3s_box_runtime::{BoxRecord, HealthCheck};
pub use file::StateFile;
pub use policy::{generate_name, parse_restart_policy, validate_restart_policy};
