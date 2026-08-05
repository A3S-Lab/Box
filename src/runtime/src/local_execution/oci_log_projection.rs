//! Detached, exact-generation projection of OCI init output into Box logs.

use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use a3s_box_core::log::{
    ManagedOciLogEndpoint, ManagedOciLogWorkerMarker, ManagedOciLogWorkerSpec,
    MANAGED_OCI_LOG_WORKER_SCHEMA,
};
use a3s_box_core::{ExecutionId, ExecutionManagerError, ExecutionManagerResult};

use super::{OciRuntimeBinding, OciRuntimeEndpoint};
use crate::BoxRecord;

const READY_TIMEOUT: Duration = Duration::from_secs(3);
const DRAIN_TIMEOUT: Duration = Duration::from_secs(30);
const POLL_INTERVAL: Duration = Duration::from_millis(10);
const START_FAILURE_LOG_BYTES: u64 = 4 * 1024;
const PROJECTION_DIRECTORY: &str = "oci-log-projection";
const READY_FILE: &str = "ready.json";
const DRAINED_FILE: &str = "drained.json";
const WORKER_LOG_FILE: &str = "worker.log";

pub(super) async fn ensure(
    record: &BoxRecord,
    binding: &OciRuntimeBinding,
) -> ExecutionManagerResult<()> {
    let record = record.clone();
    let execution_id = record.id.clone();
    let binding = binding.clone();
    tokio::task::spawn_blocking(move || ensure_blocking(&record, &binding))
        .await
        .map_err(|error| {
            ExecutionManagerError::Internal(format!(
                "managed OCI log projection startup task failed for {}: {error}",
                execution_id
            ))
        })?
}

pub(super) async fn wait_drained(
    record: &BoxRecord,
    binding: &OciRuntimeBinding,
) -> ExecutionManagerResult<()> {
    let spec = worker_spec(record, binding)?;
    let deadline = Instant::now() + DRAIN_TIMEOUT;
    loop {
        if read_marker(&spec.drained_file)?
            .as_ref()
            .is_some_and(|marker| marker_matches(&spec, marker))
        {
            return Ok(());
        }

        match read_marker(&spec.ready_file)? {
            Some(marker) if marker_matches(&spec, &marker) && marker_is_running(&marker) => {}
            Some(marker) if marker_matches(&spec, &marker) => {
                return Err(ExecutionManagerError::Unavailable(format!(
                    "managed OCI log worker for {} generation {} exited before publishing drain evidence",
                    spec.runtime_container_id, spec.runtime_generation
                )));
            }
            Some(_) => {
                return Err(ExecutionManagerError::Conflict {
                    execution_id: ExecutionId::new(record.id.clone())?,
                    message: "another managed OCI log projection owns this Box directory"
                        .to_string(),
                });
            }
            None => {
                return Err(ExecutionManagerError::Unavailable(format!(
                    "managed OCI log worker for {} generation {} has no readiness evidence",
                    spec.runtime_container_id, spec.runtime_generation
                )));
            }
        }

        if Instant::now() >= deadline {
            return Err(ExecutionManagerError::Unavailable(format!(
                "timed out draining managed OCI init logs for {} generation {}",
                spec.runtime_container_id, spec.runtime_generation
            )));
        }
        tokio::time::sleep(POLL_INTERVAL).await;
    }
}

