//! Linux capability evidence for the shared-kernel Sandbox backend.

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::ExecutionBackend;
use serde::{Deserialize, Serialize};
#[cfg(target_os = "linux")]
use sha2::{Digest, Sha256};
#[cfg(target_os = "linux")]
use std::fs::File;
#[cfg(target_os = "linux")]
use std::io::Read;
#[cfg(target_os = "linux")]
use std::path::Component;
use std::path::{Path, PathBuf};

/// Capability snapshot schema persisted with a resolved execution plan.
pub const SANDBOX_CAPABILITY_SCHEMA: &str = "a3s.box.sandbox-capabilities.v1";

#[cfg(target_os = "linux")]
const REQUIRED_CGROUP_CONTROLLERS: &[&str] = &["cpu", "memory", "pids"];

/// Verified A3S OCI Runtime artifacts used by the native Linux owner.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CertifiedA3sOci {
    pub runtime_path: PathBuf,
    pub runtime_sha256: String,
    pub agent_path: PathBuf,
    pub agent_sha256: String,
}

/// One contiguous user-namespace ID mapping.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct IdMapping {
    pub container_id: u32,
    pub host_id: u32,
    pub size: u32,
}

/// One subordinate ID range assigned to the service account.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubordinateIdRange {
    pub start: u32,
    pub size: u32,
}

/// Host identity and subordinate-ID evidence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserNamespaceEvidence {
    pub effective_uid: u32,
    pub effective_gid: u32,
    pub username: Option<String>,
    pub max_user_namespaces: Option<u64>,
    pub subordinate_uids: Vec<SubordinateIdRange>,
    pub subordinate_gids: Vec<SubordinateIdRange>,
}

/// The exact mappings compiled into the OCI specification.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxIdMappingPlan {
    pub uid_mappings: Vec<IdMapping>,
    pub gid_mappings: Vec<IdMapping>,
    pub maximum_container_uid: u32,
    pub maximum_container_gid: u32,
}

/// cgroup v2 delegation evidence for the current service process.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CgroupV2Evidence {
    pub mountpoint: Option<PathBuf>,
    pub current_path: Option<PathBuf>,
    pub controllers: Vec<String>,
    pub delegated: bool,
}

/// Serializable pre-launch evidence for every mandatory Sandbox control.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxCapabilitySnapshot {
    pub schema: String,
    pub platform: String,
    pub architecture: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a3s_oci: Option<CertifiedA3sOci>,
    pub namespaces: Vec<String>,
    pub user_namespace: Option<UserNamespaceEvidence>,
    pub seccomp_actions: Vec<String>,
    pub no_new_privileges_supported: bool,
    pub capability_bounding_supported: bool,
    pub cgroup_v2: CgroupV2Evidence,
    pub failures: Vec<String>,
}

impl SandboxCapabilitySnapshot {
    /// Whether all mandatory host controls were evidenced.
    pub fn is_ready(&self) -> bool {
        self.failures.is_empty()
    }

    /// Fail closed before rootfs preparation when a mandatory control is absent.
    pub fn require_ready(&self) -> Result<()> {
        if self.is_ready() {
            return Ok(());
        }

        Err(BoxError::BoxBootError {
            message: format!(
                "Sandbox host capability check failed: {}",
                self.failures.join("; ")
            ),
            hint: Some(
                "Use an A3S Sandbox host with the selected runtime artifacts, user namespaces, and delegated cgroup v2"
                    .to_string(),
            ),
        })
    }
}

/// Probe the current host without changing namespaces, cgroups, or mounts.
///
/// An explicit runtime path is still subject to artifact validation. When
/// omitted, only packaged A3S locations are searched; `PATH` is deliberately
/// ignored.
pub fn probe_sandbox_capabilities(runtime_path: Option<&Path>) -> SandboxCapabilitySnapshot {
    probe_sandbox_capabilities_for(ExecutionBackend::A3sOci, runtime_path, None)
}

