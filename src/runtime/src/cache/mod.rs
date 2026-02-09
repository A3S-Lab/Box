//! Cache module for cold start optimization.
//!
//! Provides two caching layers:
//! - `LayerCache`: Content-addressed cache for extracted OCI layers
//! - `RootfsCache`: Cache for fully-built rootfs directories

pub mod layer_cache;
pub mod rootfs_cache;

pub use layer_cache::LayerCache;
pub use rootfs_cache::RootfsCache;
