//! Packaged Linux/KVM OCI Host artifact discovery.
//!
//! Production cutover gate 1 ([docs/microvm-kvm-ga-evidence.md](../../../docs/microvm-kvm-ga-evidence.md)):
//! resolve `a3s-oci`, `a3s-oci-krun-shim`, and `system-image.json` from packaged
//! A3S locations without requiring `A3S_BOX_OCI_KVM_ENDPOINT`. Explicit path /
//! service env overrides still win. `PATH` is ignored (same policy as Sandbox).

use std::path::{Path, PathBuf};

use a3s_box_core::{ExecutionManagerError, ExecutionManagerResult};

/// Keep in sync with `oci_migration::OCI_KVM_*` / `OCI_KVM_ENDPOINT_ENV`.
const OCI_KVM_ENDPOINT_ENV: &str = "A3S_BOX_OCI_KVM_ENDPOINT";
const OCI_KVM_SERVICE_BIN_ENV: &str = "A3S_BOX_KVM_OCI_SERVICE_BIN";
const OCI_KVM_SERVICE_SHIM_ENV: &str = "A3S_BOX_KVM_OCI_SERVICE_SHIM";
const OCI_KVM_SERVICE_MANIFEST_ENV: &str = "A3S_BOX_KVM_OCI_SERVICE_MANIFEST";

const RUNTIME_FILENAME: &str = "a3s-oci";
const SHIM_FILENAME: &str = "a3s-oci-krun-shim";
const MANIFEST_FILENAME: &str = "system-image.json";

/// Absolute packaged paths for a Box-owned Linux/KVM OCI Host.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct PackagedLinuxKvmArtifacts {
    pub runtime_path: PathBuf,
    pub shim_path: PathBuf,
    pub system_image_manifest: PathBuf,
}

/// Optional absolute overrides (env or explicit) before packaged search.
#[derive(Debug, Clone, Default)]
pub(crate) struct PackagedLinuxKvmOverrides {
    pub runtime_path: Option<PathBuf>,
    pub shim_path: Option<PathBuf>,
    pub system_image_manifest: Option<PathBuf>,
}

/// Discover packaged KVM Host artifacts, optionally preferring overrides.
pub(crate) fn discover_packaged_linux_kvm_artifacts(
    overrides: PackagedLinuxKvmOverrides,
) -> ExecutionManagerResult<PackagedLinuxKvmArtifacts> {
    discover_packaged_linux_kvm_artifacts_in(&default_search_roots(), overrides)
}

