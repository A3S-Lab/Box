//! Detached projection of one exact OCI init process into Box-owned logs.

use std::io::Write;
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::log::{
    run_log_processor_with_ready_and_eof_policy, ConsoleEofPolicy, ManagedOciLogEndpoint,
    ManagedOciLogWorkerMarker, ManagedOciLogWorkerSpec, MANAGED_OCI_LOG_WORKER_SCHEMA,
};
use a3s_oci_sdk::oci_spec::runtime::ContainerState;
use a3s_oci_sdk::{
    ContainerId, ContainerTarget, ErrorCode, Generation, LocalIpcEndpoint, OutputChunk,
    OutputStream, ProcessId, ProcessTarget, ReadOutputRequest, RuntimeClient, StateRequest,
};

const OUTPUT_POLL_BYTES: u32 = 64 * 1024;
const OUTPUT_WAIT_MILLIS: u64 = 250;
const RETRY_DELAY_MILLIS: u64 = 25;
const PROCESSOR_READY_TIMEOUT: Duration = Duration::from_secs(3);

pub(super) fn run(config: &str) -> Result<()> {
    let spec: ManagedOciLogWorkerSpec = serde_json::from_str(config).map_err(|error| {
        boot_error(format!(
            "Failed to parse managed OCI log worker config: {error}"
        ))
    })?;
    validate_spec(&spec)?;

    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            boot_error(format!(
                "Failed to initialize managed OCI log worker runtime: {error}"
            ))
        })?;
    runtime.block_on(run_async(spec))
}

async fn run_async(spec: ManagedOciLogWorkerSpec) -> Result<()> {
    let endpoint = sdk_endpoint(&spec.endpoint)?;
    let client = RuntimeClient::connect(&endpoint).await.map_err(|error| {
        boot_error(format!(
            "Failed to connect managed OCI log worker to runtime: {error}"
        ))
    })?;
    let target = exact_process_target(&spec)?;
    let stderr_log = a3s_box_core::log::stderr_console_path(&spec.console_log);
    let log_dir = spec
        .console_log
        .parent()
        .ok_or_else(|| {
            boot_error(format!(
                "Managed OCI console path has no parent: {}",
                spec.console_log.display()
            ))
        })?
        .to_path_buf();
    std::fs::create_dir_all(&log_dir).map_err(BoxError::IoError)?;

    let mut stdout = open_projection_stream(&spec.console_log)?;
    let mut stderr = open_projection_stream(&stderr_log)?;
    let stop = Arc::new(AtomicBool::new(false));
    let ready = Arc::new(AtomicUsize::new(0));
    let processor_stop = Arc::clone(&stop);
    let processor_ready = Arc::clone(&ready);
    let console_log = spec.console_log.clone();
    let log_config = spec.log_config.clone();
    let processor = std::thread::spawn(move || {
        run_log_processor_with_ready_and_eof_policy(
            &console_log,
            &log_dir,
            &log_config,
            &processor_stop,
            Some(&processor_ready),
            ConsoleEofPolicy::WriterClosed,
        );
    });

    wait_for_processor_ready(&processor, &ready, &stop)?;
    let marker = worker_marker(&spec);
    write_marker(&spec.ready_file, &marker)?;

    let projection = project_output(&client, &target, &mut stdout, &mut stderr).await;
    let _ = stdout.sync_all();
    let _ = stderr.sync_all();
    drop((stdout, stderr));
    stop.store(true, Ordering::SeqCst);
    processor.join().map_err(|_| {
        boot_error(format!(
            "Managed OCI log processor panicked for {}",
            spec.box_id
        ))
    })?;
    projection?;

    write_marker(&spec.drained_file, &marker)?;
    tracing::debug!(
        box_id = %spec.box_id,
        runtime_container = %spec.runtime_container_id,
        runtime_generation = spec.runtime_generation,
        "Managed OCI init logs fully drained"
    );
    Ok(())
}

