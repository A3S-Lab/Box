//! Lossless Runtime protocol to Box execution creation mapping.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use a3s_box_core::config::TeeConfig;
use a3s_box_core::log::LogConfig;
use a3s_box_core::secret::{
    SecretEnvironmentBinding, SECRET_ENVIRONMENT_MANIFEST, SECRET_GUEST_ROOT,
};
use a3s_box_core::tee::RUNTIME_ATTESTATION_BINDING_ENV;
use a3s_box_core::{
    BoxConfig, CreateExecutionRequest, ExecutionIsolation, ExecutionRecordPolicy,
    ExecutionRestartPolicy, NetworkMode, ResourceConfig, ResourceLimits,
};
use a3s_runtime::contract::{
    ArtifactRef, NetworkMode as RuntimeNetworkMode, RestartPolicy, RuntimeMountSource,
    RuntimeUnitClass, RuntimeUnitSpec, SecretTarget, TransportProtocol,
};
use a3s_runtime::{RuntimeError, RuntimeResult};
use url::Position;

use super::artifact::RuntimeStoragePlan;
use super::metadata::{managed_labels, operation_id};
use super::secret::secret_file;
use super::{BoxRuntimeSevSnpConfig, OCI_IMAGE_INDEX, OCI_IMAGE_MANIFEST};

const CPU_PERIOD_US: u64 = 100_000;
const BYTES_PER_MIB: u64 = 1024 * 1024;
pub(super) fn creation_request_for(
    spec: &RuntimeUnitSpec,
    execution_isolation: ExecutionIsolation,
    sev_snp: Option<&BoxRuntimeSevSnpConfig>,
    secret_root: &Path,
    storage: &RuntimeStoragePlan,
) -> RuntimeResult<CreateExecutionRequest> {
    spec.validate().map_err(RuntimeError::InvalidRequest)?;
    storage.validate_for(spec)?;
    validate_provider_unit_id(&spec.unit_id)?;
    validate_supported_shape(spec, execution_isolation, sev_snp)?;
    let spec_digest = spec.digest().map_err(RuntimeError::InvalidRequest)?;
    let memory_mb = spec.resources.memory_bytes.div_ceil(BYTES_PER_MIB);
    let memory_mb = u32::try_from(memory_mb).map_err(|_| {
        RuntimeError::InvalidRequest("Box execution memory limit exceeds u32 MiB metadata".into())
    })?;
    let vcpus = u32::try_from(spec.resources.cpu_millis.div_ceil(1_000)).map_err(|_| {
        RuntimeError::InvalidRequest("Box execution CPU limit exceeds u32 vCPUs".into())
    })?;
    let cpu_quota = spec
        .resources
        .cpu_millis
        .checked_mul(CPU_PERIOD_US / 1_000)
        .and_then(|value| i64::try_from(value).ok())
        .ok_or_else(|| {
            RuntimeError::InvalidRequest("Box execution CPU quota overflows i64".into())
        })?;
    let memory_swap = i64::try_from(spec.resources.memory_bytes).map_err(|_| {
        RuntimeError::InvalidRequest("Box execution memory limit overflows i64".into())
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
    let (secret_volumes, secret_environment_manifest) =
        compile_secret_inputs(spec, secret_root, &spec_digest)?;
    let mut volumes = storage.volumes().to_vec();
    volumes.extend(secret_volumes);
    let mut extra_env = spec
        .process
        .environment
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect::<Vec<_>>();
    if let Some(manifest) = secret_environment_manifest {
        extra_env.push((SECRET_ENVIRONMENT_MANIFEST.into(), manifest));
    }

    let tee = match spec.isolation {
        a3s_runtime::contract::IsolationLevel::Sandbox => TeeConfig::None,
        a3s_runtime::contract::IsolationLevel::Confidential => {
            let sev_snp = sev_snp.ok_or_else(|| {
                RuntimeError::UnsupportedCapabilities(vec!["isolation:Confidential".into()])
            })?;
            let binding = spec_digest.strip_prefix("sha256:").ok_or_else(|| {
                RuntimeError::Protocol(
                    "Box confidential execution requires a SHA-256 Runtime spec digest".into(),
                )
            })?;
            extra_env.push((RUNTIME_ATTESTATION_BINDING_ENV.into(), binding.into()));
            TeeConfig::SevSnp {
                workload_id: spec.unit_id.clone(),
                generation: sev_snp.generation,
                simulate: sev_snp.simulate,
            }
        }
        _ => unreachable!("unsupported isolation was rejected before Box mapping"),
    };
    let config = BoxConfig {
        image: image_reference(&spec.artifact)?,
        isolation: execution_isolation,
        tee,
        resources: ResourceConfig {
            vcpus,
            memory_mb,
            disk_mb: BoxConfig::default().resources.disk_mb,
            timeout: task_timeout_secs.unwrap_or(0),
        },
        cmd,
        entrypoint_override,
        workdir: spec.process.working_directory.clone(),
        volumes,
        extra_env,
        network: compile_network_mode(&spec.network.mode)?,
        tmpfs,
        resource_limits: ResourceLimits {
            pids_limit: Some(u64::from(spec.resources.pids)),
            cpu_quota: Some(cpu_quota),
            cpu_period: Some(CPU_PERIOD_US),
            memory_swap: Some(memory_swap),
            sandbox_memory_limit_bytes: execution_isolation
                .is_sandbox()
                .then_some(spec.resources.memory_bytes),
            ..Default::default()
        },
        deferred_main: spec.isolation == a3s_runtime::contract::IsolationLevel::Confidential,
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
            health_check: None,
            healthcheck_disabled: true,
            log_config: LogConfig::default(),
            init: true,
            stop_signal: spec
                .service_lifecycle
                .as_ref()
                .map(|_| "SIGTERM".to_string()),
            stop_timeout: spec
                .service_lifecycle
                .as_ref()
                .map(|lifecycle| u64::from(lifecycle.shutdown_grace_seconds)),
            managed_secret_root: spec
                .secrets
                .iter()
                .any(|reference| !matches!(reference.target, SecretTarget::RegistryCredential))
                .then(|| secret_root.to_path_buf()),
            volume_names: storage.volume_names().to_vec(),
            ..Default::default()
        },
        rootfs_snapshot_id: None,
    })
}

