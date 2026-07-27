//! Shared-kernel Sandbox backend support.
//!
//! The public isolation selector stays backend-neutral. This module owns the
//! Linux host evidence and OCI artifacts required by the A3S OCI backend and
//! its certified `crun` rollback path; VM-specific code must not depend on
//! these types.

#[cfg(target_os = "linux")]
pub(crate) mod a3s_oci_client;
#[cfg(target_os = "linux")]
pub(crate) mod a3s_oci_controller;
#[cfg(target_os = "linux")]
pub(crate) mod a3s_oci_handler;
pub mod capability;
pub mod controller;
pub mod handler;
pub mod oci;
pub mod path_access;
pub mod rootfs;
#[cfg(target_os = "linux")]
pub(crate) mod runtime_record;

#[cfg(target_os = "linux")]
pub use a3s_oci_controller::A3sOciController;
#[cfg(target_os = "linux")]
pub use a3s_oci_handler::A3sOciHandler;
pub use capability::{
    map_container_gid, map_container_uid, plan_id_mappings, probe_sandbox_capabilities,
    probe_sandbox_capabilities_for, unmap_host_gid, unmap_host_uid, CertifiedA3sOci, CertifiedCrun,
    IdMapping, SandboxCapabilitySnapshot, SandboxIdMappingPlan, UserNamespaceEvidence,
    CERTIFIED_CRUN_VERSION,
};
pub use controller::{write_bundle, CrunController, SandboxLaunchSpec};
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

    pub async fn start(&self, _launch: SandboxLaunchSpec) -> a3s_box_core::Result<CrunHandler> {
        Err(a3s_box_core::BoxError::BoxBootError {
            message: "A3S OCI Sandbox execution requires Linux".to_string(),
            hint: None,
        })
    }
}
pub use handler::CrunHandler;
pub use oci::{
    compile_oci_spec, SandboxBundleSpec, SandboxMount, SandboxResources, SandboxTmpfs,
    DEFAULT_SANDBOX_PIDS_LIMIT,
};
pub use path_access::prepare_crun_path_access;
pub use rootfs::{
    inspect_rootfs_identity_requirements, mapped_root_ids, prepare_managed_mount_source,
    prepare_rootfs_ownership, validate_external_mount_access, RootfsIdentityRequirements,
};
