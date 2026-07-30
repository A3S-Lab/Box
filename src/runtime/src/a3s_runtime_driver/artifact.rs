//! Runtime Artifact, persistent Volume, and Task-output storage bindings.

use std::path::{Component, Path, PathBuf};
use std::sync::Arc;

use a3s_box_core::CreateExecutionRequest;
use a3s_runtime::contract::{
    RuntimeMount, RuntimeMountSource, RuntimeOutputArtifact, RuntimeOutputSpec, RuntimeUnitSpec,
    SecretTarget,
};
use a3s_runtime::{RuntimeError, RuntimeResult};
use async_trait::async_trait;

use crate::BoxRecord;

use super::volume_storage::{
    cleanup_output_volumes, require_output_volume, reset_output_volumes, resolve_output_volume,
    resolve_persistent_volume,
};

/// Caller-owned Artifact boundary composed into the shared Box Runtime driver.
///
/// Box accepts only shared A3S Runtime contract types. The caller remains the
/// authority for authenticated Artifact transport, admission, hashing, and
/// publication. Box owns only local mount wiring, execution fencing, and the
/// lifecycle of its existing VolumeStore entries.
#[async_trait]
pub trait BoxArtifactPort: Send + Sync {
    /// Return the already materialized, read-only host directory for one input.
    async fn mount_path(
        &self,
        spec: &RuntimeUnitSpec,
        mount: &RuntimeMount,
    ) -> Result<PathBuf, BoxArtifactPortError>;

    /// Admit one quiescent Task-output directory into the caller's Artifact store.
    async fn capture_output(
        &self,
        spec: &RuntimeUnitSpec,
        output: &RuntimeOutputSpec,
        source: &Path,
    ) -> Result<RuntimeOutputArtifact, BoxArtifactPortError>;

    /// Release caller-owned materialization views for one immutable spec.
    async fn cleanup_spec(&self, spec_digest: &str) -> Result<(), BoxArtifactPortError>;
}

