//! Durable security receipt publication and recovery validation.

use std::io::{Read, Write};
use std::path::{Path, PathBuf};

use a3s_box_core::{
    canonical_json_digest, BoxConfig, BoxError, ExecutionGeneration, ReceiptPolicy,
    ResolvedExecutionPlan, SeccompMode, SecurityConfig, SecurityReceiptArtifactDigests,
    SecurityReceiptEvidenceV1, SecurityReceiptIdMapping, SecurityReceiptImageIdentity,
    SecurityReceiptOwnerIdentity, SecurityReceiptPreparation, SecurityReceiptResources,
    SecurityReceiptRuntimeControls, SecurityReceiptV1,
};
use chrono::Utc;
use sha2::{Digest, Sha256};

use crate::{BoxRecord, ManagedExecutionMetadata};

const RECEIPTS_DIRECTORY: &str = "security/receipts";
const MAX_RECEIPT_BYTES: u64 = 1024 * 1024;

#[derive(Debug, Clone)]
pub(crate) struct ManagedSecurityContext {
    pub generation: ExecutionGeneration,
    pub request_digest: String,
}

#[derive(Debug, Clone)]
pub(crate) struct PreparedSecurityReceipt {
    pub manifest_digest: Option<String>,
    pub artifacts: SecurityReceiptArtifactDigests,
    pub owner: SecurityReceiptOwnerIdentity,
    pub runtime_controls: SecurityReceiptRuntimeControls,
    pub host_capability_digest: String,
    pub preparation: SecurityReceiptPreparation,
}

pub(crate) fn required(plan: &ResolvedExecutionPlan) -> bool {
    plan.security_policy
        .as_ref()
        .and_then(|policy| policy.receipt)
        == Some(ReceiptPolicy::Required)
}

pub(crate) async fn publish_prepared(
    home_dir: &Path,
    box_id: &str,
    config: &BoxConfig,
    plan: &ResolvedExecutionPlan,
    context: Option<&ManagedSecurityContext>,
    rootfs_path: &Path,
    prepared: PreparedSecurityReceipt,
) -> a3s_box_core::Result<Option<SecurityReceiptV1>> {
    if !required(plan) {
        return Ok(None);
    }
    let context = context.ok_or_else(|| {
        BoxError::StateError(
            "required security receipt has no managed execution context".to_string(),
        )
    })?;
    let policy = plan.security_policy.as_ref().ok_or_else(|| {
        BoxError::StateError("required security receipt has no resolved policy".to_string())
    })?;
    let policy_digest = plan.security_policy_digest.clone().ok_or_else(|| {
        BoxError::StateError("required security receipt has no policy digest".to_string())
    })?;
    let manifest_digest = match prepared.manifest_digest {
        Some(digest) => Some(digest),
        None => resolve_manifest_digest(home_dir, &config.image).await?,
    };
    let evidence = SecurityReceiptEvidenceV1 {
        execution_id: box_id.to_string(),
        generation: context.generation,
        request_digest: context.request_digest.clone(),
        policy_digest,
        execution_plan_digest: canonical_json_digest(plan)?,
        requested_isolation: plan.requested_isolation,
        backend: plan.backend,
        isolation_class: plan.isolation_class,
        image: SecurityReceiptImageIdentity {
            reference: config.image.clone(),
            manifest_digest,
            rootfs_digest: rootfs_identity_digest(rootfs_path)?,
        },
        artifacts: prepared.artifacts,
        owner: prepared.owner,
        mounts: plan.host_mounts.clone(),
        effective_egress: policy.egress.clone(),
        runtime_controls: prepared.runtime_controls,
        host_capability_digest: prepared.host_capability_digest,
        preparation: prepared.preparation,
        launch_timestamp: Utc::now(),
    };
    if let Some(existing) =
        equivalent_existing_receipt(&home_dir.join("boxes").join(box_id), &evidence)?
    {
        return Ok(Some(existing));
    }
    let receipt = SecurityReceiptV1::seal(evidence)?;
    publish(&home_dir.join("boxes").join(box_id), &receipt)?;
    Ok(Some(receipt))
}

pub(crate) fn load_for_record(
    record: &BoxRecord,
) -> a3s_box_core::Result<Option<SecurityReceiptV1>> {
    let Some(metadata) = record.managed_execution.as_ref() else {
        return Ok(None);
    };
    load_for_generation(record, metadata.generation)
}

