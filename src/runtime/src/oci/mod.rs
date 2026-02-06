//! OCI image support for A3S Box.
//!
//! This module provides functionality to parse and extract OCI images
//! for use as VM rootfs. It supports:
//!
//! - OCI image layout parsing (manifest, config)
//! - Layer extraction (tar.gz)
//! - Rootfs composition from multiple images
//!
//! # Architecture
//!
//! ```text
//! ┌─────────────────────────────────────────────────────────────┐
//! │                    OCI Image Layout                          │
//! │                                                              │
//! │  image/                                                      │
//! │  ├── oci-layout           (OCI layout marker)               │
//! │  ├── index.json           (Image index)                     │
//! │  └── blobs/                                                 │
//! │      └── sha256/                                            │
//! │          ├── <manifest>   (Image manifest)                  │
//! │          ├── <config>     (Image configuration)             │
//! │          └── <layers>     (Filesystem layers)               │
//! └─────────────────────────────────────────────────────────────┘
//! ```

mod image;
mod labels;
mod layers;
mod pull;
pub mod reference;
pub mod registry;
mod rootfs;
pub mod store;

pub use image::{OciImage, OciImageConfig};
pub use labels::AgentLabels;
pub use layers::extract_layer;
pub use pull::ImagePuller;
pub use reference::ImageReference;
pub use registry::{RegistryAuth, RegistryPuller};
pub use rootfs::{OciRootfsBuilder, RootfsComposition};
pub use store::{ImageStore, StoredImage};