/// Probe host controls and the artifacts required by one Sandbox backend.
pub fn probe_sandbox_capabilities_for(
    backend: ExecutionBackend,
    runtime_path: Option<&Path>,
    agent_path: Option<&Path>,
) -> SandboxCapabilitySnapshot {
    let mut snapshot = SandboxCapabilitySnapshot {
        schema: SANDBOX_CAPABILITY_SCHEMA.to_string(),
        platform: std::env::consts::OS.to_string(),
        architecture: std::env::consts::ARCH.to_string(),
        a3s_oci: None,
        namespaces: Vec::new(),
        user_namespace: None,
        seccomp_actions: Vec::new(),
        no_new_privileges_supported: false,
        capability_bounding_supported: false,
        cgroup_v2: CgroupV2Evidence {
            mountpoint: None,
            current_path: None,
            controllers: Vec::new(),
            delegated: false,
        },
        failures: Vec::new(),
    };

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (backend, runtime_path, agent_path);
        snapshot
            .failures
            .push("Sandbox isolation is supported only on Linux".to_string());
        snapshot
    }

    #[cfg(target_os = "linux")]
    {
        match backend {
            ExecutionBackend::A3sOci => match resolve_a3s_oci_artifacts(runtime_path, agent_path) {
                Ok(runtime) => snapshot.a3s_oci = Some(runtime),
                Err(error) => snapshot.failures.push(error.to_string()),
            },
            ExecutionBackend::Krun => snapshot
                .failures
                .push("MicroVM execution is not a Sandbox backend".to_string()),
        }

        probe_namespaces(&mut snapshot);
        probe_seccomp_and_privileges(&mut snapshot);
        snapshot.cgroup_v2 = probe_cgroup_v2();
        if !snapshot.cgroup_v2.delegated {
            snapshot.failures.push(format!(
                "cgroup v2 delegation is unavailable or lacks controllers: {}",
                REQUIRED_CGROUP_CONTROLLERS.join(", ")
            ));
        }

        snapshot
    }
}

#[cfg(target_os = "linux")]
fn resolve_a3s_oci_artifacts(
    runtime_path: Option<&Path>,
    agent_path: Option<&Path>,
) -> Result<CertifiedA3sOci> {
    let runtime_path = resolve_packaged_artifact(
        runtime_path,
        "A3S_BOX_OCI_RUNTIME_PATH",
        "a3s-oci",
        "A3S OCI Runtime",
    )?;
    let agent_path = resolve_packaged_artifact(
        agent_path,
        "A3S_BOX_OCI_AGENT_PATH",
        "a3s-oci-agent",
        "A3S OCI agent",
    )?;
    Ok(CertifiedA3sOci {
        runtime_sha256: sha256_file(&runtime_path)?,
        agent_sha256: sha256_file(&agent_path)?,
        runtime_path,
        agent_path,
    })
}

#[cfg(target_os = "linux")]
fn resolve_packaged_artifact(
    explicit: Option<&Path>,
    environment: &str,
    filename: &str,
    label: &str,
) -> Result<PathBuf> {
    let selected = if let Some(path) = explicit {
        path.to_path_buf()
    } else if let Some(path) = std::env::var_os(environment).filter(|path| !path.is_empty()) {
        PathBuf::from(path)
    } else {
        let mut candidates = Vec::new();
        if let Ok(executable) = std::env::current_exe() {
            if let Some(directory) = executable.parent() {
                candidates.push(directory.join(filename));
                if directory.file_name().is_some_and(|name| name == "deps") {
                    if let Some(target_directory) = directory.parent() {
                        candidates.push(target_directory.join(filename));
                    }
                }
            }
        }
        candidates.push(a3s_box_core::dirs_home().join("bin").join(filename));
        candidates
            .into_iter()
            .find(|candidate| candidate.is_file())
            .ok_or_else(|| BoxError::BoxBootError {
                message: format!("{label} artifact was not found in packaged A3S locations"),
                hint: Some(format!(
                    "Install the A3S Box Sandbox runtime package or set {environment} to its packaged artifact"
                )),
            })?
    };

    let canonical = selected
        .canonicalize()
        .map_err(|error| BoxError::BoxBootError {
            message: format!("Failed to resolve {label} {}: {error}", selected.display()),
            hint: None,
        })?;
    let metadata = canonical
        .metadata()
        .map_err(|error| BoxError::BoxBootError {
            message: format!("Failed to inspect {label} {}: {error}", canonical.display()),
            hint: None,
        })?;
    if !metadata.is_file() {
        return Err(BoxError::BoxBootError {
            message: format!("{label} is not a regular file: {}", canonical.display()),
            hint: None,
        });
    }
    use std::os::unix::fs::PermissionsExt;
    if metadata.permissions().mode() & 0o111 == 0 {
        return Err(BoxError::BoxBootError {
            message: format!("{label} is not executable: {}", canonical.display()),
            hint: None,
        });
    }
    Ok(canonical)
}

