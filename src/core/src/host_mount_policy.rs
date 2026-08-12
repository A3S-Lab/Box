//! Side-effect-free host bind-mount risk classification.
//!
//! The evaluator inspects an existing host path but never creates, removes, or
//! changes it. Runtime launch code must revalidate the source identity
//! immediately before exposing the mount to a workload.

use std::collections::VecDeque;
use std::fs::FileType;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::error::{BoxError, Result};
use crate::security_policy::{HostMountPolicy, HostMountPolicyMode};

const DEFAULT_SCAN_ENTRY_LIMIT: usize = 100_000;
const DEFAULT_SCAN_DEPTH_LIMIT: usize = 32;

/// Filesystem object represented by a host bind source or protected
/// descendant.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostMountSourceKind {
    File,
    Directory,
    UnixSocket,
    Fifo,
    CharacterDevice,
    BlockDevice,
    OtherSpecial,
}

/// Stable host filesystem identity used to detect source replacement between
/// planning and launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "platform", rename_all = "snake_case")]
pub enum HostMountIdentity {
    Unix { device: u64, inode: u64 },
    Windows { volume_serial: u32, file_index: u64 },
}

/// Security-relevant property found beneath a requested host bind.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostMountRisk {
    HostRoot,
    BroadHome,
    Credential,
    HostControlSocket,
    UnixSocket,
    Fifo,
    CharacterDevice,
    BlockDevice,
    OtherSpecial,
}

/// Explicit typed authorization that accepted one direct finding.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostMountAuthorization {
    ExactPath,
    HostControlSocket,
}

/// One deterministic finding retained for planning and security receipts.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostMountFinding {
    pub risk: HostMountRisk,
    pub evidence_path: PathBuf,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub authorization: Option<HostMountAuthorization>,
}

/// Result of applying the selected audit or enforcement mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostMountOutcome {
    Allow,
    Audit,
    Deny,
}

/// Canonical assessment of one existing host bind source.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostMountAssessment {
    pub source: PathBuf,
    pub source_kind: HostMountSourceKind,
    pub identity: HostMountIdentity,
    pub findings: Vec<HostMountFinding>,
    pub outcome: HostMountOutcome,
}

/// Parsed legacy `host:guest[:ro|rw]` bind intent used at the runtime
/// boundary. Named volumes are resolved and excluded before policy evaluation.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct HostBindMount {
    pub source: PathBuf,
    pub destination: String,
    pub read_only: bool,
}

impl HostBindMount {
    /// Parse from the right so a Windows drive separator remains part of the
    /// host source.
    pub fn parse(value: &str) -> Result<Self> {
        let (mount, read_only) = match value.rsplit_once(':') {
            Some((mount, "ro")) => (mount, true),
            Some((mount, "rw")) => (mount, false),
            Some((mount, mode)) if mount.contains(':') && !mode.starts_with('/') => {
                return Err(BoxError::ConfigError(format!(
                    "invalid host bind mode {mode:?}; expected ro or rw: {value}"
                )))
            }
            _ => (value, false),
        };
        let (source, destination) = mount.rsplit_once(':').ok_or_else(|| {
            BoxError::ConfigError(format!(
                "invalid host bind; expected host:guest[:ro|rw]: {value}"
            ))
        })?;
        if source.is_empty() {
            return Err(BoxError::ConfigError(format!(
                "host bind source is empty: {value}"
            )));
        }

        Ok(Self {
            source: PathBuf::from(source),
            destination: normalize_guest_mount_path(destination)?,
            read_only,
        })
    }
}

/// One host bind whose policy assessment is bound into the execution plan.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ResolvedHostMount {
    pub source: PathBuf,
    pub destination: String,
    pub read_only: bool,
    pub assessment: HostMountAssessment,
}

impl ResolvedHostMount {
    pub fn new(mount: HostBindMount, assessment: HostMountAssessment) -> Result<Self> {
        assessment.ensure_allowed()?;
        let source = mount.source.canonicalize().map_err(|error| {
            BoxError::ConfigError(format!(
                "host mount policy cannot resolve {}: {error}",
                mount.source.display()
            ))
        })?;
        if source != assessment.source {
            return Err(BoxError::ConfigError(format!(
                "host mount assessment source {} does not match {}",
                assessment.source.display(),
                source.display()
            )));
        }
        Ok(Self {
            source,
            destination: mount.destination,
            read_only: mount.read_only,
            assessment,
        })
    }
}

