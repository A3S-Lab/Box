//! Packaged Windows/WHPX OCI Host artifact discovery.
//!
//! Production cutover gate 1 ([docs/microvm-whpx-ga-evidence.md](../../../docs/microvm-whpx-ga-evidence.md)):
//! resolve `a3s-oci.exe`, `a3s-oci-krun-shim.exe`, bootstrap `vm-rootfs`, and
//! `system-image.json` from packaged A3S locations without requiring
//! `A3S_BOX_OCI_WHPX_ENDPOINT`. Explicit path / service env overrides still
//! win. `PATH` is ignored (same policy as Sandbox / KVM packaged discovery).
//!
//! Layout contract: OCI `packaging/windows/README.md` / Box #650.

use std::path::{Path, PathBuf};

use a3s_box_core::{ExecutionManagerError, ExecutionManagerResult};

/// Keep in sync with `oci_migration::OCI_WHPX_*` / `OCI_WHPX_ENDPOINT_ENV`.
const OCI_WHPX_ENDPOINT_ENV: &str = "A3S_BOX_OCI_WHPX_ENDPOINT";
const OCI_WHPX_SERVICE_BIN_ENV: &str = "A3S_BOX_WHPX_OCI_SERVICE_BIN";
const OCI_WHPX_SERVICE_SHIM_ENV: &str = "A3S_BOX_WHPX_OCI_SERVICE_SHIM";
const OCI_WHPX_SERVICE_VM_ROOTFS_ENV: &str = "A3S_BOX_WHPX_OCI_SERVICE_VM_ROOTFS";
const OCI_WHPX_SERVICE_MANIFEST_ENV: &str = "A3S_BOX_WHPX_OCI_SERVICE_MANIFEST";

#[cfg(windows)]
const RUNTIME_FILENAME: &str = "a3s-oci.exe";
#[cfg(windows)]
const SHIM_FILENAME: &str = "a3s-oci-krun-shim.exe";
#[cfg(not(windows))]
const RUNTIME_FILENAME: &str = "a3s-oci.exe";
#[cfg(not(windows))]
const SHIM_FILENAME: &str = "a3s-oci-krun-shim.exe";

const MANIFEST_FILENAME: &str = "system-image.json";
const BOOTSTRAP_DIRNAME: &str = "bootstrap-vm-rootfs";

/// Absolute packaged paths for a Box-owned Windows/WHPX OCI Host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackagedWindowsWhpxArtifacts {
    pub runtime_path: PathBuf,
    pub shim_path: PathBuf,
    pub vm_rootfs: PathBuf,
    pub system_image_manifest: PathBuf,
}

/// Optional absolute overrides (env or explicit) before packaged search.
#[derive(Debug, Clone, Default)]
pub(crate) struct PackagedWindowsWhpxOverrides {
    pub runtime_path: Option<PathBuf>,
    pub shim_path: Option<PathBuf>,
    pub vm_rootfs: Option<PathBuf>,
    pub system_image_manifest: Option<PathBuf>,
}

/// Discover packaged WHPX Host artifacts, optionally preferring overrides.
pub(crate) fn discover_packaged_windows_whpx_artifacts(
    overrides: PackagedWindowsWhpxOverrides,
) -> ExecutionManagerResult<PackagedWindowsWhpxArtifacts> {
    discover_packaged_windows_whpx_artifacts_in(&default_search_roots(), overrides)
}

