use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use a3s_runtime::contract::{
    NetworkMode, RuntimeInspection, RuntimeMount, RuntimeMountSource, RuntimeUnitState,
    SecretReference, SecretTarget,
};
use a3s_runtime::{
    FileRuntimeStateStore, RuntimeClient, RuntimeDriver, RuntimeError, RuntimeStateStore,
};

use super::super::metadata::GENERATION_LABEL;
use super::super::{BoxRuntimeDriver, BoxRuntimeDriverConfig};
use super::fixture::{
    BoxRuntimeConformanceFixture, SECRET_ENV_REFERENCE, SECRET_ENV_VALUE, SECRET_FILE_REFERENCE,
    SECRET_FILE_VALUE,
};
use super::PRIVATE_REGISTRY_SECRET_REFERENCE;
use super::{require, Result};

pub(super) async fn run(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let service = fixture.cases.service(
        "security-service",
        "printf 'r17-security-ready\\n'; exec sleep 3600",
    );
    let before = fixture
        .driver
        .manager
        .managed_records()
        .await
        .map_err(|error| super::external("load security pre-mutation provider inventory", error))?;
    reject_hostile_inputs(fixture, client, &service, before.len()).await?;

    let running = client.apply(&service).await?;
    require(
        running.state == RuntimeUnitState::Running,
        "security fixture Service did not reach running",
    )?;
    let record = fixture.record_for(&service.spec).await?;
    verify_digest_pin(&record, &service.spec)?;
    super::security_evidence::verify_provider_least_privilege(fixture, &record, &running)?;
    verify_workload_least_privilege(fixture, client, &service.spec).await?;
    metadata_tamper_fails_closed(fixture, client, &service.spec, &record.id).await?;
    namespace_separation(fixture).await?;
    secret_nondisclosure(fixture, client).await?;

    fixture
        .remove_unit(client, &service.spec, "security-service")
        .await
}

async fn reject_hostile_inputs(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
    template: &a3s_runtime::contract::RuntimeApplyRequest,
    baseline_records: usize,
) -> Result<()> {
    let mut tag_only = template.clone();
    tag_only.request_id = fixture.cases.request_id("security-tag-only");
    tag_only.spec.unit_id = fixture.cases.unit_id("security-tag-only");
    tag_only.spec.artifact.uri = "oci://docker.io/library/alpine:latest".into();
    require(
        client.apply(&tag_only).await.is_err(),
        "Box accepted a mutable artifact tag",
    )?;

    let mut mismatch = template.clone();
    mismatch.request_id = fixture.cases.request_id("security-digest-mismatch");
    mismatch.spec.unit_id = fixture.cases.unit_id("security-digest-mismatch");
    mismatch.spec.artifact.uri =
        format!("oci://docker.io/library/alpine@sha256:{}", "0".repeat(64));
    require(
        client.apply(&mismatch).await.is_err(),
        "Box accepted an artifact URI/digest mismatch",
    )?;

    let mut credentials = template.clone();
    credentials.request_id = fixture.cases.request_id("security-uri-credentials");
    credentials.spec.unit_id = fixture.cases.unit_id("security-uri-credentials");
    credentials.spec.artifact.uri =
        credentials
            .spec
            .artifact
            .uri
            .replacen("oci://", "oci://user:secret@", 1);
    require(
        client.apply(&credentials).await.is_err(),
        "Box accepted registry credentials in an artifact URI",
    )?;

    let mut traversal = template.clone();
    traversal.request_id = fixture.cases.request_id("security-path-traversal");
    traversal.spec.unit_id = "../r17-provider-escape".into();
    require(
        client.apply(&traversal).await.is_err(),
        "Box accepted a path-like Runtime unit identity",
    )?;

    let mut protected_mount = template.clone();
    protected_mount.request_id = fixture.cases.request_id("security-protected-mount");
    protected_mount.spec.unit_id = fixture.cases.unit_id("security-protected-mount");
    protected_mount.spec.mounts = vec![RuntimeMount {
        name: "host-proc".into(),
        source: RuntimeMountSource::Tmpfs {
            size_bytes: 1024 * 1024,
        },
        target: "/proc/r17-escape".into(),
        read_only: false,
    }];
    require(
        client.apply(&protected_mount).await.is_err(),
        "Box accepted a tmpfs mount below a protected host interface",
    )?;

    let mut outbound_network = template.clone();
    outbound_network.request_id = fixture.cases.request_id("security-outbound-network");
    outbound_network.spec.unit_id = fixture.cases.unit_id("security-outbound-network");
    outbound_network.spec.network.mode = NetworkMode::Outbound;
    require(
        matches!(
            client.apply(&outbound_network).await,
            Err(RuntimeError::UnsupportedCapabilities(missing))
                if missing == vec!["network_mode:Outbound"]
        ),
        "Box accepted an unadvertised outbound network",
    )?;

    let after = fixture
        .driver
        .manager
        .managed_records()
        .await
        .map_err(|error| {
            super::external("load security post-mutation provider inventory", error)
        })?;
    require(
        after.len() == baseline_records,
        "hostile input mutated provider inventory before rejection",
    )
}

