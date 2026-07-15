//! Shared-kernel Sandbox backend support.
//!
//! The public isolation selector stays backend-neutral. This module owns the
//! Linux host evidence and OCI artifacts required by the certified `crun`
//! backend; VM-specific code must not depend on these types.

pub mod capability;
pub mod controller;
pub mod handler;
pub mod oci;
pub mod path_access;
pub mod rootfs;

pub use capability::{
    plan_id_mappings, probe_sandbox_capabilities, CertifiedCrun, IdMapping,
    SandboxCapabilitySnapshot, SandboxIdMappingPlan, CERTIFIED_CRUN_VERSION,
};
pub use controller::{write_bundle, CrunController, SandboxLaunchSpec};
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
