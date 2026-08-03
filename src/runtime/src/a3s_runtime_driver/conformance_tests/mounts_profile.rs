use std::path::Path;
use std::time::Duration;

use a3s_box_core::ExecutionIsolation;
use a3s_runtime::contract::{RestartPolicy, RuntimeMount, RuntimeMountSource, RuntimeUnitState};
use a3s_runtime::RuntimeClient;

use super::fixture::BoxRuntimeConformanceFixture;
#[cfg(target_os = "linux")]
use super::mounts_evidence::sandbox_private_artifact_alias;
use super::mounts_evidence::{
    require_bind_config, require_live_tmpfs_mount, require_tmpfs_config, TMPFS_SIZE_BYTES,
};
use super::{require, Result};

pub(super) async fn run(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    #[cfg(target_os = "linux")]
    private_read_only_artifact(fixture, client).await?;
    read_only_enforcement(fixture, client).await?;
    tmpfs_isolation(fixture, client).await?;
    persistent_volume_reuse(fixture, client).await?;
    mount_cleanup(fixture, client).await
}

#[cfg(target_os = "linux")]
async fn private_read_only_artifact(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    const TARGET: &str = "/mnt/r17-private-artifact";
    let mut service = fixture.cases.service(
        "mount-private-artifact",
        "if printf forbidden > /mnt/r17-private-artifact/forbidden 2>/dev/null; then exit 76; fi; test \"$(cat /mnt/r17-private-artifact/payload.txt)\" = r17-private-artifact || exit 77; printf ready > /workspace/r17-private-artifact-ready; exec sleep 3600",
    );
    service.spec.mounts = vec![RuntimeMount {
        name: "private-artifact".into(),
        source: RuntimeMountSource::Artifact {
            artifact: service.spec.artifact.clone(),
        },
        target: TARGET.into(),
        read_only: true,
    }];

    let running = client.apply(&service).await?;
    require(
        running.state == RuntimeUnitState::Running,
        "private Artifact attachment did not reach running",
    )?;
    let record = fixture.record_for(&service.spec).await?;
    wait_for_file(
        &record.box_dir.join("workspace/r17-private-artifact-ready"),
        "provider workload did not read the private Artifact attachment",
    )
    .await?;
    require(
        !fixture.private_artifact_source().join("forbidden").exists(),
        "provider workload wrote through the read-only private Artifact attachment",
    )?;
    require(
        std::fs::metadata(fixture.private_artifact_root())
            .map_err(|error| super::external("inspect private Artifact root", error))?
            .permissions()
            .mode()
            & 0o7777
            == 0o700,
        "Box changed the caller-owned private Artifact root permissions",
    )?;

    require_bind_config(&record, TARGET, true)?;
    let attachment_alias = match fixture.driver.execution_isolation() {
        ExecutionIsolation::Sandbox => {
            Some(sandbox_private_artifact_alias(fixture, &record, TARGET)?)
        }
        ExecutionIsolation::Microvm => None,
    };

    fixture
        .remove_unit(client, &service.spec, "mount-private-artifact")
        .await?;
    if let Some((alias_root, alias)) = attachment_alias {
        require(
            !alias_root.exists()
                && !std::fs::read_to_string("/proc/self/mountinfo")
                    .map_err(|error| super::external("re-read private Artifact mountinfo", error))?
                    .contains(alias.to_string_lossy().as_ref()),
            "private Artifact removal retained a Box attachment alias",
        )?;
    }
    Ok(())
}

