use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use a3s_box_core::rootfs_metadata::RUNTIME_ENV_PATH;
use a3s_box_core::secret::SECRET_ENVIRONMENT_MANIFEST;
use a3s_box_core::{ExecutionBackend, ExecutionIsolation, IsolationClass};
use a3s_runtime::contract::RuntimeObservation;

use super::fixture::{BoxRuntimeConformanceFixture, SECRET_ENV_VALUE, SECRET_FILE_VALUE};
use super::{require, Result};

pub(super) fn verify_provider_least_privilege(
    fixture: &BoxRuntimeConformanceFixture,
    record: &crate::BoxRecord,
    observation: &RuntimeObservation,
) -> Result<()> {
    let execution_isolation = fixture.driver.execution_isolation();
    let metadata = record
        .managed_execution
        .as_ref()
        .ok_or_else(|| super::protocol("security record lost managed metadata"))?;
    let config = &metadata.request.config;
    require(
        record.isolation == execution_isolation && config.isolation == execution_isolation,
        "security execution changed its requested Box isolation",
    )?;
    let expected_plan = a3s_box_core::resolve_execution(config)
        .map_err(|error| super::external("resolve persisted security execution plan", error))?;
    require(
        metadata.plan == expected_plan,
        "security execution persisted a plan that drifted from its creation intent",
    )?;
    let exact_boundary = match execution_isolation {
        ExecutionIsolation::Sandbox => {
            metadata.plan.backend == ExecutionBackend::A3sOci
                && metadata.plan.isolation_class == IsolationClass::SharedKernel
                && !metadata.plan.required_controls.is_empty()
        }
        ExecutionIsolation::Microvm => {
            metadata.plan.backend == ExecutionBackend::Krun
                && metadata.plan.isolation_class == IsolationClass::HardwareVm
                && metadata.plan.required_controls.is_empty()
        }
    };
    require(
        metadata.plan.requested_isolation == execution_isolation && exact_boundary,
        "security execution did not retain its exact isolation backend and boundary",
    )?;
    require(
        !config.privileged
            && config.cap_add.is_empty()
            && exactly_one(&config.cap_drop, "ALL")
            && exactly_one(&config.security_opt, "no-new-privileges")
            && !record.privileged
            && record.cap_add.is_empty()
            && exactly_one(&record.cap_drop, "ALL")
            && exactly_one(&record.security_opt, "no-new-privileges"),
        "security execution did not persist the exact least-privilege policy",
    )?;

    let provider_build = observation
        .provider_build
        .as_deref()
        .ok_or_else(|| super::protocol("security observation omitted provider build identity"))?;
    match execution_isolation {
        ExecutionIsolation::Sandbox => {
            require(
                provider_build.starts_with("a3s-box/")
                    && provider_build.contains(" isolation/sandbox a3s-oci/sha256:")
                    && provider_build.contains(" agent/sha256:"),
                format!("Sandbox provider build identity is incomplete: {provider_build:?}"),
            )?;
            verify_sandbox_least_privilege(record)
        }
        ExecutionIsolation::Microvm => {
            let hypervisor = provider_build
                .split_once(" isolation/microvm hypervisor/")
                .filter(|(prefix, backend)| {
                    prefix.starts_with("a3s-box/")
                        && !backend.is_empty()
                        && !backend.chars().any(char::is_whitespace)
                });
            require(
                hypervisor.is_some(),
                format!("MicroVM provider build identity is incomplete: {provider_build:?}"),
            )?;
            verify_microvm_process_identity(fixture, record)
        }
    }
}

pub(super) fn verify_secret_persistence(
    fixture: &BoxRuntimeConformanceFixture,
    record: &crate::BoxRecord,
    creation_intent: &[u8],
    boxes: &[u8],
) -> Result<()> {
    verify_no_plaintext("managed creation intent", creation_intent)?;
    verify_no_plaintext("boxes.json", boxes)?;

    match fixture.driver.execution_isolation() {
        ExecutionIsolation::Sandbox => verify_sandbox_secret_persistence(record),
        ExecutionIsolation::Microvm => verify_microvm_secret_persistence(record),
    }
}