async fn secret_nondisclosure(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let mut request = fixture.cases.service(
        "security-secret-nondisclosure",
        "test -n \"$R17_PROVIDER_TOKEN\"; test -s /run/a3s-secrets/provider-token; printf 'secret-env=%s\\n' \"$R17_PROVIDER_TOKEN\"; printf 'secret-file=%s\\n' \"$(cat /run/a3s-secrets/provider-token)\"; printf 'secret-mode=%s\\n' \"$(stat -c %a /run/a3s-secrets/provider-token)\"; exec sleep 3600",
    );
    request.spec.secrets = vec![
        SecretReference {
            name: "provider-token".into(),
            reference: SECRET_ENV_REFERENCE.into(),
            target: SecretTarget::Environment {
                variable: "R17_PROVIDER_TOKEN".into(),
            },
        },
        SecretReference {
            name: "provider-token-file".into(),
            reference: SECRET_FILE_REFERENCE.into(),
            target: SecretTarget::File {
                path: "/run/a3s-secrets/provider-token".into(),
                mode: 0o400,
            },
        },
        SecretReference {
            name: "registry-credential".into(),
            reference: PRIVATE_REGISTRY_SECRET_REFERENCE.into(),
            target: SecretTarget::RegistryCredential,
        },
    ];
    let secret_directory = super::super::secret::secret_directory(
        &fixture.home_dir.join("runtime-secrets"),
        &request.spec,
    )?;
    let calls_before_apply = fixture.secret_materialization_calls();

    let running = client.apply(&request).await?;
    require(
        running.state == RuntimeUnitState::Running,
        "Secret nondisclosure Service did not reach running",
    )?;
    require(
        fixture.secret_materialization_calls() == calls_before_apply + 2,
        "Secret materialization did not resolve container references exactly once or resolved a cached registry credential",
    )?;
    require(
        secret_directory.is_dir(),
        "Secret materialization directory was not retained for the running generation",
    )?;

    let record = fixture.record_for(&request.spec).await?;
    let creation_intent = serde_json::to_vec(
        &record
            .managed_execution
            .as_ref()
            .ok_or_else(|| super::protocol("Secret execution lost managed creation intent"))?
            .request,
    )
    .map_err(|error| super::external("encode Secret creation intent", error))?;
    let boxes = std::fs::read(fixture.home_dir.join("boxes.json"))
        .map_err(|error| super::external("read Box state for Secret nondisclosure", error))?;
    super::security_evidence::verify_secret_persistence(
        fixture,
        &record,
        &creation_intent,
        &boxes,
    )?;

    let calls_before_reconstruction = fixture.secret_materialization_calls();
    let restarted_driver = fixture.restarted_driver()?;
    let restarted = fixture.client_with(restarted_driver, fixture.state.clone());
    require(
        matches!(
            restarted.inspect(&request.spec.unit_id).await?,
            RuntimeInspection::Found { observation, .. }
                if observation.state == RuntimeUnitState::Running
        ),
        "reconstructed Box driver did not adopt the running Secret generation",
    )?;
    require(
        fixture.secret_materialization_calls() == calls_before_reconstruction,
        "driver reconstruction rematerialized a live Secret generation",
    )?;

    let chunks = wait_for_redacted_secret_logs(fixture, &restarted, &request.spec).await?;
    require(
        chunks.iter().all(|chunk| {
            !chunk.data.contains(SECRET_ENV_VALUE) && !chunk.data.contains(SECRET_FILE_VALUE)
        }),
        "Runtime log projection exposed Secret plaintext",
    )?;
    require(
        chunks
            .iter()
            .map(|chunk| chunk.data.matches("[REDACTED]").count())
            .sum::<usize>()
            >= 2,
        "Runtime log projection did not redact both Secret values",
    )?;
    require(
        chunks
            .iter()
            .any(|chunk| chunk.data.contains("secret-mode=400")),
        "file Secret did not preserve its requested mode",
    )?;
    require(
        fixture.secret_materialization_calls() >= calls_before_reconstruction + 2,
        "Runtime log read did not reauthorize Secret references",
    )?;

    fixture.set_secret_authorized(false);
    let denied = restarted
        .logs(&fixture.cases.logs(&request.spec, None, 100, None))
        .await;
    fixture.set_secret_authorized(true);
    require(
        matches!(denied, Err(RuntimeError::InvalidRequest(_))),
        "Runtime log read ignored revoked Secret authorization",
    )?;

    let stopped = restarted
        .stop(&fixture.cases.action("security-secret-stop", &request.spec))
        .await?;
    require(
        matches!(
            stopped,
            RuntimeInspection::Found { ref observation, .. }
                if observation.state == RuntimeUnitState::Stopped
        ),
        "Secret Service did not stop cleanly",
    )?;
    require(
        secret_directory.is_dir(),
        "stop removed Secret material needed for an explicit restart",
    )?;

    restarted
        .remove(
            &fixture
                .cases
                .action("security-secret-remove", &request.spec),
        )
        .await?;
    require(
        !secret_directory.exists(),
        "removal left Secret material behind",
    )
}

