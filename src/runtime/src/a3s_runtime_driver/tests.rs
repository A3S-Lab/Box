use std::collections::BTreeMap;
use std::time::Duration;

use a3s_box_core::secret::{SecretEnvironmentBinding, SECRET_ENVIRONMENT_MANIFEST};
use a3s_box_core::{ExecutionIsolation, ExecutionManager};
use a3s_runtime::contract::{
    ArtifactRef, HealthCheckKind, HealthProbe, IsolationLevel, MountKind, NetworkMode,
    ResourceControl, ResourceLimits, RestartPolicy, RuntimeFeature, RuntimeHealthCheck,
    RuntimeMount, RuntimeMountSource, RuntimeNetworkSpec, RuntimePort, RuntimeProcessSpec,
    RuntimeUnitClass, RuntimeUnitSpec, SecretReference, SecretTarget, TransportProtocol,
};
use a3s_runtime::RuntimeDriver;

use super::mapping::{creation_request, operation};
use super::metadata::validate_record_for_spec;
use super::*;

const TEST_EXECUTION_ISOLATION: ExecutionIsolation = ExecutionIsolation::Microvm;

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
        secret_root: directory.path().join("runtime-secrets"),
        control_timeout: Duration::from_secs(2),
        task_poll_interval: Duration::from_millis(5),
    })
    .unwrap()
}

#[test]
fn driver_defaults_to_microvm_without_shared_kernel_fallback() {
    let directory = tempfile::tempdir().unwrap();
    assert_eq!(
        driver(&directory).execution_isolation(),
        ExecutionIsolation::Microvm
    );
}

#[test]
fn driver_allows_explicit_shared_kernel_selection() {
    let directory = tempfile::tempdir().unwrap();
    let driver = BoxRuntimeDriver::new_with_isolation(
        BoxRuntimeDriverConfig {
            home_dir: directory.path().join("home"),
            secret_root: directory.path().join("runtime-secrets"),
            control_timeout: Duration::from_secs(2),
            task_poll_interval: Duration::from_millis(5),
        },
        ExecutionIsolation::Sandbox,
    )
    .unwrap();

    assert_eq!(driver.execution_isolation(), ExecutionIsolation::Sandbox);
}

fn mutate_record(
    driver: &BoxRuntimeDriver,
    execution_id: &str,
    mutation: impl FnOnce(&mut crate::BoxRecord),
) {
    crate::BoxStateStore::modify(driver.manager.state_path(), move |store| {
        let record = store.find_by_id_mut(execution_id).ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::NotFound,
                format!("missing managed execution {execution_id}"),
            )
        })?;
        mutation(record);
        Ok(())
    })
    .unwrap();
}

#[tokio::test]
async fn capabilities_claim_only_the_mapped_box_surface() {
    let directory = tempfile::tempdir().unwrap();
    let driver = driver(&directory);
    driver
        .provider_build
        .set("a3s-box/test isolation/microvm hypervisor/test".into())
        .unwrap();

    let capabilities = driver.capabilities().await.unwrap();
    assert_eq!(capabilities.provider_id.as_str(), "a3s-box");
    assert_eq!(
        capabilities.artifact_media_types,
        vec![OCI_IMAGE_MANIFEST.to_string(), OCI_IMAGE_INDEX.to_string(),]
    );
    assert_eq!(
        capabilities.unit_classes,
        vec![RuntimeUnitClass::Task, RuntimeUnitClass::Service]
    );
    assert_eq!(capabilities.isolation_levels, vec![IsolationLevel::Sandbox]);
    assert_eq!(
        capabilities.network_modes,
        vec![NetworkMode::None, NetworkMode::Service]
    );
    assert_eq!(
        capabilities.mount_kinds,
        vec![MountKind::Volume, MountKind::Tmpfs]
    );
    assert_eq!(
        capabilities.health_check_kinds,
        vec![
            HealthCheckKind::Http,
            HealthCheckKind::Tcp,
            HealthCheckKind::Command,
        ]
    );
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
            RuntimeFeature::ServiceTcp,
            RuntimeFeature::Logs,
            RuntimeFeature::Exec,
        ]
    );
}