/// Produce complete UID and GID mappings for the IDs present in one rootfs.
///
/// A non-root service account maps container root to itself and consumes
/// subordinate IDs from container ID 1 onward. A root service account must use
/// subordinate IDs even for container root, so host UID/GID 0 are never mapped.
pub fn plan_id_mappings(
    evidence: &UserNamespaceEvidence,
    maximum_container_uid: u32,
    maximum_container_gid: u32,
) -> Result<SandboxIdMappingPlan> {
    let uid_mappings = allocate_id_mappings(
        evidence.effective_uid,
        &evidence.subordinate_uids,
        maximum_container_uid,
        "UID",
    )?;
    let gid_mappings = allocate_id_mappings(
        evidence.effective_gid,
        &evidence.subordinate_gids,
        maximum_container_gid,
        "GID",
    )?;

    if uid_mappings
        .iter()
        .any(|mapping| mapping.container_id == 0 && mapping.host_id == 0)
        || gid_mappings
            .iter()
            .any(|mapping| mapping.container_id == 0 && mapping.host_id == 0)
    {
        return Err(BoxError::ConfigError(
            "Sandbox container root must not map to host root".to_string(),
        ));
    }

    Ok(SandboxIdMappingPlan {
        uid_mappings,
        gid_mappings,
        maximum_container_uid,
        maximum_container_gid,
    })
}

/// Translate one container UID through the same mapping allocation used by
/// Sandbox OCI specifications.
pub fn map_container_uid(evidence: &UserNamespaceEvidence, uid: u32) -> Result<u32> {
    map_container_identity(
        evidence.effective_uid,
        &evidence.subordinate_uids,
        uid,
        "UID",
    )
}

/// Translate one container GID through the same mapping allocation used by
/// Sandbox OCI specifications.
pub fn map_container_gid(evidence: &UserNamespaceEvidence, gid: u32) -> Result<u32> {
    map_container_identity(
        evidence.effective_gid,
        &evidence.subordinate_gids,
        gid,
        "GID",
    )
}

/// Recover the container UID represented by a mapped host UID.
pub fn unmap_host_uid(evidence: &UserNamespaceEvidence, uid: u32) -> Result<u32> {
    unmap_host_identity(
        evidence.effective_uid,
        &evidence.subordinate_uids,
        uid,
        "UID",
    )
}

/// Recover the container GID represented by a mapped host GID.
pub fn unmap_host_gid(evidence: &UserNamespaceEvidence, gid: u32) -> Result<u32> {
    unmap_host_identity(
        evidence.effective_gid,
        &evidence.subordinate_gids,
        gid,
        "GID",
    )
}

fn map_container_identity(
    effective_id: u32,
    subordinate_ranges: &[SubordinateIdRange],
    container_id: u32,
    kind: &str,
) -> Result<u32> {
    let mappings = allocate_id_mappings(effective_id, subordinate_ranges, container_id, kind)?;
    translate_container_id(&mappings, container_id, kind)
}