async fn wait_for_redacted_secret_logs(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
    spec: &a3s_runtime::contract::RuntimeUnitSpec,
) -> Result<Vec<a3s_runtime::contract::RuntimeLogChunk>> {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let chunks = client
            .logs(&fixture.cases.logs(spec, None, 100, None))
            .await?;
        if chunks
            .iter()
            .any(|chunk| chunk.data.contains("secret-mode=400"))
            && chunks
                .iter()
                .map(|chunk| chunk.data.matches("[REDACTED]").count())
                .sum::<usize>()
                >= 2
        {
            return Ok(chunks);
        }
        if Instant::now() >= deadline {
            return Err(super::protocol(
                "Secret workload logs did not converge within five seconds",
            ));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}

fn verify_digest_pin(
    record: &crate::BoxRecord,
    spec: &a3s_runtime::contract::RuntimeUnitSpec,
) -> Result<()> {
    let image = &record
        .managed_execution
        .as_ref()
        .ok_or_else(|| super::protocol("security record lost managed metadata"))?
        .request
        .config
        .image;
    require(
        image.ends_with(&format!("@{}", spec.artifact.digest)) && image.matches('@').count() == 1,
        "provider creation was not bound to the requested image digest",
    )
}

async fn verify_workload_least_privilege(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
    spec: &a3s_runtime::contract::RuntimeUnitSpec,
) -> Result<()> {
    let output = client
        .exec(&fixture.cases.exec(
            "security-workload-capabilities",
            spec,
            vec![
                "/bin/sh".into(),
                "-c".into(),
                "awk '$1 == \"CapInh:\" || $1 == \"CapPrm:\" || \
                 $1 == \"CapEff:\" || $1 == \"CapBnd:\" || $1 == \"CapAmb:\" || \
                 $1 == \"NoNewPrivs:\" || $1 == \"Seccomp:\" \
                 { print $1 \"=\" $2 }' /proc/self/status"
                    .into(),
            ],
            5_000,
        ))
        .await
        .map_err(|error| super::external("execute workload capability probe", error))?;
    let status = output
        .stdout
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect::<BTreeMap<_, _>>();
    require(
        output.exit_code == 0
            && output.stderr.is_empty()
            && !output.truncated
            && status.len() == 7
            && ["CapInh:", "CapPrm:", "CapEff:", "CapBnd:", "CapAmb:"]
                .into_iter()
                .all(|field| {
                    status
                        .get(field)
                        .is_some_and(|value| value.bytes().all(|byte| byte == b'0'))
                })
            && status.get("NoNewPrivs:") == Some(&"1")
            && status.get("Seccomp:") == Some(&"2"),
        format!(
            "provider workload escaped its least-privilege policy: exit_code={} stdout={:?} stderr={:?}",
            output.exit_code, output.stdout, output.stderr
        ),
    )
}

async fn metadata_tamper_fails_closed(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
    spec: &a3s_runtime::contract::RuntimeUnitSpec,
    execution_id: &str,
) -> Result<()> {
    let state_path = fixture.driver.manager.state_path().to_path_buf();
    crate::BoxStateStore::modify(&state_path, |store| {
        let record = store.find_by_id_mut(execution_id).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "security record disappeared")
        })?;
        record
            .labels
            .insert(GENERATION_LABEL.into(), "999999".into());
        Ok(())
    })
    .map_err(|error| super::external("tamper Runtime provider metadata", error))?;
    let result = client.inspect(&spec.unit_id).await;
    crate::BoxStateStore::modify(&state_path, |store| {
        let record = store.find_by_id_mut(execution_id).ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::NotFound, "security record disappeared")
        })?;
        record
            .labels
            .insert(GENERATION_LABEL.into(), spec.generation.to_string());
        Ok(())
    })
    .map_err(|error| super::external("restore Runtime provider metadata", error))?;
    require(
        matches!(result, Err(RuntimeError::Protocol(_))),
        "tampered Runtime provider metadata did not fail closed",
    )
}

