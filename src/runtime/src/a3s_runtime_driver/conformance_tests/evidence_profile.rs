use std::path::Path;

use a3s_runtime::contract::{
    ArtifactRef, IsolationLevel, RuntimeInspection, RuntimeObservation, RuntimeUnitSpec,
    RuntimeUnitState,
};
use a3s_runtime::RuntimeClient;
use serde::Deserialize;
use sha2::{Digest, Sha256};

use crate::tee::attestation::SNP_REPORT_SIZE;
use crate::tee::{
    is_simulated_report, parse_platform_info, verify_attestation, AttestationPolicy,
    AttestationReport,
};

use super::super::attestation::{attestation_path, ATTESTATION_MEDIA_TYPE};
use super::fixture::BoxRuntimeConformanceFixture;
use super::{require, Result};

const ATTESTATION_SCHEMA: &str = "a3s.box.runtime.attestation.v1";
const SEMANTICS_PROFILE_DIGEST: &str =
    "sha256:8d65d845f5e5523e34fe91ffbebc35315bc814a2c48b76da1f4e82f20e09f78d";
const IDENTITY_ATTACHMENT_DIGEST: &str =
    "sha256:8a29be89b1fa2103fe694ec9588705774bf279c4651bc0891977f84a7a3d05c1";
const REPORT_DATA_OFFSET: usize = 0x50;
const REPORT_DATA_SIZE: usize = 64;
const RUNTIME_BINDING_OFFSET: usize = REPORT_DATA_OFFSET + 32;
const RUNTIME_BINDING_SIZE: usize = 32;

#[derive(Deserialize)]
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

pub(super) async fn run(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let config = fixture.sev_snp_config().ok_or_else(|| {
        super::protocol("Evidence profile requires explicit SEV-SNP configuration")
    })?;

    let mut task_request = fixture.cases.task(
        "evidence-confidential-task",
        "printf 'r17-confidential-task-attested\\n'",
        10_000,
    );
    task_request.spec.isolation = IsolationLevel::Confidential;
    task_request.spec.semantics_profile_digest = Some(SEMANTICS_PROFILE_DIGEST.into());
    task_request.spec.identity_attachment_digest = Some(IDENTITY_ATTACHMENT_DIGEST.into());

    let succeeded = client.apply(&task_request).await?;
    let task_attestation = verify_observation(
        &succeeded,
        &task_request.spec,
        config.simulate,
        RuntimeUnitState::Succeeded,
    )?;
    let task_provider_id = succeeded
        .provider_resource_id
        .as_deref()
        .ok_or_else(|| super::protocol("confidential Task omitted provider identity"))?;
    let task_record = fixture.record_for(&task_request.spec).await?;
    let task_metadata = task_record
        .managed_execution
        .as_ref()
        .ok_or_else(|| super::protocol("confidential Task lost managed metadata"))?;
    require(
        task_metadata.request.config.deferred_main,
        "confidential Task was not protected by the attestation-before-execution gate",
    )?;
    let task_generation = task_metadata.generation;
    let task_path = attestation_path(&task_record, task_generation);
    let task_bytes = std::fs::read(&task_path)
        .map_err(|error| super::external("read confidential Task attestation", error))?;
    verify_artifact(
        &task_path,
        &task_bytes,
        &task_attestation,
        &task_request.spec,
        task_provider_id,
        task_generation.get(),
        config,
    )?;
    fixture
        .remove_unit(client, &task_request.spec, "evidence-confidential-task")
        .await?;
    require(
        !task_path.exists(),
        "confidential Task removal retained its execution attestation artifact",
    )?;

    let mut request = fixture.cases.service(
        "evidence-confidential-service",
        "printf 'r17-confidential-ready\\n'; exec sleep 3600",
    );
    request.spec.isolation = IsolationLevel::Confidential;
    request.spec.semantics_profile_digest = Some(SEMANTICS_PROFILE_DIGEST.into());
    request.spec.identity_attachment_digest = Some(IDENTITY_ATTACHMENT_DIGEST.into());

    let running = client.apply(&request).await?;
    let first_attestation = verify_observation(
        &running,
        &request.spec,
        config.simulate,
        RuntimeUnitState::Running,
    )?;
    let first_provider_id = running
        .provider_resource_id
        .as_deref()
        .ok_or_else(|| super::protocol("confidential observation omitted provider identity"))?;

    let record = fixture.record_for(&request.spec).await?;
    let execution_generation = record
        .managed_execution
        .as_ref()
        .ok_or_else(|| super::protocol("confidential execution lost managed metadata"))?
        .generation;
    let path = attestation_path(&record, execution_generation);
    let bytes = std::fs::read(&path)
        .map_err(|error| super::external("read persisted SEV-SNP attestation", error))?;
    verify_artifact(
        &path,
        &bytes,
        &first_attestation,
        &request.spec,
        first_provider_id,
        execution_generation.get(),
        config,
    )?;

    let inspected = found(client.inspect(&request.spec.unit_id).await?)?;
    let inspected_attestation = verify_observation(
        &inspected,
        &request.spec,
        config.simulate,
        RuntimeUnitState::Running,
    )?;
    require(
        inspected.provider_resource_id == running.provider_resource_id
            && inspected_attestation == first_attestation,
        "live reinspection changed identity or attestation within one execution generation",
    )?;

    let restarted_driver = fixture.restarted_driver()?;
    let restarted = fixture.client_with(restarted_driver, fixture.state.clone());
    let recovered = found(restarted.inspect(&request.spec.unit_id).await?)?;
    let recovered_attestation = verify_observation(
        &recovered,
        &request.spec,
        config.simulate,
        RuntimeUnitState::Running,
    )?;
    require(
        recovered.provider_resource_id == running.provider_resource_id
            && recovered_attestation == first_attestation,
        "driver reconstruction did not preserve and live-verify the exact attestation",
    )?;

    let mut tampered: serde_json::Value = serde_json::from_slice(&bytes)
        .map_err(|error| super::external("decode attestation tamper fixture", error))?;
    tampered["spec_digest"] = serde_json::Value::String(format!("sha256:{}", "0".repeat(64)));
    let tampered = serde_json::to_vec(&tampered)
        .map_err(|error| super::external("encode attestation tamper fixture", error))?;
    std::fs::write(&path, tampered)
        .map_err(|error| super::external("write attestation tamper fixture", error))?;
    let rejected = restarted.inspect(&request.spec.unit_id).await;
    std::fs::write(&path, &bytes)
        .map_err(|error| super::external("restore attestation after tamper fixture", error))?;
    require(
        rejected.is_err(),
        "Runtime inspection accepted a tampered persisted attestation",
    )?;
    let restored = found(restarted.inspect(&request.spec.unit_id).await?)?;
    require(
        verify_observation(
            &restored,
            &request.spec,
            config.simulate,
            RuntimeUnitState::Running,
        )? == first_attestation,
        "restored attestation did not recover the exact live evidence",
    )?;

    fixture
        .remove_unit(&restarted, &request.spec, "evidence-confidential-service")
        .await?;
    require(
        !path.exists(),
        "confidential removal retained the execution attestation artifact",
    )
}