impl HostMountAssessment {
    /// Reject only an enforcing assessment. Audit findings remain observable
    /// without being mislabeled as enforcement.
    pub fn ensure_allowed(&self) -> Result<()> {
        if self.outcome != HostMountOutcome::Deny {
            return Ok(());
        }

        let risks = self
            .findings
            .iter()
            .filter(|finding| finding.authorization.is_none())
            .map(|finding| format!("{:?} at {}", finding.risk, finding.evidence_path.display()))
            .collect::<Vec<_>>();
        Err(BoxError::ConfigError(format!(
            "host mount policy denied {}: {}",
            self.source.display(),
            risks.join(", ")
        )))
    }
}

/// Bounded evaluator for one normalized host mount policy.
#[derive(Debug, Clone)]
pub struct HostMountPolicyEvaluator {
    policy: HostMountPolicy,
    home_dir: Option<PathBuf>,
    scan_entry_limit: usize,
    scan_depth_limit: usize,
}

impl HostMountPolicyEvaluator {
    /// Build an evaluator using the current service user's home directory.
    pub fn for_host(policy: &HostMountPolicy) -> Result<Self> {
        Self::with_home_dir(policy, dirs::home_dir())
    }

    /// Build an evaluator with an explicit host home directory context.
    pub fn with_home_dir(policy: &HostMountPolicy, home_dir: Option<PathBuf>) -> Result<Self> {
        let policy = policy.normalized()?;
        let home_dir = home_dir.map(|path| path.canonicalize().unwrap_or(path));
        Ok(Self {
            policy,
            home_dir,
            scan_entry_limit: DEFAULT_SCAN_ENTRY_LIMIT,
            scan_depth_limit: DEFAULT_SCAN_DEPTH_LIMIT,
        })
    }

    /// Inspect one existing source without mutating the filesystem.
    pub fn evaluate(&self, source: &Path) -> Result<HostMountAssessment> {
        let source = source.canonicalize().map_err(|error| {
            BoxError::ConfigError(format!(
                "host mount policy cannot resolve {}: {error}",
                source.display()
            ))
        })?;
        let metadata = std::fs::symlink_metadata(&source).map_err(|error| {
            BoxError::ConfigError(format!(
                "host mount policy cannot inspect {}: {error}",
                source.display()
            ))
        })?;
        let source_kind = source_kind(metadata.file_type());
        let identity = source_identity(&source, &metadata)?;
        let mut findings = Vec::new();

        let host_root = is_filesystem_root(&source);
        let broad_home = self
            .home_dir
            .as_ref()
            .is_some_and(|home| source == *home || home.starts_with(&source));
        if host_root {
            findings.push(finding(HostMountRisk::HostRoot, &source));
        }
        if broad_home {
            findings.push(finding(HostMountRisk::BroadHome, &source));
        }
        if is_credential_path(&source, self.home_dir.as_deref()) {
            findings.push(finding(HostMountRisk::Credential, &source));
        }
        if let Some(risk) = special_file_risk(&source, source_kind) {
            findings.push(finding(risk, &source));
        }

        // Root and broad-home findings are sufficient to reject the first
        // enforcing profile. Recursing through those trees would be unbounded
        // and would incorrectly imply that a complete carveout was computed.
        if source_kind == HostMountSourceKind::Directory && !host_root && !broad_home {
            self.scan_descendants(&source, &mut findings)?;
        }

        findings.sort();
        findings.dedup();
        self.apply_direct_authorizations(&source, &mut findings);
        let has_unapproved = findings
            .iter()
            .any(|finding| finding.authorization.is_none());
        let outcome = match (has_unapproved, self.policy.mode) {
            (false, _) => HostMountOutcome::Allow,
            (true, HostMountPolicyMode::Audit) => HostMountOutcome::Audit,
            (true, HostMountPolicyMode::Enforce) => HostMountOutcome::Deny,
        };

        Ok(HostMountAssessment {
            source,
            source_kind,
            identity,
            findings,
            outcome,
        })
    }

