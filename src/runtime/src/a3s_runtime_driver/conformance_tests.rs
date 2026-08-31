//! Opt-in real-provider certification for the Box Runtime driver.
//!
//! This module is deliberately compiled only for tests and its destructive
//! tests are ignored by default. The exact test name, explicit acknowledgement
//! variable, dedicated home, pinned runtime artifacts, pinned image, and
//! single-threaded test selection are all release-gate prerequisites.

mod cases;
mod evidence_profile;
mod exec_profile;
mod fixture;
mod health_profile;
mod logs_profile;
mod mounts_evidence;
mod mounts_profile;
mod networking_profile;
mod outputs_profile;
mod private_registry_profile;
mod recovery_profile;
mod resources_profile;
mod security_evidence;
mod security_profile;
mod service_lifecycle_profile;

use std::fmt::Display;

use a3s_box_core::{config::SevSnpGeneration, ExecutionIsolation};
use a3s_runtime::{
    required_runtime_profiles, verify_runtime_profiles, RuntimeClient, RuntimeConformanceProfile,
    RuntimeError, RuntimeResult,
};

use self::fixture::BoxRuntimeConformanceFixture;
use super::BoxRuntimeSevSnpConfig;
use crate::tee::AttestationPolicy;

type Result<T> = RuntimeResult<T>;

const R17_RUNNER_STACK_BYTES: usize = 32 * 1024 * 1024;
const R17_WORKER_STACK_BYTES: usize = 8 * 1024 * 1024;
pub(super) const PRIVATE_REGISTRY_SECRET_REFERENCE: &str = "secret://r17/registry-credential/v1";
pub(super) const PRIVATE_REGISTRY_USERNAME: &str = "r17-registry-user";
pub(super) const PRIVATE_REGISTRY_PASSWORD: &str = "r17-registry-password-long";

fn failure(message: impl Into<String>) -> RuntimeError {
    RuntimeError::ProviderUnavailable(message.into())
}

fn protocol(message: impl Into<String>) -> RuntimeError {
    RuntimeError::Protocol(message.into())
}

fn invalid(message: impl Into<String>) -> RuntimeError {
    RuntimeError::InvalidRequest(message.into())
}

fn external(context: &str, error: impl Display) -> RuntimeError {
    RuntimeError::ProviderUnavailable(format!("{context}: {error}"))
}

fn require(condition: bool, message: impl Into<String>) -> Result<()> {
    if condition {
        Ok(())
    } else {
        Err(protocol(message))
    }
}

#[test]
#[ignore = "requires a dedicated A3S OS Sandbox certification home"]
fn box_runtime_passes_all_advertised_profiles() {
    let runner = std::thread::Builder::new()
        .name("a3s-box-r17".into())
        .stack_size(R17_RUNNER_STACK_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .thread_name("a3s-box-r17-worker")
                .thread_stack_size(R17_WORKER_STACK_BYTES)
                .enable_all()
                .build()
                .expect("R17 Tokio runtime must start");
            runtime.block_on(run_all_advertised_profiles(
                ExecutionIsolation::Sandbox,
                None,
            ));
        })
        .expect("R17 runner thread must start");
    runner.join().expect("R17 runner thread must not panic");
}

async fn run_all_advertised_profiles(
    execution_isolation: ExecutionIsolation,
    sev_snp: Option<BoxRuntimeSevSnpConfig>,
) {
    let attestation_expected = sev_snp.is_some();
    let fixture = match sev_snp {
        Some(config) => BoxRuntimeConformanceFixture::from_environment_with_sev_snp(
            execution_isolation,
            Some(config),
        ),
        None => BoxRuntimeConformanceFixture::from_environment(execution_isolation),
    }
    .expect("R17 Box conformance preflight must pass");
    let client = fixture.primary_client();
    let capabilities = client
        .capabilities()
        .await
        .expect("R17 Box capabilities must be available");
    let required = required_runtime_profiles(&capabilities)
        .expect("R17 Box capabilities must derive valid profiles");
    let mut expected = [
        RuntimeConformanceProfile::Base,
        RuntimeConformanceProfile::Recovery,
        RuntimeConformanceProfile::Networking,
        RuntimeConformanceProfile::Mounts,
        RuntimeConformanceProfile::Health,
        RuntimeConformanceProfile::Resources,
        RuntimeConformanceProfile::Logs,
        RuntimeConformanceProfile::Exec,
        RuntimeConformanceProfile::Security,
        RuntimeConformanceProfile::Outputs,
    ]
    .into_iter()
    .collect::<std::collections::BTreeSet<_>>();
    if attestation_expected {
        expected.insert(RuntimeConformanceProfile::Evidence);
    }
    assert_eq!(
        required, expected,
        "R17 must execute every profile activated by Box capabilities"
    );

    let report = verify_runtime_profiles(&client, &fixture)
        .await
        .expect("R17 Box real-provider conformance must pass");
    assert_eq!(report.inventory_after, report.inventory_before);
    assert_eq!(
        report
            .profiles
            .iter()
            .map(|evidence| evidence.profile)
            .collect::<std::collections::BTreeSet<_>>(),
        expected
    );

    private_registry_profile::run(&fixture)
        .await
        .expect("R17 authenticated private-registry pull must pass");
}