/// Same as [`discover_packaged_windows_whpx_artifacts`] with explicit search
/// roots (tests plant fixtures without relying on `current_exe`).
pub(crate) fn discover_packaged_windows_whpx_artifacts_in(
    search_roots: &[PathBuf],
    overrides: PackagedWindowsWhpxOverrides,
) -> ExecutionManagerResult<PackagedWindowsWhpxArtifacts> {
    let runtime_path = resolve_file(
        overrides.runtime_path.as_deref(),
        OCI_WHPX_SERVICE_BIN_ENV,
        RUNTIME_FILENAME,
        "A3S OCI Runtime (WHPX Host)",
        search_roots,
    )?;
    let shim_path = resolve_file(
        overrides.shim_path.as_deref(),
        OCI_WHPX_SERVICE_SHIM_ENV,
        SHIM_FILENAME,
        "A3S OCI WHPX shim",
        search_roots,
    )?;
    let system_image_manifest =
        resolve_manifest(overrides.system_image_manifest.as_deref(), search_roots)?;
    let vm_rootfs = resolve_bootstrap_dir(overrides.vm_rootfs.as_deref(), search_roots)?;
    Ok(PackagedWindowsWhpxArtifacts {
        runtime_path,
        shim_path,
        vm_rootfs,
        system_image_manifest,
    })
}

fn default_search_roots() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(executable) = std::env::current_exe() {
        if let Some(directory) = executable.parent() {
            roots.push(directory.to_path_buf());
            if directory.file_name().is_some_and(|name| name == "deps") {
                if let Some(target_directory) = directory.parent() {
                    roots.push(target_directory.to_path_buf());
                }
            }
        }
    }
    roots.push(a3s_box_core::dirs_home().join("bin"));
    roots.push(a3s_box_core::dirs_home().join("share").join("a3s"));
    roots
}

fn resolve_manifest(
    explicit: Option<&Path>,
    search_roots: &[PathBuf],
) -> ExecutionManagerResult<PathBuf> {
    if let Some(path) = explicit {
        return finalize_file(path, OCI_WHPX_SERVICE_MANIFEST_ENV, "WHPX system-image manifest");
    }
    for root in search_roots {
        for candidate in [
            root.join(MANIFEST_FILENAME),
            root.join("system-image").join(MANIFEST_FILENAME),
        ] {
            if candidate.is_file() {
                return finalize_file(
                    &candidate,
                    OCI_WHPX_SERVICE_MANIFEST_ENV,
                    "WHPX system-image manifest",
                );
            }
        }
    }
    Err(missing_packaged(
        "WHPX system-image manifest",
        MANIFEST_FILENAME,
        OCI_WHPX_SERVICE_MANIFEST_ENV,
    ))
}

fn resolve_bootstrap_dir(
    explicit: Option<&Path>,
    search_roots: &[PathBuf],
) -> ExecutionManagerResult<PathBuf> {
    if let Some(path) = explicit {
        return finalize_dir(path, OCI_WHPX_SERVICE_VM_ROOTFS_ENV, "WHPX bootstrap vm-rootfs");
    }
    for root in search_roots {
        let candidate = root.join(BOOTSTRAP_DIRNAME);
        if candidate.is_dir() {
            return finalize_dir(
                &candidate,
                OCI_WHPX_SERVICE_VM_ROOTFS_ENV,
                "WHPX bootstrap vm-rootfs",
            );
        }
    }
    Err(missing_packaged(
        "WHPX bootstrap vm-rootfs",
        BOOTSTRAP_DIRNAME,
        OCI_WHPX_SERVICE_VM_ROOTFS_ENV,
    ))
}

fn resolve_file(
    explicit: Option<&Path>,
    environment: &str,
    filename: &str,
    label: &str,
    search_roots: &[PathBuf],
) -> ExecutionManagerResult<PathBuf> {
    if let Some(path) = explicit {
        return finalize_file(path, environment, label);
    }
    for root in search_roots {
        let candidate = root.join(filename);
        if candidate.is_file() {
            return finalize_file(&candidate, environment, label);
        }
    }
    Err(missing_packaged(label, filename, environment))
}