    /// Re-evaluate an assessment immediately before launch and reject source
    /// replacement or a changed risk classification.
    pub fn revalidate(&self, planned: &HostMountAssessment) -> Result<HostMountAssessment> {
        let current = self.evaluate(&planned.source)?;
        if current.source != planned.source
            || current.source_kind != planned.source_kind
            || current.identity != planned.identity
        {
            return Err(BoxError::ConfigError(format!(
                "host mount source identity changed after planning: {}",
                planned.source.display()
            )));
        }
        if current.findings != planned.findings || current.outcome != planned.outcome {
            return Err(BoxError::ConfigError(format!(
                "host mount risk classification changed after planning: {}",
                planned.source.display()
            )));
        }
        Ok(current)
    }

    fn scan_descendants(&self, source: &Path, findings: &mut Vec<HostMountFinding>) -> Result<()> {
        let mut pending = VecDeque::from([(source.to_path_buf(), 0_usize)]);
        let mut inspected = 0_usize;

        while let Some((directory, depth)) = pending.pop_front() {
            let entries = std::fs::read_dir(&directory).map_err(|error| {
                BoxError::ConfigError(format!(
                    "host mount policy cannot scan {}: {error}",
                    directory.display()
                ))
            })?;
            for entry in entries {
                let entry = entry.map_err(|error| {
                    BoxError::ConfigError(format!(
                        "host mount policy cannot scan {}: {error}",
                        directory.display()
                    ))
                })?;
                inspected = inspected.checked_add(1).ok_or_else(|| {
                    BoxError::ConfigError("host mount policy scan counter overflowed".to_string())
                })?;
                if inspected > self.scan_entry_limit {
                    return Err(BoxError::ConfigError(format!(
                        "host mount policy scan of {} exceeded {} entries",
                        source.display(),
                        self.scan_entry_limit
                    )));
                }

                let path = entry.path();
                let metadata = std::fs::symlink_metadata(&path).map_err(|error| {
                    BoxError::ConfigError(format!(
                        "host mount policy cannot inspect {}: {error}",
                        path.display()
                    ))
                })?;
                if metadata.file_type().is_symlink() {
                    continue;
                }
                let kind = source_kind(metadata.file_type());
                if is_credential_path(&path, self.home_dir.as_deref()) {
                    findings.push(finding(HostMountRisk::Credential, &path));
                }
                if let Some(risk) = special_file_risk(&path, kind) {
                    findings.push(finding(risk, &path));
                }
                if kind == HostMountSourceKind::Directory {
                    if depth >= self.scan_depth_limit {
                        return Err(BoxError::ConfigError(format!(
                            "host mount policy scan of {} exceeded depth {} at {}",
                            source.display(),
                            self.scan_depth_limit,
                            path.display()
                        )));
                    }
                    pending.push_back((path, depth + 1));
                }
            }
        }
        Ok(())
    }

    fn apply_direct_authorizations(&self, source: &Path, findings: &mut [HostMountFinding]) {
        let allowed_paths = resolved_exception_paths(&self.policy.allowed_paths);
        let allowed_control_sockets =
            resolved_exception_paths(&self.policy.allowed_host_control_sockets);
        for finding in findings {
            // The first version never authorizes a broad parent through an
            // exception and never treats a descendant exception as a carveout.
            if finding.evidence_path != source
                || matches!(
                    finding.risk,
                    HostMountRisk::HostRoot | HostMountRisk::BroadHome
                )
            {
                continue;
            }
            finding.authorization = if finding.risk == HostMountRisk::HostControlSocket {
                allowed_control_sockets
                    .contains(&source.to_path_buf())
                    .then_some(HostMountAuthorization::HostControlSocket)
            } else {
                allowed_paths
                    .contains(&source.to_path_buf())
                    .then_some(HostMountAuthorization::ExactPath)
            };
        }
    }

    #[cfg(test)]
    fn with_scan_limits(mut self, entries: usize, depth: usize) -> Self {
        self.scan_entry_limit = entries;
        self.scan_depth_limit = depth;
        self
    }
}

fn finding(risk: HostMountRisk, evidence_path: &Path) -> HostMountFinding {
    HostMountFinding {
        risk,
        evidence_path: evidence_path.to_path_buf(),
        authorization: None,
    }
}

fn normalize_guest_mount_path(value: &str) -> Result<String> {
    if !value.starts_with('/') || value.contains('\0') {
        return Err(BoxError::ConfigError(format!(
            "host bind destination must be an absolute Linux path: {value:?}"
        )));
    }
    let mut components = Vec::new();
    for component in value.split('/').filter(|component| !component.is_empty()) {
        if matches!(component, "." | "..") {
            return Err(BoxError::ConfigError(format!(
                "host bind destination must be normalized: {value:?}"
            )));
        }
        components.push(component);
    }
    if components.is_empty() {
        Ok("/".to_string())
    } else {
        Ok(format!("/{}", components.join("/")))
    }
}

