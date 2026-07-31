//! Cancellable subprocess boundary for native Dockerfile `RUN`.

use std::process::{Output, Stdio};

use a3s_box_core::error::{BoxError, Result};
use tokio::io::{AsyncRead, AsyncReadExt};

use super::control::BuildExecutionControl;

pub(super) async fn command_output(
    command: std::process::Command,
    control: Option<&BuildExecutionControl>,
) -> Result<Output> {
    let mut command = tokio::process::Command::from(command);
    command
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    let mut child = command.spawn().map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to spawn isolated Dockerfile RUN process: {error}"
        ))
    })?;
    let pid = child.id().ok_or_else(|| {
        BoxError::BuildError("isolated Dockerfile RUN process has no host PID".to_string())
    })?;
    let start_time = crate::process::pid_start_time(pid);
    #[cfg(target_os = "linux")]
    if start_time.is_none() {
        let _ = child.start_kill();
        let _ = child.wait().await;
        return Err(BoxError::BuildError(format!(
            "Failed to capture stable identity for Dockerfile RUN process {pid}"
        )));
    }

    let stdout = child.stdout.take().ok_or_else(|| {
        BoxError::BuildError("Dockerfile RUN stdout pipe was not created".to_string())
    })?;
    let stderr = child.stderr.take().ok_or_else(|| {
        BoxError::BuildError("Dockerfile RUN stderr pipe was not created".to_string())
    })?;
    let stdout_task = tokio::spawn(read_pipe(stdout));
    let stderr_task = tokio::spawn(read_pipe(stderr));

    if let Some(control) = control {
        if let Err(error) = control.run_process_started(pid, start_time).await {
            let _ = child.start_kill();
            let _ = child.wait().await;
            let _ = collect_pipe(stdout_task, "stdout").await;
            let _ = collect_pipe(stderr_task, "stderr").await;
            return Err(error);
        }
    }

    let mut cancellation_error = None;
    let status = if let Some(control) = control {
        tokio::select! {
            status = child.wait() => status,
            cancellation = control.wait_for_cancellation() => {
                if let Err(error) = cancellation {
                    cancellation_error = Some(error);
                }
                let _ = child.start_kill();
                child.wait().await
            }
        }
    } else {
        child.wait().await
    };

    let finish_result = if let Some(control) = control {
        control.run_process_finished(pid, start_time).await
    } else {
        Ok(())
    };
    let stdout = collect_pipe(stdout_task, "stdout").await?;
    let stderr = collect_pipe(stderr_task, "stderr").await?;
    let status = status.map_err(|error| {
        BoxError::BuildError(format!(
            "Failed to wait for isolated Dockerfile RUN process {pid}: {error}"
        ))
    })?;
    finish_result?;
    if let Some(error) = cancellation_error {
        return Err(error);
    }
    if let Some(control) = control {
        control.ensure_active().await?;
    }
    Ok(Output {
        status,
        stdout,
        stderr,
    })
}

async fn read_pipe(mut pipe: impl AsyncRead + Unpin) -> std::io::Result<Vec<u8>> {
    let mut bytes = Vec::new();
    pipe.read_to_end(&mut bytes).await?;
    Ok(bytes)
}

async fn collect_pipe(
    task: tokio::task::JoinHandle<std::io::Result<Vec<u8>>>,
    stream: &str,
) -> Result<Vec<u8>> {
    task.await
        .map_err(|error| {
            BoxError::BuildError(format!("Dockerfile RUN {stream} task failed: {error}"))
        })?
        .map_err(|error| {
            BoxError::BuildError(format!("Failed to read Dockerfile RUN {stream}: {error}"))
        })
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::{Arc, Mutex};

    use async_trait::async_trait;
    use tokio::sync::Semaphore;

    use super::*;
    use crate::oci::build::engine::BuildExecutionObserver;

    struct TestObserver {
        cancelled: AtomicBool,
        started: Semaphore,
        finished: AtomicBool,
        process: Mutex<Option<(u32, Option<u64>)>>,
    }

    impl TestObserver {
        fn new() -> Self {
            Self {
                cancelled: AtomicBool::new(false),
                started: Semaphore::new(0),
                finished: AtomicBool::new(false),
                process: Mutex::new(None),
            }
        }
    }

    #[async_trait]
    impl BuildExecutionObserver for TestObserver {
        async fn cancellation_requested(&self) -> Result<bool> {
            Ok(self.cancelled.load(Ordering::SeqCst))
        }

        async fn run_process_started(&self, pid: u32, start_time: Option<u64>) -> Result<()> {
            *self.process.lock().unwrap() = Some((pid, start_time));
            self.started.add_permits(1);
            Ok(())
        }

        async fn run_process_finished(&self, _pid: u32, _start_time: Option<u64>) -> Result<()> {
            self.finished.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[tokio::test]
    async fn cancellation_kills_and_reaps_the_recorded_run_process() {
        let observer = Arc::new(TestObserver::new());
        let control = BuildExecutionControl::new(observer.clone());
        let mut command = std::process::Command::new("/bin/sh");
        command.args(["-c", "exec /bin/sleep 30"]);

        let execution = tokio::spawn(async move { command_output(command, Some(&control)).await });
        observer
            .started
            .acquire()
            .await
            .expect("test observer remains open")
            .forget();
        let process = observer
            .process
            .lock()
            .unwrap()
            .as_ref()
            .copied()
            .expect("RUN identity was recorded");
        observer.cancelled.store(true, Ordering::SeqCst);

        let error = tokio::time::timeout(std::time::Duration::from_secs(2), execution)
            .await
            .expect("cancelled RUN must stop promptly")
            .expect("RUN task must not panic")
            .unwrap_err();
        assert!(error.to_string().contains("cancelled"), "{error}");
        assert!(observer.finished.load(Ordering::SeqCst));
        assert!(!crate::process::is_process_running_with_identity(
            process.0, process.1
        ));
    }
}