fn ensure_blocking(record: &BoxRecord, binding: &OciRuntimeBinding) -> ExecutionManagerResult<()> {
    let spec = worker_spec(record, binding)?;
    prepare_marker_paths(&spec)?;

    if let Some(ready) = read_marker(&spec.ready_file)? {
        if marker_matches(&spec, &ready) {
            if read_marker(&spec.drained_file)?
                .as_ref()
                .is_some_and(|marker| marker_matches(&spec, marker))
            {
                return Ok(());
            }
            if marker_is_running(&ready) {
                return Ok(());
            }
            return Err(ExecutionManagerError::Unavailable(format!(
                "managed OCI log worker for {} generation {} exited before drain; refusing to replay its Box log projection",
                spec.runtime_container_id, spec.runtime_generation
            )));
        }
        if marker_is_running(&ready) {
            return Err(ExecutionManagerError::Conflict {
                execution_id: ExecutionId::new(record.id.clone())?,
                message: format!(
                    "managed OCI log worker for {} generation {} still owns this Box directory",
                    ready.runtime_container_id, ready.runtime_generation
                ),
            });
        }
        remove_file_if_present(&spec.ready_file)?;
    }

    if let Some(drained) = read_marker(&spec.drained_file)? {
        if marker_matches(&spec, &drained) {
            return Ok(());
        }
        remove_file_if_present(&spec.drained_file)?;
    }

    let shim = crate::vmm::VmController::find_shim().map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "managed OCI log projection requires a3s-box-shim: {error}"
        ))
    })?;
    let encoded = serde_json::to_string(&spec).map_err(|error| {
        ExecutionManagerError::Internal(format!(
            "failed to encode managed OCI log worker configuration: {error}"
        ))
    })?;
    let worker_log_path = projection_directory(record).join(WORKER_LOG_FILE);
    let stdout = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&worker_log_path)
        .map_err(|error| projection_io("open worker log", &worker_log_path, error))?;
    let stderr = stdout
        .try_clone()
        .map_err(|error| projection_io("clone worker log", &worker_log_path, error))?;
    let mut child = Command::new(shim)
        .arg("--managed-oci-log-worker-config")
        .arg(encoded)
        .env("LC_ALL", "C")
        .stdin(Stdio::null())
        .stdout(Stdio::from(stdout))
        .stderr(Stdio::from(stderr))
        .spawn()
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to start managed OCI log worker for {}: {error}",
                record.id
            ))
        })?;

    let deadline = Instant::now() + READY_TIMEOUT;
    loop {
        if let Some(marker) = read_marker(&spec.ready_file)? {
            if marker_matches(&spec, &marker)
                && marker.pid == child.id()
                && marker_is_running(&marker)
            {
                reap_in_background(child);
                return Ok(());
            }
            reap_failed_worker(&mut child);
            return Err(ExecutionManagerError::Internal(format!(
                "managed OCI log worker published mismatched readiness for {}",
                record.id
            )));
        }
        match child.try_wait() {
            Ok(Some(status)) => {
                let diagnostics = read_log_tail(&worker_log_path, START_FAILURE_LOG_BYTES)
                    .map(|tail| format!(": {tail}"))
                    .unwrap_or_default();
                return Err(ExecutionManagerError::Unavailable(format!(
                    "managed OCI log worker exited before readiness with {status}{diagnostics}"
                )));
            }
            Ok(None) => {}
            Err(error) => {
                reap_failed_worker(&mut child);
                return Err(projection_io(
                    "inspect managed OCI log worker",
                    &worker_log_path,
                    error,
                ));
            }
        }
        if Instant::now() >= deadline {
            reap_failed_worker(&mut child);
            return Err(ExecutionManagerError::Unavailable(format!(
                "timed out waiting for managed OCI log worker readiness for {}",
                record.id
            )));
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

fn worker_spec(
    record: &BoxRecord,
    binding: &OciRuntimeBinding,
) -> ExecutionManagerResult<ManagedOciLogWorkerSpec> {
    let execution_id = ExecutionId::new(record.id.clone())?;
    binding.validate_for(&execution_id)?;
    let metadata = record.managed_execution.as_ref().ok_or_else(|| {
        ExecutionManagerError::Internal(format!(
            "execution {execution_id} has no managed metadata for log projection"
        ))
    })?;
    let runtime_generation = binding.target.generation.ok_or_else(|| {
        ExecutionManagerError::Internal(format!(
            "execution {execution_id} has no exact runtime generation for log projection"
        ))
    })?;
    let endpoint = match &binding.endpoint {
        OciRuntimeEndpoint::UnixSocket { path } => {
            ManagedOciLogEndpoint::UnixSocket { path: path.clone() }
        }
        OciRuntimeEndpoint::WindowsNamedPipe { name } => {
            ManagedOciLogEndpoint::WindowsNamedPipe { name: name.clone() }
        }
    };
    let directory = projection_directory(record);
    Ok(ManagedOciLogWorkerSpec {
        schema: MANAGED_OCI_LOG_WORKER_SCHEMA.to_string(),
        box_id: record.id.clone(),
        execution_generation: metadata.generation.get(),
        endpoint,
        runtime_container_id: binding.target.id.to_string(),
        runtime_generation: runtime_generation.0,
        console_log: record.console_log.clone(),
        log_config: record.log_config.clone(),
        ready_file: directory.join(READY_FILE),
        drained_file: directory.join(DRAINED_FILE),
    })
}

fn projection_directory(record: &BoxRecord) -> PathBuf {
    record.box_dir.join(PROJECTION_DIRECTORY)
}

fn prepare_marker_paths(spec: &ManagedOciLogWorkerSpec) -> ExecutionManagerResult<()> {
    let directory = spec.ready_file.parent().ok_or_else(|| {
        ExecutionManagerError::Internal("managed OCI ready marker has no parent".to_string())
    })?;
    if spec.drained_file.parent() != Some(directory) {
        return Err(ExecutionManagerError::Internal(
            "managed OCI projection markers do not share one directory".to_string(),
        ));
    }
    std::fs::create_dir_all(directory)
        .map_err(|error| projection_io("create projection directory", directory, error))
}

fn read_marker(path: &Path) -> ExecutionManagerResult<Option<ManagedOciLogWorkerMarker>> {
    let metadata = match std::fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(projection_io("inspect marker", path, error)),
    };
    if !metadata.is_file() || metadata.len() > 16 * 1024 {
        return Err(ExecutionManagerError::Internal(format!(
            "managed OCI projection marker is not a bounded regular file: {}",
            path.display()
        )));
    }
    let bytes = std::fs::read(path).map_err(|error| projection_io("read marker", path, error))?;
    serde_json::from_slice(&bytes).map(Some).map_err(|error| {
        ExecutionManagerError::Internal(format!(
            "managed OCI projection marker is invalid at {}: {error}",
            path.display()
        ))
    })
}

fn marker_matches(spec: &ManagedOciLogWorkerSpec, marker: &ManagedOciLogWorkerMarker) -> bool {
    marker.schema == MANAGED_OCI_LOG_WORKER_SCHEMA
        && marker.box_id == spec.box_id
        && marker.execution_generation == spec.execution_generation
        && marker.runtime_container_id == spec.runtime_container_id
        && marker.runtime_generation == spec.runtime_generation
        && marker.pid != 0
}

fn marker_is_running(marker: &ManagedOciLogWorkerMarker) -> bool {
    crate::process::is_process_running_with_identity(marker.pid, marker.pid_start_time)
}

fn remove_file_if_present(path: &Path) -> ExecutionManagerResult<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(projection_io("remove stale marker", path, error)),
    }
}

