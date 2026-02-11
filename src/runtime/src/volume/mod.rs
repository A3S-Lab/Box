//! Volume management for persistent named volumes.
//!
//! Provides `VolumeStore` for persisting volume state and
//! managing volume data directories.

mod store;

pub use store::VolumeStore;