fn validate_spec(spec: &ManagedOciLogWorkerSpec) -> Result<()> {
    let endpoint_valid = match &spec.endpoint {
        ManagedOciLogEndpoint::UnixSocket { path } => path.is_absolute(),
        ManagedOciLogEndpoint::WindowsNamedPipe { name } => {
            name.to_ascii_lowercase().starts_with(r"\\.\pipe\")
                && name.len() > r"\\.\pipe\".len()
                && !name.as_bytes().contains(&0)
        }
    };
    if spec.schema != MANAGED_OCI_LOG_WORKER_SCHEMA
        || spec.box_id.is_empty()
        || spec.execution_generation == 0
        || spec.runtime_generation == 0
        || ContainerId::new(spec.runtime_container_id.clone()).is_err()
        || !endpoint_valid
        || !spec.console_log.is_absolute()
        || !spec.ready_file.is_absolute()
        || !spec.drained_file.is_absolute()
        || spec.ready_file == spec.drained_file
    {
        return Err(boot_error(
            "Invalid managed OCI log worker identity or path configuration",
        ));
    }
    Ok(())
}

fn sdk_endpoint(endpoint: &ManagedOciLogEndpoint) -> Result<LocalIpcEndpoint> {
    #[cfg(unix)]
    {
        return match endpoint {
            ManagedOciLogEndpoint::UnixSocket { path } => {
                LocalIpcEndpoint::unix_socket(path.clone()).map_err(|error| {
                    boot_error(format!("Invalid managed OCI Unix endpoint: {error}"))
                })
            }
            ManagedOciLogEndpoint::WindowsNamedPipe { .. } => Err(boot_error(
                "A Windows managed OCI endpoint cannot be opened on this host",
            )),
        };
    }
    #[cfg(windows)]
    {
        return match endpoint {
            ManagedOciLogEndpoint::WindowsNamedPipe { name } => {
                LocalIpcEndpoint::windows_named_pipe(name.clone()).map_err(|error| {
                    boot_error(format!("Invalid managed OCI named-pipe endpoint: {error}"))
                })
            }
            ManagedOciLogEndpoint::UnixSocket { .. } => Err(boot_error(
                "A Unix managed OCI endpoint cannot be opened on this host",
            )),
        };
    }
    #[allow(unreachable_code)]
    Err(boot_error(
        "Managed OCI log projection is unsupported on this host",
    ))
}

fn exact_process_target(spec: &ManagedOciLogWorkerSpec) -> Result<ProcessTarget> {
    let id = ContainerId::new(spec.runtime_container_id.clone())
        .map_err(|error| boot_error(format!("Invalid managed OCI container ID: {error}")))?;
    Ok(ProcessTarget {
        container: ContainerTarget::exact(id, Generation(spec.runtime_generation)),
        process_id: ProcessId::init(),
    })
}

fn open_projection_stream(path: &Path) -> Result<std::fs::File> {
    let parent = path.parent().ok_or_else(|| {
        boot_error(format!(
            "Managed OCI projection path has no parent: {}",
            path.display()
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(BoxError::IoError)?;
    std::fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(path)
        .map_err(BoxError::IoError)
}

fn wait_for_processor_ready(
    processor: &std::thread::JoinHandle<()>,
    ready: &AtomicUsize,
    stop: &AtomicBool,
) -> Result<()> {
    let deadline = std::time::Instant::now() + PROCESSOR_READY_TIMEOUT;
    while ready.load(Ordering::Acquire) < 2 {
        if processor.is_finished() {
            return Err(boot_error(
                "Managed OCI log processor exited before opening both console streams",
            ));
        }
        if std::time::Instant::now() >= deadline {
            stop.store(true, Ordering::SeqCst);
            return Err(boot_error(
                "Managed OCI log processor did not become ready before timeout",
            ));
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    Ok(())
}

async fn project_output(
    client: &RuntimeClient,
    target: &ProcessTarget,
    stdout: &mut std::fs::File,
    stderr: &mut std::fs::File,
) -> Result<()> {
    let mut cursor = 0_u64;
    let mut stdout_eof = false;
    let mut stderr_eof = false;
    let mut process_observed = false;

    while !stdout_eof || !stderr_eof {
        let chunks = match client
            .read_output(ReadOutputRequest {
                process: target.clone(),
                after_sequence: cursor,
                max_bytes: OUTPUT_POLL_BYTES,
                wait_timeout_ms: Some(OUTPUT_WAIT_MILLIS),
            })
            .await
        {
            Ok(chunks) => {
                process_observed = true;
                chunks
            }
            Err(error) if error.code == ErrorCode::Unavailable => {
                tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MILLIS)).await;
                continue;
            }
            Err(error)
                if !process_observed
                    && matches!(
                        error.code,
                        ErrorCode::NotFound | ErrorCode::FailedPrecondition
                    ) =>
            {
                match client
                    .state(StateRequest {
                        target: target.container.clone(),
                    })
                    .await
                {
                    Ok(record)
                        if matches!(
                            record.state.status(),
                            ContainerState::Creating
                                | ContainerState::Created
                                | ContainerState::Running
                        ) =>
                    {
                        tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MILLIS)).await;
                        continue;
                    }
                    Err(state_error) if state_error.code == ErrorCode::Unavailable => {
                        tokio::time::sleep(Duration::from_millis(RETRY_DELAY_MILLIS)).await;
                        continue;
                    }
                    Ok(record) => {
                        return Err(boot_error(format!(
                            "Managed OCI init output never became available before container state {:?}",
                            record.state.status()
                        )));
                    }
                    Err(state_error) => {
                        return Err(boot_error(format!(
                            "Managed OCI init output target disappeared: {state_error}"
                        )));
                    }
                }
            }
            Err(error) => {
                return Err(boot_error(format!(
                    "Failed to read managed OCI init output at cursor {cursor}: {error}"
                )));
            }
        };

        accept_chunks(
            chunks,
            &mut cursor,
            &mut stdout_eof,
            &mut stderr_eof,
            stdout,
            stderr,
        )?;
    }
    Ok(())
}

fn accept_chunks(
    chunks: Vec<OutputChunk>,
    cursor: &mut u64,
    stdout_eof: &mut bool,
    stderr_eof: &mut bool,
    stdout: &mut std::fs::File,
    stderr: &mut std::fs::File,
) -> Result<()> {
    for chunk in chunks {
        let width = if chunk.eof {
            if !chunk.data.is_empty() {
                return Err(boot_error("Managed OCI output EOF frame contains data"));
            }
            1
        } else {
            if chunk.data.is_empty() {
                return Err(boot_error("Managed OCI output data frame is empty"));
            }
            u64::try_from(chunk.data.len())
                .map_err(|_| boot_error("Managed OCI output frame is too large"))?
        };
        let expected = cursor
            .checked_add(width)
            .ok_or_else(|| boot_error("Managed OCI output cursor is exhausted"))?;
        if chunk.sequence != expected {
            return Err(boot_error(format!(
                "Managed OCI output cursor was {}, expected {expected}",
                chunk.sequence
            )));
        }
        *cursor = chunk.sequence;
        let (output, eof) = match chunk.stream {
            OutputStream::Stdout => (&mut *stdout, &mut *stdout_eof),
            OutputStream::Stderr => (&mut *stderr, &mut *stderr_eof),
        };
        if chunk.eof {
            if *eof {
                return Err(boot_error("Managed OCI output repeated an EOF frame"));
            }
            *eof = true;
        } else {
            if *eof {
                return Err(boot_error("Managed OCI output arrived after stream EOF"));
            }
            output.write_all(&chunk.data).map_err(BoxError::IoError)?;
            output.flush().map_err(BoxError::IoError)?;
        }
    }
    Ok(())
}

fn worker_marker(spec: &ManagedOciLogWorkerSpec) -> ManagedOciLogWorkerMarker {
    ManagedOciLogWorkerMarker {
        schema: MANAGED_OCI_LOG_WORKER_SCHEMA.to_string(),
        box_id: spec.box_id.clone(),
        execution_generation: spec.execution_generation,
        runtime_container_id: spec.runtime_container_id.clone(),
        runtime_generation: spec.runtime_generation,
        pid: std::process::id(),
        pid_start_time: current_process_start_time(),
    }
}

fn write_marker(path: &Path, marker: &ManagedOciLogWorkerMarker) -> Result<()> {
    let parent = path.parent().ok_or_else(|| {
        boot_error(format!(
            "Managed OCI marker has no parent: {}",
            path.display()
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(BoxError::IoError)?;
    let nonce = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = path.with_extension(format!("tmp-{}-{nonce}", std::process::id()));
    let bytes = serde_json::to_vec(marker)
        .map_err(|error| boot_error(format!("Failed to encode managed OCI marker: {error}")))?;
    let mut file = std::fs::OpenOptions::new()
        .create_new(true)
        .write(true)
        .open(&temporary)
        .map_err(BoxError::IoError)?;
    file.write_all(&bytes).map_err(BoxError::IoError)?;
    file.write_all(b"\n").map_err(BoxError::IoError)?;
    file.sync_all().map_err(BoxError::IoError)?;
    if path.exists() {
        std::fs::remove_file(path).map_err(BoxError::IoError)?;
    }
    std::fs::rename(temporary, path).map_err(BoxError::IoError)
}

#[cfg(target_os = "linux")]
fn current_process_start_time() -> Option<u64> {
    let stat = std::fs::read_to_string(format!("/proc/{}/stat", std::process::id())).ok()?;
    let close = stat.rfind(')')?;
    stat.get(close + 1..)?
        .split_whitespace()
        .nth(19)?
        .parse()
        .ok()
}

#[cfg(not(target_os = "linux"))]
fn current_process_start_time() -> Option<u64> {
    None
}

fn boot_error(message: impl Into<String>) -> BoxError {
    BoxError::BoxBootError {
        message: message.into(),
        hint: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn accepts_contiguous_split_output_and_both_eofs() {
        let directory = tempfile::tempdir().unwrap();
        let mut stdout = std::fs::File::create(directory.path().join("stdout")).unwrap();
        let mut stderr = std::fs::File::create(directory.path().join("stderr")).unwrap();
        let mut cursor = 0;
        let mut stdout_eof = false;
        let mut stderr_eof = false;

        accept_chunks(
            vec![
                OutputChunk {
                    sequence: 3,
                    stream: OutputStream::Stdout,
                    data: b"out".to_vec(),
                    eof: false,
                },
                OutputChunk {
                    sequence: 6,
                    stream: OutputStream::Stderr,
                    data: b"err".to_vec(),
                    eof: false,
                },
                OutputChunk {
                    sequence: 7,
                    stream: OutputStream::Stdout,
                    data: Vec::new(),
                    eof: true,
                },
                OutputChunk {
                    sequence: 8,
                    stream: OutputStream::Stderr,
                    data: Vec::new(),
                    eof: true,
                },
            ],
            &mut cursor,
            &mut stdout_eof,
            &mut stderr_eof,
            &mut stdout,
            &mut stderr,
        )
        .unwrap();
        drop((stdout, stderr));

        assert_eq!(cursor, 8);
        assert!(stdout_eof && stderr_eof);
        assert_eq!(
            std::fs::read(directory.path().join("stdout")).unwrap(),
            b"out"
        );
        assert_eq!(
            std::fs::read(directory.path().join("stderr")).unwrap(),
            b"err"
        );
    }

    #[test]
    fn rejects_non_contiguous_output_without_writing_it() {
        let directory = tempfile::tempdir().unwrap();
        let mut stdout = std::fs::File::create(directory.path().join("stdout")).unwrap();
        let mut stderr = std::fs::File::create(directory.path().join("stderr")).unwrap();
        let mut cursor = 0;
        let mut stdout_eof = false;
        let mut stderr_eof = false;
        let error = accept_chunks(
            vec![OutputChunk {
                sequence: 4,
                stream: OutputStream::Stdout,
                data: b"gap".to_vec(),
                eof: false,
            }],
            &mut cursor,
            &mut stdout_eof,
            &mut stderr_eof,
            &mut stdout,
            &mut stderr,
        )
        .unwrap_err();

        assert!(error.to_string().contains("expected 3"));
        assert_eq!(
            std::fs::metadata(directory.path().join("stdout"))
                .unwrap()
                .len(),
            0
        );
    }
}