fn unmap_host_identity(
    effective_id: u32,
    subordinate_ranges: &[SubordinateIdRange],
    host_id: u32,
    kind: &str,
) -> Result<u32> {
    if effective_id != 0 && host_id == effective_id {
        return Ok(0);
    }

    let mut next_container_id = u32::from(effective_id != 0);
    for range in subordinate_ranges {
        if range.size == 0 || range.start == 0 {
            continue;
        }
        let Some(host_end) = range.start.checked_add(range.size) else {
            continue;
        };
        if effective_id != 0 && range.start <= effective_id && effective_id < host_end {
            continue;
        }
        if range.start <= host_id && host_id < host_end {
            return next_container_id
                .checked_add(host_id - range.start)
                .ok_or_else(|| {
                    BoxError::ConfigError(format!("Sandbox {kind} reverse mapping overflows u32"))
                });
        }
        next_container_id = next_container_id.checked_add(range.size).ok_or_else(|| {
            BoxError::ConfigError(format!("Sandbox {kind} mapping range overflows u32"))
        })?;
    }

    Err(BoxError::ConfigError(format!(
        "Sandbox host {kind} {host_id} is outside the configured mappings"
    )))
}

fn translate_container_id(mappings: &[IdMapping], container_id: u32, kind: &str) -> Result<u32> {
    for mapping in mappings {
        let Some(end) = mapping.container_id.checked_add(mapping.size) else {
            continue;
        };
        if mapping.container_id <= container_id && container_id < end {
            return mapping
                .host_id
                .checked_add(container_id - mapping.container_id)
                .ok_or_else(|| {
                    BoxError::ConfigError(format!("Sandbox {kind} mapping overflows u32"))
                });
        }
    }
    Err(BoxError::ConfigError(format!(
        "Sandbox mappings do not cover container {kind} {container_id}"
    )))
}

fn allocate_id_mappings(
    effective_id: u32,
    subordinate_ranges: &[SubordinateIdRange],
    maximum_container_id: u32,
    kind: &str,
) -> Result<Vec<IdMapping>> {
    let mut mappings = Vec::new();
    let mut next_container_id = 0u32;

    if effective_id != 0 {
        mappings.push(IdMapping {
            container_id: 0,
            host_id: effective_id,
            size: 1,
        });
        next_container_id = 1;
    }

    let required_end = maximum_container_id
        .checked_add(1)
        .ok_or_else(|| BoxError::ConfigError(format!("Sandbox {kind} range overflows u32")))?;

    for range in subordinate_ranges {
        if next_container_id >= required_end {
            break;
        }
        if range.size == 0 || range.start == 0 {
            continue;
        }
        let host_end = match range.start.checked_add(range.size) {
            Some(end) => end,
            None => continue,
        };
        if effective_id != 0 && range.start <= effective_id && effective_id < host_end {
            continue;
        }

        let remaining = required_end - next_container_id;
        let size = remaining.min(range.size);
        mappings.push(IdMapping {
            container_id: next_container_id,
            host_id: range.start,
            size,
        });
        next_container_id += size;
    }

    if next_container_id < required_end {
        return Err(BoxError::ConfigError(format!(
            "Sandbox needs mappings through container {kind} {maximum_container_id}, but the service account has only {} mapped IDs",
            next_container_id
        )));
    }

    Ok(mappings)
}

#[cfg(target_os = "linux")]
fn sha256_file(path: &Path) -> Result<String> {
    let mut file = File::open(path)?;
    let mut hasher = Sha256::new();
    let mut buffer = [0u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer)?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
    }
    Ok(hex::encode(hasher.finalize()))
}

