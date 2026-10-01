//! Volume management for persistent named volumes.
//!
//! Provides `VolumeStore` for persisting volume state and
//! managing volume data directories.

mod store;

#[cfg(windows)]
pub(crate) use store::managed_path_can_be_a_volume_directory;
pub(crate) use store::managed_volume_ancestor_is_link;
pub use store::VolumeStore;
