//! VMM module - Virtual Machine Manager subsystem.
//!
//! This module provides the infrastructure for managing MicroVM instances:
//! - `InstanceSpec`: Complete VM configuration
//! - `VmController`: Spawns VM subprocesses
//! - `VmHandler`: Runtime operations on running VMs

mod controller;
mod handler;
mod spec;

pub use controller::VmController;
pub use handler::{VmHandler, VmMetrics};
pub use spec::{Entrypoint, FsMount, InstanceSpec, TeeInstanceConfig};