fn exactly_one(values: &[String], expected: &str) -> bool {
    matches!(values, [value] if value == expected)
}

fn verify_sandbox_least_privilege(record: &crate::BoxRecord) -> Result<()> {
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
            .and_then(serde_json::Value::as_bool)
            == Some(true),
        "Sandbox OCI process did not enable no-new-privileges",
    )?;
    let expected = BOOTSTRAP_CAPABILITIES.into_iter().collect::<BTreeSet<_>>();
    for set in ["bounding", "effective", "permitted"] {
        let actual = config
            .pointer(&format!("/process/capabilities/{set}"))
            .and_then(serde_json::Value::as_array)
            .map(|values| {
                values
                    .iter()
                    .filter_map(serde_json::Value::as_str)
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
                .and_then(serde_json::Value::as_array)
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
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| super::protocol("Sandbox OCI namespaces are missing"))?;
    for required in ["user", "mount", "pid", "ipc", "uts", "network", "cgroup"] {
        require(
            namespaces.iter().any(|namespace| {
                namespace.get("type").and_then(serde_json::Value::as_str) == Some(required)
            }),
            format!("Sandbox OCI configuration omitted the {required} namespace"),
        )?;
    }
    let mappings = config
        .pointer("/linux/uidMappings")
        .and_then(serde_json::Value::as_array)
        .ok_or_else(|| super::protocol("Sandbox OCI UID mappings are missing"))?;
    require(
        mappings.iter().any(|mapping| {
            mapping
                .get("containerID")
                .and_then(serde_json::Value::as_u64)
                == Some(0)
                && mapping.get("hostID").and_then(serde_json::Value::as_u64) != Some(0)
        }),
        "Sandbox container root maps to host root",
    )?;
    require(
        config
            .pointer("/linux/cgroupsPath")
            .and_then(serde_json::Value::as_str)
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

fn verify_microvm_process_identity(
    fixture: &BoxRuntimeConformanceFixture,
    record: &crate::BoxRecord,
) -> Result<()> {
    let pid = record
        .pid
        .ok_or_else(|| super::protocol("running MicroVM record has no shim PID"))?;
    let start_time = record
        .pid_start_time
        .ok_or_else(|| super::protocol("running MicroVM record has no shim start identity"))?;
    require(
        crate::process::is_process_alive_with_identity(pid, Some(start_time)),
        format!("MicroVM shim identity {pid}/{start_time} is not alive"),
    )?;
    verify_microvm_shim_binary(fixture, pid)
}

#[cfg(target_os = "linux")]
fn verify_microvm_shim_binary(fixture: &BoxRuntimeConformanceFixture, pid: u32) -> Result<()> {
    let expected = fixture
        .home_dir
        .join("bin/a3s-box-shim")
        .canonicalize()
        .map_err(|error| super::external("canonicalize packaged MicroVM shim", error))?;
    let actual = std::fs::read_link(Path::new("/proc").join(pid.to_string()).join("exe"))
        .map_err(|error| super::external("read MicroVM shim executable identity", error))?
        .canonicalize()
        .map_err(|error| super::external("canonicalize live MicroVM shim", error))?;
    require(
        actual == expected,
        format!(
            "MicroVM process did not launch the packaged shim: actual={} expected={}",
            actual.display(),
            expected.display()
        ),
    )
}

#[cfg(not(target_os = "linux"))]
fn verify_microvm_shim_binary(_fixture: &BoxRuntimeConformanceFixture, _pid: u32) -> Result<()> {
    Ok(())
}

fn verify_sandbox_secret_persistence(record: &crate::BoxRecord) -> Result<()> {
    let bundle_directory = record.box_dir.join("sandbox/bundle");
    let oci = std::fs::read(bundle_directory.join("config.json"))
        .map_err(|error| super::external("read Secret Sandbox OCI configuration", error))?;
    let oci_spec: serde_json::Value = serde_json::from_slice(&oci)
        .map_err(|error| super::external("decode Secret Sandbox OCI configuration", error))?;
    let rootfs_path = oci_spec
        .pointer("/root/path")
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| super::protocol("Secret Sandbox OCI configuration omitted root.path"))?;
    let rootfs_path = if Path::new(rootfs_path).is_absolute() {
        PathBuf::from(rootfs_path)
    } else {
        bundle_directory.join(rootfs_path)
    };
    let staged_environment = read_regular_file(
        &rootfs_path.join(RUNTIME_ENV_PATH.trim_start_matches('/')),
        "Secret Sandbox staged environment",
    )?;
    verify_no_plaintext("Sandbox OCI configuration", &oci)?;
    verify_no_plaintext("Sandbox staged environment", &staged_environment)?;
    require(
        String::from_utf8_lossy(&oci).contains(&format!("BOX_EXEC_ENV_FILE={RUNTIME_ENV_PATH}")),
        "Sandbox OCI configuration omitted the protected environment staging pointer",
    )?;
    verify_environment_manifest("Sandbox", &staged_environment)
}

fn verify_microvm_secret_persistence(record: &crate::BoxRecord) -> Result<()> {
    let resolved_image = read_regular_file(
        &record.box_dir.join(crate::RESOLVED_IMAGE_CONFIG_FILE),
        "Secret MicroVM resolved image configuration",
    )?;
    let staged_environment = read_microvm_staged_environment(record)?;
    verify_no_plaintext("MicroVM resolved image configuration", &resolved_image)?;
    verify_no_plaintext("MicroVM staged environment", &staged_environment)?;
    verify_environment_manifest("MicroVM", &staged_environment)
}

fn read_microvm_staged_environment(record: &crate::BoxRecord) -> Result<Vec<u8>> {
    let relative = RUNTIME_ENV_PATH.trim_start_matches('/');
    let candidates = [
        record.box_dir.join("merged").join(relative),
        record.box_dir.join("rootfs/.a3s-rootfs").join(relative),
        record.box_dir.join("rootfs").join(relative),
        record.box_dir.join("upper").join(relative),
    ];
    for path in &candidates {
        match std::fs::symlink_metadata(path) {
            Ok(_) => return read_regular_file(path, "Secret MicroVM staged environment"),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => {
                return Err(super::external(
                    "inspect Secret MicroVM staged environment",
                    error,
                ))
            }
        }
    }
    Err(super::protocol(format!(
        "Secret MicroVM staged environment is missing below {}",
        record.box_dir.display()
    )))
}

fn read_regular_file(path: &Path, label: &str) -> Result<Vec<u8>> {
    let metadata = std::fs::symlink_metadata(path)
        .map_err(|error| super::external(&format!("inspect {label}"), error))?;
    require(
        metadata.file_type().is_file() && !metadata.file_type().is_symlink(),
        format!("{label} is not a regular file: {}", path.display()),
    )?;
    std::fs::read(path).map_err(|error| super::external(&format!("read {label}"), error))
}

fn verify_no_plaintext(label: &str, bytes: &[u8]) -> Result<()> {
    let value = String::from_utf8_lossy(bytes);
    require(
        !value.contains(SECRET_ENV_VALUE) && !value.contains(SECRET_FILE_VALUE),
        format!("{label} persisted Secret plaintext"),
    )
}

fn verify_environment_manifest(provider: &str, staged_environment: &[u8]) -> Result<()> {
    require(
        String::from_utf8_lossy(staged_environment)
            .lines()
            .any(|line| line.starts_with(&format!("{SECRET_ENVIRONMENT_MANIFEST}="))),
        format!(
            "{provider} staged environment omitted the non-secret environment binding manifest"
        ),
    )
}
