//! Guest rootfs management module.
//!
//! This module handles preparation and management of guest rootfs for MicroVM instances.
//! The rootfs contains the minimal filesystem required to boot the guest agent.

mod builder;
mod layout;

pub use builder::{find_agent_binary, RootfsBuilder};
pub use layout::{GuestLayout, GUEST_AGENT_PATH, GUEST_SKILLS_DIR, GUEST_WORKDIR};
