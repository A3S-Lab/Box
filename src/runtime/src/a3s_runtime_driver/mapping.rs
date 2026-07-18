//! Lossless Runtime protocol to Box Sandbox creation mapping.

use std::collections::BTreeMap;
use std::path::Path;

use a3s_box_core::log::LogConfig;
use a3s_box_core::{
    BoxConfig, CreateExecutionRequest, ExecutionIsolation, ExecutionRecordPolicy,
    ExecutionRestartPolicy, NetworkMode, ResourceConfig, ResourceLimits,
};
use a3s_runtime::contract::{
    ArtifactRef, MountKind, RestartPolicy, RuntimeMountSource, RuntimeUnitClass, RuntimeUnitSpec,
};
use a3s_runtime::{RuntimeError, RuntimeResult};
use url::Position;

use super::metadata::{managed_labels, operation_id};
use super::{DOCKER_IMAGE_MANIFEST, OCI_IMAGE_MANIFEST};

const CPU_PERIOD_US: u64 = 100_000;
const BYTES_PER_MIB: u64 = 1024 * 1024;

pub(super) fn creation_request(spec: &RuntimeUnitSpec) -> RuntimeResult<CreateExecutionRequest> {
    spec.validate().map_err(RuntimeError::InvalidRequest)?;
    validate_provider_unit_id(&spec.unit_id)?;
    validate_supported_shape(spec)?;
    let spec_digest = spec.digest().map_err(RuntimeError::InvalidRequest)?;
    let memory_mb = spec.resources.memory_bytes.div_ceil(BYTES_PER_MIB);
    let memory_mb = u32::try_from(memory_mb).map_err(|_| {
        RuntimeError::InvalidRequest("Box Sandbox memory limit exceeds u32 MiB metadata".into())
    })?;
    let vcpus = u32::try_from(spec.resources.cpu_millis.div_ceil(1_000)).map_err(|_| {
        RuntimeError::InvalidRequest("Box Sandbox CPU limit exceeds u32 vCPUs".into())
    })?;
    let cpu_quota = spec
        .resources
        .cpu_millis
        .checked_mul(CPU_PERIOD_US / 1_000)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| {
            RuntimeError::InvalidRequest("Box Sandbox CPU quota overflows i64".into())
        })?;
    let memory_swap = i64::try_from(spec.resources.memory_bytes).map_err(|_| {
        RuntimeError::InvalidRequest("Box Sandbox memory limit overflows i64".into())
    })?;
    let task_timeout_secs = spec
        .resources
        .execution_timeout_ms
        .map(|milliseconds| milliseconds.div_ceil(1_000));
    let (entrypoint_override, cmd) = if spec.process.command.is_empty() {
        (None, spec.process.args.clone())
    } else {
        (
            Some(spec.process.command.clone()),
            spec.process.args.clone(),
        )
    };
    let tmpfs = compile_tmpfs_mounts(spec)?;

    let config = BoxConfig {
        image: image_reference(&spec.artifact)?,
        isolation: ExecutionIsolation::Sandbox,
        resources: ResourceConfig {
            vcpus,
            memory_mb,
            disk_mb: BoxConfig::default().resources.disk_mb,
            timeout: task_timeout_secs.unwrap_or(0),
        },
        cmd,
        entrypoint_override,
        workdir: spec.process.working_directory.clone(),
        extra_env: spec
            .process
            .environment
            .iter()
            .map(|(key, value)| (key.clone(), value.clone()))
            .collect(),
        network: NetworkMode::None,
        tmpfs,
        resource_limits: ResourceLimits {
            pids_limit: Some(u64::from(spec.resources.pids)),
            cpu_quota: Some(cpu_quota),
            cpu_period: Some(CPU_PERIOD_US),
            memory_swap: Some(memory_swap),
            sandbox_memory_limit_bytes: Some(spec.resources.memory_bytes),
            ..Default::default()
        },
        persistent: true,
        cap_drop: vec!["ALL".into()],
        security_opt: vec!["no-new-privileges".into()],
        ..Default::default()
    };

    Ok(CreateExecutionRequest {
        external_sandbox_id: format!("{}:{}", spec.unit_id, spec.generation),
        config,
        labels: managed_labels(spec, &spec_digest),
        policy: ExecutionRecordPolicy {
            auto_remove: false,
            restart_policy: ExecutionRestartPolicy::No,
            log_config: LogConfig::default(),
            init: true,
            ..Default::default()
        },
        rootfs_snapshot_id: None,
    })
}

pub(super) fn operation(spec: &RuntimeUnitSpec) -> RuntimeResult<a3s_box_core::OperationId> {
    operation_id(
        &spec.unit_id,
        spec.generation,
        &spec.digest().map_err(RuntimeError::InvalidRequest)?,
    )
}