#[cfg(target_os = "linux")]
fn probe_namespaces(snapshot: &mut SandboxCapabilitySnapshot) {
    const REQUIRED: &[(&str, &str)] = &[
        ("user", "user namespace"),
        ("mnt", "mount namespace"),
        ("pid", "PID namespace"),
        ("ipc", "IPC namespace"),
        ("uts", "UTS namespace"),
        ("net", "network namespace"),
        ("cgroup", "cgroup namespace"),
    ];

    for (name, label) in REQUIRED {
        if Path::new("/proc/self/ns").join(name).exists() {
            snapshot.namespaces.push((*name).to_string());
        } else {
            snapshot
                .failures
                .push(format!("Kernel does not expose the required {label}"));
        }
    }

    let effective_uid = unsafe { libc::geteuid() };
    let effective_gid = unsafe { libc::getegid() };
    let username = username_for_uid(effective_uid);
    let max_user_namespaces = read_trimmed("/proc/sys/user/max_user_namespaces")
        .and_then(|value| value.parse::<u64>().ok());
    if max_user_namespaces == Some(0) || max_user_namespaces.is_none() {
        snapshot
            .failures
            .push("User namespaces are disabled by the host".to_string());
    }

    let subordinate_uids =
        read_subordinate_ranges("/etc/subuid", effective_uid, username.as_deref());
    let subordinate_gids =
        read_subordinate_ranges("/etc/subgid", effective_uid, username.as_deref());
    if effective_uid == 0 && subordinate_uids.is_empty() {
        snapshot.failures.push(
            "A root-run Sandbox service requires a non-root subordinate UID range".to_string(),
        );
    }
    if effective_gid == 0 && subordinate_gids.is_empty() {
        snapshot.failures.push(
            "A root-run Sandbox service requires a non-root subordinate GID range".to_string(),
        );
    }

    snapshot.user_namespace = Some(UserNamespaceEvidence {
        effective_uid,
        effective_gid,
        username,
        max_user_namespaces,
        subordinate_uids,
        subordinate_gids,
    });
}

#[cfg(target_os = "linux")]
fn probe_seccomp_and_privileges(snapshot: &mut SandboxCapabilitySnapshot) {
    snapshot.seccomp_actions = read_trimmed("/proc/sys/kernel/seccomp/actions_avail")
        .map(|line| line.split_whitespace().map(ToString::to_string).collect())
        .unwrap_or_default();
    if !snapshot
        .seccomp_actions
        .iter()
        .any(|action| action == "allow")
        || !snapshot
            .seccomp_actions
            .iter()
            .any(|action| action == "errno")
    {
        snapshot
            .failures
            .push("Kernel seccomp ERRNO/ALLOW actions are unavailable".to_string());
    }

    let status = read_trimmed("/proc/self/status").unwrap_or_default();
    snapshot.no_new_privileges_supported =
        status.lines().any(|line| line.starts_with("NoNewPrivs:"));
    snapshot.capability_bounding_supported = status.lines().any(|line| line.starts_with("CapBnd:"));
    if !snapshot.no_new_privileges_supported {
        snapshot
            .failures
            .push("Kernel does not expose no_new_privs state".to_string());
    }
    if !snapshot.capability_bounding_supported {
        snapshot
            .failures
            .push("Kernel does not expose a capability bounding set".to_string());
    }
}

#[cfg(target_os = "linux")]
fn probe_cgroup_v2() -> CgroupV2Evidence {
    let mountinfo = read_trimmed("/proc/self/mountinfo");
    let mountpoint = mountinfo.as_deref().and_then(parse_cgroup2_mountpoint);
    let current_path = process_cgroup_v2_path(std::process::id());
    let controllers: Vec<String> = current_path
        .as_ref()
        .and_then(|path| read_trimmed(path.join("cgroup.controllers")))
        .map(|line| line.split_whitespace().map(ToString::to_string).collect())
        .unwrap_or_default();
    let has_controllers = REQUIRED_CGROUP_CONTROLLERS
        .iter()
        .all(|required| controllers.iter().any(|value| value == required));
    let delegated = current_path.as_ref().is_some_and(|path| {
        has_controllers
            && path.join("cgroup.procs").exists()
            && path.join("cgroup.subtree_control").exists()
            && path_is_writable(path)
            && path_is_writable(&path.join("cgroup.procs"))
            && path_is_writable(&path.join("cgroup.subtree_control"))
    });

    CgroupV2Evidence {
        mountpoint,
        current_path,
        controllers,
        delegated,
    }
}

