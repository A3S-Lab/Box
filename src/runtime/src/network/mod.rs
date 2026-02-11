//! Network management for container-to-container communication.
//!
//! Provides `NetworkStore` for persisting network state and
//! `PasstManager` for orchestrating passt-based networking.

mod passt;
mod store;

pub use passt::PasstManager;
pub use store::NetworkStore;