fn reap_in_background(mut child: Child) {
    std::thread::spawn(move || {
        let _ = child.wait();
    });
}

fn reap_failed_worker(child: &mut Child) {
    let _ = child.kill();
    let _ = child.wait();
}

fn read_log_tail(path: &Path, limit: u64) -> Option<String> {
    let mut file = std::fs::File::open(path).ok()?;
    let length = file.metadata().ok()?.len();
    let offset = length.saturating_sub(limit);
    file.seek(SeekFrom::Start(offset)).ok()?;
    let mut bytes = Vec::with_capacity((length - offset) as usize);
    file.take(limit).read_to_end(&mut bytes).ok()?;
    let tail = String::from_utf8_lossy(&bytes).trim().to_string();
    (!tail.is_empty()).then_some(tail)
}

fn projection_io(operation: &str, path: &Path, error: std::io::Error) -> ExecutionManagerError {
    ExecutionManagerError::Internal(format!(
        "failed to {operation} at {}: {error}",
        path.display()
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::log::LogConfig;

    fn spec(directory: &Path) -> ManagedOciLogWorkerSpec {
        ManagedOciLogWorkerSpec {
            schema: MANAGED_OCI_LOG_WORKER_SCHEMA.to_string(),
            box_id: "box-id".to_string(),
            execution_generation: 7,
            endpoint: ManagedOciLogEndpoint::UnixSocket {
                path: directory.join("runtime.sock"),
            },
            runtime_container_id: "a3s-box-box-id".to_string(),
            runtime_generation: 11,
            console_log: directory.join("console.log"),
            log_config: LogConfig::default(),
            ready_file: directory.join(READY_FILE),
            drained_file: directory.join(DRAINED_FILE),
        }
    }

    #[test]
    fn markers_are_fenced_by_both_box_and_runtime_generation() {
        let directory = tempfile::tempdir().unwrap();
        let spec = spec(directory.path());
        let marker = ManagedOciLogWorkerMarker {
            schema: MANAGED_OCI_LOG_WORKER_SCHEMA.to_string(),
            box_id: spec.box_id.clone(),
            execution_generation: spec.execution_generation,
            runtime_container_id: spec.runtime_container_id.clone(),
            runtime_generation: spec.runtime_generation,
            pid: 42,
            pid_start_time: Some(99),
        };

        assert!(marker_matches(&spec, &marker));
        let mut stale_box = marker.clone();
        stale_box.execution_generation -= 1;
        assert!(!marker_matches(&spec, &stale_box));
        let mut stale_runtime = marker;
        stale_runtime.runtime_generation -= 1;
        assert!(!marker_matches(&spec, &stale_runtime));
    }

    #[test]
    fn marker_reader_rejects_oversized_or_invalid_evidence() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("marker.json");
        std::fs::write(&path, b"not-json").unwrap();
        assert!(read_marker(&path)
            .unwrap_err()
            .to_string()
            .contains("invalid"));
        std::fs::write(&path, vec![b'x'; 16 * 1024 + 1]).unwrap();
        assert!(read_marker(&path)
            .unwrap_err()
            .to_string()
            .contains("bounded regular file"));
    }
}