fn resolved_exception_paths(paths: &[PathBuf]) -> Vec<PathBuf> {
    paths
        .iter()
        .map(|path| path.canonicalize().unwrap_or_else(|_| path.clone()))
        .collect()
}

fn is_filesystem_root(path: &Path) -> bool {
    path.parent().is_none()
}

fn is_credential_path(path: &Path, home_dir: Option<&Path>) -> bool {
    let name = path
        .file_name()
        .and_then(|name| name.to_str())
        .map(str::to_ascii_lowercase);
    if name.as_deref().is_some_and(is_sensitive_name) {
        return true;
    }

    let Some(home_dir) = home_dir else {
        return false;
    };
    let Ok(relative) = path.strip_prefix(home_dir) else {
        return false;
    };
    let components = relative
        .components()
        .filter_map(|component| component.as_os_str().to_str())
        .map(str::to_ascii_lowercase)
        .collect::<Vec<_>>();
    matches!(
        components.as_slice(),
        [config, provider, ..]
            if config == ".config"
                && matches!(provider.as_str(), "gcloud" | "gh" | "oci" | "op")
    )
}

fn is_sensitive_name(name: &str) -> bool {
    matches!(
        name,
        ".ssh"
            | ".aws"
            | ".azure"
            | ".gnupg"
            | ".kube"
            | ".docker"
            | ".netrc"
            | ".npmrc"
            | ".pypirc"
            | ".git-credentials"
            | "credentials.tfrc.json"
    ) || name == ".env"
        || name.starts_with(".env.")
}

fn special_file_risk(path: &Path, kind: HostMountSourceKind) -> Option<HostMountRisk> {
    match kind {
        HostMountSourceKind::File | HostMountSourceKind::Directory => None,
        HostMountSourceKind::UnixSocket if is_host_control_socket(path) => {
            Some(HostMountRisk::HostControlSocket)
        }
        HostMountSourceKind::UnixSocket => Some(HostMountRisk::UnixSocket),
        HostMountSourceKind::Fifo => Some(HostMountRisk::Fifo),
        HostMountSourceKind::CharacterDevice => Some(HostMountRisk::CharacterDevice),
        HostMountSourceKind::BlockDevice => Some(HostMountRisk::BlockDevice),
        HostMountSourceKind::OtherSpecial => Some(HostMountRisk::OtherSpecial),
    }
}

fn is_host_control_socket(path: &Path) -> bool {
    path.file_name()
        .and_then(|name| name.to_str())
        .map(str::to_ascii_lowercase)
        .is_some_and(|name| {
            matches!(
                name.as_str(),
                "docker.sock"
                    | "containerd.sock"
                    | "podman.sock"
                    | "crio.sock"
                    | "cri-dockerd.sock"
            )
        })
}

#[cfg(unix)]
fn source_identity(_source: &Path, metadata: &std::fs::Metadata) -> Result<HostMountIdentity> {
    use std::os::unix::fs::MetadataExt;

    Ok(HostMountIdentity::Unix {
        device: metadata.dev(),
        inode: metadata.ino(),
    })
}

#[cfg(windows)]
fn source_identity(source: &Path, _metadata: &std::fs::Metadata) -> Result<HostMountIdentity> {
    let identity = crate::windows_file::path_identity_no_follow(source).map_err(|error| {
        BoxError::ConfigError(format!(
            "host mount policy cannot identify {}: {error}",
            source.display()
        ))
    })?;
    Ok(HostMountIdentity::Windows {
        volume_serial: identity.volume_serial_number,
        file_index: identity.file_id,
    })
}

#[cfg(unix)]
fn source_kind(file_type: FileType) -> HostMountSourceKind {
    use std::os::unix::fs::FileTypeExt;

    if file_type.is_file() {
        HostMountSourceKind::File
    } else if file_type.is_dir() {
        HostMountSourceKind::Directory
    } else if file_type.is_socket() {
        HostMountSourceKind::UnixSocket
    } else if file_type.is_fifo() {
        HostMountSourceKind::Fifo
    } else if file_type.is_char_device() {
        HostMountSourceKind::CharacterDevice
    } else if file_type.is_block_device() {
        HostMountSourceKind::BlockDevice
    } else {
        HostMountSourceKind::OtherSpecial
    }
}