fn finalize_file(
    selected: &Path,
    environment: &str,
    label: &str,
) -> ExecutionManagerResult<PathBuf> {
    let canonical = selected.canonicalize().map_err(|error| {
        ExecutionManagerError::InvalidRequest(format!(
            "Failed to resolve {label} {} (set {environment} or install the packaged artifact): {error}",
            selected.display()
        ))
    })?;
    let metadata = canonical.metadata().map_err(|error| {
        ExecutionManagerError::InvalidRequest(format!(
            "Failed to inspect {label} {}: {error}",
            canonical.display()
        ))
    })?;
    if !metadata.is_file() {
        return Err(ExecutionManagerError::InvalidRequest(format!(
            "{label} is not a regular file: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn finalize_dir(
    selected: &Path,
    environment: &str,
    label: &str,
) -> ExecutionManagerResult<PathBuf> {
    let canonical = selected.canonicalize().map_err(|error| {
        ExecutionManagerError::InvalidRequest(format!(
            "Failed to resolve {label} {} (set {environment} or install the packaged artifact): {error}",
            selected.display()
        ))
    })?;
    let metadata = canonical.metadata().map_err(|error| {
        ExecutionManagerError::InvalidRequest(format!(
            "Failed to inspect {label} {}: {error}",
            canonical.display()
        ))
    })?;
    if !metadata.is_dir() {
        return Err(ExecutionManagerError::InvalidRequest(format!(
            "{label} is not a directory: {}",
            canonical.display()
        )));
    }
    Ok(canonical)
}

fn missing_packaged(label: &str, filename: &str, environment: &str) -> ExecutionManagerError {
    ExecutionManagerError::InvalidRequest(format!(
        "{label} `{filename}` was not found in packaged A3S locations; install the A3S Box/OCI WHPX Host package, set {environment}, or set {OCI_WHPX_ENDPOINT_ENV} for an external qualification Host"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn plant_file(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"artifact\n").unwrap();
    }

    #[test]
    fn discovers_runtime_shim_manifest_and_bootstrap_from_search_root() {
        let root =
            std::env::temp_dir().join(format!("a3s-whpx-packaged-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        plant_file(&root.join(RUNTIME_FILENAME));
        plant_file(&root.join(SHIM_FILENAME));
        plant_file(&root.join(MANIFEST_FILENAME));
        fs::create_dir_all(root.join(BOOTSTRAP_DIRNAME)).unwrap();

        let found = discover_packaged_windows_whpx_artifacts_in(
            std::slice::from_ref(&root),
            PackagedWindowsWhpxOverrides::default(),
        )
        .unwrap();
        assert_eq!(
            found.runtime_path.file_name().and_then(|n| n.to_str()),
            Some(RUNTIME_FILENAME)
        );
        assert_eq!(
            found.shim_path.file_name().and_then(|n| n.to_str()),
            Some(SHIM_FILENAME)
        );
        assert_eq!(
            found
                .system_image_manifest
                .file_name()
                .and_then(|n| n.to_str()),
            Some(MANIFEST_FILENAME)
        );
        assert_eq!(
            found.vm_rootfs.file_name().and_then(|n| n.to_str()),
            Some(BOOTSTRAP_DIRNAME)
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discovers_manifest_under_system_image_subdirectory() {
        let root = std::env::temp_dir().join(format!(
            "a3s-whpx-packaged-subdir-{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("system-image")).unwrap();
        plant_file(&root.join(RUNTIME_FILENAME));
        plant_file(&root.join(SHIM_FILENAME));
        plant_file(&root.join("system-image").join(MANIFEST_FILENAME));
        fs::create_dir_all(root.join(BOOTSTRAP_DIRNAME)).unwrap();

        let found = discover_packaged_windows_whpx_artifacts_in(
            std::slice::from_ref(&root),
            PackagedWindowsWhpxOverrides::default(),
        )
        .unwrap();
        assert!(found
            .system_image_manifest
            .to_string_lossy()
            .contains("system-image"));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn missing_packaged_artifacts_fail_closed() {
        let root =
            std::env::temp_dir().join(format!("a3s-whpx-packaged-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        let error = discover_packaged_windows_whpx_artifacts_in(
            std::slice::from_ref(&root),
            PackagedWindowsWhpxOverrides::default(),
        )
        .expect_err("missing package must fail");
        let message = error.to_string();
        assert!(
            message.contains("was not found in packaged A3S locations"),
            "{message}"
        );
        let _ = fs::remove_dir_all(&root);
    }
}