fn validate_supported_shape(spec: &RuntimeUnitSpec) -> RuntimeResult<()> {
    if !matches!(
        spec.artifact.media_type.as_str(),
        OCI_IMAGE_MANIFEST | DOCKER_IMAGE_MANIFEST
    ) {
        return Err(RuntimeError::UnsupportedCapabilities(vec![format!(
            "artifact_media_type:{}",
            spec.artifact.media_type
        )]));
    }
    if spec.isolation != a3s_runtime::contract::IsolationLevel::Sandbox {
        return Err(RuntimeError::UnsupportedCapabilities(vec![format!(
            "isolation:{:?}",
            spec.isolation
        )]));
    }
    if spec.network.mode != a3s_runtime::contract::NetworkMode::None {
        return Err(RuntimeError::UnsupportedCapabilities(vec![format!(
            "network_mode:{:?}",
            spec.network.mode
        )]));
    }
    let unsupported_mount_kinds = spec
        .mounts
        .iter()
        .map(|mount| mount.source.kind())
        .filter(|kind| *kind != MountKind::Tmpfs)
        .collect::<std::collections::BTreeSet<_>>();
    if !unsupported_mount_kinds.is_empty() {
        return Err(RuntimeError::UnsupportedCapabilities(
            unsupported_mount_kinds
                .into_iter()
                .map(|kind| format!("mount_kind:{kind:?}"))
                .collect(),
        ));
    }
    if spec.health.is_some() {
        return Err(RuntimeError::UnsupportedCapabilities(vec![
            "health checks are not supported by the Box Runtime driver".into(),
        ]));
    }
    if !spec.secrets.is_empty() {
        return Err(RuntimeError::UnsupportedCapabilities(vec![
            "feature:SecretReferences".into(),
        ]));
    }
    if !spec.outputs.is_empty() {
        return Err(RuntimeError::UnsupportedCapabilities(vec![
            "feature:OutputArtifacts".into(),
        ]));
    }
    if spec.resources.ephemeral_storage_bytes.is_some() {
        return Err(RuntimeError::UnsupportedCapabilities(vec![
            "resource_control:EphemeralStorage".into(),
        ]));
    }
    match (&spec.class, &spec.restart) {
        (RuntimeUnitClass::Task, RestartPolicy::Never | RestartPolicy::OnFailure { .. })
        | (RuntimeUnitClass::Service, _) => Ok(()),
        (RuntimeUnitClass::Task, RestartPolicy::Always) => Err(RuntimeError::InvalidRequest(
            "Runtime Tasks cannot use an always restart policy".into(),
        )),
    }
}

fn validate_provider_unit_id(unit_id: &str) -> RuntimeResult<()> {
    if unit_id
        .split(['/', '\\'])
        .any(|segment| segment.is_empty() || matches!(segment, "." | ".."))
    {
        return Err(RuntimeError::InvalidRequest(format!(
            "Box Runtime unit identity must not contain path traversal: {unit_id:?}"
        )));
    }
    Ok(())
}

fn compile_tmpfs_mounts(spec: &RuntimeUnitSpec) -> RuntimeResult<Vec<String>> {
    spec.mounts
        .iter()
        .map(|mount| {
            let RuntimeMountSource::Tmpfs { size_bytes } = &mount.source else {
                return Err(RuntimeError::Protocol(
                    "Box tmpfs compilation received an unsupported mount kind".into(),
                ));
            };
            validate_tmpfs_target(&mount.target)?;
            Ok(format!(
                "{}:size={size_bytes},{}",
                mount.target,
                if mount.read_only { "ro" } else { "rw" }
            ))
        })
        .collect()
}

fn validate_tmpfs_target(target: &str) -> RuntimeResult<()> {
    let path = Path::new(target);
    let normalized = target.strip_prefix('/').is_some_and(|relative| {
        !relative.is_empty()
            && relative
                .split('/')
                .all(|segment| !segment.is_empty() && !matches!(segment, "." | ".."))
    });
    if !normalized || target.contains(':') {
        return Err(RuntimeError::InvalidRequest(format!(
            "Box Sandbox tmpfs target must be an encodable normalized absolute path: {target:?}"
        )));
    }
    let is_or_below = |root: &Path| {
        path == root
            || path
                .strip_prefix(root)
                .is_ok_and(|suffix| !suffix.as_os_str().is_empty())
    };
    let protected = path == Path::new("/")
        || is_or_below(Path::new("/proc"))
        || is_or_below(Path::new("/sys"))
        || (is_or_below(Path::new("/dev")) && path != Path::new("/dev/shm"))
        || is_or_below(Path::new("/run/a3s-box"));
    if protected {
        return Err(RuntimeError::InvalidRequest(format!(
            "Box Sandbox tmpfs target is protected: {target:?}"
        )));
    }
    Ok(())
}

fn image_reference(artifact: &ArtifactRef) -> RuntimeResult<String> {
    artifact.validate().map_err(RuntimeError::InvalidRequest)?;
    let parsed = url::Url::parse(&artifact.uri)
        .map_err(|error| RuntimeError::InvalidRequest(error.to_string()))?;
    if parsed.scheme() != "oci"
        || !parsed.username().is_empty()
        || parsed.password().is_some()
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || parsed.path().contains('%')
    {
        return Err(RuntimeError::InvalidRequest(
            "Box artifacts require a credential-free canonical oci:// URI".into(),
        ));
    }
    let authority = &parsed[Position::BeforeHost..Position::AfterPort];
    if authority.is_empty() || parsed.path() == "/" {
        return Err(RuntimeError::InvalidRequest(
            "Box artifact URI requires a registry and repository path".into(),
        ));
    }
    let image = format!("{authority}{}", parsed.path());
    let expected_suffix = format!("@{}", artifact.digest);
    if !image.ends_with(&expected_suffix) || image.matches('@').count() != 1 {
        return Err(RuntimeError::InvalidRequest(
            "Box artifact URI must end with its authoritative digest".into(),
        ));
    }
    Ok(image)
}

pub(super) fn labels_as_hash_map(
    labels: &BTreeMap<String, String>,
) -> std::collections::HashMap<String, String> {
    labels
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}
