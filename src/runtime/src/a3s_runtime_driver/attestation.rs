//! Provider-owned SEV-SNP attestation acquisition and immutable persistence.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use a3s_box_core::ExecutionGeneration;
use a3s_runtime::contract::{ArtifactRef, IsolationLevel, RuntimeObservation, RuntimeUnitSpec};
use a3s_runtime::{RuntimeError, RuntimeResult};
use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::grpc::{ExecClient, RaTlsAttestationClient};
use crate::tee::attestation::SNP_REPORT_SIZE;
use crate::tee::{
    is_simulated_report, parse_platform_info, verify_attestation, AttestationPolicy,
    AttestationReport,
};
use crate::{BoxRecord, ManagedExecutionState};

use super::metadata::local_identity;
use super::BoxRuntimeSevSnpConfig;

pub(super) const ATTESTATION_MEDIA_TYPE: &str =
    "application/vnd.a3s.box.sev-snp-attestation.v1+json";
const ATTESTATION_SCHEMA: &str = "a3s.box.runtime.attestation.v1";
const EXECUTION_GENERATION_CLAIM: &str = "a3s.box.execution-generation";
const MAX_ATTESTATION_BYTES: u64 = 1024 * 1024;
const RUNTIME_BINDING_OFFSET: usize = 0x70;
const RUNTIME_BINDING_SIZE: usize = 32;

#[derive(Clone)]
pub(super) struct BoxAttestationPayload {
    pub(super) report: AttestationReport,
    pub(super) certificate_der: Vec<u8>,
}

#[async_trait]
pub(super) trait BoxAttestationTransport: Send + Sync {
    async fn fetch_report(
        &self,
        socket_path: &Path,
        policy: &AttestationPolicy,
        allow_simulated: bool,
        expected_runtime_binding: &[u8; RUNTIME_BINDING_SIZE],
    ) -> RuntimeResult<BoxAttestationPayload>;
}

#[async_trait]
pub(super) trait BoxAttestedMainStarter: Send + Sync {
    async fn start(&self, record: &BoxRecord) -> RuntimeResult<()>;
}

struct RaTlsAttestationTransport;

#[async_trait]
impl BoxAttestationTransport for RaTlsAttestationTransport {
    async fn fetch_report(
        &self,
        socket_path: &Path,
        policy: &AttestationPolicy,
        allow_simulated: bool,
        _expected_runtime_binding: &[u8; RUNTIME_BINDING_SIZE],
    ) -> RuntimeResult<BoxAttestationPayload> {
        let evidence = RaTlsAttestationClient::new(socket_path)
            .fetch_evidence_with_policy(policy.clone(), allow_simulated)
            .await
            .map_err(|error| {
                RuntimeError::ProviderUnavailable(format!(
                    "Box SEV-SNP RA-TLS attestation failed: {error}"
                ))
            })?;
        Ok(BoxAttestationPayload {
            report: evidence.report,
            certificate_der: evidence.certificate_der,
        })
    }
}

struct GuestAttestedMainStarter;

#[async_trait]
impl BoxAttestedMainStarter for GuestAttestedMainStarter {
    async fn start(&self, record: &BoxRecord) -> RuntimeResult<()> {
        let acknowledged = ExecClient::for_socket(&record.exec_socket_path)
            .spawn_main(None)
            .await
            .map_err(|error| {
                RuntimeError::ProviderUnavailable(format!(
                    "Box could not release the attested confidential main process: {error}"
                ))
            })?;
        if !acknowledged {
            return Err(RuntimeError::ProviderUnavailable(format!(
                "Box guest {} did not acknowledge the attested main-process release",
                record.id
            )));
        }
        Ok(())
    }
}

#[cfg(test)]
struct NoopAttestedMainStarter;

#[cfg(test)]
#[async_trait]
impl BoxAttestedMainStarter for NoopAttestedMainStarter {
    async fn start(&self, _record: &BoxRecord) -> RuntimeResult<()> {
        Ok(())
    }
}

pub(super) struct AttestationArtifactOwner {
    transport: Arc<dyn BoxAttestationTransport>,
    main_starter: Arc<dyn BoxAttestedMainStarter>,
}

impl Default for AttestationArtifactOwner {
    fn default() -> Self {
        Self {
            transport: Arc::new(RaTlsAttestationTransport),
            main_starter: Arc::new(GuestAttestedMainStarter),
        }
    }
}