async fn persistent_volume_reuse(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    const TARGET: &str = "/mnt/r17-persistent";
    let mut writer = fixture.cases.service(
        "mount-persistent-writer",
        "printf durable > /mnt/r17-persistent/marker; exec sleep 3600",
    );
    let volume_id = format!("{}-volume", writer.spec.unit_id);
    writer.spec.mounts = vec![volume("workspace", &volume_id, TARGET, false)];
    let running = client.apply(&writer).await?;
    require(
        running.state == RuntimeUnitState::Running,
        "persistent-Volume writer did not reach running",
    )?;
    let writer_record = fixture.record_for(&writer.spec).await?;
    let volume_name = writer_record
        .volume_names
        .first()
        .cloned()
        .ok_or_else(|| super::protocol("persistent-Volume writer has no named Volume"))?;
    require(
        writer_record.volume_names.len() == 1,
        "persistent-Volume writer attached an unexpected named Volume",
    )?;
    require_bind_config(&writer_record, TARGET, false)?;
    let store = crate::VolumeStore::new(
        fixture.home_dir.join("volumes.json"),
        fixture.home_dir.join("volumes"),
    );
    let attached = store
        .get(&volume_name)
        .map_err(|error| super::external("load attached persistent Volume", error))?
        .ok_or_else(|| super::protocol("persistent Volume disappeared while attached"))?;
    require(
        attached.in_use_by == vec![writer_record.id.clone()],
        "persistent Volume did not fence its live Box attachment",
    )?;
    let marker = Path::new(&attached.mount_point).join("marker");
    require(
        std::fs::read(&marker)
            .map_err(|error| super::external("read persistent-Volume marker", error))?
            == b"durable",
        "persistent Volume did not expose the workload write on its canonical host path",
    )?;

    fixture
        .remove_unit(client, &writer.spec, "mount-persistent-writer")
        .await?;
    let detached = store
        .get(&volume_name)
        .map_err(|error| super::external("load detached persistent Volume", error))?
        .ok_or_else(|| super::protocol("persistent Volume was deleted with its first workload"))?;
    require(
        detached.in_use_by.is_empty() && marker.is_file(),
        "persistent Volume did not detach while retaining its data",
    )?;

    let mut reader = fixture.cases.service(
        "mount-persistent-reader",
        "if sh -c 'printf forbidden > /mnt/r17-persistent/forbidden' 2>/dev/null; then exit 74; fi; test \"$(cat /mnt/r17-persistent/marker)\" = durable || exit 75; printf ready > /workspace/r17-persistent-reader-ready; exec sleep 3600",
    );
    reader.spec.mounts = vec![volume("workspace", &volume_id, TARGET, true)];
    let read = client.apply(&reader).await?;
    require(
        read.state == RuntimeUnitState::Running,
        "persistent-Volume reader did not reach running",
    )?;
    let reader_record = fixture.record_for(&reader.spec).await?;
    wait_for_file(
        &reader_record
            .box_dir
            .join("workspace/r17-persistent-reader-ready"),
        "persistent-Volume reader did not verify retained read-only data",
    )
    .await?;
    require(
        !Path::new(&detached.mount_point).join("forbidden").exists(),
        "persistent Volume accepted a write through its read-only reuse",
    )?;
    require(
        reader_record.volume_names == vec![volume_name.clone()],
        "persistent Volume identity changed across Runtime units",
    )?;
    require(
        store
            .get(&volume_name)
            .map_err(|error| super::external("load reused persistent Volume", error))?
            .is_some_and(|volume| volume.in_use_by == vec![reader_record.id.clone()]),
        "persistent Volume did not fence its second live Box attachment",
    )?;
    require_bind_config(&reader_record, TARGET, true)?;
    fixture
        .remove_unit(client, &reader.spec, "mount-persistent-reader")
        .await?;
    require(
        store
            .get(&volume_name)
            .map_err(|error| super::external("load reused persistent Volume", error))?
            .is_some_and(|volume| volume.in_use_by.is_empty()),
        "persistent Volume remained attached after second removal",
    )?;
    store
        .remove(&volume_name, false)
        .map_err(|error| super::external("remove conformance persistent Volume", error))?;
    require(
        !marker.exists(),
        "persistent-Volume conformance cleanup retained workload data",
    )
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
        "if [ ! -e /workspace/r17-tmpfs-restart-generation ]; then touch /workspace/r17-tmpfs-restart-generation /r17-tmpfs-restart-marker; printf private > /mnt/r17-isolation/token; exit 17; fi; test -e /r17-tmpfs-restart-marker || exit 72; test ! -e /mnt/r17-isolation/token || exit 73",
        10_000,
    );
    request.spec.mounts = vec![tmpfs("scratch", TARGET, false)];
    request.spec.restart = RestartPolicy::OnFailure { max_retries: 1 };
    let isolated = client.apply(&request).await?;
    let record = fixture.record_for(&request.spec).await?;
    let rootfs_marker = ["rootfs", "upper", "merged"].map(|root| {
        record
            .box_dir
            .join(root)
            .join("r17-tmpfs-restart-marker")
            .exists()
    });
    let underlying_token = ["rootfs", "upper", "merged"].map(|root| {
        record
            .box_dir
            .join(root)
            .join("mnt/r17-isolation/token")
            .exists()
    });
    let workspace_marker = record
        .box_dir
        .join("workspace/r17-tmpfs-restart-generation")
        .exists();
    require(
        isolated.state == RuntimeUnitState::Succeeded,
        format!(
            "tmpfs restart isolation failed: observation_state={:?}, observation_failure={:?}, managed_state={:?}, generation={:?}, exit_code={:?}, workspace_marker={workspace_marker}, rootfs_marker[rootfs,upper,merged]={rootfs_marker:?}, underlying_token[rootfs,upper,merged]={underlying_token:?}",
            isolated.state,
            isolated.failure,
            record.managed_state(),
            record
                .managed_execution
                .as_ref()
                .map(|metadata| metadata.generation.get()),
            record.exit_code,
        ),
    )?;

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
        .ok_or_else(|| super::protocol("tmpfs cleanup fixture has no provider owner PID"))?;
    let log_worker = match fixture.driver.execution_isolation() {
        ExecutionIsolation::Sandbox => {
            let mountinfo =
                std::fs::read_to_string(Path::new("/proc").join(pid.to_string()).join("mountinfo"))
                    .map_err(|error| {
                        super::external("read tmpfs cleanup mount namespace", error)
                    })?;
            require(
                mountinfo.lines().any(|line| {
                    line.contains(&format!(" {TARGET} ")) && line.contains(" - tmpfs ")
                }),
                "running Sandbox did not expose the requested tmpfs mount",
            )?;
            Some(require_log_worker_identity(fixture, &record)?)
        }
        ExecutionIsolation::Microvm => {
            let visible = client
                .exec(&fixture.cases.exec(
                    "mount-cleanup-visible",
                    &service.spec,
                    vec![
                        "/bin/sh".into(),
                        "-c".into(),
                        format!(
                            "awk '$5 == \"{TARGET}\" && $0 ~ / - tmpfs / {{ found=1 }} END {{ exit found ? 0 : 1 }}' /proc/self/mountinfo"
                        ),
                    ],
                    5_000,
                ))
                .await?;
            require(
                visible.exit_code == 0 && visible.stderr.is_empty(),
                format!(
                    "running MicroVM did not expose the requested tmpfs mount: exit_code={} stdout={:?} stderr={:?}",
                    visible.exit_code, visible.stdout, visible.stderr
                ),
            )?;
            None
        }
    };
    let pid_start_time = record
        .pid_start_time
        .ok_or_else(|| super::protocol("tmpfs cleanup fixture has no owner PID start time"))?;
    let box_dir = record.box_dir.clone();

    fixture
        .remove_unit(client, &service.spec, "mount-cleanup")
        .await?;
    require(
        !crate::process::is_process_alive_with_identity(pid, Some(pid_start_time)),
        "tmpfs owner process survived Runtime removal",
    )?;
    if let Some((log_worker_pid, log_worker_pid_start_time)) = log_worker {
        require(
            !crate::process::is_process_alive_with_identity(
                log_worker_pid,
                Some(log_worker_pid_start_time),
            ),
            "tmpfs log worker survived Runtime removal",
        )?;
    }
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

fn volume(name: &str, volume_id: &str, target: &str, read_only: bool) -> RuntimeMount {
    RuntimeMount {
        name: name.into(),
        source: RuntimeMountSource::Volume {
            volume_id: volume_id.into(),
        },
        target: target.into(),
        read_only,
    }
}

async fn wait_for_file(path: &Path, message: &str) -> Result<()> {
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    loop {
        if path.is_file() {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            return Err(super::failure(message));
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
}
