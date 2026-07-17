use std::collections::BTreeMap;
use std::time::Duration;

use a3s_box_core::ExecutionManager;
use a3s_runtime::contract::{
    ArtifactRef, IsolationLevel, NetworkMode, ResourceControl, ResourceLimits, RestartPolicy,
    RuntimeFeature, RuntimeNetworkSpec, RuntimeProcessSpec, RuntimeUnitClass, RuntimeUnitSpec,
};
use a3s_runtime::RuntimeDriver;

use super::mapping::{creation_request, operation};
use super::metadata::validate_record_for_spec;
use super::*;

fn spec(class: RuntimeUnitClass) -> RuntimeUnitSpec {
    RuntimeUnitSpec {
        schema: RuntimeUnitSpec::SCHEMA.into(),
        unit_id: "box-runtime-test".into(),
        generation: 7,
        class,
        artifact: ArtifactRef {
            uri: format!(
                "oci://registry.example/a3s/runtime@sha256:{}",
                "a".repeat(64)
            ),
            digest: format!("sha256:{}", "a".repeat(64)),
            media_type: OCI_IMAGE_MANIFEST.into(),
        },
        process: RuntimeProcessSpec {
            command: vec!["/bin/sh".into(), "-c".into()],
            args: vec!["echo ready".into()],
            working_directory: Some("/work".into()),
            environment: BTreeMap::from([("LANG".into(), "C.UTF-8".into())]),
        },
        mounts: Vec::new(),
        secrets: Vec::new(),
        network: RuntimeNetworkSpec {
            mode: NetworkMode::None,
            ports: Vec::new(),
        },
        resources: ResourceLimits {
            cpu_millis: 1_501,
            memory_bytes: 65 * 1024 * 1024 + 17,
            pids: 37,
            ephemeral_storage_bytes: None,
            execution_timeout_ms: (class == RuntimeUnitClass::Task).then_some(2_500),
        },
        isolation: IsolationLevel::Sandbox,
        health: None,
        restart: if class == RuntimeUnitClass::Task {
            RestartPolicy::Never
        } else {
            RestartPolicy::Always
        },
        outputs: Vec::new(),
        semantics_profile_digest: None,
    }
}

fn driver(directory: &tempfile::TempDir) -> BoxRuntimeDriver {
    BoxRuntimeDriver::new(BoxRuntimeDriverConfig {
        home_dir: directory.path().join("home"),
        control_timeout: Duration::from_secs(2),
        task_poll_interval: Duration::from_millis(5),
    })
    .unwrap()
}

#[tokio::test]
async fn capabilities_claim_only_the_mapped_box_surface() {
    let directory = tempfile::tempdir().unwrap();
    let driver = driver(&directory);
    driver
        .provider_build
        .set("a3s-box/test crun/test sha256:0123456789abcdef".into())
        .unwrap();

    let capabilities = driver.capabilities().await.unwrap();
    assert_eq!(capabilities.provider_id.as_str(), "a3s-box");
    assert_eq!(
        capabilities.unit_classes,
        vec![RuntimeUnitClass::Task, RuntimeUnitClass::Service]
    );
    assert_eq!(capabilities.isolation_levels, vec![IsolationLevel::Sandbox]);
    assert_eq!(capabilities.network_modes, vec![NetworkMode::None]);
    assert!(capabilities.mount_kinds.is_empty());
    assert!(capabilities.health_check_kinds.is_empty());
    assert_eq!(
        capabilities.resource_controls,
        vec![
            ResourceControl::Cpu,
            ResourceControl::Memory,
            ResourceControl::Pids,
            ResourceControl::ExecutionTimeout,
        ]
    );
    assert_eq!(
        capabilities.features,
        vec![
            RuntimeFeature::DurableIdentity,
            RuntimeFeature::Stop,
            RuntimeFeature::Remove,
            RuntimeFeature::Logs,
            RuntimeFeature::Exec,
        ]
    );
}

#[test]
fn mapping_preserves_digest_resources_timeout_and_hardening() {
    let spec = spec(RuntimeUnitClass::Task);
    let request = creation_request(&spec).unwrap();
    assert_eq!(
        request.config.image,
        format!("registry.example/a3s/runtime@sha256:{}", "a".repeat(64))
    );
    assert_eq!(request.config.resources.vcpus, 2);
    assert_eq!(request.config.resources.memory_mb, 66);
    assert_eq!(request.config.resources.timeout, 3);
    assert_eq!(request.config.resource_limits.cpu_period, Some(100_000));
    assert_eq!(request.config.resource_limits.cpu_quota, Some(150_100));
    assert_eq!(request.config.resource_limits.pids_limit, Some(37));
    assert_eq!(
        request.config.resource_limits.sandbox_memory_limit_bytes,
        Some(spec.resources.memory_bytes)
    );
    assert_eq!(
        request.config.resource_limits.memory_swap,
        Some(spec.resources.memory_bytes as i64)
    );
    assert!(request.config.persistent);
    assert_eq!(request.config.cap_drop, vec!["ALL"]);
    assert_eq!(request.config.security_opt, vec!["no-new-privileges"]);
}

#[test]
fn mapping_rejects_unpinned_mismatched_and_unsupported_artifacts() {
    let mut value = spec(RuntimeUnitClass::Service);
    value.artifact.uri = "oci://registry.example/a3s/runtime:latest".into();
    assert!(creation_request(&value).is_err());

    let mut value = spec(RuntimeUnitClass::Service);
    value.artifact.uri = format!(
        "oci://registry.example/a3s/runtime@sha256:{}",
        "b".repeat(64)
    );
    assert!(creation_request(&value).is_err());

    let mut value = spec(RuntimeUnitClass::Service);
    value.artifact.media_type = "application/vnd.oci.image.index.v1+json".into();
    assert!(matches!(
        creation_request(&value),
        Err(RuntimeError::UnsupportedCapabilities(_))
    ));

    let mut value = spec(RuntimeUnitClass::Service);
    value.artifact.uri = format!(
        "oci://user:secret@registry.example/a3s/runtime@sha256:{}",
        "a".repeat(64)
    );
    assert!(creation_request(&value).is_err());
}

#[test]
fn mapping_rejects_numeric_overflow_before_mutation() {
    let mut value = spec(RuntimeUnitClass::Service);
    value.resources.memory_bytes = i64::MAX as u64 + 1;
    assert!(matches!(
        creation_request(&value),
        Err(RuntimeError::InvalidRequest(message)) if message.contains("memory")
    ));

    let mut value = spec(RuntimeUnitClass::Service);
    value.resources.cpu_millis = u64::MAX;
    assert!(matches!(
        creation_request(&value),
        Err(RuntimeError::InvalidRequest(message)) if message.contains("CPU")
    ));
}

#[tokio::test]
async fn metadata_tamper_is_rejected_fail_closed() {
    let directory = tempfile::tempdir().unwrap();
    let driver = driver(&directory);
    let spec = spec(RuntimeUnitClass::Service);
    let operation_id = operation(&spec).unwrap();
    let reservation = driver
        .manager
        .create(creation_request(&spec).unwrap(), &operation_id)
        .await
        .unwrap();
    let mut record = driver
        .manager
        .managed_record(&reservation.execution_id)
        .await
        .unwrap()
        .unwrap();
    validate_record_for_spec(&record, &spec).unwrap();

    record
        .labels
        .insert(super::metadata::GENERATION_LABEL.into(), "8".into());
    assert!(matches!(
        validate_record_for_spec(&record, &spec),
        Err(RuntimeError::Protocol(message)) if message.contains("identity") || message.contains("intent")
    ));
}
