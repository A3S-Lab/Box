//! Guest init library for a3s-box VM.
//!
//! Provides namespace isolation utilities for running agent and business code
//! in isolated environments within the same VM.

pub mod namespace;

pub use namespace::{spawn_isolated, NamespaceConfig, NamespaceError};