/// Same as [`discover_packaged_linux_kvm_artifacts`] with explicit search roots
/// (tests plant fixtures without relying on `current_exe`).
pub(crate) fn discover_packaged_linux_kvm_artifacts_in(
    search_roots: &[PathBuf],
    overrides: PackagedLinuxKvmOverrides,
) -> ExecutionManagerResult<PackagedLinuxKvmArtifacts> {
    let runtime_path = resolve_artifact(
        overrides.runtime_path.as_deref(),
        OCI_KVM_SERVICE_BIN_ENV,
        RUNTIME_FILENAME,
        "A3S OCI Runtime (KVM Host)",
        search_roots,
        true,
    )?;
    let shim_path = resolve_artifact(
        overrides.shim_path.as_deref(),
        OCI_KVM_SERVICE_SHIM_ENV,
        SHIM_FILENAME,
        "A3S OCI KVM shim",
        search_roots,
        true,
    )?;
    let system_image_manifest =
        resolve_manifest(overrides.system_image_manifest.as_deref(), search_roots)?;
    Ok(PackagedLinuxKvmArtifacts {
        runtime_path,
        shim_path,
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
        return finalize_file(
            path,
            OCI_KVM_SERVICE_MANIFEST_ENV,
            "KVM system-image manifest",
            false,
        );
    }
    for root in search_roots {
        for candidate in [
            root.join(MANIFEST_FILENAME),
            root.join("system-image").join(MANIFEST_FILENAME),
        ] {
            if candidate.is_file() {
                return finalize_file(
                    &candidate,
                    OCI_KVM_SERVICE_MANIFEST_ENV,
                    "KVM system-image manifest",
                    false,
                );
            }
        }
    }
    Err(missing_packaged(
        "KVM system-image manifest",
        MANIFEST_FILENAME,
        OCI_KVM_SERVICE_MANIFEST_ENV,
    ))
}

fn resolve_artifact(
    explicit: Option<&Path>,
    environment: &str,
    filename: &str,
    label: &str,
    search_roots: &[PathBuf],
    require_executable: bool,
) -> ExecutionManagerResult<PathBuf> {
    if let Some(path) = explicit {
        return finalize_file(path, environment, label, require_executable);
    }
    for root in search_roots {
        let candidate = root.join(filename);
        if candidate.is_file() {
            return finalize_file(&candidate, environment, label, require_executable);
        }
    }
    Err(missing_packaged(label, filename, environment))
}

fn finalize_file(
    selected: &Path,
    environment: &str,
    label: &str,
    require_executable: bool,
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
    if require_executable {
        use std::os::unix::fs::PermissionsExt;
        if metadata.permissions().mode() & 0o111 == 0 {
            return Err(ExecutionManagerError::InvalidRequest(format!(
                "{label} is not executable: {}",
                canonical.display()
            )));
        }
    }
    Ok(canonical)
}

fn missing_packaged(label: &str, filename: &str, environment: &str) -> ExecutionManagerError {
    ExecutionManagerError::InvalidRequest(format!(
        "{label} `{filename}` was not found in packaged A3S locations; install the A3S Box/OCI KVM package, set {environment}, or set {OCI_KVM_ENDPOINT_ENV} for an external qualification Host"
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::os::unix::fs::PermissionsExt;

    fn plant_executable(path: &Path) {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).unwrap();
        }
        fs::write(path, b"#!/bin/sh\n").unwrap();
        let mut perms = fs::metadata(path).unwrap().permissions();
        perms.set_mode(0o755);
        fs::set_permissions(path, perms).unwrap();
    }

    #[test]
    fn discovers_runtime_shim_and_manifest_from_search_root() {
        let root = std::env::temp_dir().join(format!("a3s-kvm-packaged-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(&root).unwrap();
        plant_executable(&root.join(RUNTIME_FILENAME));
        plant_executable(&root.join(SHIM_FILENAME));
        fs::write(root.join(MANIFEST_FILENAME), b"{}\n").unwrap();

        let found = discover_packaged_linux_kvm_artifacts_in(
            &[root.clone()],
            PackagedLinuxKvmOverrides::default(),
        )
        .unwrap();
        assert_eq!(
            found.runtime_path,
            root.join(RUNTIME_FILENAME).canonicalize().unwrap()
        );
        assert_eq!(
            found.shim_path,
            root.join(SHIM_FILENAME).canonicalize().unwrap()
        );
        assert_eq!(
            found.system_image_manifest,
            root.join(MANIFEST_FILENAME).canonicalize().unwrap()
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn discovers_manifest_under_system_image_subdirectory() {
        let root =
            std::env::temp_dir().join(format!("a3s-kvm-packaged-subdir-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        fs::create_dir_all(root.join("system-image")).unwrap();
        plant_executable(&root.join(RUNTIME_FILENAME));
        plant_executable(&root.join(SHIM_FILENAME));
        fs::write(root.join("system-image").join(MANIFEST_FILENAME), b"{}\n").unwrap();

        let found = discover_packaged_linux_kvm_artifacts_in(
            &[root.clone()],
            PackagedLinuxKvmOverrides::default(),
        )
        .unwrap();
        assert_eq!(
            found.system_image_manifest,
            root.join("system-image")
                .join(MANIFEST_FILENAME)
                .canonicalize()
                .unwrap()
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn overrides_win_over_search_root() {
        let root =
            std::env::temp_dir().join(format!("a3s-kvm-packaged-override-{}", std::process::id()));
        let alt = std::env::temp_dir().join(format!("a3s-kvm-packaged-alt-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&alt);
        fs::create_dir_all(&root).unwrap();
        fs::create_dir_all(&alt).unwrap();
        plant_executable(&root.join(RUNTIME_FILENAME));
        plant_executable(&root.join(SHIM_FILENAME));
        fs::write(root.join(MANIFEST_FILENAME), b"{}\n").unwrap();
        plant_executable(&alt.join("custom-oci"));
        plant_executable(&alt.join("custom-shim"));
        fs::write(alt.join("custom-manifest.json"), b"{}\n").unwrap();

        let found = discover_packaged_linux_kvm_artifacts_in(
            &[root.clone()],
            PackagedLinuxKvmOverrides {
                runtime_path: Some(alt.join("custom-oci")),
                shim_path: Some(alt.join("custom-shim")),
                system_image_manifest: Some(alt.join("custom-manifest.json")),
            },
        )
        .unwrap();
        assert_eq!(
            found.runtime_path,
            alt.join("custom-oci").canonicalize().unwrap()
        );
        assert_eq!(
            found.shim_path,
            alt.join("custom-shim").canonicalize().unwrap()
        );
        assert_eq!(
            found.system_image_manifest,
            alt.join("custom-manifest.json").canonicalize().unwrap()
        );
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&alt);
    }

    #[test]
    fn fails_closed_when_artifacts_missing() {
        let empty =
            std::env::temp_dir().join(format!("a3s-kvm-packaged-empty-{}", std::process::id()));
        let _ = fs::remove_dir_all(&empty);
        fs::create_dir_all(&empty).unwrap();
        let error = discover_packaged_linux_kvm_artifacts_in(
            &[empty.clone()],
            PackagedLinuxKvmOverrides::default(),
        )
        .unwrap_err();
        let message = error.to_string();
        assert!(
            message.contains(RUNTIME_FILENAME) || message.contains("not found"),
            "unexpected error: {message}"
        );
        assert!(
            message.contains(OCI_KVM_ENDPOINT_ENV),
            "error should mention qualification endpoint escape hatch: {message}"
        );
        let _ = fs::remove_dir_all(&empty);
    }
}
