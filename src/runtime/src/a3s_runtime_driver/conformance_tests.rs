//! Opt-in real-provider certification for the Box Sandbox Runtime driver.
//!
//! This module is deliberately compiled only for tests and its destructive
//! tests are ignored by default. The exact test name, explicit acknowledgement
//! variable, dedicated home, pinned runtime artifacts, pinned image, and
//! single-threaded test selection are all release-gate prerequisites.

mod cases;
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
mod security_profile;

use std::fmt::Display;

use a3s_box_core::ExecutionIsolation;
use a3s_runtime::{
    required_runtime_profiles, verify_runtime_base, verify_runtime_profiles, RuntimeClient,
    RuntimeConformanceFixture, RuntimeConformanceProfile, RuntimeError, RuntimeResult,
};

use self::fixture::BoxRuntimeConformanceFixture;

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
            runtime.block_on(run_all_advertised_profiles());
        })
        .expect("R17 runner thread must start");
    runner.join().expect("R17 runner thread must not panic");
}

async fn run_all_advertised_profiles() {
    let fixture = BoxRuntimeConformanceFixture::from_environment(ExecutionIsolation::Sandbox)
        .expect("R17 Box conformance preflight must pass");
    let client = fixture.primary_client();
    let capabilities = client
        .capabilities()
        .await
        .expect("R17 Box capabilities must be available");
    let required = required_runtime_profiles(&capabilities)
        .expect("R17 Box capabilities must derive valid profiles");
    let expected = [
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
    .collect();
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
fn box_runtime_microvm_passes_supported_profiles() {
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
            runtime.block_on(run_microvm_supported_profiles());
        })
        .expect("R17 MicroVM runner thread must start");
    runner
        .join()
        .expect("R17 MicroVM runner thread must not panic");
}

async fn run_microvm_supported_profiles() {
    let fixture = BoxRuntimeConformanceFixture::from_environment(ExecutionIsolation::Microvm)
        .expect("R17 MicroVM conformance preflight must pass");
    let client = fixture.primary_client();
    let inventory_before = fixture
        .inventory()
        .await
        .expect("R17 MicroVM baseline inventory must be available");

    let execution: Result<()> = async {
        verify_runtime_base(&client, fixture.base_case()).await?;
        let capabilities = client.capabilities().await?;
        for profile in [
            RuntimeConformanceProfile::Recovery,
            RuntimeConformanceProfile::Networking,
            RuntimeConformanceProfile::Mounts,
            RuntimeConformanceProfile::Health,
            RuntimeConformanceProfile::Logs,
            RuntimeConformanceProfile::Exec,
            RuntimeConformanceProfile::Outputs,
        ] {
            let evidence = fixture.run_profile(&client, &capabilities, profile).await?;
            require(
                evidence.profile == profile,
                format!(
                    "R17 MicroVM {} profile returned mismatched evidence",
                    profile.as_str()
                ),
            )?;
        }
        Ok(())
    }
    .await;

    let cleanup = fixture.cleanup().await;
    let inventory_after = fixture
        .inventory()
        .await
        .expect("R17 MicroVM final inventory must be available");
    cleanup.expect("R17 MicroVM cleanup must succeed");
    assert_eq!(inventory_after, inventory_before);
    execution.expect(
        "R17 MicroVM Base, Recovery, Networking, Mounts, Health, Logs, Exec, and Outputs profiles must pass",
    );
}
