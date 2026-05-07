//! A3S Box Docker Engine API
//!
//! This crate provides a Docker Engine API compatible HTTP server that allows
//! Docker tools and SDKs to work seamlessly with a3s-box.

pub mod server;
pub mod handlers;
pub mod models;
pub mod error;

pub use server::ApiServer;
pub use error::{ApiError, ApiResult};
