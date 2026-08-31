//! Mount-free OCI layer assembly for guest-native ext4 root filesystems.

use std::path::Path;

use a3s_box_core::error::{BoxError, Result};

use crate::oci::OciImage;

use super::ext4::{publish_ext4_build_plan, Ext4Artifact, Ext4ArtifactOptions};

mod layer;
mod plan;
mod runtime;
mod spool;
mod tree;

use tree::LogicalRootfs;

pub(super) const CONTENT_STAGING_PREFIX: &str = ".a3s-oci-ext4-content-";

pub(super) fn publish_oci_layers_ext4(
    image: &OciImage,
    guest_init: &Path,
    guest_init_sha256: &str,
    destination: &Path,
    options: Ext4ArtifactOptions,
) -> Result<Ext4Artifact> {
    let parent = destination.parent().ok_or_else(|| {
        BoxError::BuildError(format!(
            "OCI ext4 destination has no parent: {}",
            destination.display()
        ))
    })?;
    let mut rootfs = LogicalRootfs::new(parent)?;
    rootfs.create_base_structure()?;
    for layer in image.layer_blobs() {
        layer::apply_layer(layer.path, layer.digest, layer.size, &mut rootfs)?;
    }
    rootfs.install_guest_init(guest_init, guest_init_sha256)?;
    rootfs.create_essential_files()?;
    rootfs.validate_boot_contract()?;
    let (builder, fills) = rootfs.declare(options)?;
    publish_ext4_build_plan(destination, options, builder, fills)
}

#[cfg(test)]
mod tests;