/// Resolve the host cgroup v2 directory containing `pid`.
///
/// OCI runtimes may place a workload below a private manager cgroup, so callers
/// must derive this path from the process rather than assume a fixed hierarchy.
#[cfg(target_os = "linux")]
pub(crate) fn process_cgroup_v2_path(pid: u32) -> Option<PathBuf> {
    let mountinfo = read_trimmed("/proc/self/mountinfo")?;
    let cgroup = read_trimmed(PathBuf::from("/proc").join(pid.to_string()).join("cgroup"))?;
    parse_cgroup_v2_path(&mountinfo, &cgroup)
}

#[cfg(target_os = "linux")]
fn parse_cgroup_v2_path(mountinfo: &str, cgroup: &str) -> Option<PathBuf> {
    let mountpoint = parse_cgroup2_mountpoint(mountinfo)?;
    let relative = parse_current_cgroup_path(cgroup)?;
    safe_join_cgroup(&mountpoint, relative)
}

#[cfg(target_os = "linux")]
fn parse_cgroup2_mountpoint(mountinfo: &str) -> Option<PathBuf> {
    mountinfo.lines().find_map(|line| {
        let (left, right) = line.split_once(" - ")?;
        if right.split_whitespace().next()? != "cgroup2" {
            return None;
        }
        let mountpoint = left.split_whitespace().nth(4)?;
        Some(PathBuf::from(unescape_mountinfo(mountpoint)))
    })
}

#[cfg(target_os = "linux")]
fn parse_current_cgroup_path(contents: &str) -> Option<&str> {
    contents.lines().find_map(|line| {
        let mut fields = line.splitn(3, ':');
        let hierarchy = fields.next()?;
        let controllers = fields.next()?;
        let path = fields.next()?;
        (hierarchy == "0" && controllers.is_empty()).then_some(path)
    })
}

#[cfg(target_os = "linux")]
fn safe_join_cgroup(mountpoint: &Path, relative: &str) -> Option<PathBuf> {
    let relative = Path::new(relative.trim_start_matches('/'));
    if relative.components().any(|component| {
        matches!(
            component,
            Component::ParentDir | Component::RootDir | Component::Prefix(_)
        )
    }) {
        return None;
    }
    Some(mountpoint.join(relative))
}

#[cfg(target_os = "linux")]
fn unescape_mountinfo(value: &str) -> String {
    value
        .replace("\\040", " ")
        .replace("\\011", "\t")
        .replace("\\012", "\n")
        .replace("\\134", "\\")
}

#[cfg(target_os = "linux")]
fn path_is_writable(path: &Path) -> bool {
    use std::os::unix::ffi::OsStrExt;

    let Ok(path) = std::ffi::CString::new(path.as_os_str().as_bytes()) else {
        return false;
    };
    unsafe { libc::access(path.as_ptr(), libc::W_OK) == 0 }
}

#[cfg(target_os = "linux")]
fn read_trimmed(path: impl AsRef<Path>) -> Option<String> {
    std::fs::read_to_string(path)
        .ok()
        .map(|value| value.trim().to_string())
}

#[cfg(target_os = "linux")]
fn username_for_uid(uid: u32) -> Option<String> {
    let passwd = std::fs::read_to_string("/etc/passwd").ok()?;
    passwd.lines().find_map(|line| {
        if line.trim_start().starts_with('#') {
            return None;
        }
        let mut fields = line.split(':');
        let name = fields.next()?;
        let _password = fields.next()?;
        let entry_uid = fields.next()?.parse::<u32>().ok()?;
        (entry_uid == uid).then(|| name.to_string())
    })
}

#[cfg(target_os = "linux")]
fn read_subordinate_ranges(
    path: &str,
    uid: u32,
    username: Option<&str>,
) -> Vec<SubordinateIdRange> {
    let Some(contents) = read_trimmed(path) else {
        return Vec::new();
    };
    parse_subordinate_ranges(&contents, uid, username)
}

