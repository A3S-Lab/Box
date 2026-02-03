//! Krun module - libkrun integration for MicroVM management.
//!
//! This module provides a safe wrapper around libkrun FFI bindings
//! for creating and managing MicroVMs.

mod context;

pub use context::KrunContext;

use a3s_box_core::error::{BoxError, Result};

/// Check libkrun FFI call status and convert to Result.
pub fn check_status(fn_name: &str, status: i32) -> Result<()> {
    if status < 0 {
        tracing::error!(status, fn_name, "libkrun call failed");
        Err(BoxError::BoxBootError {
            message: format!("{} failed with status {}", fn_name, status),
            hint: Some("Check libkrun installation and VM configuration".to_string()),
        })
    } else {
        Ok(())
    }
}
