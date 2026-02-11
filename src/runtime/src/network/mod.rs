//! Network management for container-to-container communication.
//!
//! Provides `NetworkStore` for persisting network state and
//! `NetworkManager` for orchestrating passt-based networking.

mod store;

pub use store::NetworkStore;