#[test]
#[ignore = "requires a dedicated A3S OS KVM MicroVM certification home"]
fn box_runtime_microvm_passes_all_advertised_profiles() {
    let runner = std::thread::Builder::new()
        .name("a3s-box-r17-microvm".into())
        .stack_size(R17_RUNNER_STACK_BYTES)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .thread_name("a3s-box-r17-microvm-worker")
                .thread_stack_size(R17_WORKER_STACK_BYTES)
                .enable_all()
                .build()
                .expect("R17 MicroVM Tokio runtime must start");
            runtime.block_on(run_all_advertised_profiles(
                ExecutionIsolation::Microvm,
                None,
            ));
        })
        .expect("R17 MicroVM runner thread must start");
    runner
        .join()
        .expect("R17 MicroVM runner thread must not panic");
}

#[test]
#[ignore = "requires a dedicated A3S OS KVM MicroVM certification home"]
fn box_runtime_sev_snp_simulated_passes_all_advertised_profiles() {
    run_confidential_profiles(
        "a3s-box-r17-sev-snp-simulated",
        BoxRuntimeSevSnpConfig {
            generation: SevSnpGeneration::Milan,
            simulate: true,
            attestation_policy: AttestationPolicy::default(),
        },
    );
}

#[test]
#[ignore = "requires a dedicated AMD SEV-SNP hardware certification home"]
fn box_runtime_sev_snp_hardware_passes_all_advertised_profiles() {
    run_confidential_profiles(
        "a3s-box-r17-sev-snp-hardware",
        hardware_sev_snp_config().expect("R17 SEV-SNP hardware policy must be explicit"),
    );
}

fn run_confidential_profiles(thread_name: &str, config: BoxRuntimeSevSnpConfig) {
    let thread_name = thread_name.to_owned();
    let runner = std::thread::Builder::new()
        .name(thread_name)
        .stack_size(R17_RUNNER_STACK_BYTES)
        .spawn(move || {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .worker_threads(4)
                .thread_name("a3s-box-r17-sev-snp-worker")
                .thread_stack_size(R17_WORKER_STACK_BYTES)
                .enable_all()
                .build()
                .expect("R17 SEV-SNP Tokio runtime must start");
            runtime.block_on(run_all_advertised_profiles(
                ExecutionIsolation::Microvm,
                Some(config),
            ));
        })
        .expect("R17 SEV-SNP runner thread must start");
    runner
        .join()
        .expect("R17 SEV-SNP runner thread must not panic");
}

fn hardware_sev_snp_config() -> Result<BoxRuntimeSevSnpConfig> {
    let generation =
        match std::env::var("A3S_BOX_RUNTIME_CONFORMANCE_SEV_SNP_GENERATION").as_deref() {
            Ok("milan") => SevSnpGeneration::Milan,
            Ok("genoa") => SevSnpGeneration::Genoa,
            _ => {
                return Err(failure(
                    "A3S_BOX_RUNTIME_CONFORMANCE_SEV_SNP_GENERATION must be exactly milan or genoa",
                ))
            }
        };
    let measurement = std::env::var("A3S_BOX_RUNTIME_CONFORMANCE_SEV_SNP_MEASUREMENT")
        .map_err(|_| {
            failure(
                "A3S_BOX_RUNTIME_CONFORMANCE_SEV_SNP_MEASUREMENT must pin the expected launch measurement",
            )
        })?;
    require(
        measurement.len() == 96
            && measurement
                .bytes()
                .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte)),
        "A3S_BOX_RUNTIME_CONFORMANCE_SEV_SNP_MEASUREMENT must be 96 lowercase hex characters",
    )?;
    Ok(BoxRuntimeSevSnpConfig {
        generation,
        simulate: false,
        attestation_policy: AttestationPolicy {
            expected_measurement: Some(measurement),
            ..AttestationPolicy::default()
        },
    })
}
