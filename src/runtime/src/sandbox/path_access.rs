//! Host path access required while `crun` enters a user namespace.
//!
//! A root-run service deliberately maps container root to a subordinate host
//! identity.  `crun` changes into the OCI bundle before entering that user
//! namespace, then resolves `/proc/self/cwd` while setting up the container.
//! Every parent of the bundle and rootfs must therefore be searchable by the
//! mapped identity even when the service uses a restrictive umask.

use std::path::Path;

use a3s_box_core::error::{BoxError, Result};

#[cfg(target_os = "linux")]
use super::mapped_root_ids;
use super::SandboxIdMappingPlan;

/// Make only A3S-owned bundle/rootfs parents searchable by mapped container root.
///
/// The bundle artifacts and credentials keep their private modes.  Paths above
/// `A3S_HOME` are never modified; an inaccessible deployment parent is rejected
/// with an actionable error instead.
#[cfg(target_os = "linux")]
pub fn prepare_crun_path_access(
    home_dir: &Path,
    box_id: &str,
    bundle_dir: &Path,
    rootfs_path: &Path,
    id_mappings: &SandboxIdMappingPlan,
) -> Result<()> {
    let boxes_dir = home_dir.join("boxes");
    let box_dir = boxes_dir.join(box_id);
    let sandbox_dir = box_dir.join("sandbox");
    let expected_bundle = sandbox_dir.join("bundle");

    if bundle_dir != expected_bundle {
        return Err(invalid_runtime_path("bundle", bundle_dir, &expected_bundle));
    }
    if rootfs_path == box_dir || !rootfs_path.starts_with(&box_dir) {
        return Err(BoxError::BoxBootError {
            message: format!(
                "Sandbox rootfs {} is outside its managed box directory {}",
                rootfs_path.display(),
                box_dir.display()
            ),
            hint: Some("Rebuild the Sandbox rootfs inside its A3S box directory".to_string()),
        });
    }

    let (mapped_uid, mapped_gid) = mapped_root_ids(id_mappings)?;
    // `home_dir`, `boxes`, the per-box directory, and `sandbox` are all owned
    // by A3S.  Add only the single search bit selected by normal DAC rules.
    // Do not touch siblings such as `auth` or any bundle artifact mode.
    for path in [home_dir, &boxes_dir, &box_dir, &sandbox_dir] {
        make_managed_directory_searchable(path, mapped_uid, mapped_gid)?;
    }

    require_directory(bundle_dir, "Sandbox bundle")?;
    require_directory(rootfs_path, "Sandbox rootfs")?;
    require_searchable_parents(bundle_dir, mapped_uid, mapped_gid)?;
    require_searchable_path(rootfs_path, mapped_uid, mapped_gid)?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn prepare_crun_path_access(
    _home_dir: &Path,
    _box_id: &str,
    _bundle_dir: &Path,
    _rootfs_path: &Path,
    _id_mappings: &SandboxIdMappingPlan,
) -> Result<()> {
    Err(BoxError::ConfigError(
        "Sandbox runtime path preparation requires Linux".to_string(),
    ))
}

#[cfg(target_os = "linux")]
fn invalid_runtime_path(label: &str, actual: &Path, expected: &Path) -> BoxError {
    BoxError::BoxBootError {
        message: format!(
            "Sandbox {label} path {} does not match the managed path {}",
            actual.display(),
            expected.display()
        ),
        hint: Some("Remove the invalid Sandbox state and retry creation".to_string()),
    }
}

#[cfg(target_os = "linux")]
fn make_managed_directory_searchable(path: &Path, uid: u32, gid: u32) -> Result<()> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};

    let metadata = require_directory(path, "A3S-managed Sandbox directory")?;
    let search_bit = identity_search_bit(&metadata, uid, gid);
    let mode = metadata.mode() & 0o7777;
    if mode & search_bit == 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode | search_bit))
            .map_err(|error| BoxError::BoxBootError {
                message: format!(
                    "Failed to make A3S-managed Sandbox directory {} searchable: {error}",
                    path.display()
                ),
                hint: None,
            })?;
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn require_searchable_parents(path: &Path, uid: u32, gid: u32) -> Result<()> {
    let parent = path.parent().ok_or_else(|| BoxError::BoxBootError {
        message: format!("Sandbox runtime path has no parent: {}", path.display()),
        hint: None,
    })?;
    require_searchable_chain(parent, uid, gid)
}

#[cfg(target_os = "linux")]
fn require_searchable_path(path: &Path, uid: u32, gid: u32) -> Result<()> {
    require_searchable_chain(path, uid, gid)
}

#[cfg(target_os = "linux")]
fn require_searchable_chain(path: &Path, uid: u32, gid: u32) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    for ancestor in path.ancestors() {
        let metadata = std::fs::metadata(ancestor).map_err(|error| BoxError::BoxBootError {
            message: format!(
                "Failed to inspect Sandbox runtime path ancestor {}: {error}",
                ancestor.display()
            ),
            hint: None,
        })?;
        if !metadata.is_dir() {
            return Err(BoxError::BoxBootError {
                message: format!(
                    "Sandbox runtime path ancestor is not a directory: {}",
                    ancestor.display()
                ),
                hint: None,
            });
        }
        let search_bit = identity_search_bit(&metadata, uid, gid);
        if metadata.permissions().mode() & search_bit == 0 {
            return Err(BoxError::BoxBootError {
                message: format!(
                    "Sandbox runtime path ancestor {} is not searchable by mapped container root {uid}:{gid}",
                    ancestor.display()
                ),
                hint: Some(
                    "Grant execute-only traversal on the deployment parent or move A3S_HOME under a searchable service directory"
                        .to_string(),
                ),
            });
        }
    }
    Ok(())
}

