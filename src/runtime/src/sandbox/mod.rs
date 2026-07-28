//! Shared-kernel Sandbox backend support.
//!
//! The public isolation selector stays backend-neutral. This module owns the
//! Linux host evidence and OCI artifacts required by the A3S OCI backend;
//! VM-specific code must not depend on these types.

#[cfg(target_os = "linux")]
pub(crate) mod a3s_oci_client;
#[cfg(target_os = "linux")]
pub(crate) mod a3s_oci_controller;
#[cfg(target_os = "linux")]
pub(crate) mod a3s_oci_handler;
#[cfg(target_os = "linux")]
pub(crate) mod a3s_oci_owner;
pub mod capability;
pub mod controller;
pub mod oci;
pub mod path_access;
pub mod rootfs;
#[cfg(target_os = "linux")]
pub(crate) mod runtime_record;

/// Lifecycle envelope certified by the pinned A3S OCI native service.
///
/// The native service allows its Linux executor up to ten seconds to reap a
/// process and uses a fifteen-second shutdown envelope in its own lifecycle
/// certification. Box must give container wait and owner shutdown that same
/// bounded opportunity before declaring provider loss.
#[cfg(target_os = "linux")]
pub(crate) const A3S_OCI_LIFECYCLE_TIMEOUT_MS: u64 = 15_000;

/// Apply a complete resource contract to the exact recorded Sandbox generation.
///
/// The A3S OCI SDK is the only live-update path for a host Sandbox. MicroVM
/// cgroup updates remain guest-local and never pass through this function.
#[cfg(target_os = "linux")]
pub fn update_recorded_resources(
    box_dir: &std::path::Path,
    box_id: &str,
    config: &a3s_box_core::config::BoxConfig,
) -> a3s_box_core::Result<()> {
    if !config.isolation.is_sandbox() {
        return Err(a3s_box_core::BoxError::ConfigError(
            "A3S OCI resource updates require Sandbox isolation".to_string(),
        ));
    }
    let runtime = crate::vm::reap::load_recorded_sandbox_runtime(
        &a3s_box_core::dirs_home(),
        box_dir,
        box_id,
    )?
    .ok_or_else(|| {
        a3s_box_core::BoxError::StateError(format!(
            "Recorded A3S OCI runtime is missing for Sandbox {box_id}"
        ))
    })?;
    let socket = runtime.runtime_socket.as_deref().ok_or_else(|| {
        a3s_box_core::BoxError::StateError(format!(
            "Recorded A3S OCI runtime socket is missing for Sandbox {box_id}"
        ))
    })?;
    let generation = runtime.generation.ok_or_else(|| {
        a3s_box_core::BoxError::StateError(format!(
            "Recorded A3S OCI generation is missing for Sandbox {box_id}"
        ))
    })?;
    let resources = oci::compile_resources(&SandboxResources::from_box_config(config)?)?;
    A3sOciHandler::update_at(socket, box_id, generation, resources)
}

#[cfg(not(target_os = "linux"))]
pub fn update_recorded_resources(
    _box_dir: &std::path::Path,
    _box_id: &str,
    _config: &a3s_box_core::config::BoxConfig,
) -> a3s_box_core::Result<()> {
    Err(a3s_box_core::BoxError::StateError(
        "A3S OCI Sandbox resource updates require Linux".to_string(),
    ))
}

#[cfg(target_os = "linux")]
pub use a3s_oci_controller::A3sOciController;
#[cfg(target_os = "linux")]
pub use a3s_oci_handler::A3sOciHandler;
pub use capability::{
    map_container_gid, map_container_uid, plan_id_mappings, probe_sandbox_capabilities,
    probe_sandbox_capabilities_for, unmap_host_gid, unmap_host_uid, CertifiedA3sOci, IdMapping,
    SandboxCapabilitySnapshot, SandboxIdMappingPlan, UserNamespaceEvidence,
};
pub use controller::{write_bundle, SandboxLaunchSpec};
#[cfg(not(target_os = "linux"))]
pub struct A3sOciController;
#[cfg(not(target_os = "linux"))]
impl A3sOciController {
    pub fn new(_runtime: CertifiedA3sOci) -> Self {
        Self
    }

    pub fn require_absent(
        &self,
        _runtime_root: &std::path::Path,
        _container_id: &str,
    ) -> a3s_box_core::Result<()> {
        Err(a3s_box_core::BoxError::BoxBootError {
            message: "A3S OCI Sandbox execution requires Linux".to_string(),
            hint: None,
        })
    }

    pub async fn start(&self, _launch: SandboxLaunchSpec) -> a3s_box_core::Result<A3sOciHandler> {
        Err(a3s_box_core::BoxError::BoxBootError {
            message: "A3S OCI Sandbox execution requires Linux".to_string(),
            hint: None,
        })
    }
}

#[cfg(not(target_os = "linux"))]
pub struct A3sOciHandler;
#[cfg(not(target_os = "linux"))]
impl a3s_box_core::vmm::VmHandler for A3sOciHandler {
    fn stop(&mut self, _signal: i32, _timeout_ms: u64) -> a3s_box_core::Result<()> {
        Err(a3s_box_core::BoxError::StateError(
            "A3S OCI Sandbox execution requires Linux".to_string(),
        ))
    }

    fn metrics(&self) -> a3s_box_core::vmm::VmMetrics {
        a3s_box_core::vmm::VmMetrics::default()
    }

    fn is_running(&self) -> bool {
        false
    }

    fn pid(&self) -> u32 {
        0
    }
}
pub use oci::{
    compile_oci_spec, SandboxBundleSpec, SandboxMount, SandboxResources, SandboxTmpfs,
    DEFAULT_SANDBOX_PIDS_LIMIT,
};
pub use path_access::prepare_sandbox_path_access;
pub use rootfs::{
    inspect_rootfs_identity_requirements, mapped_root_ids, prepare_managed_mount_source,
    prepare_rootfs_ownership, validate_external_mount_access, RootfsIdentityRequirements,
};