#[cfg(test)]
pub(super) fn creation_request(
    spec: &RuntimeUnitSpec,
    execution_isolation: ExecutionIsolation,
) -> RuntimeResult<CreateExecutionRequest> {
    let storage = RuntimeStoragePlan::empty(spec)?;
    creation_request_for(
        spec,
        execution_isolation,
        None,
        Path::new("/run/a3s-box/runtime-secrets"),
        &storage,
    )
}

#[cfg(test)]
pub(super) fn creation_request_with_sev_snp(
    spec: &RuntimeUnitSpec,
    execution_isolation: ExecutionIsolation,
    sev_snp: &BoxRuntimeSevSnpConfig,
) -> RuntimeResult<CreateExecutionRequest> {
    let storage = RuntimeStoragePlan::empty(spec)?;
    creation_request_for(
        spec,
        execution_isolation,
        Some(sev_snp),
        Path::new("/run/a3s-box/runtime-secrets"),
        &storage,
    )
}

/// Validate every provider-owned creation field before storage preparation can
/// create a named Volume. The empty plan is replaced by the exact prepared
/// plan only after this side-effect-free pass succeeds.
pub(super) fn validate_creation_spec(
    spec: &RuntimeUnitSpec,
    execution_isolation: ExecutionIsolation,
    sev_snp: Option<&BoxRuntimeSevSnpConfig>,
    secret_root: &Path,
) -> RuntimeResult<()> {
    let storage = RuntimeStoragePlan::empty(spec)?;
    creation_request_for(spec, execution_isolation, sev_snp, secret_root, &storage).map(|_| ())
}

pub(super) fn operation(spec: &RuntimeUnitSpec) -> RuntimeResult<a3s_box_core::OperationId> {
    operation_id(
        &spec.unit_id,
        spec.generation,
        &spec.digest().map_err(RuntimeError::InvalidRequest)?,
    )
}