fn found(inspection: RuntimeInspection) -> Result<RuntimeObservation> {
    match inspection {
        RuntimeInspection::Found { observation, .. } => Ok(*observation),
        RuntimeInspection::NotFound { .. } => Err(super::protocol(
            "confidential Service disappeared during Evidence certification",
        )),
    }
}

fn verify_observation(
    observation: &RuntimeObservation,
    spec: &RuntimeUnitSpec,
    simulated: bool,
    expected_state: RuntimeUnitState,
) -> Result<ArtifactRef> {
    observation
        .validate_against(spec)
        .map_err(super::protocol)?;
    require(
        observation.state == expected_state,
        format!(
            "Evidence fixture expected confidential unit state {expected_state:?}, observed {:?}",
            observation.state
        ),
    )?;
    let evidence = observation
        .evidence
        .as_ref()
        .ok_or_else(|| super::protocol("confidential observation omitted Runtime evidence"))?;
    require(
        evidence.spec_digest == spec.digest().map_err(super::protocol)?
            && evidence.semantics_profile_digest == spec.semantics_profile_digest
            && evidence.identity_attachment_digest == spec.identity_attachment_digest
            && observation.provider_build.as_ref() == Some(&evidence.provider_build),
        "Runtime evidence did not bind the exact spec, semantics profile, identity attachment, and provider build",
    )?;
    require(
        evidence
            .claims
            .get("a3s.box.execution-isolation")
            .map(String::as_str)
            == Some("microvm"),
        "confidential Runtime evidence did not identify MicroVM isolation",
    )?;
    let expected_mode = if simulated {
        "sev-snp-simulated"
    } else {
        "sev-snp-hardware"
    };
    require(
        evidence.claims.get("a3s.box.tee").map(String::as_str) == Some(expected_mode)
            && evidence
                .provider_build
                .contains(&format!("tee/{expected_mode}")),
        "confidential Runtime evidence reported the wrong TEE mode",
    )?;
    require(
        evidence
            .claims
            .get("a3s.box.execution-generation")
            .and_then(|value| value.parse::<u64>().ok())
            .is_some_and(|value| value > 0),
        "confidential Runtime evidence omitted a valid execution generation",
    )?;
    observation
        .provider_attestation
        .clone()
        .ok_or_else(|| super::protocol("confidential observation omitted provider attestation"))
}

