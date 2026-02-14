//! VMM module - Virtual Machine Manager subsystem.
//!
//! This module provides the infrastructure for managing MicroVM instances:
//! - `InstanceSpec`: Complete VM configuration
//! - `VmController`: Spawns VM subprocesses
//! - `VmHandler`: Runtime operations on running VMs

mod controller;
mod handler;
mod provider;
mod spec;

pub use controller::VmController;
pub use handler::{ShimHandler, VmHandler, VmMetrics, DEFAULT_SHUTDOWN_TIMEOUT_MS};
pub use provider::VmmProvider;
pub use spec::{Entrypoint, FsMount, InstanceSpec, NetworkInstanceConfig, TeeInstanceConfig};
