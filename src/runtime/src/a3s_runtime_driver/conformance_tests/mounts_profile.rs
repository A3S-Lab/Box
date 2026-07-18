use std::path::Path;

use a3s_runtime::contract::{RestartPolicy, RuntimeMount, RuntimeMountSource, RuntimeUnitState};
use a3s_runtime::RuntimeClient;

use super::fixture::BoxRuntimeConformanceFixture;
use super::{require, Result};

const TMPFS_SIZE_BYTES: u64 = 4 * 1024 * 1024;

pub(super) async fn run(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    read_only_enforcement(fixture, client).await?;
    tmpfs_isolation(fixture, client).await?;
    mount_cleanup(fixture, client).await
}

async fn read_only_enforcement(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    const TARGET: &str = "/mnt/r17-read-only";
    let mut request = fixture.cases.task(
        "mount-read-only",
        "if sh -c 'printf forbidden > /mnt/r17-read-only/forbidden' 2>/dev/null; then printf 'read-only tmpfs accepted a write\\n' >&2; exit 71; fi; test ! -e /mnt/r17-read-only/forbidden",
        10_000,
    );
    request.spec.mounts = vec![tmpfs("sealed", TARGET, true)];

    let observation = client.apply(&request).await?;
    require(
        observation.state == RuntimeUnitState::Succeeded,
        "read-only tmpfs accepted a workload write",
    )?;
    let record = fixture.record_for(&request.spec).await?;
    require_tmpfs_config(&record, TARGET, true)?;
    fixture
        .remove_unit(client, &request.spec, "mount-read-only")
        .await
}

async fn tmpfs_isolation(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    const TARGET: &str = "/mnt/r17-isolation";
    let mut request = fixture.cases.task(
        "mount-tmpfs-isolation",
        "if [ ! -e /r17-tmpfs-restart-marker ]; then touch /r17-tmpfs-restart-marker; printf private > /mnt/r17-isolation/token; exit 17; fi; test ! -e /mnt/r17-isolation/token",
        10_000,
    );
    request.spec.mounts = vec![tmpfs("scratch", TARGET, false)];
    request.spec.restart = RestartPolicy::OnFailure { max_retries: 1 };
    let isolated = client.apply(&request).await?;
    require(
        isolated.state == RuntimeUnitState::Succeeded,
        "a restarted Runtime unit observed its prior tmpfs contents",
    )?;

    let record = fixture.record_for(&request.spec).await?;
    require(
        record
            .managed_execution
            .as_ref()
            .is_some_and(|metadata| metadata.generation.get() == 2),
        "tmpfs isolation fixture did not execute its required restart",
    )?;
    require_tmpfs_config(&record, TARGET, false)?;
    fixture
        .remove_unit(client, &request.spec, "mount-tmpfs-isolation")
        .await
}

async fn mount_cleanup(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    const TARGET: &str = "/mnt/r17-cleanup";
    let mut service = fixture.cases.service(
        "mount-cleanup",
        "printf mounted > /mnt/r17-cleanup/marker; exec sleep 3600",
    );
    service.spec.mounts = vec![tmpfs("ephemeral", TARGET, false)];
    let running = client.apply(&service).await?;
    require(
        running.state == RuntimeUnitState::Running,
        "tmpfs cleanup fixture Service did not reach running",
    )?;

    let record = fixture.record_for(&service.spec).await?;
    require_live_tmpfs_mount(&record, TARGET, false)?;
    let pid = record
        .pid
        .ok_or_else(|| super::protocol("tmpfs cleanup fixture has no Sandbox init PID"))?;
    let mountinfo =
        std::fs::read_to_string(Path::new("/proc").join(pid.to_string()).join("mountinfo"))
            .map_err(|error| super::external("read tmpfs cleanup mount namespace", error))?;
    require(
        mountinfo
            .lines()
            .any(|line| line.contains(&format!(" {TARGET} ")) && line.contains(" - tmpfs ")),
        "running Sandbox did not expose the requested tmpfs mount",
    )?;
    let pid_start_time = record
        .pid_start_time
        .ok_or_else(|| super::protocol("tmpfs cleanup fixture has no init PID start time"))?;
    let box_dir = record.box_dir.clone();
    let (log_worker_pid, log_worker_pid_start_time) =
        require_log_worker_identity(fixture, &record)?;

    fixture
        .remove_unit(client, &service.spec, "mount-cleanup")
        .await?;
    require(
        !crate::process::is_process_alive_with_identity(pid, Some(pid_start_time)),
        "tmpfs owner process survived Runtime removal",
    )?;
    require(
        !crate::process::is_process_alive_with_identity(
            log_worker_pid,
            Some(log_worker_pid_start_time),
        ),
        "tmpfs log worker survived Runtime removal",
    )?;
    require(
        fixture
            .driver
            .find_generation(&service.spec)
            .await?
            .is_none(),
        "tmpfs cleanup left a provider execution record",
    )?;
    require(
        !box_dir.exists(),
        "tmpfs cleanup left its provider filesystem",
    )
}

