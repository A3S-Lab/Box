//! Guest rootfs management module.
//!
//! This module handles preparation and management of guest rootfs for MicroVM instances.
//! The rootfs contains the minimal filesystem required to boot the guest agent.
//!
//! Two rootfs providers are available:
//! - `CopyProvider` — full recursive copy (works everywhere)
//! - `OverlayProvider` — Linux overlayfs mount (near-instant CoW)

mod builder;
mod layout;
pub(crate) mod overlay;
mod provider;

pub use builder::RootfsBuilder;
pub use layout::{GuestLayout, GUEST_WORKDIR};
pub use provider::{default_provider, CopyProvider, OverlayProvider, RootfsProvider};