impl AttestationArtifactOwner {
    #[cfg(test)]
    pub(super) fn with_transport(transport: Arc<dyn BoxAttestationTransport>) -> Self {
        Self {
            transport,
            main_starter: Arc::new(NoopAttestedMainStarter),
        }
    }

    #[cfg(test)]
    pub(super) fn with_main_starter(
        mut self,
        main_starter: Arc<dyn BoxAttestedMainStarter>,
    ) -> Self {
        self.main_starter = main_starter;
        self
    }

    pub(super) async fn reference_for(
        &self,
        spec: &RuntimeUnitSpec,
        record: &BoxRecord,
        sev_snp: Option<&BoxRuntimeSevSnpConfig>,
    ) -> RuntimeResult<Option<ArtifactRef>> {
        if spec.isolation == IsolationLevel::Sandbox {
            return Ok(None);
        }
        let config = sev_snp.ok_or_else(|| {
            RuntimeError::Protocol(
                "Box produced a confidential Runtime resource without SEV-SNP configuration".into(),
            )
        })?;
        let expected_runtime_binding = runtime_binding(spec)?;
        let (_, execution_generation, state) = local_identity(record)?;
        let path = attestation_path(record, execution_generation);
        let existing = read_existing(&path).await?;
        if let Some(bytes) = existing.as_deref() {
            validate_persisted(
                bytes,
                spec,
                record,
                execution_generation,
                config,
                &expected_runtime_binding,
            )?;
        }

        if state != ManagedExecutionState::Running {
            let bytes = existing.ok_or_else(|| {
                RuntimeError::ProviderUnavailable(format!(
                    "Box cannot acquire SEV-SNP attestation for execution {} while it is {state}",
                    record.id
                ))
            })?;
            return artifact_reference(record, execution_generation, &bytes).map(Some);
        }

        // A persisted report is a crash-recovery baseline, not proof that the
        // current guest is live. Every running observation repeats RA-TLS and
        // requires the same certificate/report for this Box generation.
        let socket_path = record.exec_socket_path.with_file_name("attest.sock");
        let payload = self
            .transport
            .fetch_report(
                &socket_path,
                &config.attestation_policy,
                config.simulate,
                &expected_runtime_binding,
            )
            .await?;
        validate_acquired(&payload, config, &expected_runtime_binding)?;
        let stored = StoredAttestation {
            schema: ATTESTATION_SCHEMA.into(),
            unit_id: spec.unit_id.clone(),
            runtime_generation: spec.generation,
            spec_digest: spec.digest().map_err(RuntimeError::Protocol)?,
            provider_resource_id: record.id.clone(),
            execution_generation: execution_generation.get(),
            mode: attestation_mode(config).into(),
            policy: config.attestation_policy.clone(),
            ratls_certificate: payload.certificate_der,
            report: payload.report,
        };
        let acquired = serde_json::to_vec(&stored).map_err(|error| {
            RuntimeError::Protocol(format!(
                "Box could not encode the SEV-SNP attestation artifact: {error}"
            ))
        })?;

        let bytes = match existing {
            Some(existing) => {
                if existing != acquired {
                    return Err(RuntimeError::Protocol(format!(
                        "Box live SEV-SNP attestation changed within execution {} generation {}",
                        record.id,
                        execution_generation.get()
                    )));
                }
                existing
            }
            None => match persist_new(&path, &acquired).await? {
                PublishOutcome::Created => acquired,
                PublishOutcome::Existing => {
                    let concurrent = read_existing(&path).await?.ok_or_else(|| {
                        RuntimeError::ProviderUnavailable(
                            "Box concurrent SEV-SNP attestation publication disappeared".into(),
                        )
                    })?;
                    validate_persisted(
                        &concurrent,
                        spec,
                        record,
                        execution_generation,
                        config,
                        &expected_runtime_binding,
                    )?;
                    if concurrent != acquired {
                        return Err(RuntimeError::Protocol(format!(
                            "Box concurrent SEV-SNP attestation disagrees for execution {} generation {}",
                            record.id,
                            execution_generation.get()
                        )));
                    }
                    concurrent
                }
            },
        };
        let reference = artifact_reference(record, execution_generation, &bytes)?;

        // Confidential guests boot with their main process deferred. Persist and
        // validate the exact live evidence before crossing the execution boundary.
        // A repeated bare trigger is guest-idempotent, closing the response-loss
        // window without allowing a caller-supplied pool command to be replayed.
        self.main_starter.start(record).await?;
        Ok(Some(reference))
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredAttestation {
    schema: String,
    unit_id: String,
    runtime_generation: u64,
    spec_digest: String,
    provider_resource_id: String,
    execution_generation: u64,
    mode: String,
    policy: AttestationPolicy,
    ratls_certificate: Vec<u8>,
    report: AttestationReport,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum PublishOutcome {
    Created,
    Existing,
}

pub(super) fn attestation_path(
    record: &BoxRecord,
    execution_generation: ExecutionGeneration,
) -> PathBuf {
    record.box_dir.join(format!(
        "runtime-attestation-{}.json",
        execution_generation.get()
    ))
}

pub(super) fn validate_continuity(
    previous: &RuntimeObservation,
    next: &RuntimeObservation,
) -> RuntimeResult<()> {
    let Some(previous_attestation) = previous.provider_attestation.as_ref() else {
        return Ok(());
    };
    if previous.provider_resource_id != next.provider_resource_id {
        return Ok(());
    }
    let next_attestation = next.provider_attestation.as_ref().ok_or_else(|| {
        RuntimeError::Protocol(
            "Box dropped confidential attestation without changing provider identity".into(),
        )
    })?;
    let previous_generation = execution_generation_claim(previous)?;
    let next_generation = execution_generation_claim(next)?;
    if previous_generation == next_generation && previous_attestation != next_attestation {
        return Err(RuntimeError::Protocol(format!(
            "Box changed confidential attestation without changing execution generation {next_generation}"
        )));
    }
    Ok(())
}

fn execution_generation_claim(observation: &RuntimeObservation) -> RuntimeResult<u64> {
    let value = observation
        .evidence
        .as_ref()
        .and_then(|evidence| evidence.claims.get(EXECUTION_GENERATION_CLAIM))
        .ok_or_else(|| {
            RuntimeError::Protocol(
                "Box confidential observation is missing execution-generation evidence".into(),
            )
        })?;
    value
        .parse::<u64>()
        .ok()
        .filter(|value| *value > 0)
        .ok_or_else(|| {
            RuntimeError::Protocol(
                "Box confidential observation has invalid execution-generation evidence".into(),
            )
        })
}

fn attestation_mode(config: &BoxRuntimeSevSnpConfig) -> &'static str {
    if config.simulate {
        "sev-snp-simulated"
    } else {
        "sev-snp-hardware"
    }
}

fn runtime_binding(spec: &RuntimeUnitSpec) -> RuntimeResult<[u8; RUNTIME_BINDING_SIZE]> {
    let digest = spec.digest().map_err(RuntimeError::Protocol)?;
    let encoded = digest.strip_prefix("sha256:").ok_or_else(|| {
        RuntimeError::Protocol("Box SEV-SNP attestation requires a SHA-256 spec digest".into())
    })?;
    let decoded = hex::decode(encoded).map_err(|error| {
        RuntimeError::Protocol(format!(
            "Box SEV-SNP attestation spec digest is malformed: {error}"
        ))
    })?;
    decoded.try_into().map_err(|_| {
        RuntimeError::Protocol("Box SEV-SNP attestation spec digest is not exactly 32 bytes".into())
    })
}

fn validate_acquired(
    payload: &BoxAttestationPayload,
    config: &BoxRuntimeSevSnpConfig,
    expected_runtime_binding: &[u8; RUNTIME_BINDING_SIZE],
) -> RuntimeResult<()> {
    validate_payload(payload, config, expected_runtime_binding).map_err(|message| {
        RuntimeError::ProviderUnavailable(format!(
            "Box rejected the acquired SEV-SNP attestation: {message}"
        ))
    })
}

fn validate_persisted(
    bytes: &[u8],
    spec: &RuntimeUnitSpec,
    record: &BoxRecord,
    execution_generation: ExecutionGeneration,
    config: &BoxRuntimeSevSnpConfig,
    expected_runtime_binding: &[u8; RUNTIME_BINDING_SIZE],
) -> RuntimeResult<()> {
    let stored: StoredAttestation = serde_json::from_slice(bytes).map_err(|error| {
        RuntimeError::Protocol(format!(
            "Box persisted SEV-SNP attestation is malformed: {error}"
        ))
    })?;
    let spec_digest = spec.digest().map_err(RuntimeError::Protocol)?;
    if stored.schema != ATTESTATION_SCHEMA
        || stored.unit_id != spec.unit_id
        || stored.runtime_generation != spec.generation
        || stored.spec_digest != spec_digest
        || stored.provider_resource_id != record.id
        || stored.execution_generation != execution_generation.get()
        || stored.mode != attestation_mode(config)
        || stored.policy != config.attestation_policy
    {
        return Err(RuntimeError::Protocol(
            "Box persisted SEV-SNP attestation does not match the Runtime execution identity or policy"
                .into(),
        ));
    }
    validate_payload(
        &BoxAttestationPayload {
            report: stored.report,
            certificate_der: stored.ratls_certificate,
        },
        config,
        expected_runtime_binding,
    )
    .map_err(|message| {
        RuntimeError::Protocol(format!(
            "Box persisted SEV-SNP attestation failed verification: {message}"
        ))
    })
}

fn validate_payload(
    payload: &BoxAttestationPayload,
    config: &BoxRuntimeSevSnpConfig,
    expected_runtime_binding: &[u8; RUNTIME_BINDING_SIZE],
) -> Result<(), String> {
    validate_report(&payload.report, config, expected_runtime_binding)?;
    if payload.certificate_der.is_empty() {
        if config.simulate {
            return Ok(());
        }
        return Err("hardware report is missing its live RA-TLS certificate".into());
    }
    let embedded = crate::tee::ratls::extract_report_from_cert(&payload.certificate_der)
        .map_err(|error| error.to_string())?;
    if embedded != payload.report {
        return Err("RA-TLS certificate does not contain the persisted report".into());
    }
    if !crate::tee::ratls::verify_pubkey_binding(&payload.certificate_der, &payload.report.report)
        .map_err(|error| error.to_string())?
    {
        return Err("RA-TLS certificate public key is not bound to the SNP report".into());
    }
    Ok(())
}

fn validate_report(
    report: &AttestationReport,
    config: &BoxRuntimeSevSnpConfig,
    expected_runtime_binding: &[u8; RUNTIME_BINDING_SIZE],
) -> Result<(), String> {
    if report.report.len() != SNP_REPORT_SIZE {
        return Err(format!(
            "expected {SNP_REPORT_SIZE} report bytes, got {}",
            report.report.len()
        ));
    }
    if is_simulated_report(&report.report) != config.simulate {
        return Err(format!(
            "report mode does not match configured {} mode",
            attestation_mode(config)
        ));
    }
    let parsed_platform = parse_platform_info(&report.report)
        .ok_or_else(|| "report platform fields could not be parsed".to_string())?;
    if report.platform != parsed_platform {
        return Err("report platform metadata does not match the signed report bytes".into());
    }
    if report.report[RUNTIME_BINDING_OFFSET..RUNTIME_BINDING_OFFSET + RUNTIME_BINDING_SIZE]
        != expected_runtime_binding[..]
    {
        return Err("report is not bound to the exact Runtime specification".into());
    }
    let report_data = &report.report[0x50..0x90];
    let verification = verify_attestation(
        report,
        report_data,
        &config.attestation_policy,
        config.simulate,
    )
    .map_err(|error| error.to_string())?;
    if !verification.verified {
        return Err(verification.failures.join("; "));
    }
    Ok(())
}

async fn read_existing(path: &Path) -> RuntimeResult<Option<Vec<u8>>> {
    let mut options = tokio::fs::OpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let file = match options.open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => {
            if let Ok(metadata) = tokio::fs::symlink_metadata(path).await {
                if metadata.file_type().is_symlink() || !metadata.is_file() {
                    return Err(RuntimeError::Protocol(format!(
                        "Box persisted SEV-SNP attestation {} is not a regular file",
                        path.display()
                    )));
                }
            }
            return Err(RuntimeError::ProviderUnavailable(format!(
                "Box could not open persisted SEV-SNP attestation {}: {error}",
                path.display()
            )));
        }
    };
    let metadata = file.metadata().await.map_err(|error| {
        RuntimeError::ProviderUnavailable(format!(
            "Box could not inspect persisted SEV-SNP attestation {}: {error}",
            path.display()
        ))
    })?;
    if !metadata.is_file() || metadata.len() > MAX_ATTESTATION_BYTES {
        return Err(RuntimeError::Protocol(format!(
            "Box persisted SEV-SNP attestation {} is not a bounded regular file",
            path.display()
        )));
    }
    use std::os::unix::fs::MetadataExt as _;
    if metadata.uid() != unsafe { libc::geteuid() } || metadata.mode() & 0o077 != 0 {
        return Err(RuntimeError::Protocol(format!(
            "Box persisted SEV-SNP attestation {} is not private to the current user",
            path.display()
        )));
    }
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_ATTESTATION_BYTES + 1)
        .read_to_end(&mut bytes)
        .await
        .map_err(|error| {
            RuntimeError::ProviderUnavailable(format!(
                "Box could not read persisted SEV-SNP attestation {}: {error}",
                path.display()
            ))
        })?;
    if bytes.len() as u64 > MAX_ATTESTATION_BYTES {
        return Err(RuntimeError::Protocol(format!(
            "Box persisted SEV-SNP attestation {} exceeds the size limit",
            path.display()
        )));
    }
    Ok(Some(bytes))
}

