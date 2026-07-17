//! Opt-in real-provider certification for the Box Sandbox Runtime driver.
//!
//! This module is deliberately compiled only for tests and its single test is
//! ignored by default. The exact test name, explicit acknowledgement variable,
//! dedicated home, certified runtime, pinned image, and single-threaded test
//! selection are all release-gate prerequisites.

mod cases;
mod exec_profile;
mod fixture;
mod logs_profile;
mod networking_profile;
mod recovery_profile;
mod resources_profile;
mod security_profile;

use std::fmt::Display;

use a3s_runtime::{
    required_runtime_profiles, verify_runtime_profiles, RuntimeClient, RuntimeConformanceProfile,
    RuntimeError, RuntimeResult,
};

use self::fixture::BoxRuntimeConformanceFixture;

type Result<T> = RuntimeResult<T>;

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

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "requires a dedicated A3S OS Sandbox certification home"]
async fn box_runtime_passes_all_advertised_profiles() {
    let fixture = BoxRuntimeConformanceFixture::from_environment()
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
        RuntimeConformanceProfile::Resources,
        RuntimeConformanceProfile::Logs,
        RuntimeConformanceProfile::Exec,
        RuntimeConformanceProfile::Security,
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
}
