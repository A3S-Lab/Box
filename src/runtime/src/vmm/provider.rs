//! VmmProvider - Trait for VMM backend implementations.

use a3s_box_core::error::Result;
use async_trait::async_trait;

use super::handler::VmHandler;
use super::spec::InstanceSpec;

/// Trait for VMM backend implementations.
#[async_trait]
pub trait VmmProvider: Send + Sync {
    /// Start a VM with the given configuration.
    async fn start(&self, spec: &InstanceSpec) -> Result<Box<dyn VmHandler>>;
}