#[cfg(any(target_os = "linux", test))]
fn parse_subordinate_ranges(
    contents: &str,
    uid: u32,
    username: Option<&str>,
) -> Vec<SubordinateIdRange> {
    let uid = uid.to_string();
    let mut ranges: Vec<_> = contents
        .lines()
        .filter_map(|line| {
            let line = line.split('#').next()?.trim();
            let mut fields = line.split(':');
            let owner = fields.next()?;
            if owner != uid && username != Some(owner) {
                return None;
            }
            let start = fields.next()?.parse::<u32>().ok()?;
            let size = fields.next()?.parse::<u32>().ok()?;
            if fields.next().is_some() || start == 0 || size == 0 {
                return None;
            }
            start.checked_add(size)?;
            Some(SubordinateIdRange { start, size })
        })
        .collect();
    ranges.sort_by_key(|range| range.start);
    ranges
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(target_os = "linux")]
    #[test]
    fn resolves_and_digests_explicit_a3s_oci_artifacts() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let runtime = temporary.path().join("a3s-oci");
        let agent = temporary.path().join("a3s-oci-agent");
        std::fs::write(&runtime, b"runtime-artifact").unwrap();
        std::fs::write(&agent, b"agent-artifact").unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o700)).unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();

        let artifacts = resolve_a3s_oci_artifacts(Some(&runtime), Some(&agent)).unwrap();

        assert_eq!(artifacts.runtime_path, runtime.canonicalize().unwrap());
        assert_eq!(artifacts.agent_path, agent.canonicalize().unwrap());
        assert_eq!(artifacts.runtime_sha256.len(), 64);
        assert_eq!(artifacts.agent_sha256.len(), 64);
        assert_ne!(artifacts.runtime_sha256, artifacts.agent_sha256);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn rejects_a_non_executable_a3s_oci_artifact() {
        use std::os::unix::fs::PermissionsExt;

        let temporary = tempfile::tempdir().unwrap();
        let runtime = temporary.path().join("a3s-oci");
        let agent = temporary.path().join("a3s-oci-agent");
        std::fs::write(&runtime, b"runtime-artifact").unwrap();
        std::fs::write(&agent, b"agent-artifact").unwrap();
        std::fs::set_permissions(&runtime, std::fs::Permissions::from_mode(0o600)).unwrap();
        std::fs::set_permissions(&agent, std::fs::Permissions::from_mode(0o700)).unwrap();

        let error = resolve_a3s_oci_artifacts(Some(&runtime), Some(&agent)).unwrap_err();

        assert!(error.to_string().contains("not executable"));
    }

    #[test]
    fn subordinate_ranges_match_name_or_numeric_uid() {
        let contents = "alice:100000:65536\n1001:200000:42\nbob:300000:5\n";
        assert_eq!(
            parse_subordinate_ranges(contents, 1001, Some("alice")),
            vec![
                SubordinateIdRange {
                    start: 100000,
                    size: 65536,
                },
                SubordinateIdRange {
                    start: 200000,
                    size: 42,
                },
            ]
        );
    }

    #[test]
    fn non_root_mapping_uses_effective_id_for_container_root() {
        let evidence = UserNamespaceEvidence {
            effective_uid: 1000,
            effective_gid: 1000,
            username: Some("box".to_string()),
            max_user_namespaces: Some(1024),
            subordinate_uids: vec![SubordinateIdRange {
                start: 100000,
                size: 65536,
            }],
            subordinate_gids: vec![SubordinateIdRange {
                start: 200000,
                size: 65536,
            }],
        };
        let plan = plan_id_mappings(&evidence, 65535, 65535).unwrap();
        assert_eq!(
            plan.uid_mappings,
            vec![
                IdMapping {
                    container_id: 0,
                    host_id: 1000,
                    size: 1,
                },
                IdMapping {
                    container_id: 1,
                    host_id: 100000,
                    size: 65535,
                },
            ]
        );
        assert_eq!(plan.gid_mappings[0].host_id, 1000);
        assert_eq!(plan.gid_mappings[1].host_id, 200000);
    }

    #[test]
    fn root_service_maps_container_root_to_subordinate_id() {
        let evidence = UserNamespaceEvidence {
            effective_uid: 0,
            effective_gid: 0,
            username: Some("root".to_string()),
            max_user_namespaces: Some(1024),
            subordinate_uids: vec![SubordinateIdRange {
                start: 100000,
                size: 65536,
            }],
            subordinate_gids: vec![SubordinateIdRange {
                start: 200000,
                size: 65536,
            }],
        };
        let plan = plan_id_mappings(&evidence, 65535, 65535).unwrap();
        assert_eq!(plan.uid_mappings[0].container_id, 0);
        assert_eq!(plan.uid_mappings[0].host_id, 100000);
        assert_eq!(plan.gid_mappings[0].host_id, 200000);
        assert_eq!(map_container_uid(&evidence, 0).unwrap(), 100000);
        assert_eq!(map_container_uid(&evidence, 1000).unwrap(), 101000);
        assert_eq!(unmap_host_uid(&evidence, 100000).unwrap(), 0);
        assert_eq!(unmap_host_uid(&evidence, 101000).unwrap(), 1000);
    }

    #[test]
    fn identity_translation_matches_multi_range_allocation() {
        let evidence = UserNamespaceEvidence {
            effective_uid: 1000,
            effective_gid: 2000,
            username: None,
            max_user_namespaces: Some(1024),
            subordinate_uids: vec![
                SubordinateIdRange {
                    start: 100000,
                    size: 2,
                },
                SubordinateIdRange {
                    start: 200000,
                    size: 3,
                },
            ],
            subordinate_gids: vec![SubordinateIdRange {
                start: 300000,
                size: 8,
            }],
        };

        assert_eq!(map_container_uid(&evidence, 0).unwrap(), 1000);
        assert_eq!(map_container_uid(&evidence, 1).unwrap(), 100000);
        assert_eq!(map_container_uid(&evidence, 2).unwrap(), 100001);
        assert_eq!(map_container_uid(&evidence, 3).unwrap(), 200000);
        assert_eq!(unmap_host_uid(&evidence, 1000).unwrap(), 0);
        assert_eq!(unmap_host_uid(&evidence, 200002).unwrap(), 5);
        assert!(map_container_uid(&evidence, 6).is_err());
        assert!(unmap_host_uid(&evidence, 42).is_err());
        assert_eq!(map_container_gid(&evidence, 1).unwrap(), 300000);
        assert_eq!(unmap_host_gid(&evidence, 300000).unwrap(), 1);
    }

    #[test]
    fn one_id_rootless_mapping_is_allowed_only_when_sufficient() {
        let evidence = UserNamespaceEvidence {
            effective_uid: 1000,
            effective_gid: 1000,
            username: None,
            max_user_namespaces: Some(1024),
            subordinate_uids: Vec::new(),
            subordinate_gids: Vec::new(),
        };
        assert!(plan_id_mappings(&evidence, 0, 0).is_ok());
        assert!(plan_id_mappings(&evidence, 1, 0).is_err());
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parses_cgroup_v2_paths_without_traversal() {
        let mountinfo =
            "29 23 0:26 / /sys/fs/cgroup rw,nosuid,nodev,noexec,relatime - cgroup2 cgroup rw\n";
        assert_eq!(
            parse_cgroup2_mountpoint(mountinfo).as_deref(),
            Some(Path::new("/sys/fs/cgroup"))
        );
        assert_eq!(
            parse_current_cgroup_path("0::/user.slice/a3s.service\n"),
            Some("/user.slice/a3s.service")
        );
        assert_eq!(
            parse_cgroup_v2_path(mountinfo, "0::/a3s-oci-42/a3s-box/unit-1\n").as_deref(),
            Some(Path::new("/sys/fs/cgroup/a3s-oci-42/a3s-box/unit-1"))
        );
        assert!(safe_join_cgroup(Path::new("/sys/fs/cgroup"), "/../../etc").is_none());
        assert!(parse_cgroup_v2_path(mountinfo, "0::/../../etc\n").is_none());
    }
}