fn validate_supported_shape(
    spec: &RuntimeUnitSpec,
    execution_isolation: ExecutionIsolation,
    sev_snp: Option<&BoxRuntimeSevSnpConfig>,
) -> RuntimeResult<()> {
    if spec
        .process
        .environment
        .contains_key(RUNTIME_ATTESTATION_BINDING_ENV)
    {
        return Err(RuntimeError::InvalidRequest(format!(
            "Runtime process environment cannot use reserved Box key {RUNTIME_ATTESTATION_BINDING_ENV:?}"
        )));
    }
    if !matches!(
        spec.artifact.media_type.as_str(),
        OCI_IMAGE_MANIFEST | OCI_IMAGE_INDEX
    ) {
        return Err(RuntimeError::UnsupportedCapabilities(vec![format!(
            "artifact_media_type:{}",
            spec.artifact.media_type
        )]));
    }
    let isolation_supported = match spec.isolation {
        a3s_runtime::contract::IsolationLevel::Sandbox => true,
        a3s_runtime::contract::IsolationLevel::Confidential => {
            execution_isolation == ExecutionIsolation::Microvm && sev_snp.is_some()
        }
        _ => false,
    };
    if !isolation_supported {
        return Err(RuntimeError::UnsupportedCapabilities(vec![format!(
            "isolation:{:?}",
            spec.isolation
        )]));
    }
    compile_network_mode(&spec.network.mode)?;
    if spec
        .network
        .ports
        .iter()
        .any(|port| port.protocol != TransportProtocol::Tcp)
    {
        return Err(RuntimeError::UnsupportedCapabilities(vec![
            "feature:ServiceUdp".into(),
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

fn compile_network_mode(mode: &RuntimeNetworkMode) -> RuntimeResult<NetworkMode> {
    match mode {
        // Runtime Service reachability is provided by the generation-fenced
        // vsock connector. Enabling TSI here would redirect the guest-side
        // loopback connection to the host instead of the local workload.
        RuntimeNetworkMode::None | RuntimeNetworkMode::Service => Ok(NetworkMode::None),
        unsupported => Err(RuntimeError::UnsupportedCapabilities(vec![format!(
            "network_mode:{unsupported:?}"
        )])),
    }
}

fn compile_secret_inputs(
    spec: &RuntimeUnitSpec,
    secret_root: &Path,
    spec_digest: &str,
) -> RuntimeResult<(Vec<String>, Option<String>)> {
    if spec.secrets.is_empty() {
        return Ok((Vec::new(), None));
    }
    if spec
        .process
        .environment
        .contains_key(SECRET_ENVIRONMENT_MANIFEST)
    {
        return Err(RuntimeError::InvalidRequest(format!(
            "Runtime process environment cannot use reserved Box key {SECRET_ENVIRONMENT_MANIFEST:?}"
        )));
    }
    let digest = spec_digest.strip_prefix("sha256:").ok_or_else(|| {
        RuntimeError::Protocol("Box Secret compilation requires a SHA-256 spec digest".into())
    })?;
    let internal_root = PathBuf::from(format!("{SECRET_GUEST_ROOT}/{digest}"));
    let mut destinations = spec
        .mounts
        .iter()
        .map(|mount| PathBuf::from(&mount.target))
        .collect::<Vec<_>>();
    let mut volumes = Vec::with_capacity(spec.secrets.len());
    let mut environment = Vec::new();

    for (index, secret) in spec.secrets.iter().enumerate() {
        let host = secret_file(secret_root, spec, index)?;
        let host = host.to_str().filter(|value| {
            !value.contains([':', '\0']) && !value.bytes().any(|byte| byte.is_ascii_control())
        });
        let host = host.ok_or_else(|| {
            RuntimeError::InvalidRequest(
                "Box Runtime Secret root cannot be encoded as a bind-mount source".into(),
            )
        })?;
        let destination = match &secret.target {
            SecretTarget::Environment { variable } => {
                if matches!(
                    variable.as_str(),
                    SECRET_ENVIRONMENT_MANIFEST | RUNTIME_ATTESTATION_BINDING_ENV
                ) {
                    return Err(RuntimeError::InvalidRequest(format!(
                        "Runtime Secret environment target cannot use reserved Box key {variable:?}"
                    )));
                }
                if spec.process.environment.contains_key(variable) {
                    return Err(RuntimeError::InvalidRequest(format!(
                        "Runtime Secret environment target {variable:?} conflicts with a literal process value"
                    )));
                }
                let path = internal_root.join(format!("{index:03}.secret"));
                let binding = SecretEnvironmentBinding {
                    variable: variable.clone(),
                    path: path.to_string_lossy().into_owned(),
                };
                binding.validate().map_err(RuntimeError::Protocol)?;
                environment.push(binding);
                path
            }
            SecretTarget::File { path, .. } => {
                validate_secret_file_target(path)?;
                PathBuf::from(path)
            }
            SecretTarget::RegistryCredential => continue,
        };
        if destinations
            .iter()
            .any(|existing| paths_overlap(existing, &destination))
        {
            return Err(RuntimeError::InvalidRequest(format!(
                "Runtime Secret target {:?} overlaps another mount or Secret target",
                destination
            )));
        }
        destinations.push(destination.clone());
        volumes.push(format!("{host}:{}:ro", destination.display()));
    }

    let manifest = if environment.is_empty() {
        None
    } else {
        Some(serde_json::to_string(&environment).map_err(|error| {
            RuntimeError::Protocol(format!(
                "Box could not encode the non-secret environment binding manifest: {error}"
            ))
        })?)
    };
    Ok((volumes, manifest))
}

fn paths_overlap(left: &Path, right: &Path) -> bool {
    left == right || left.starts_with(right) || right.starts_with(left)
}

fn validate_secret_file_target(target: &str) -> RuntimeResult<()> {
    let path = Path::new(target);
    let normalized = target.strip_prefix('/').is_some_and(|relative| {
        !relative.is_empty()
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
        || is_or_below(Path::new("/dev"))
        || is_or_below(Path::new("/run/a3s-box"))
        || is_or_below(Path::new(SECRET_GUEST_ROOT));
    if !normalized
        || target.contains([':', '\0'])
        || target.bytes().any(|byte| byte.is_ascii_control())
        || protected
    {
        return Err(RuntimeError::InvalidRequest(format!(
            "Box Sandbox Secret file target must be an encodable normalized unprotected absolute path: {target:?}"
        )));
    }
    Ok(())
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
        .filter_map(|mount| {
            let RuntimeMountSource::Tmpfs { size_bytes } = &mount.source else {
                return None;
            };
            Some(validate_tmpfs_target(&mount.target).map(|()| {
                format!(
                    "{}:size={size_bytes},{}",
                    mount.target,
                    if mount.read_only { "ro" } else { "rw" }
                )
            }))
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
            "Box Runtime tmpfs target must be an encodable normalized absolute path: {target:?}"
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
            "Box Runtime tmpfs target is protected: {target:?}"
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