pub(crate) fn load_for_generation(
    record: &BoxRecord,
    generation: ExecutionGeneration,
) -> a3s_box_core::Result<Option<SecurityReceiptV1>> {
    let Some(metadata) = record.managed_execution.as_ref() else {
        return Ok(None);
    };
    if !required(&metadata.plan) {
        return Ok(None);
    }
    load_and_validate(record, metadata, generation)
        .map(Some)
        .map_err(|error| {
            BoxError::StateError(format!(
                "required security receipt for execution {} is invalid: {error}",
                record.id
            ))
        })
}

pub(crate) fn remove_uncommitted(
    box_dir: &Path,
    generation: ExecutionGeneration,
) -> a3s_box_core::Result<()> {
    let path = receipt_path(box_dir, generation);
    match std::fs::remove_file(&path) {
        Ok(()) => {
            if let Some(directory) = path.parent() {
                sync_parent(directory)?;
            }
            Ok(())
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(BoxError::IoError(error)),
    }
}

pub(crate) fn publish_resume(
    record: &BoxRecord,
    target_generation: ExecutionGeneration,
) -> a3s_box_core::Result<Option<SecurityReceiptV1>> {
    let Some(metadata) = record.managed_execution.as_ref() else {
        return Ok(None);
    };
    if !required(&metadata.plan) {
        return Ok(None);
    }
    match load_and_validate(record, metadata, target_generation) {
        Ok(receipt) => {
            if receipt.evidence.preparation != SecurityReceiptPreparation::ReadyToResume {
                return Err(BoxError::StateError(format!(
                    "security receipt for resumed generation {} has the wrong preparation state",
                    target_generation.get()
                )));
            }
            return Ok(Some(receipt));
        }
        Err(BoxError::IoError(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let mut receipt = latest_valid_receipt(record, metadata)?;
    receipt.evidence.generation = target_generation;
    receipt.evidence.preparation = SecurityReceiptPreparation::ReadyToResume;
    receipt.evidence.launch_timestamp = Utc::now();
    let receipt = SecurityReceiptV1::seal(receipt.evidence)?;
    publish(&record.box_dir, &receipt)?;
    Ok(Some(receipt))
}

pub(crate) fn target_launch_generation(
    record: &BoxRecord,
) -> a3s_box_core::Result<ExecutionGeneration> {
    let metadata = record.managed_execution.as_ref().ok_or_else(|| {
        BoxError::StateError(format!("execution {} has no managed metadata", record.id))
    })?;
    if record.status == "resuming" {
        return next_generation(metadata.generation);
    }
    Ok(metadata.generation)
}

pub(crate) fn host_owner_identity() -> SecurityReceiptOwnerIdentity {
    #[cfg(unix)]
    {
        SecurityReceiptOwnerIdentity {
            platform: std::env::consts::OS.to_string(),
            // SAFETY: these process identity queries have no preconditions.
            effective_uid: Some(unsafe { libc::geteuid() }),
            // SAFETY: these process identity queries have no preconditions.
            effective_gid: Some(unsafe { libc::getegid() }),
            username: std::env::var("USER").ok().filter(|value| !value.is_empty()),
        }
    }
    #[cfg(not(unix))]
    {
        SecurityReceiptOwnerIdentity {
            platform: std::env::consts::OS.to_string(),
            effective_uid: None,
            effective_gid: None,
            username: std::env::var("USERNAME")
                .ok()
                .filter(|value| !value.is_empty()),
        }
    }
}

pub(crate) fn microvm_runtime_controls(
    config: &BoxConfig,
) -> a3s_box_core::Result<SecurityReceiptRuntimeControls> {
    let security = SecurityConfig::from_options(
        &config.security_opt,
        &config.cap_add,
        &config.cap_drop,
        config.privileged,
    );
    security.validate().map_err(BoxError::ConfigError)?;
    let seccomp = match security.seccomp {
        SeccompMode::Default => "default".to_string(),
        SeccompMode::Unconfined => "unconfined".to_string(),
        SeccompMode::Custom(path) => format!("custom:{path}"),
    };
    Ok(SecurityReceiptRuntimeControls {
        uid_mappings: Vec::new(),
        gid_mappings: Vec::new(),
        capabilities: security.cap_add,
        dropped_capabilities: security.cap_drop,
        seccomp,
        no_new_privileges: security.no_new_privileges,
        resources: resources_from_config(config),
    })
}

pub(crate) fn resources_from_config(config: &BoxConfig) -> SecurityReceiptResources {
    SecurityReceiptResources {
        vcpus: config.resources.vcpus,
        memory_bytes: u64::from(config.resources.memory_mb) * 1024 * 1024,
        pids_limit: config.resource_limits.pids_limit,
        cpu_shares: config.resource_limits.cpu_shares,
        cpu_quota: config.resource_limits.cpu_quota,
        cpu_period: config.resource_limits.cpu_period,
        cpuset_cpus: config.resource_limits.cpuset_cpus.clone(),
        memory_reservation: config.resource_limits.memory_reservation,
        memory_swap: config.resource_limits.memory_swap,
    }
}

pub(crate) fn sandbox_runtime_controls(
    config: &BoxConfig,
    bundle: &crate::sandbox::SandboxBundleSpec,
) -> a3s_box_core::Result<SecurityReceiptRuntimeControls> {
    let resources = &bundle.resources;
    let mut dropped_capabilities = config
        .cap_drop
        .iter()
        .map(|capability| normalize_capability_name(capability))
        .collect::<a3s_box_core::Result<Vec<_>>>()?;
    dropped_capabilities.sort();
    dropped_capabilities.dedup();

    Ok(SecurityReceiptRuntimeControls {
        uid_mappings: bundle
            .id_mappings
            .uid_mappings
            .iter()
            .map(|mapping| map_id_mapping(mapping.container_id, mapping.host_id, mapping.size))
            .collect(),
        gid_mappings: bundle
            .id_mappings
            .gid_mappings
            .iter()
            .map(|mapping| map_id_mapping(mapping.container_id, mapping.host_id, mapping.size))
            .collect(),
        capabilities: crate::sandbox::oci::effective_capability_names(
            &bundle.requested_capabilities,
        )?,
        dropped_capabilities,
        seccomp: crate::sandbox::oci::SANDBOX_SECCOMP_POSTURE.to_string(),
        no_new_privileges: true,
        resources: SecurityReceiptResources {
            vcpus: config.resources.vcpus,
            memory_bytes: u64::try_from(resources.memory_limit).map_err(|_| {
                BoxError::StateError(
                    "compiled Sandbox memory limit is negative or overflows u64".to_string(),
                )
            })?,
            pids_limit: Some(u64::try_from(resources.pids_limit).map_err(|_| {
                BoxError::StateError(
                    "compiled Sandbox PID limit is negative or overflows u64".to_string(),
                )
            })?),
            cpu_shares: resources.cpu_shares,
            cpu_quota: Some(resources.cpu_quota),
            cpu_period: Some(resources.cpu_period),
            cpuset_cpus: resources.cpuset_cpus.clone(),
            memory_reservation: resources
                .memory_reservation
                .map(|value| {
                    u64::try_from(value).map_err(|_| {
                        BoxError::StateError(
                            "compiled Sandbox memory reservation is negative or overflows u64"
                                .to_string(),
                        )
                    })
                })
                .transpose()?,
            memory_swap: resources.memory_swap,
        },
    })
}

pub(crate) fn map_id_mapping(
    container_id: u32,
    host_id: u32,
    size: u32,
) -> SecurityReceiptIdMapping {
    SecurityReceiptIdMapping {
        container_id,
        host_id,
        size,
    }
}

pub(crate) fn sha256_file(path: &Path) -> a3s_box_core::Result<String> {
    let mut file = std::fs::File::open(path).map_err(|error| {
        BoxError::StateError(format!(
            "cannot open security receipt artifact {}: {error}",
            path.display()
        ))
    })?;
    let mut digest = Sha256::new();
    let mut buffer = [0_u8; 64 * 1024];
    loop {
        let read = file.read(&mut buffer).map_err(|error| {
            BoxError::StateError(format!(
                "cannot read security receipt artifact {}: {error}",
                path.display()
            ))
        })?;
        if read == 0 {
            break;
        }
        digest.update(&buffer[..read]);
    }
    Ok(format!("sha256:{}", hex::encode(digest.finalize())))
}

fn publish(box_dir: &Path, receipt: &SecurityReceiptV1) -> a3s_box_core::Result<()> {
    let security_directory = box_dir.join("security");
    let directory = box_dir.join(RECEIPTS_DIRECTORY);
    std::fs::create_dir_all(&security_directory).map_err(BoxError::IoError)?;
    ensure_receipt_directory(&security_directory)?;
    std::fs::create_dir_all(&directory).map_err(BoxError::IoError)?;
    ensure_receipt_directory(&directory)?;
    let path = receipt_path(box_dir, receipt.evidence.generation);
    let mut temporary = tempfile::NamedTempFile::new_in(&directory).map_err(BoxError::IoError)?;
    #[cfg(unix)]
    temporary
        .as_file()
        .set_permissions(std::os::unix::fs::PermissionsExt::from_mode(0o600))
        .map_err(BoxError::IoError)?;
    serde_json::to_writer_pretty(&mut temporary, receipt).map_err(|error| {
        BoxError::SerializationError(format!("failed to encode security receipt: {error}"))
    })?;
    temporary.write_all(b"\n").map_err(BoxError::IoError)?;
    temporary.as_file().sync_all().map_err(BoxError::IoError)?;
    match temporary.persist_noclobber(&path) {
        Ok(_) => sync_parent(&directory),
        Err(error) if error.error.kind() == std::io::ErrorKind::AlreadyExists => {
            let existing = read_receipt(&path)?;
            if &existing == receipt {
                Ok(())
            } else {
                Err(BoxError::StateError(format!(
                    "security receipt already exists with different evidence: {}",
                    path.display()
                )))
            }
        }
        Err(error) => Err(BoxError::IoError(error.error)),
    }
}

fn ensure_receipt_directory(path: &Path) -> a3s_box_core::Result<()> {
    let metadata = std::fs::symlink_metadata(path).map_err(BoxError::IoError)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(BoxError::StateError(format!(
            "security receipt path is not a plain directory: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        // SAFETY: querying the effective process UID has no preconditions.
        if metadata.uid() != unsafe { libc::geteuid() } {
            return Err(BoxError::StateError(format!(
                "security receipt directory is not owned by the runtime user: {}",
                path.display()
            )));
        }
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o700))
            .map_err(BoxError::IoError)?;
    }
    Ok(())
}

fn equivalent_existing_receipt(
    box_dir: &Path,
    candidate: &SecurityReceiptEvidenceV1,
) -> a3s_box_core::Result<Option<SecurityReceiptV1>> {
    let path = receipt_path(box_dir, candidate.generation);
    let existing = match read_receipt(&path) {
        Ok(receipt) => receipt,
        Err(BoxError::IoError(error)) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(None)
        }
        Err(error) => return Err(error),
    };
    let mut comparable = candidate.clone();
    comparable.launch_timestamp = existing.evidence.launch_timestamp;
    if existing.evidence != comparable {
        return Err(BoxError::StateError(format!(
            "security receipt already exists with different evidence: {}",
            path.display()
        )));
    }
    Ok(Some(existing))
}

fn load_and_validate(
    record: &BoxRecord,
    metadata: &ManagedExecutionMetadata,
    generation: ExecutionGeneration,
) -> a3s_box_core::Result<SecurityReceiptV1> {
    let receipt = read_receipt(&receipt_path(&record.box_dir, generation))?;
    validate_common(&receipt, record, metadata)?;
    if receipt.evidence.generation != generation {
        return Err(BoxError::StateError(
            "security receipt generation does not match durable state".to_string(),
        ));
    }
    Ok(receipt)
}

fn validate_common(
    receipt: &SecurityReceiptV1,
    record: &BoxRecord,
    metadata: &ManagedExecutionMetadata,
) -> a3s_box_core::Result<()> {
    receipt.validate()?;
    let policy = metadata.plan.security_policy.as_ref().ok_or_else(|| {
        BoxError::StateError("security receipt has no resolved policy".to_string())
    })?;
    let evidence = &receipt.evidence;
    if evidence.execution_id != record.id
        || evidence.request_digest != canonical_json_digest(&metadata.request)?
        || Some(&evidence.policy_digest) != metadata.plan.security_policy_digest.as_ref()
        || evidence.execution_plan_digest != canonical_json_digest(&metadata.plan)?
        || evidence.requested_isolation != metadata.plan.requested_isolation
        || evidence.backend != metadata.plan.backend
        || evidence.isolation_class != metadata.plan.isolation_class
        || evidence.mounts != metadata.plan.host_mounts
        || evidence.effective_egress != policy.egress
    {
        return Err(BoxError::StateError(
            "security receipt does not match durable execution intent".to_string(),
        ));
    }
    Ok(())
}

fn latest_valid_receipt(
    record: &BoxRecord,
    metadata: &ManagedExecutionMetadata,
) -> a3s_box_core::Result<SecurityReceiptV1> {
    let directory = record.box_dir.join(RECEIPTS_DIRECTORY);
    let mut candidates = std::fs::read_dir(&directory)
        .map_err(BoxError::IoError)?
        .filter_map(std::result::Result::ok)
        .filter_map(|entry| {
            parse_generation_filename(&entry.file_name())
                .map(|generation| (generation, entry.path()))
        })
        .filter(|(generation, _)| *generation <= metadata.generation.get())
        .collect::<Vec<_>>();
    candidates.sort_by_key(|(generation, _)| std::cmp::Reverse(*generation));
    for (generation, _) in candidates {
        let generation = ExecutionGeneration::new(generation)
            .map_err(|error| BoxError::StateError(error.to_string()))?;
        return load_and_validate(record, metadata, generation);
    }
    Err(BoxError::StateError(format!(
        "required security receipt history is missing for execution {}",
        record.id
    )))
}

fn read_receipt(path: &Path) -> a3s_box_core::Result<SecurityReceiptV1> {
    let metadata = std::fs::symlink_metadata(path).map_err(BoxError::IoError)?;
    if !metadata.is_file()
        || metadata.file_type().is_symlink()
        || metadata.len() > MAX_RECEIPT_BYTES
    {
        return Err(BoxError::StateError(format!(
            "security receipt is not a bounded regular file: {}",
            path.display()
        )));
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        if metadata.uid() != unsafe { libc::geteuid() }
            || metadata.permissions().mode() & 0o077 != 0
        {
            return Err(BoxError::StateError(format!(
                "security receipt ownership or permissions are unsafe: {}",
                path.display()
            )));
        }
    }
    let bytes = std::fs::read(path).map_err(BoxError::IoError)?;
    let receipt = serde_json::from_slice::<SecurityReceiptV1>(&bytes).map_err(|error| {
        BoxError::StateError(format!(
            "invalid security receipt {}: {error}",
            path.display()
        ))
    })?;
    receipt.validate()?;
    Ok(receipt)
}

async fn resolve_manifest_digest(
    home_dir: &Path,
    image: &str,
) -> a3s_box_core::Result<Option<String>> {
    if image.is_empty() {
        return Ok(None);
    }
    let store =
        crate::oci::ImageStore::new(&home_dir.join("images"), crate::DEFAULT_IMAGE_CACHE_SIZE)?;
    Ok(store.resolve(image).await.map(|stored| stored.digest))
}

fn rootfs_identity_digest(rootfs: &Path) -> a3s_box_core::Result<String> {
    let canonical = rootfs.canonicalize().map_err(BoxError::IoError)?;
    let metadata = std::fs::symlink_metadata(&canonical).map_err(BoxError::IoError)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(BoxError::StateError(format!(
            "security receipt rootfs is not a plain directory: {}",
            canonical.display()
        )));
    }
    let mut digest = Sha256::new();
    digest.update(std::env::consts::OS.as_bytes());
    digest.update([0]);
    digest.update(canonical.to_string_lossy().as_bytes());
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        digest.update(metadata.dev().to_le_bytes());
        digest.update(metadata.ino().to_le_bytes());
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::MetadataExt;

        digest.update(metadata.len().to_le_bytes());
        digest.update(metadata.creation_time().to_le_bytes());
        digest.update(metadata.file_attributes().to_le_bytes());
    }
    #[cfg(not(any(unix, windows)))]
    digest.update(metadata.len().to_le_bytes());
    Ok(format!("sha256:{}", hex::encode(digest.finalize())))
}

fn receipt_path(box_dir: &Path, generation: ExecutionGeneration) -> PathBuf {
    box_dir
        .join(RECEIPTS_DIRECTORY)
        .join(format!("generation-{}.json", generation.get()))
}

fn parse_generation_filename(name: &std::ffi::OsStr) -> Option<u64> {
    name.to_str()?
        .strip_prefix("generation-")?
        .strip_suffix(".json")?
        .parse()
        .ok()
}

fn next_generation(current: ExecutionGeneration) -> a3s_box_core::Result<ExecutionGeneration> {
    let value = current
        .get()
        .checked_add(1)
        .ok_or_else(|| BoxError::StateError("execution generation is exhausted".to_string()))?;
    ExecutionGeneration::new(value).map_err(|error| BoxError::StateError(error.to_string()))
}

fn normalize_capability_name(value: &str) -> a3s_box_core::Result<String> {
    let normalized = value.trim().to_ascii_uppercase();
    if normalized.is_empty() {
        return Err(BoxError::StateError(
            "security receipt cannot encode an empty dropped capability".to_string(),
        ));
    }
    if normalized.starts_with("CAP_") {
        Ok(normalized)
    } else {
        Ok(format!("CAP_{normalized}"))
    }
}

fn sync_parent(directory: &Path) -> a3s_box_core::Result<()> {
    #[cfg(unix)]
    {
        std::fs::File::open(directory)
            .and_then(|file| file.sync_all())
            .map_err(BoxError::IoError)?;
    }
    #[cfg(not(unix))]
    let _ = directory;
    Ok(())
}

#[cfg(test)]
pub(crate) fn publish_test_receipt(record: &BoxRecord) -> a3s_box_core::Result<SecurityReceiptV1> {
    let metadata = record.managed_execution.as_ref().ok_or_else(|| {
        BoxError::StateError(format!("execution {} has no managed metadata", record.id))
    })?;
    let policy =
        metadata.plan.security_policy.as_ref().ok_or_else(|| {
            BoxError::StateError("test receipt has no resolved policy".to_string())
        })?;
    let generation = target_launch_generation(record)?;
    match load_and_validate(record, metadata, generation) {
        Ok(receipt) => return Ok(receipt),
        Err(BoxError::IoError(error)) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }
    let digest = |byte: char| format!("sha256:{}", byte.to_string().repeat(64));
    let receipt = SecurityReceiptV1::seal(SecurityReceiptEvidenceV1 {
        execution_id: record.id.clone(),
        generation,
        request_digest: canonical_json_digest(&metadata.request)?,
        policy_digest: metadata
            .plan
            .security_policy_digest
            .clone()
            .ok_or_else(|| BoxError::StateError("test receipt has no policy digest".to_string()))?,
        execution_plan_digest: canonical_json_digest(&metadata.plan)?,
        requested_isolation: metadata.plan.requested_isolation,
        backend: metadata.plan.backend,
        isolation_class: metadata.plan.isolation_class,
        image: SecurityReceiptImageIdentity {
            reference: metadata.request.config.image.clone(),
            manifest_digest: Some(digest('a')),
            rootfs_digest: digest('b'),
        },
        artifacts: SecurityReceiptArtifactDigests {
            runtime_sha256: digest('c'),
            agent_sha256: digest('d'),
        },
        owner: SecurityReceiptOwnerIdentity {
            platform: "test".to_string(),
            effective_uid: None,
            effective_gid: None,
            username: None,
        },
        mounts: metadata.plan.host_mounts.clone(),
        effective_egress: policy.egress.clone(),
        runtime_controls: SecurityReceiptRuntimeControls {
            uid_mappings: Vec::new(),
            gid_mappings: Vec::new(),
            capabilities: Vec::new(),
            dropped_capabilities: Vec::new(),
            seccomp: "test-default".to_string(),
            no_new_privileges: true,
            resources: resources_from_config(&metadata.request.config),
        },
        host_capability_digest: digest('e'),
        preparation: if record.status == "resuming" && metadata.paused_with_memory {
            SecurityReceiptPreparation::ReadyToResume
        } else {
            SecurityReceiptPreparation::ReadyToLaunch
        },
        launch_timestamp: Utc::now(),
    })?;
    publish(&record.box_dir, &receipt)?;
    Ok(receipt)
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use a3s_box_core::{
        CreateExecutionRequest, ExecutionId, ExecutionIsolation, ExecutionRecordPolicy,
        HostMountPolicy, OperationId, SandboxSecurityPolicy,
    };

    use super::*;

    fn digest(byte: char) -> String {
        format!("sha256:{}", byte.to_string().repeat(64))
    }

    fn required_record(home_dir: &Path, mut config: BoxConfig) -> BoxRecord {
        std::fs::create_dir_all(home_dir).unwrap();
        if config.security_policy.is_none() {
            config.security_policy =
                Some(SandboxSecurityPolicy::new().receipt(ReceiptPolicy::Required));
        }
        let execution_id = ExecutionId::new("10000000-0000-4000-8000-000000000001").unwrap();
        crate::local_execution::build_managed_record_for_test(
            home_dir,
            &execution_id,
            OperationId::new("security-receipt-test").unwrap(),
            CreateExecutionRequest {
                external_sandbox_id: "receipt-test".to_string(),
                config,
                labels: BTreeMap::new(),
                policy: ExecutionRecordPolicy::default(),
                rootfs_snapshot_id: None,
            },
            Utc::now(),
        )
        .unwrap()
    }

    fn prepared(config: &BoxConfig) -> PreparedSecurityReceipt {
        PreparedSecurityReceipt {
            manifest_digest: Some(digest('a')),
            artifacts: SecurityReceiptArtifactDigests {
                runtime_sha256: digest('b'),
                agent_sha256: digest('c'),
            },
            owner: SecurityReceiptOwnerIdentity {
                platform: "test".to_string(),
                effective_uid: Some(1000),
                effective_gid: Some(1000),
                username: Some("runner".to_string()),
            },
            runtime_controls: microvm_runtime_controls(config).unwrap(),
            host_capability_digest: digest('d'),
            preparation: SecurityReceiptPreparation::ReadyToLaunch,
        }
    }

    async fn publish_fixture(
        record: &BoxRecord,
        prepared: PreparedSecurityReceipt,
    ) -> a3s_box_core::Result<SecurityReceiptV1> {
        let metadata = record.managed_execution.as_ref().unwrap();
        let rootfs = record.box_dir.join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();
        publish_prepared(
            record.box_dir.parent().unwrap().parent().unwrap(),
            &record.id,
            &metadata.request.config,
            &metadata.plan,
            Some(&ManagedSecurityContext {
                generation: metadata.generation,
                request_digest: canonical_json_digest(&metadata.request).unwrap(),
            }),
            &rootfs,
            prepared,
        )
        .await
        .map(Option::unwrap)
    }

    #[tokio::test]
    async fn publication_is_atomic_idempotent_and_never_clobbers_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let home_dir = directory.path().join("home");
        let record = required_record(
            &home_dir,
            BoxConfig {
                image: "alpine:3.20".to_string(),
                isolation: ExecutionIsolation::Microvm,
                ..BoxConfig::default()
            },
        );
        let receipt_directory = record.box_dir.join(RECEIPTS_DIRECTORY);
        std::fs::create_dir_all(&receipt_directory).unwrap();
        std::fs::write(receipt_directory.join(".interrupted.tmp"), b"{\"partial\":").unwrap();

        let first = publish_fixture(
            &record,
            prepared(&record.managed_execution.as_ref().unwrap().request.config),
        )
        .await
        .unwrap();
        let second = publish_fixture(
            &record,
            prepared(&record.managed_execution.as_ref().unwrap().request.config),
        )
        .await
        .unwrap();

        assert_eq!(second, first);
        assert_eq!(
            read_receipt(&receipt_path(&record.box_dir, ExecutionGeneration::INITIAL)).unwrap(),
            first
        );

        let mut conflicting = prepared(&record.managed_execution.as_ref().unwrap().request.config);
        conflicting.artifacts.agent_sha256 = digest('e');
        let error = publish_fixture(&record, conflicting)
            .await
            .unwrap_err()
            .to_string();
        assert!(error.contains("already exists with different evidence"));
        assert_eq!(
            read_receipt(&receipt_path(&record.box_dir, ExecutionGeneration::INITIAL)).unwrap(),
            first
        );
    }

    #[tokio::test]
    async fn required_publication_failure_is_reported_without_a_target_receipt() {
        let directory = tempfile::tempdir().unwrap();
        let home_dir = directory.path().join("home");
        let record = required_record(&home_dir, BoxConfig::default());
        std::fs::create_dir_all(record.box_dir.join("security")).unwrap();
        std::fs::write(record.box_dir.join(RECEIPTS_DIRECTORY), b"not-a-directory").unwrap();

        let error = publish_fixture(
            &record,
            prepared(&record.managed_execution.as_ref().unwrap().request.config),
        )
        .await
        .unwrap_err();

        assert!(matches!(error, BoxError::IoError(_)));
        assert!(!receipt_path(&record.box_dir, ExecutionGeneration::INITIAL).exists());
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn publication_rejects_a_symlinked_security_directory() {
        let directory = tempfile::tempdir().unwrap();
        let home_dir = directory.path().join("home");
        let record = required_record(&home_dir, BoxConfig::default());
        let external = directory.path().join("external-security");
        std::fs::create_dir_all(&record.box_dir).unwrap();
        std::fs::create_dir_all(&external).unwrap();
        std::os::unix::fs::symlink(&external, record.box_dir.join("security")).unwrap();

        let error = publish_fixture(
            &record,
            prepared(&record.managed_execution.as_ref().unwrap().request.config),
        )
        .await
        .unwrap_err()
        .to_string();

        assert!(error.contains("not a plain directory"));
        assert!(!external.join("receipts").exists());
    }

    #[test]
    fn recovery_rejects_missing_tampered_truncated_and_stale_receipts() {
        let directory = tempfile::tempdir().unwrap();
        let home_dir = directory.path().join("home");
        let mut record = required_record(&home_dir, BoxConfig::default());
        record.status = "running".to_string();
        assert!(load_for_record(&record).is_err());
        assert!(record.managed_state().is_err());

        record.status = "starting".to_string();
        let receipt = publish_test_receipt(&record).unwrap();
        let path = receipt_path(&record.box_dir, ExecutionGeneration::INITIAL);
        let original = std::fs::read(&path).unwrap();
        record.status = "running".to_string();
        assert_eq!(load_for_record(&record).unwrap(), Some(receipt));
        assert_eq!(
            record.managed_state().unwrap(),
            Some(crate::ManagedExecutionState::Running)
        );

        let mut tampered: serde_json::Value = serde_json::from_slice(&original).unwrap();
        tampered["evidence"]["owner"]["platform"] = serde_json::json!("tampered");
        std::fs::write(&path, serde_json::to_vec(&tampered).unwrap()).unwrap();
        assert!(load_for_record(&record).is_err());
        assert!(record.managed_state().is_err());

        std::fs::write(&path, b"{\"schema\":").unwrap();
        assert!(load_for_record(&record).is_err());

        std::fs::write(&path, &original).unwrap();
        let generation_two = ExecutionGeneration::new(2).unwrap();
        std::fs::copy(&path, receipt_path(&record.box_dir, generation_two)).unwrap();
        record.managed_execution.as_mut().unwrap().generation = generation_two;
        assert!(load_for_record(&record)
            .unwrap_err()
            .to_string()
            .contains("generation does not match"));
    }

    #[tokio::test]
    async fn receipt_contains_exact_mount_evidence_but_no_environment_values() {
        let directory = tempfile::tempdir().unwrap();
        let home_dir = directory.path().join("home");
        let source = directory.path().join("workspace");
        std::fs::create_dir_all(&source).unwrap();
        let config = BoxConfig {
            image: "alpine:3.20".to_string(),
            volumes: vec![format!("{}:/workspace:ro", source.display())],
            extra_env: vec![(
                "PRIVATE_TOKEN".to_string(),
                "receipt-must-redact-this-value".to_string(),
            )],
            security_policy: Some(
                SandboxSecurityPolicy::new()
                    .host_mounts(HostMountPolicy::agent_safe().allow_path(source))
                    .receipt(ReceiptPolicy::Required),
            ),
            ..BoxConfig::default()
        };
        let record = required_record(&home_dir, config);
        let plan = &record.managed_execution.as_ref().unwrap().plan;
        assert_eq!(plan.host_mounts.len(), 1);

        let receipt = publish_fixture(
            &record,
            prepared(&record.managed_execution.as_ref().unwrap().request.config),
        )
        .await
        .unwrap();
        let encoded = serde_json::to_string(&receipt).unwrap();

        assert_eq!(receipt.evidence.mounts, plan.host_mounts);
        assert!(!encoded.contains("PRIVATE_TOKEN"));
        assert!(!encoded.contains("receipt-must-redact-this-value"));
    }
}