#[test]
fn mapping_preserves_digest_resources_timeout_and_hardening() {
    let spec = spec(RuntimeUnitClass::Task);
    let request = creation_request(&spec, TEST_EXECUTION_ISOLATION).unwrap();
    assert_eq!(request.config.isolation, ExecutionIsolation::Microvm);
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
        None
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
fn mapping_keeps_none_and_service_on_the_isolated_vsock_path() {
    let none = spec(RuntimeUnitClass::Task);
    assert_eq!(
        creation_request(&none, TEST_EXECUTION_ISOLATION)
            .unwrap()
            .config
            .network,
        a3s_box_core::NetworkMode::None
    );

    let mut service = spec(RuntimeUnitClass::Service);
    service.network.mode = NetworkMode::Service;
    assert_eq!(
        creation_request(&service, TEST_EXECUTION_ISOLATION)
            .unwrap()
            .config
            .network,
        a3s_box_core::NetworkMode::None
    );
}

#[test]
fn runtime_health_does_not_enable_cli_or_image_health_policy() {
    let mut spec = spec(RuntimeUnitClass::Service);
    spec.network = RuntimeNetworkSpec {
        mode: NetworkMode::Service,
        ports: vec![RuntimePort {
            name: "health".into(),
            container_port: 8_080,
            protocol: TransportProtocol::Tcp,
        }],
    };
    spec.health = Some(RuntimeHealthCheck {
        probe: HealthProbe::Http {
            port: "health".into(),
            path: "/ready".into(),
            expected_statuses: vec![200],
        },
        interval_ms: 1_000,
        timeout_ms: 500,
        start_period_ms: 0,
        success_threshold: 1,
        failure_threshold: 3,
    });

    let request = creation_request(&spec, TEST_EXECUTION_ISOLATION).unwrap();

    assert!(request.policy.health_check.is_none());
    assert!(request.policy.healthcheck_disabled);
}

#[test]
fn mapping_honors_an_explicit_shared_kernel_backend() {
    let spec = spec(RuntimeUnitClass::Service);
    let request = creation_request(&spec, ExecutionIsolation::Sandbox).unwrap();

    assert_eq!(request.config.isolation, ExecutionIsolation::Sandbox);
    assert_eq!(
        request.config.resource_limits.sandbox_memory_limit_bytes,
        Some(spec.resources.memory_bytes)
    );
}

#[test]
fn mapping_rejects_path_like_unit_identity_before_mutation() {
    for unit_id in [
        "../provider-escape",
        "/absolute-provider-id",
        "tenant/../provider-escape",
        "tenant//provider-id",
    ] {
        let mut spec = spec(RuntimeUnitClass::Service);
        spec.unit_id = unit_id.into();
        assert!(matches!(
            creation_request(&spec, TEST_EXECUTION_ISOLATION),
            Err(RuntimeError::InvalidRequest(message))
                if message.contains("path traversal")
        ));
    }

    let mut namespaced = spec(RuntimeUnitClass::Service);
    namespaced.unit_id = "tenant/provider-id".into();
    assert!(creation_request(&namespaced, TEST_EXECUTION_ISOLATION).is_ok());
}

#[test]
fn mapping_compiles_bounded_tmpfs_mounts_and_read_only_intent() {
    let mut spec = spec(RuntimeUnitClass::Service);
    spec.mounts = vec![
        RuntimeMount {
            name: "scratch".into(),
            source: RuntimeMountSource::Tmpfs {
                size_bytes: 8 * 1024 * 1024,
            },
            target: "/runtime/scratch".into(),
            read_only: false,
        },
        RuntimeMount {
            name: "sealed".into(),
            source: RuntimeMountSource::Tmpfs {
                size_bytes: 4 * 1024 * 1024,
            },
            target: "/runtime/sealed".into(),
            read_only: true,
        },
    ];

    let request = creation_request(&spec, TEST_EXECUTION_ISOLATION).unwrap();

    assert_eq!(
        request.config.tmpfs,
        vec![
            "/runtime/scratch:size=8388608,rw",
            "/runtime/sealed:size=4194304,ro",
        ]
    );
    assert!(request.config.volumes.is_empty());
    assert!(request.policy.volume_names.is_empty());
}

#[test]
fn mapping_compiles_deterministic_non_secret_environment_and_file_bindings() {
    let mut spec = spec(RuntimeUnitClass::Service);
    spec.secrets = vec![
        SecretReference {
            name: "provider-token".into(),
            reference: "secret://provider/token/v7".into(),
            target: SecretTarget::Environment {
                variable: "A3S_PROVIDER_TOKEN".into(),
            },
        },
        SecretReference {
            name: "provider-certificate".into(),
            reference: "secret://provider/certificate/v4".into(),
            target: SecretTarget::File {
                path: "/run/provider/certificate.pem".into(),
                mode: 0o440,
            },
        },
    ];

    let request = creation_request(&spec, TEST_EXECUTION_ISOLATION).unwrap();
    let digest = spec.digest().unwrap();
    let digest = digest.strip_prefix("sha256:").unwrap();
    assert_eq!(
        request.config.volumes,
        vec![
            format!(
                "/run/a3s-box/runtime-secrets/{digest}/000.secret:/.a3s-box-secrets/{digest}/000.secret:ro"
            ),
            format!(
                "/run/a3s-box/runtime-secrets/{digest}/001.secret:/run/provider/certificate.pem:ro"
            ),
        ]
    );
    assert_eq!(
        request.policy.managed_secret_root.as_deref(),
        Some(std::path::Path::new("/run/a3s-box/runtime-secrets"))
    );
    let manifest = request
        .config
        .extra_env
        .iter()
        .find(|(key, _)| key == SECRET_ENVIRONMENT_MANIFEST)
        .map(|(_, value)| value)
        .unwrap();
    assert_eq!(
        serde_json::from_str::<Vec<SecretEnvironmentBinding>>(manifest).unwrap(),
        vec![SecretEnvironmentBinding {
            variable: "A3S_PROVIDER_TOKEN".into(),
            path: format!("/.a3s-box-secrets/{digest}/000.secret"),
        }]
    );
    let intent = serde_json::to_string(&request).unwrap();
    assert!(!intent.contains("fixture-secret-plaintext"));
}

#[test]
fn mapping_rejects_secret_collisions_and_unencodable_targets() {
    let environment_secret = SecretReference {
        name: "provider-token".into(),
        reference: "secret://provider/token/v7".into(),
        target: SecretTarget::Environment {
            variable: "A3S_PROVIDER_TOKEN".into(),
        },
    };

    let mut literal_collision = spec(RuntimeUnitClass::Service);
    literal_collision
        .process
        .environment
        .insert("A3S_PROVIDER_TOKEN".into(), "literal".into());
    literal_collision.secrets.push(environment_secret.clone());
    assert!(matches!(
        creation_request(&literal_collision, TEST_EXECUTION_ISOLATION),
        Err(RuntimeError::InvalidRequest(message)) if message.contains("conflicts")
    ));

    let mut reserved = spec(RuntimeUnitClass::Service);
    reserved.process.environment.insert(
        SECRET_ENVIRONMENT_MANIFEST.into(),
        "caller-controlled".into(),
    );
    reserved.secrets.push(environment_secret);
    assert!(matches!(
        creation_request(&reserved, TEST_EXECUTION_ISOLATION),
        Err(RuntimeError::InvalidRequest(message)) if message.contains("reserved")
    ));

    for target in [
        "/run/provider/token:alternate",
        "/run/provider//token",
        "/run/provider/./token",
        "/proc/provider-token",
        "/.a3s-box-secrets/escape",
    ] {
        let mut invalid = spec(RuntimeUnitClass::Service);
        invalid.secrets.push(SecretReference {
            name: "provider-token".into(),
            reference: "secret://provider/token/v7".into(),
            target: SecretTarget::File {
                path: target.into(),
                mode: 0o400,
            },
        });
        assert!(matches!(
            creation_request(&invalid, TEST_EXECUTION_ISOLATION),
            Err(RuntimeError::InvalidRequest(message)) if message.contains("Secret file target")
        ));
    }

    let mut overlap = spec(RuntimeUnitClass::Service);
    overlap.mounts.push(RuntimeMount {
        name: "runtime".into(),
        source: RuntimeMountSource::Tmpfs { size_bytes: 4096 },
        target: "/run/provider".into(),
        read_only: false,
    });
    overlap.secrets.push(SecretReference {
        name: "provider-token".into(),
        reference: "secret://provider/token/v7".into(),
        target: SecretTarget::File {
            path: "/run/provider/token".into(),
            mode: 0o400,
        },
    });
    assert!(matches!(
        creation_request(&overlap, TEST_EXECUTION_ISOLATION),
        Err(RuntimeError::InvalidRequest(message)) if message.contains("overlaps")
    ));

    let mut registry = spec(RuntimeUnitClass::Service);
    registry.secrets.push(SecretReference {
        name: "registry".into(),
        reference: "secret://registry/credential/v2".into(),
        target: SecretTarget::RegistryCredential,
    });
    let request = creation_request(&registry, TEST_EXECUTION_ISOLATION).unwrap();
    assert!(request.config.volumes.is_empty());
    assert!(request.policy.managed_secret_root.is_none());

    registry.secrets.push(SecretReference {
        name: "registry-secondary".into(),
        reference: "secret://registry/credential/v3".into(),
        target: SecretTarget::RegistryCredential,
    });
    assert!(matches!(
        creation_request(&registry, TEST_EXECUTION_ISOLATION),
        Err(RuntimeError::InvalidRequest(message)) if message.contains("unique")
    ));
}

#[test]
fn mapping_rejects_protected_or_unencodable_tmpfs_targets() {
    for target in ["/proc/runtime", "/dev/shm/nested", "/run/a3s-box/data"] {
        let mut spec = spec(RuntimeUnitClass::Service);
        spec.mounts.push(RuntimeMount {
            name: "scratch".into(),
            source: RuntimeMountSource::Tmpfs { size_bytes: 4096 },
            target: target.into(),
            read_only: false,
        });
        assert!(matches!(
            creation_request(&spec, TEST_EXECUTION_ISOLATION),
            Err(RuntimeError::InvalidRequest(message)) if message.contains("protected")
        ));
    }

    for target in [
        "/runtime/ambiguous:target",
        "/runtime/./scratch",
        "/runtime//scratch",
        "/runtime/scratch/",
    ] {
        let mut spec = spec(RuntimeUnitClass::Service);
        spec.mounts.push(RuntimeMount {
            name: "scratch".into(),
            source: RuntimeMountSource::Tmpfs { size_bytes: 4096 },
            target: target.into(),
            read_only: false,
        });
        assert!(matches!(
            creation_request(&spec, TEST_EXECUTION_ISOLATION),
            Err(RuntimeError::InvalidRequest(message)) if message.contains("encodable")
        ));
    }
}

#[test]
fn mapping_accepts_oci_indexes_and_rejects_unpinned_mismatched_or_unsupported_artifacts() {
    let mut value = spec(RuntimeUnitClass::Service);
    value.artifact.uri = "oci://registry.example/a3s/runtime:latest".into();
    assert!(creation_request(&value, TEST_EXECUTION_ISOLATION).is_err());

    let mut value = spec(RuntimeUnitClass::Service);
    value.artifact.uri = format!(
        "oci://registry.example/a3s/runtime@sha256:{}",
        "b".repeat(64)
    );
    assert!(creation_request(&value, TEST_EXECUTION_ISOLATION).is_err());

    let mut value = spec(RuntimeUnitClass::Service);
    value.artifact.media_type = OCI_IMAGE_INDEX.into();
    assert!(creation_request(&value, TEST_EXECUTION_ISOLATION).is_ok());

    let mut value = spec(RuntimeUnitClass::Service);
    value.artifact.media_type = "application/vnd.docker.distribution.manifest.v2+json".into();
    assert!(matches!(
        creation_request(&value, TEST_EXECUTION_ISOLATION),
        Err(RuntimeError::UnsupportedCapabilities(_))
    ));

    let mut value = spec(RuntimeUnitClass::Service);
    value.artifact.uri = format!(
        "oci://user:secret@registry.example/a3s/runtime@sha256:{}",
        "a".repeat(64)
    );
    assert!(creation_request(&value, TEST_EXECUTION_ISOLATION).is_err());
}

#[test]
fn mapping_rejects_numeric_overflow_before_mutation() {
    let mut value = spec(RuntimeUnitClass::Service);
    value.resources.memory_bytes = i64::MAX as u64 + 1;
    assert!(matches!(
        creation_request(&value, TEST_EXECUTION_ISOLATION),
        Err(RuntimeError::InvalidRequest(message)) if message.contains("memory")
    ));

    let mut value = spec(RuntimeUnitClass::Service);
    value.resources.cpu_millis = u64::MAX;
    assert!(matches!(
        creation_request(&value, TEST_EXECUTION_ISOLATION),
        Err(RuntimeError::InvalidRequest(message)) if message.contains("CPU")
    ));
}

#[tokio::test]
async fn metadata_tamper_is_rejected_fail_closed() {
    let directory = tempfile::tempdir().unwrap();
    let driver = driver(&directory);
    let mut spec = spec(RuntimeUnitClass::Service);
    spec.resources.cpu_millis = 500;
    let operation_id = operation(&spec).unwrap();
    let reservation = driver
        .manager
        .create(
            creation_request(&spec, TEST_EXECUTION_ISOLATION).unwrap(),
            &operation_id,
        )
        .await
        .unwrap();
    let mut record = driver
        .manager
        .managed_record(&reservation.execution_id)
        .await
        .unwrap()
        .unwrap();
    validate_record_for_spec(
        &record,
        &spec,
        TEST_EXECUTION_ISOLATION,
        &driver.config.secret_root,
    )
    .unwrap();

    record
        .labels
        .insert(super::metadata::GENERATION_LABEL.into(), "8".into());
    assert!(matches!(
        validate_record_for_spec(
            &record,
            &spec,
            TEST_EXECUTION_ISOLATION,
            &driver.config.secret_root,
        ),
        Err(RuntimeError::Protocol(message)) if message.contains("identity") || message.contains("intent")
    ));
}

#[tokio::test]
async fn metadata_rejects_a_record_created_for_another_box_isolation_backend() {
    let directory = tempfile::tempdir().unwrap();
    let driver = driver(&directory);
    let spec = spec(RuntimeUnitClass::Service);
    let operation_id = operation(&spec).unwrap();
    let reservation = driver
        .manager
        .create(
            creation_request(&spec, ExecutionIsolation::Sandbox).unwrap(),
            &operation_id,
        )
        .await
        .unwrap();
    let record = driver
        .manager
        .managed_record(&reservation.execution_id)
        .await
        .unwrap()
        .unwrap();

    assert!(matches!(
        validate_record_for_spec(
            &record,
            &spec,
            ExecutionIsolation::Microvm,
            &driver.config.secret_root,
        ),
        Err(RuntimeError::Protocol(message)) if message.contains("creation intent")
    ));
}

#[tokio::test]
async fn discovery_rejects_a_runtime_record_hidden_by_unit_label_tamper() {
    let directory = tempfile::tempdir().unwrap();
    let driver = driver(&directory);
    let mut spec = spec(RuntimeUnitClass::Service);
    spec.resources.cpu_millis = 500;
    let reservation = driver
        .manager
        .create(
            creation_request(&spec, TEST_EXECUTION_ISOLATION).unwrap(),
            &operation(&spec).unwrap(),
        )
        .await
        .unwrap();

    mutate_record(&driver, reservation.execution_id.as_str(), |record| {
        record
            .labels
            .insert(super::metadata::UNIT_LABEL.into(), "hidden-unit".into());
    });

    assert!(matches!(
        driver.find_generation(&spec).await,
        Err(RuntimeError::Protocol(message)) if message.contains("ownership")
    ));
}

#[tokio::test]
async fn discovery_rejects_a_runtime_operation_that_lost_all_labels() {
    let directory = tempfile::tempdir().unwrap();
    let driver = driver(&directory);
    let mut spec = spec(RuntimeUnitClass::Service);
    spec.resources.cpu_millis = 500;
    let reservation = driver
        .manager
        .create(
            creation_request(&spec, TEST_EXECUTION_ISOLATION).unwrap(),
            &operation(&spec).unwrap(),
        )
        .await
        .unwrap();

    mutate_record(&driver, reservation.execution_id.as_str(), |record| {
        record.labels.clear();
        record
            .managed_execution
            .as_mut()
            .unwrap()
            .request
            .labels
            .clear();
    });

    assert!(matches!(
        driver.find_generation(&spec).await,
        Err(RuntimeError::Protocol(message)) if message.contains("no unit identity")
    ));
}