/// Stable, non-sensitive failure categories for caller-provided Artifact ports.
#[derive(Debug, thiserror::Error)]
pub enum BoxArtifactPortError {
    #[error("Artifact request was rejected: {0}")]
    Rejected(String),
    #[error("Artifact boundary is temporarily unavailable: {0}")]
    Unavailable(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct RuntimeStoragePlan {
    spec_digest: String,
    volumes: Vec<String>,
    volume_names: Vec<String>,
}

impl RuntimeStoragePlan {
    pub(super) fn empty(spec: &RuntimeUnitSpec) -> RuntimeResult<Self> {
        Ok(Self {
            spec_digest: spec.digest().map_err(RuntimeError::InvalidRequest)?,
            volumes: Vec::new(),
            volume_names: Vec::new(),
        })
    }

    pub(super) fn from_request(
        spec: &RuntimeUnitSpec,
        request: &CreateExecutionRequest,
    ) -> RuntimeResult<Self> {
        let secret_mounts = spec
            .secrets
            .iter()
            .filter(|secret| !matches!(secret.target, SecretTarget::RegistryCredential))
            .count();
        let storage_mounts = request
            .config
            .volumes
            .len()
            .checked_sub(secret_mounts)
            .ok_or_else(|| {
                RuntimeError::Protocol("Box creation request omitted a Runtime Secret mount".into())
            })?;
        Ok(Self {
            spec_digest: spec.digest().map_err(RuntimeError::Protocol)?,
            volumes: request.config.volumes[..storage_mounts].to_vec(),
            volume_names: request.policy.volume_names.clone(),
        })
    }

    pub(super) fn volumes(&self) -> &[String] {
        &self.volumes
    }

    pub(super) fn volume_names(&self) -> &[String] {
        &self.volume_names
    }

    pub(super) fn validate_for(&self, spec: &RuntimeUnitSpec) -> RuntimeResult<()> {
        let digest = spec.digest().map_err(RuntimeError::InvalidRequest)?;
        if self.spec_digest != digest {
            return Err(RuntimeError::Protocol(
                "Box Runtime storage plan belongs to another specification".into(),
            ));
        }
        Ok(())
    }
}

#[derive(Clone)]
pub(super) struct ArtifactStorageOwner {
    home_dir: PathBuf,
    port: Option<Arc<dyn BoxArtifactPort>>,
}

impl ArtifactStorageOwner {
    pub(super) fn new(home_dir: PathBuf, port: Option<Arc<dyn BoxArtifactPort>>) -> Self {
        Self { home_dir, port }
    }

    pub(super) fn artifact_configured(&self) -> bool {
        self.port.is_some()
    }

    pub(super) fn require_configured_for(&self, spec: &RuntimeUnitSpec) -> RuntimeResult<()> {
        let needs_artifacts = spec
            .mounts
            .iter()
            .any(|mount| matches!(mount.source, RuntimeMountSource::Artifact { .. }));
        let mut missing = Vec::new();
        if needs_artifacts && self.port.is_none() {
            missing.push("mount_kind:Artifact".into());
        }
        if !spec.outputs.is_empty() && self.port.is_none() {
            missing.push("feature:OutputArtifacts".into());
        }
        if missing.is_empty() {
            Ok(())
        } else {
            Err(RuntimeError::UnsupportedCapabilities(missing))
        }
    }

    pub(super) async fn prepare_plan(
        &self,
        spec: &RuntimeUnitSpec,
    ) -> RuntimeResult<RuntimeStoragePlan> {
        self.build_plan(spec, true).await
    }

    pub(super) async fn require_plan(
        &self,
        spec: &RuntimeUnitSpec,
    ) -> RuntimeResult<RuntimeStoragePlan> {
        self.build_plan(spec, false).await
    }

    pub(super) async fn validate_record(
        &self,
        spec: &RuntimeUnitSpec,
        record: &BoxRecord,
    ) -> RuntimeResult<()> {
        let request = &record
            .managed_execution
            .as_ref()
            .ok_or_else(|| RuntimeError::Protocol("Box execution lost metadata".into()))?
            .request;
        let actual = RuntimeStoragePlan::from_request(spec, request)?;
        let expected = self.require_plan(spec).await?;
        if actual != expected {
            return Err(RuntimeError::Protocol(format!(
                "Box execution {} storage bindings do not match the Runtime specification",
                record.id
            )));
        }
        Ok(())
    }

    pub(super) async fn reset_outputs_for_start(
        &self,
        spec: &RuntimeUnitSpec,
    ) -> RuntimeResult<()> {
        if spec.outputs.is_empty() {
            return Ok(());
        }
        let home_dir = self.home_dir.clone();
        let spec = spec.clone();
        tokio::task::spawn_blocking(move || reset_output_volumes(&home_dir, &spec))
            .await
            .map_err(|error| {
                RuntimeError::ProviderUnavailable(format!(
                    "Box Task-output reset task failed: {error}"
                ))
            })?
    }

    pub(super) async fn capture_outputs(
        &self,
        spec: &RuntimeUnitSpec,
    ) -> RuntimeResult<Vec<RuntimeOutputArtifact>> {
        if spec.outputs.is_empty() {
            return Ok(Vec::new());
        }
        let port = self.port.as_ref().ok_or_else(|| {
            RuntimeError::UnsupportedCapabilities(vec!["feature:OutputArtifacts".into()])
        })?;
        let home_dir = self.home_dir.clone();
        let spec_for_paths = spec.clone();
        let sources = tokio::task::spawn_blocking(move || {
            spec_for_paths
                .outputs
                .iter()
                .map(|output| require_output_volume(&home_dir, &spec_for_paths, output))
                .collect::<RuntimeResult<Vec<_>>>()
        })
        .await
        .map_err(|error| {
            RuntimeError::ProviderUnavailable(format!(
                "Box Task-output lookup task failed: {error}"
            ))
        })??;

        let mut captured = Vec::with_capacity(spec.outputs.len());
        for (output, source) in spec.outputs.iter().zip(sources) {
            let artifact = port
                .capture_output(spec, output, &source)
                .await
                .map_err(map_port_error)?;
            validate_captured_output(output, &artifact)?;
            captured.push(artifact);
        }
        Ok(captured)
    }

    pub(super) async fn cleanup_spec(&self, spec: &RuntimeUnitSpec) -> RuntimeResult<()> {
        let digest = spec.digest().map_err(RuntimeError::InvalidRequest)?;
        self.cleanup_digest(&digest).await
    }

    pub(super) async fn cleanup_digest(&self, digest: &str) -> RuntimeResult<()> {
        validate_digest(digest)?;
        if let Some(port) = &self.port {
            port.cleanup_spec(digest).await.map_err(map_port_error)?;
        }
        let home_dir = self.home_dir.clone();
        let digest = digest.to_owned();
        tokio::task::spawn_blocking(move || cleanup_output_volumes(&home_dir, &digest))
            .await
            .map_err(|error| {
                RuntimeError::ProviderUnavailable(format!(
                    "Box Task-output cleanup task failed: {error}"
                ))
            })?
    }

    async fn build_plan(
        &self,
        spec: &RuntimeUnitSpec,
        create_volumes: bool,
    ) -> RuntimeResult<RuntimeStoragePlan> {
        spec.validate().map_err(RuntimeError::InvalidRequest)?;
        self.require_configured_for(spec)?;
        validate_storage_targets(spec)?;

        let mut plan = RuntimeStoragePlan::empty(spec)?;
        for mount in &spec.mounts {
            match &mount.source {
                RuntimeMountSource::Artifact { .. } => {
                    if !mount.read_only {
                        return Err(RuntimeError::InvalidRequest(
                            "Box Runtime Artifact mounts must be read-only".into(),
                        ));
                    }
                    let port = self.port.as_ref().ok_or_else(|| {
                        RuntimeError::UnsupportedCapabilities(vec!["mount_kind:Artifact".into()])
                    })?;
                    let source = port.mount_path(spec, mount).await.map_err(map_port_error)?;
                    let source = validate_artifact_mount_path(source).await?;
                    plan.volumes.push(bind_mount(&source, &mount.target, true)?);
                }
                RuntimeMountSource::Volume { volume_id } => {
                    let home_dir = self.home_dir.clone();
                    let volume_id = volume_id.clone();
                    let resolved = tokio::task::spawn_blocking(move || {
                        resolve_persistent_volume(&home_dir, &volume_id, create_volumes)
                    })
                    .await
                    .map_err(|error| {
                        RuntimeError::ProviderUnavailable(format!(
                            "Box persistent-Volume lookup task failed: {error}"
                        ))
                    })??;
                    plan.volumes
                        .push(bind_mount(&resolved.path, &mount.target, mount.read_only)?);
                    plan.volume_names.push(resolved.name);
                }
                RuntimeMountSource::Tmpfs { .. } => {}
            }
        }

        for output in &spec.outputs {
            let home_dir = self.home_dir.clone();
            let spec = spec.clone();
            let output = output.clone();
            let output_for_lookup = output.clone();
            let resolved = tokio::task::spawn_blocking(move || {
                resolve_output_volume(&home_dir, &spec, &output_for_lookup, create_volumes)
            })
            .await
            .map_err(|error| {
                RuntimeError::ProviderUnavailable(format!(
                    "Box Task-output Volume lookup task failed: {error}"
                ))
            })??;
            plan.volumes
                .push(bind_mount(&resolved.path, &output.path, false)?);
            plan.volume_names.push(resolved.name);
        }
        Ok(plan)
    }
}

async fn validate_artifact_mount_path(path: PathBuf) -> RuntimeResult<PathBuf> {
    let display = path.to_str().ok_or_else(|| {
        RuntimeError::InvalidRequest("Box Artifact mount path is not UTF-8".into())
    })?;
    if !path.is_absolute()
        || display.contains([':', '\0'])
        || display.bytes().any(|byte| byte.is_ascii_control())
        || path.components().any(|component| {
            matches!(
                component,
                Component::CurDir | Component::ParentDir | Component::Prefix(_)
            )
        })
    {
        return Err(RuntimeError::InvalidRequest(
            "Box Artifact mount path must be an encodable normalized absolute path".into(),
        ));
    }
    let metadata = tokio::fs::symlink_metadata(&path)
        .await
        .map_err(artifact_io_error)?;
    let canonical = tokio::fs::canonicalize(&path)
        .await
        .map_err(artifact_io_error)?;
    if metadata.file_type().is_symlink() || !metadata.file_type().is_dir() || canonical != path {
        return Err(RuntimeError::InvalidRequest(
            "Box Artifact mount source must be a canonical plain directory".into(),
        ));
    }
    Ok(path)
}

fn bind_mount(source: &Path, target: &str, read_only: bool) -> RuntimeResult<String> {
    let source = source.to_str().ok_or_else(|| {
        RuntimeError::InvalidRequest("Box Runtime mount source is not UTF-8".into())
    })?;
    if source.contains([':', '\0']) || source.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(RuntimeError::InvalidRequest(
            "Box Runtime mount source cannot be encoded".into(),
        ));
    }
    Ok(format!(
        "{source}:{target}:{}",
        if read_only { "ro" } else { "rw" }
    ))
}

fn validate_storage_targets(spec: &RuntimeUnitSpec) -> RuntimeResult<()> {
    let mut targets = Vec::new();
    for mount in &spec.mounts {
        validate_target(
            &mount.target,
            matches!(mount.source, RuntimeMountSource::Tmpfs { .. }),
        )?;
        targets.push(PathBuf::from(&mount.target));
    }
    for output in &spec.outputs {
        validate_target(&output.path, false)?;
        targets.push(PathBuf::from(&output.path));
    }
    for secret in &spec.secrets {
        if let SecretTarget::File { path, .. } = &secret.target {
            targets.push(PathBuf::from(path));
        }
    }
    for (index, target) in targets.iter().enumerate() {
        if targets
            .iter()
            .skip(index + 1)
            .any(|other| paths_overlap(target, other))
        {
            return Err(RuntimeError::InvalidRequest(
                "Box Runtime mount, output, and Secret targets must not overlap".into(),
            ));
        }
    }
    Ok(())
}

fn validate_target(target: &str, tmpfs: bool) -> RuntimeResult<()> {
    let path = Path::new(target);
    let normalized = target.strip_prefix('/').is_some_and(|relative| {
        !relative.is_empty()
            && !relative.ends_with('/')
            && relative
                .split('/')
                .all(|segment| !segment.is_empty() && !matches!(segment, "." | ".."))
    });
    let is_or_below = |root: &Path| {
        path == root
            || path
                .strip_prefix(root)
                .is_ok_and(|suffix| !suffix.as_os_str().is_empty())
    };
    let protected = path == Path::new("/")
        || is_or_below(Path::new("/proc"))
        || is_or_below(Path::new("/sys"))
        || (is_or_below(Path::new("/dev")) && !(tmpfs && path == Path::new("/dev/shm")))
        || is_or_below(Path::new("/run/a3s-box"))
        || is_or_below(Path::new("/.a3s-box-secrets"));
    if !normalized
        || target.contains([':', '\0'])
        || target.bytes().any(|byte| byte.is_ascii_control())
        || protected
    {
        return Err(RuntimeError::InvalidRequest(format!(
            "Box Runtime mount target must be an encodable normalized unprotected absolute path: {target:?}"
        )));
    }
    Ok(())
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn validate_captured_output(
    expected: &RuntimeOutputSpec,
    actual: &RuntimeOutputArtifact,
) -> RuntimeResult<()> {
    actual.artifact.validate().map_err(RuntimeError::Protocol)?;
    if actual.name != expected.name
        || actual.artifact.media_type != expected.media_type
        || actual.size_bytes == 0
        || actual.size_bytes > expected.max_bytes
    {
        return Err(RuntimeError::Protocol(
            "Box Artifact port returned an output outside its Runtime declaration".into(),
        ));
    }
    Ok(())
}

fn validate_digest(digest: &str) -> RuntimeResult<()> {
    let valid = digest.strip_prefix("sha256:").is_some_and(|hex| {
        hex.len() == 64
            && hex
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
    });
    if valid {
        Ok(())
    } else {
        Err(RuntimeError::Protocol(
            "Box Artifact cleanup requires a lowercase SHA-256 spec digest".into(),
        ))
    }
}

fn map_port_error(error: BoxArtifactPortError) -> RuntimeError {
    match error {
        BoxArtifactPortError::Rejected(_) => RuntimeError::InvalidRequest(
            "Box Artifact request was rejected by the caller boundary".into(),
        ),
        BoxArtifactPortError::Unavailable(_) => RuntimeError::ProviderUnavailable(
            "Box Artifact boundary is temporarily unavailable".into(),
        ),
    }
}

fn artifact_io_error(error: std::io::Error) -> RuntimeError {
    RuntimeError::ProviderUnavailable(format!("Box Artifact mount I/O failed: {error}"))
}