async fn persist_new(path: &Path, bytes: &[u8]) -> RuntimeResult<PublishOutcome> {
    if bytes.len() as u64 > MAX_ATTESTATION_BYTES {
        return Err(RuntimeError::Protocol(
            "Box SEV-SNP attestation artifact exceeds the size limit".into(),
        ));
    }
    let parent = path.parent().ok_or_else(|| {
        RuntimeError::Protocol("Box SEV-SNP attestation path has no parent".into())
    })?;
    tokio::fs::create_dir_all(parent).await.map_err(|error| {
        RuntimeError::ProviderUnavailable(format!(
            "Box could not create the SEV-SNP attestation directory {}: {error}",
            parent.display()
        ))
    })?;
    let parent_metadata = tokio::fs::symlink_metadata(parent).await.map_err(|error| {
        RuntimeError::ProviderUnavailable(format!(
            "Box could not inspect the SEV-SNP attestation directory {}: {error}",
            parent.display()
        ))
    })?;
    if parent_metadata.file_type().is_symlink() || !parent_metadata.is_dir() {
        return Err(RuntimeError::Protocol(format!(
            "Box SEV-SNP attestation directory {} is not a regular directory",
            parent.display()
        )));
    }

    let temp = parent.join(format!(".runtime-attestation-{}.tmp", uuid::Uuid::new_v4()));
    let mut options = tokio::fs::OpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC);
    let mut file = options.open(&temp).await.map_err(|error| {
        RuntimeError::ProviderUnavailable(format!(
            "Box could not create a temporary SEV-SNP attestation artifact: {error}"
        ))
    })?;
    let result = async {
        file.write_all(bytes).await?;
        file.flush().await?;
        file.sync_all().await?;
        drop(file);
        match tokio::fs::hard_link(&temp, path).await {
            Ok(()) => {
                tokio::fs::remove_file(&temp).await?;
                sync_directory(parent).await?;
                Ok(PublishOutcome::Created)
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {
                tokio::fs::remove_file(&temp).await?;
                Ok(PublishOutcome::Existing)
            }
            Err(error) => Err(error),
        }
    }
    .await;
    match result {
        Ok(outcome) => Ok(outcome),
        Err(error) => {
            let _ = tokio::fs::remove_file(&temp).await;
            Err(RuntimeError::ProviderUnavailable(format!(
                "Box could not persist the SEV-SNP attestation artifact {}: {error}",
                path.display()
            )))
        }
    }
}

async fn sync_directory(path: &Path) -> std::io::Result<()> {
    let path = path.to_path_buf();
    tokio::task::spawn_blocking(move || {
        let directory = std::fs::File::open(path)?;
        directory.sync_all()
    })
    .await
    .map_err(std::io::Error::other)?
}

fn artifact_reference(
    record: &BoxRecord,
    execution_generation: ExecutionGeneration,
    bytes: &[u8],
) -> RuntimeResult<ArtifactRef> {
    let digest = format!("sha256:{:x}", Sha256::digest(bytes));
    let artifact = ArtifactRef {
        uri: format!(
            "a3s-box-attestation://{}/executions/{}/{}",
            record.id,
            execution_generation.get(),
            digest.trim_start_matches("sha256:")
        ),
        digest,
        media_type: ATTESTATION_MEDIA_TYPE.into(),
    };
    artifact.validate().map_err(RuntimeError::Protocol)?;
    Ok(artifact)
}