#[cfg(target_os = "linux")]
fn identity_search_bit(metadata: &std::fs::Metadata, uid: u32, gid: u32) -> u32 {
    use std::os::unix::fs::MetadataExt;

    if metadata.uid() == uid {
        0o100
    } else if metadata.gid() == gid {
        0o010
    } else {
        0o001
    }
}

#[cfg(target_os = "linux")]
fn require_directory(path: &Path, label: &str) -> Result<std::fs::Metadata> {
    let metadata = std::fs::symlink_metadata(path).map_err(|error| BoxError::BoxBootError {
        message: format!("Failed to inspect {label} {}: {error}", path.display()),
        hint: None,
    })?;
    if metadata.file_type().is_symlink() || !metadata.is_dir() {
        return Err(BoxError::BoxBootError {
            message: format!("{label} is not a real directory: {}", path.display()),
            hint: Some("Remove the invalid Sandbox state and retry creation".to_string()),
        });
    }
    Ok(metadata)
}

#[cfg(all(test, target_os = "linux"))]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    use super::*;
    use crate::sandbox::IdMapping;

    fn mappings(uid: u32, gid: u32) -> SandboxIdMappingPlan {
        SandboxIdMappingPlan {
            uid_mappings: vec![IdMapping {
                container_id: 0,
                host_id: uid,
                size: 1,
            }],
            gid_mappings: vec![IdMapping {
                container_id: 0,
                host_id: gid,
                size: 1,
            }],
            maximum_container_uid: 0,
            maximum_container_gid: 0,
        }
    }

    fn set_mode(path: &Path, mode: u32) {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode)).unwrap();
    }

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o7777
    }

    fn fixture(outer_mode: u32) -> (tempfile::TempDir, PathBuf, PathBuf, PathBuf) {
        let outer = tempfile::tempdir().unwrap();
        set_mode(outer.path(), outer_mode);
        let home = outer.path().join("home");
        let box_dir = home.join("boxes/execution-1");
        let bundle = box_dir.join("sandbox/bundle");
        let rootfs = box_dir.join("merged");
        std::fs::create_dir_all(&bundle).unwrap();
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(home.join("auth")).unwrap();
        std::fs::write(home.join("auth/credentials.json"), b"private").unwrap();
        for path in [
            &home,
            &home.join("boxes"),
            &box_dir,
            &box_dir.join("sandbox"),
            &bundle,
            &rootfs,
            &home.join("auth"),
        ] {
            set_mode(path, 0o700);
        }
        set_mode(&home.join("auth/credentials.json"), 0o600);
        (outer, home, bundle, rootfs)
    }

    #[test]
    fn restrictive_umask_paths_become_searchable_without_exposing_credentials() {
        let (outer, home, bundle, rootfs) = fixture(0o701);
        let uid = unsafe { libc::geteuid() }.saturating_add(100_000);
        let gid = unsafe { libc::getegid() }.saturating_add(200_000);
        // The real rootfs is owned by mapped root.  An execute-only bit models
        // that access without requiring this unit test to run as host root.
        set_mode(&rootfs, 0o701);

        prepare_crun_path_access(&home, "execution-1", &bundle, &rootfs, &mappings(uid, gid))
            .unwrap();

        for path in [
            &home,
            &home.join("boxes"),
            &home.join("boxes/execution-1"),
            &home.join("boxes/execution-1/sandbox"),
        ] {
            assert_eq!(mode(path), 0o701);
        }
        assert_eq!(mode(&bundle), 0o700);
        assert_eq!(mode(&home.join("auth")), 0o700);
        assert_eq!(mode(&home.join("auth/credentials.json")), 0o600);
        assert_eq!(mode(outer.path()), 0o701);
    }

    #[test]
    fn inaccessible_deployment_parent_is_rejected_without_chmod() {
        let (outer, home, bundle, rootfs) = fixture(0o700);
        let uid = unsafe { libc::geteuid() }.saturating_add(100_000);
        let gid = unsafe { libc::getegid() }.saturating_add(200_000);
        set_mode(&rootfs, 0o701);

        let error =
            prepare_crun_path_access(&home, "execution-1", &bundle, &rootfs, &mappings(uid, gid))
                .unwrap_err();

        assert!(error.to_string().contains("not searchable"));
        assert_eq!(mode(outer.path()), 0o700);
    }

    #[test]
    fn mismatched_bundle_path_is_rejected_before_permissions_change() {
        let (outer, home, _bundle, rootfs) = fixture(0o701);
        let wrong = home.join("other/bundle");
        std::fs::create_dir_all(&wrong).unwrap();

        let error = prepare_crun_path_access(
            &home,
            "execution-1",
            &wrong,
            &rootfs,
            &mappings(100_000, 200_000),
        )
        .unwrap_err();

        assert!(error.to_string().contains("does not match"));
        assert_eq!(mode(&home), 0o700);
        assert_eq!(mode(outer.path()), 0o701);
    }
}
