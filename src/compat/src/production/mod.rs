//! ACL-configured production composition for the E2B compatibility service.

mod config;
mod identity;
mod service;
mod template;

pub use config::{E2bCompatConfig, E2bConfigError, E2bConfigResult, SupervisorConfig};
pub use identity::UuidSandboxIdentityProvider;
pub use service::{E2bCompatService, E2bServiceError, E2bServiceResult};
pub use template::StaticTemplateProvider;

#[cfg(test)]
mod tests;