#[allow(clippy::too_many_arguments)]
fn verify_artifact(
    path: &Path,
    bytes: &[u8],
    reference: &ArtifactRef,
    spec: &RuntimeUnitSpec,
    provider_resource_id: &str,
    execution_generation: u64,
    config: &super::super::BoxRuntimeSevSnpConfig,
) -> Result<()> {
    verify_private_regular_file(path)?;
    let digest = format!("sha256:{:x}", Sha256::digest(bytes));
    require(
        reference.digest == digest
            && reference.media_type == ATTESTATION_MEDIA_TYPE
            && reference.uri
                == format!(
                    "a3s-box-attestation://{provider_resource_id}/executions/{execution_generation}/{}",
                    digest.trim_start_matches("sha256:")
                ),
        "attestation Artifact reference did not bind its exact immutable bytes and execution",
    )?;

    let stored: StoredAttestation = serde_json::from_slice(bytes)
        .map_err(|error| super::external("decode persisted SEV-SNP attestation", error))?;
    let expected_mode = if config.simulate {
        "sev-snp-simulated"
    } else {
        "sev-snp-hardware"
    };
    require(
        stored.schema == ATTESTATION_SCHEMA
            && stored.unit_id == spec.unit_id
            && stored.runtime_generation == spec.generation
            && stored.spec_digest == spec.digest().map_err(super::protocol)?
            && stored.provider_resource_id == provider_resource_id
            && stored.execution_generation == execution_generation
            && stored.mode == expected_mode
            && stored.policy == config.attestation_policy,
        "persisted attestation did not bind the Runtime and Box execution identity or policy",
    )?;
    require(
        stored.report.report.len() == SNP_REPORT_SIZE
            && is_simulated_report(&stored.report.report) == config.simulate
            && parse_platform_info(&stored.report.report).as_ref() == Some(&stored.report.platform),
        "persisted attestation report mode or platform metadata was invalid",
    )?;

    let spec_digest = spec.digest().map_err(super::protocol)?;
    let expected_binding = hex::decode(
        spec_digest
            .strip_prefix("sha256:")
            .ok_or_else(|| super::protocol("Runtime spec digest was not SHA-256"))?,
    )
    .map_err(|error| super::external("decode Runtime spec binding", error))?;
    require(
        stored.report.report[RUNTIME_BINDING_OFFSET..RUNTIME_BINDING_OFFSET + RUNTIME_BINDING_SIZE]
            == expected_binding,
        "SEV-SNP report_data did not bind the exact Runtime specification",
    )?;
    let report_data =
        &stored.report.report[REPORT_DATA_OFFSET..REPORT_DATA_OFFSET + REPORT_DATA_SIZE];
    let verification = verify_attestation(
        &stored.report,
        report_data,
        &config.attestation_policy,
        config.simulate,
    )
    .map_err(|error| super::external("verify persisted SEV-SNP report", error))?;
    require(
        verification.verified,
        format!(
            "persisted SEV-SNP report failed policy verification: {}",
            verification.failures.join("; ")
        ),
    )?;
    require(
        !stored.ratls_certificate.is_empty(),
        "real-provider Evidence certification requires the live RA-TLS certificate",
    )?;
    let embedded = crate::tee::ratls::extract_report_from_cert(&stored.ratls_certificate)
        .map_err(|error| super::external("extract report from RA-TLS certificate", error))?;
    require(
        embedded == stored.report
            && crate::tee::ratls::verify_pubkey_binding(
                &stored.ratls_certificate,
                &stored.report.report,
            )
            .map_err(|error| super::external("verify RA-TLS public-key binding", error))?,
        "RA-TLS certificate did not contain and key-bind the persisted report",
    )
}

#[cfg(unix)]
fn verify_private_regular_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::MetadataExt;

    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| super::external("inspect persisted attestation permissions", error))?;
    require(
        metadata.is_file()
            && !metadata.file_type().is_symlink()
            && metadata.uid() == unsafe { libc::geteuid() }
            && metadata.mode() & 0o077 == 0,
        "persisted attestation is not a private same-owner regular file",
    )
}

#[cfg(not(unix))]
fn verify_private_regular_file(path: &Path) -> Result<()> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| super::external("inspect persisted attestation file", error))?;
    require(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "persisted attestation is not a regular file",
    )
}