fn require_log_worker_identity(
    fixture: &BoxRuntimeConformanceFixture,
    record: &crate::BoxRecord,
) -> Result<(u32, u64)> {
    let runtime = crate::vm::reap::load_recorded_sandbox_runtime(
        &fixture.home_dir,
        &record.box_dir,
        &record.id,
    )
    .map_err(|error| super::external("load tmpfs cleanup runtime", error))?
    .ok_or_else(|| super::protocol("tmpfs cleanup runtime record disappeared"))?;
    let pid = runtime
        .log_worker_pid
        .ok_or_else(|| super::protocol("tmpfs cleanup fixture has no log-worker PID"))?;
    let start_time = runtime
        .log_worker_pid_start_time
        .ok_or_else(|| super::protocol("tmpfs cleanup fixture has no log-worker start time"))?;
    Ok((pid, start_time))
}

fn tmpfs(name: &str, target: &str, read_only: bool) -> RuntimeMount {
    RuntimeMount {
        name: name.into(),
        source: RuntimeMountSource::Tmpfs {
            size_bytes: TMPFS_SIZE_BYTES,
        },
        target: target.into(),
        read_only,
    }
}

fn require_tmpfs_config(record: &crate::BoxRecord, target: &str, read_only: bool) -> Result<()> {
    let config = &record
        .managed_execution
        .as_ref()
        .ok_or_else(|| super::protocol("tmpfs fixture lost managed metadata"))?
        .request
        .config;
    let expected = format!(
        "{target}:size={TMPFS_SIZE_BYTES},{}",
        if read_only { "ro" } else { "rw" }
    );
    require(
        config.tmpfs == vec![expected],
        "Runtime tmpfs intent changed before provider launch",
    )
}

fn require_live_tmpfs_mount(
    record: &crate::BoxRecord,
    target: &str,
    read_only: bool,
) -> Result<()> {
    require_tmpfs_config(record, target, read_only)?;
    let bundle = record.box_dir.join("sandbox/bundle/config.json");
    let value: serde_json::Value = serde_json::from_slice(
        &std::fs::read(&bundle)
            .map_err(|error| super::external("read tmpfs Sandbox OCI configuration", error))?,
    )
    .map_err(|error| super::external("decode tmpfs Sandbox OCI configuration", error))?;
    let mount = value["mounts"]
        .as_array()
        .and_then(|mounts| {
            mounts
                .iter()
                .find(|mount| mount["destination"].as_str() == Some(target))
        })
        .ok_or_else(|| super::protocol("Sandbox OCI configuration omitted the Runtime tmpfs"))?;
    let options = mount["options"]
        .as_array()
        .ok_or_else(|| super::protocol("Runtime tmpfs has no OCI mount options"))?;
    let expected_mode = if read_only { "ro" } else { "rw" };
    let expected_size = format!("size={TMPFS_SIZE_BYTES}");
    require(
        mount["type"] == "tmpfs"
            && options.iter().any(|option| option == expected_mode)
            && options
                .iter()
                .any(|option| option.as_str() == Some(expected_size.as_str())),
        "Sandbox OCI tmpfs did not preserve size and access mode",
    )
}