async fn namespace_separation(fixture: &BoxRuntimeConformanceFixture) -> Result<()> {
    // Keep this independent provider namespace short enough for the fixed
    // A3S OCI `runtime.sock` path. This profile certifies namespace separation;
    // nesting it under the already isolated primary home would instead test
    // the platform `sockaddr_un.sun_path` limit. Seed only the immutable,
    // digest-addressed image store so a registry outage cannot turn this local
    // security oracle into a network test.
    let namespace_id = uuid::Uuid::new_v4().simple().to_string();
    let sibling_home = std::env::temp_dir().join(format!("a3s-r17n-{}", &namespace_id[..16]));
    std::fs::create_dir(&sibling_home)
        .map_err(|error| super::external("create sibling provider namespace", error))?;
    fixture.register_provider_home(sibling_home.clone());
    crate::cache::layer_cache::copy_dir_recursive(
        &fixture.home_dir.join("images"),
        &sibling_home.join("images"),
    )
    .map_err(|error| super::external("seed sibling immutable image store", error))?;
    let sibling_state_root = sibling_home.join("runtime-state");
    fixture.register_state_root(sibling_state_root.clone());
    let sibling_driver = Arc::new(BoxRuntimeDriver::new_with_isolation(
        BoxRuntimeDriverConfig {
            secret_root: sibling_home.join("runtime-secrets"),
            home_dir: sibling_home,
            control_timeout: Duration::from_secs(120),
            task_poll_interval: Duration::from_millis(25),
        },
        fixture.driver.execution_isolation(),
    )?);
    fixture.register_driver(sibling_driver.clone());
    let sibling_state = Arc::new(FileRuntimeStateStore::new(&sibling_state_root));
    let sibling = fixture.client_with(sibling_driver.clone(), sibling_state);
    let request = fixture.cases.service(
        "security-sibling-namespace",
        "printf 'r17-sibling-namespace\\n'; exec sleep 3600",
    );
    let running = sibling.apply(&request).await?;
    let sibling_id = running.provider_resource_id.clone();

    let now_ms = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u128::from(u64::MAX)) as u64;
    let reservation = fixture.state.reserve_apply(&request, now_ms).await?;
    let foreign_probe = fixture.driver.inspect(&reservation.record).await?;
    require(
        matches!(foreign_probe, RuntimeInspection::NotFound { .. }),
        "one Box provider namespace discovered another namespace's resource",
    )?;
    let remove = fixture
        .cases
        .action("security-foreign-remove", &request.spec);
    let foreign_remove = fixture.driver.remove(&reservation.record, &remove).await?;
    require(
        foreign_remove.already_absent,
        "one Box provider namespace removed another namespace's resource",
    )?;
    let RuntimeInspection::Found { observation, .. } =
        sibling.inspect(&request.spec.unit_id).await?
    else {
        return Err(super::protocol(
            "foreign namespace probe removed the sibling resource",
        ));
    };
    require(
        observation.state == RuntimeUnitState::Running
            && observation.provider_resource_id == sibling_id,
        "foreign namespace probe changed sibling provider identity",
    )?;
    fixture
        .remove_unit(&sibling, &request.spec, "security-sibling-namespace")
        .await
}