#[cfg(not(unix))]
fn source_kind(file_type: FileType) -> HostMountSourceKind {
    if file_type.is_file() {
        HostMountSourceKind::File
    } else if file_type.is_dir() {
        HostMountSourceKind::Directory
    } else {
        HostMountSourceKind::OtherSpecial
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn evaluator(home: &Path, policy: HostMountPolicy) -> HostMountPolicyEvaluator {
        HostMountPolicyEvaluator::with_home_dir(&policy, Some(home.to_path_buf())).unwrap()
    }

    #[test]
    fn safe_project_directory_is_allowed() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("home");
        let project = fixture.path().join("workspace/project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("README.md"), b"safe").unwrap();

        let assessment = evaluator(&home, HostMountPolicy::agent_safe())
            .evaluate(&project)
            .unwrap();

        assert_eq!(assessment.outcome, HostMountOutcome::Allow);
        assert!(assessment.findings.is_empty());
        assessment.ensure_allowed().unwrap();
    }

    #[test]
    fn host_bind_parser_is_typed_normalized_and_windows_safe() {
        let mount = HostBindMount::parse(r"C:\Users\agent:/workspace//src/:ro").unwrap();
        assert_eq!(mount.source, PathBuf::from(r"C:\Users\agent"));
        assert_eq!(mount.destination, "/workspace/src");
        assert!(mount.read_only);

        for invalid in ["missing-separator", ":/workspace", "/tmp:relative"] {
            assert!(
                HostBindMount::parse(invalid).is_err(),
                "accepted {invalid:?}"
            );
        }
        assert!(HostBindMount::parse("/tmp:/workspace:invalid").is_err());
        assert!(HostBindMount::parse("/tmp:/workspace/../secret").is_err());
    }

    #[test]
    fn filesystem_root_and_broad_home_are_never_carved_out() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("home/user");
        std::fs::create_dir_all(&home).unwrap();
        let broad_home = home.parent().unwrap();
        let policy = HostMountPolicy::agent_safe()
            .allow_path(broad_home)
            .allow_path(Path::new("/"));
        let evaluator = evaluator(&home, policy);

        let root = evaluator.evaluate(Path::new("/")).unwrap();
        assert_eq!(root.outcome, HostMountOutcome::Deny);
        assert!(root
            .findings
            .iter()
            .any(|finding| finding.risk == HostMountRisk::HostRoot));

        let home = evaluator.evaluate(broad_home).unwrap();
        assert_eq!(home.outcome, HostMountOutcome::Deny);
        assert!(home
            .findings
            .iter()
            .any(|finding| finding.risk == HostMountRisk::BroadHome));
    }

    #[test]
    fn direct_credential_requires_an_exact_path_exception() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("home");
        let credentials = home.join(".ssh");
        std::fs::create_dir_all(&credentials).unwrap();

        let denied = evaluator(&home, HostMountPolicy::agent_safe())
            .evaluate(&credentials)
            .unwrap();
        assert_eq!(denied.outcome, HostMountOutcome::Deny);

        let allowed = evaluator(
            &home,
            HostMountPolicy::agent_safe().allow_path(&credentials),
        )
        .evaluate(&credentials)
        .unwrap();
        assert_eq!(allowed.outcome, HostMountOutcome::Allow);
        assert_eq!(
            allowed.findings[0].authorization,
            Some(HostMountAuthorization::ExactPath)
        );
    }

    #[test]
    fn protected_descendant_cannot_be_carved_out_of_parent_mount() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("home");
        let project = fixture.path().join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join(".env.local"), b"TOKEN=secret").unwrap();
        let policy = HostMountPolicy::agent_safe().allow_path(&project);

        let assessment = evaluator(&home, policy).evaluate(&project).unwrap();

        assert_eq!(assessment.outcome, HostMountOutcome::Deny);
        assert!(assessment.findings.iter().any(|finding| {
            finding.risk == HostMountRisk::Credential
                && finding.evidence_path.ends_with(".env.local")
                && finding.authorization.is_none()
        }));
    }

    #[test]
    fn audit_mode_reports_without_enforcing() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("home");
        let project = fixture.path().join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(project.join(".aws")).unwrap();

        let assessment = evaluator(&home, HostMountPolicy::agent_safe().audit_only())
            .evaluate(&project)
            .unwrap();

        assert_eq!(assessment.outcome, HostMountOutcome::Audit);
        assessment.ensure_allowed().unwrap();
    }

    #[cfg(unix)]
    #[test]
    fn host_control_socket_requires_its_distinct_authorization() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("home");
        let socket = fixture.path().join("docker.sock");
        std::fs::create_dir_all(&home).unwrap();
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();

        let generic = evaluator(&home, HostMountPolicy::agent_safe().allow_path(&socket))
            .evaluate(&socket)
            .unwrap();
        assert_eq!(generic.outcome, HostMountOutcome::Deny);
        assert_eq!(generic.findings[0].risk, HostMountRisk::HostControlSocket);
        assert!(generic.findings[0].authorization.is_none());

        let explicit = evaluator(
            &home,
            HostMountPolicy::agent_safe().allow_host_control_socket(&socket),
        )
        .evaluate(&socket)
        .unwrap();
        assert_eq!(explicit.outcome, HostMountOutcome::Allow);
        assert_eq!(
            explicit.findings[0].authorization,
            Some(HostMountAuthorization::HostControlSocket)
        );
    }

    #[cfg(unix)]
    #[test]
    fn ordinary_socket_fifo_and_character_device_are_classified() {
        use std::ffi::CString;
        use std::os::unix::ffi::OsStrExt;

        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("home");
        let socket = fixture.path().join("service.sock");
        let fifo = fixture.path().join("events.fifo");
        std::fs::create_dir_all(&home).unwrap();
        let _listener = std::os::unix::net::UnixListener::bind(&socket).unwrap();
        let fifo_path = CString::new(fifo.as_os_str().as_bytes()).unwrap();
        // SAFETY: `fifo_path` is a valid NUL-terminated path and the mode is
        // restricted to the current user for this temporary test directory.
        assert_eq!(unsafe { libc::mkfifo(fifo_path.as_ptr(), 0o600) }, 0);
        let evaluator = evaluator(&home, HostMountPolicy::agent_safe());

        assert_eq!(
            evaluator.evaluate(&socket).unwrap().findings[0].risk,
            HostMountRisk::UnixSocket
        );
        assert_eq!(
            evaluator.evaluate(&fifo).unwrap().findings[0].risk,
            HostMountRisk::Fifo
        );
        assert_eq!(
            evaluator.evaluate(Path::new("/dev/null")).unwrap().findings[0].risk,
            HostMountRisk::CharacterDevice
        );
    }

    #[test]
    fn bounded_scan_fails_closed() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("home");
        let project = fixture.path().join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        std::fs::write(project.join("one"), b"1").unwrap();
        std::fs::write(project.join("two"), b"2").unwrap();
        let evaluator = evaluator(&home, HostMountPolicy::agent_safe()).with_scan_limits(1, 32);

        let error = evaluator.evaluate(&project).unwrap_err().to_string();
        assert!(error.contains("exceeded 1 entries"));
    }

    #[test]
    fn replacement_deletion_and_new_protected_descendant_fail_revalidation() {
        let fixture = tempfile::tempdir().unwrap();
        let home = fixture.path().join("home");
        let project = fixture.path().join("project");
        std::fs::create_dir_all(&home).unwrap();
        std::fs::create_dir_all(&project).unwrap();
        let evaluator = evaluator(&home, HostMountPolicy::agent_safe());
        let planned = evaluator.evaluate(&project).unwrap();

        let replaced = fixture.path().join("replaced");
        std::fs::rename(&project, &replaced).unwrap();
        std::fs::create_dir(&project).unwrap();
        let error = evaluator.revalidate(&planned).unwrap_err().to_string();
        assert!(error.contains("identity changed"));

        let deleted = fixture.path().join("deleted-project");
        std::fs::create_dir(&deleted).unwrap();
        let planned = evaluator.evaluate(&deleted).unwrap();
        std::fs::remove_dir(&deleted).unwrap();
        let error = evaluator.revalidate(&planned).unwrap_err().to_string();
        assert!(error.contains("cannot resolve"));

        let other = fixture.path().join("other-project");
        std::fs::create_dir(&other).unwrap();
        let planned = evaluator.evaluate(&other).unwrap();
        std::fs::write(other.join(".env"), b"TOKEN=secret").unwrap();
        let error = evaluator.revalidate(&planned).unwrap_err().to_string();
        assert!(error.contains("classification changed"));
    }
}
