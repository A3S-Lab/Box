use std::collections::BTreeSet;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use a3s_runtime::contract::{
    HealthProbe, NetworkMode, RuntimeHealthCheck, RuntimeInspection, RuntimeMount,
    RuntimeMountSource, RuntimeUnitState, SecretReference, SecretTarget,
};
use a3s_runtime::{
    FileRuntimeStateStore, RuntimeClient, RuntimeDriver, RuntimeError, RuntimeStateStore,
};

use super::super::metadata::GENERATION_LABEL;
use super::super::{BoxRuntimeDriver, BoxRuntimeDriverConfig};
use super::fixture::BoxRuntimeConformanceFixture;
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
    verify_least_privilege(&record)?;
    verify_workload_least_privilege(fixture, client, &service.spec).await?;
    metadata_tamper_fails_closed(fixture, client, &service.spec, &record.id).await?;
    namespace_separation(fixture).await?;

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

    let mut volume = template.clone();
    volume.request_id = fixture.cases.request_id("security-volume");
    volume.spec.unit_id = fixture.cases.unit_id("security-volume");
    volume.spec.mounts = vec![RuntimeMount {
        name: "provider-volume".into(),
        source: RuntimeMountSource::Volume {
            volume_id: "r17-provider-volume".into(),
        },
        target: "/mnt/provider-volume".into(),
        read_only: false,
    }];
    require(
        matches!(
            client.apply(&volume).await,
            Err(RuntimeError::UnsupportedCapabilities(missing))
                if missing == vec!["mount_kind:Volume"]
        ),
        "Box accepted an unadvertised volume mount",
    )?;

    let mut health = template.clone();
    health.request_id = fixture.cases.request_id("security-health");
    health.spec.unit_id = fixture.cases.unit_id("security-health");
    health.spec.health = Some(RuntimeHealthCheck {
        probe: HealthProbe::Command {
            command: vec!["/bin/true".into()],
        },
        interval_ms: 1_000,
        timeout_ms: 500,
        start_period_ms: 0,
        success_threshold: 1,
        failure_threshold: 3,
    });
    require(
        matches!(
            client.apply(&health).await,
            Err(RuntimeError::UnsupportedCapabilities(missing))
                if missing == vec!["health_check:Command"]
        ),
        "Box accepted an unadvertised health check",
    )?;

    let mut secret = template.clone();
    secret.request_id = fixture.cases.request_id("security-secret");
    secret.spec.unit_id = fixture.cases.unit_id("security-secret");
    secret.spec.secrets = vec![SecretReference {
        name: "provider-token".into(),
        reference: "secret://r17/provider-token".into(),
        target: SecretTarget::Environment {
            variable: "R17_PROVIDER_TOKEN".into(),
        },
    }];
    require(
        matches!(
            client.apply(&secret).await,
            Err(RuntimeError::UnsupportedCapabilities(missing))
                if missing == vec!["feature:SecretReferences"]
        ),
        "Box accepted an unadvertised secret reference",
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

fn verify_least_privilege(record: &crate::BoxRecord) -> Result<()> {
    const BOOTSTRAP_CAPABILITIES: [&str; 11] = [
        "CAP_CHOWN",
        "CAP_DAC_OVERRIDE",
        "CAP_FOWNER",
        "CAP_FSETID",
        "CAP_KILL",
        "CAP_NET_ADMIN",
        "CAP_NET_BIND_SERVICE",
        "CAP_SETGID",
        "CAP_SETPCAP",
        "CAP_SETUID",
        "CAP_SYS_CHROOT",
    ];
    const BOOTSTRAP_CAPABILITY_MASK: u64 = 0x415fb;

    let path = record.box_dir.join("sandbox/bundle/config.json");
    let config: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&path)
            .map_err(|error| super::external("read Sandbox OCI configuration", error))?,
    )
    .map_err(|error| super::external("decode Sandbox OCI configuration", error))?;
    require(
        config
            .pointer("/process/noNewPrivileges")
            .and_then(|v| v.as_bool())
            == Some(true),
        "Sandbox OCI process did not enable no-new-privileges",
    )?;
    let expected = BOOTSTRAP_CAPABILITIES.into_iter().collect::<BTreeSet<_>>();
    for set in ["bounding", "effective", "permitted"] {
        let actual = config
            .pointer(&format!("/process/capabilities/{set}"))
            .and_then(|value| value.as_array())
            .map(|values| {
                values
                    .iter()
                    .filter_map(|value| value.as_str())
                    .collect::<BTreeSet<_>>()
            });
        require(
            actual.as_ref() == Some(&expected),
            format!("Sandbox OCI bootstrap capability set {set} changed: actual={actual:?}"),
        )?;
    }
    for set in ["inheritable", "ambient"] {
        require(
            config
                .pointer(&format!("/process/capabilities/{set}"))
                .and_then(|value| value.as_array())
                .is_some_and(Vec::is_empty),
            format!("Sandbox OCI capability set {set} is not empty"),
        )?;
    }
    require(
        config.pointer("/linux/seccomp/defaultAction").is_some(),
        "Sandbox OCI configuration omitted seccomp",
    )?;
    let namespaces = config
        .pointer("/linux/namespaces")
        .and_then(|value| value.as_array())
        .ok_or_else(|| super::protocol("Sandbox OCI namespaces are missing"))?;
    for required in ["user", "mount", "pid", "ipc", "uts", "network", "cgroup"] {
        require(
            namespaces.iter().any(|namespace| {
                namespace.get("type").and_then(|value| value.as_str()) == Some(required)
            }),
            format!("Sandbox OCI configuration omitted the {required} namespace"),
        )?;
    }
    let mappings = config
        .pointer("/linux/uidMappings")
        .and_then(|value| value.as_array())
        .ok_or_else(|| super::protocol("Sandbox OCI UID mappings are missing"))?;
    require(
        mappings.iter().any(|mapping| {
            mapping.get("containerID").and_then(|value| value.as_u64()) == Some(0)
                && mapping.get("hostID").and_then(|value| value.as_u64()) != Some(0)
        }),
        "Sandbox container root maps to host root",
    )?;
    require(
        config
            .pointer("/linux/cgroupsPath")
            .and_then(|value| value.as_str())
            == Some(format!("a3s-box/{}", record.id).as_str()),
        "Sandbox OCI cgroup path is not execution-scoped",
    )?;

    let pid = record
        .pid
        .ok_or_else(|| super::protocol("running Sandbox record has no init PID"))?;
    let status = std::fs::read_to_string(Path::new("/proc").join(pid.to_string()).join("status"))
        .map_err(|error| super::external("read Sandbox init process status", error))?;
    require(
        status.lines().any(|line| line == "NoNewPrivs:\t1"),
        "Sandbox init process does not have no_new_privs",
    )?;
    let effective = status
        .lines()
        .find_map(|line| line.strip_prefix("CapEff:\t"))
        .ok_or_else(|| super::protocol("Sandbox init process has no CapEff evidence"))?;
    let effective = u64::from_str_radix(effective, 16)
        .map_err(|error| super::external("decode Sandbox init CapEff", error))?;
    require(
        effective & !BOOTSTRAP_CAPABILITY_MASK == 0,
        format!("Sandbox init process escaped its bootstrap capability set: {effective:#x}"),
    )?;
    let host_uid = status
        .lines()
        .find_map(|line| line.strip_prefix("Uid:\t"))
        .and_then(|line| line.split_whitespace().next())
        .and_then(|value| value.parse::<u32>().ok())
        .ok_or_else(|| super::protocol("Sandbox init process has no host UID evidence"))?;
    require(host_uid != 0, "Sandbox init process runs as host root")
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
                 $1 == \"CapEff:\" || $1 == \"CapBnd:\" || $1 == \"CapAmb:\" \
                 { print $1 \"=\" $2 }' /proc/self/status"
                    .into(),
            ],
            5_000,
        ))
        .await
        .map_err(|error| super::external("execute workload capability probe", error))?;
    let capabilities = output
        .stdout
        .lines()
        .filter_map(|line| line.split_once('='))
        .collect::<Vec<_>>();
    require(
        output.exit_code == 0
            && output.stderr.is_empty()
            && !output.truncated
            && capabilities.len() == 5
            && capabilities
                .iter()
                .all(|(_, value)| value.bytes().all(|byte| byte == b'0')),
        format!(
            "Sandbox workload retained capabilities: exit_code={} stdout={:?} stderr={:?}",
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
    let sibling_home = fixture
        .home_dir
        .join("namespaces")
        .join(uuid::Uuid::new_v4().simple().to_string());
    std::fs::create_dir_all(&sibling_home)
        .map_err(|error| super::external("create sibling provider namespace", error))?;
    fixture.register_removable_home(sibling_home.clone());
    let sibling_state_root = sibling_home.join("runtime-state");
    fixture.register_state_root(sibling_state_root.clone());
    let sibling_driver = Arc::new(BoxRuntimeDriver::new(BoxRuntimeDriverConfig {
        home_dir: sibling_home,
        control_timeout: Duration::from_secs(120),
        task_poll_interval: Duration::from_millis(25),
    })?);
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
