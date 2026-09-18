//! Stable, bounded host paths used by VM and Sandbox runtimes.

use std::path::{Path, PathBuf};

pub(crate) use crate::host_sockets::{ensure_runtime_socket_dir, runtime_socket_dir};

/// Short host path for one Sandbox runtime owner and its private control socket.
///
/// Unix-domain socket paths have a small fixed kernel limit. Keeping the A3S
/// OCI root below the already-short external socket directory makes Sandbox
/// startup independent of the configured A3S home path length.
pub(crate) fn sandbox_runtime_root(home_dir: &Path, box_id: &str) -> PathBuf {
    runtime_socket_dir(home_dir, box_id).join("oci")
}

/// Runtime root used before Sandbox owners moved beside the short socket paths.
pub(crate) fn legacy_sandbox_runtime_root(home_dir: &Path, box_id: &str) -> PathBuf {
    home_dir.join("run").join("a3s-oci").join(box_id)
}
