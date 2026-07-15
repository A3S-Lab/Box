use std::num::NonZeroU16;
use std::time::Duration;

use a3s_box_core::{
    ExecutionBackend, ExecutionGeneration, ExecutionId, ExecutionManagerError,
    ExecutionManagerResult, ExecutionPortConnector, ExecutionPortStream,
};
use async_trait::async_trait;

use super::LocalExecutionManager;
use crate::BoxRecord;

#[async_trait]
impl ExecutionPortConnector for LocalExecutionManager {
    async fn connect_port(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        port: NonZeroU16,
        timeout: Duration,
    ) -> ExecutionManagerResult<ExecutionPortStream> {
        if timeout.is_zero() {
            return Err(ExecutionManagerError::InvalidRequest(
                "port connection timeout must be non-zero".to_string(),
            ));
        }

        #[cfg(target_os = "linux")]
        {
            let record = self.require_connectable(execution_id, generation).await?;
            let pid = record
                .pid
                .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
            let pid_start_time = record.pid_start_time;
            if !crate::process::is_process_alive_with_identity(pid, pid_start_time) {
                return Err(ExecutionManagerError::NotFound(execution_id.clone()));
            }

            let stream = connect_in_network_namespace(
                execution_id.clone(),
                pid,
                pid_start_time,
                port,
                timeout,
            )
            .await?;

            // The lifecycle may have advanced while the blocking connect was in
            // flight. Re-read the canonical record before publishing the stream.
            let current = self.require_connectable(execution_id, generation).await?;
            if current.pid != Some(pid)
                || current.pid_start_time != pid_start_time
                || !crate::process::is_process_alive_with_identity(pid, pid_start_time)
            {
                return Err(ExecutionManagerError::Conflict {
                    execution_id: execution_id.clone(),
                    message: "runtime generation changed while connecting its data plane"
                        .to_string(),
                });
            }
            return Ok(Box::pin(stream));
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = (execution_id, generation, port, timeout);
            Err(ExecutionManagerError::Unavailable(
                "Sandbox port connections require Linux network namespaces".to_string(),
            ))
        }
    }
}

impl LocalExecutionManager {
    async fn require_connectable(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<BoxRecord> {
        let record = self
            .require_running_record(execution_id, generation)
            .await?;
        let backend = record
            .managed_execution
            .as_ref()
            .map(|metadata| metadata.plan.backend)
            .ok_or_else(|| {
                ExecutionManagerError::Internal(format!(
                    "execution {execution_id} has no managed execution plan"
                ))
            })?;
        if backend != ExecutionBackend::Crun {
            return Err(ExecutionManagerError::Unavailable(format!(
                "execution {execution_id} does not expose a Sandbox network namespace"
            )));
        }
        Ok(record)
    }
}

#[cfg(target_os = "linux")]
async fn connect_in_network_namespace(
    execution_id: ExecutionId,
    pid: u32,
    pid_start_time: Option<u64>,
    port: NonZeroU16,
    timeout: Duration,
) -> ExecutionManagerResult<tokio::net::TcpStream> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name(format!("a3s-port-{pid}-{}", port.get()))
        .spawn(move || {
            let result = connect_in_network_namespace_blocking(
                &execution_id,
                pid,
                pid_start_time,
                port,
                timeout,
            );
            let _ = sender.send(result);
        })
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to start Sandbox port connector: {error}"
            ))
        })?;

    let stream = receiver.await.map_err(|_| {
        ExecutionManagerError::Internal(
            "Sandbox port connector exited without a result".to_string(),
        )
    })??;
    tokio::net::TcpStream::from_std(stream).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to register Sandbox port stream with Tokio: {error}"
        ))
    })
}

#[cfg(target_os = "linux")]
fn connect_in_network_namespace_blocking(
    execution_id: &ExecutionId,
    pid: u32,
    pid_start_time: Option<u64>,
    port: NonZeroU16,
    timeout: Duration,
) -> ExecutionManagerResult<std::net::TcpStream> {
    use std::fs::File;
    use std::os::fd::AsRawFd;

    if !crate::process::is_process_alive_with_identity(pid, pid_start_time) {
        return Err(ExecutionManagerError::NotFound(execution_id.clone()));
    }
    let namespace_path = format!("/proc/{pid}/ns/net");
    let namespace = File::open(&namespace_path).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to open Sandbox network namespace {namespace_path}: {error}"
        ))
    })?;
    let result = unsafe { libc::setns(namespace.as_raw_fd(), libc::CLONE_NEWNET) };
    if result != 0 {
        return Err(ExecutionManagerError::Unavailable(format!(
            "failed to enter Sandbox network namespace for PID {pid}: {}",
            std::io::Error::last_os_error()
        )));
    }
    if !crate::process::is_process_alive_with_identity(pid, pid_start_time) {
        return Err(ExecutionManagerError::Unavailable(
            "Sandbox runtime exited while entering its network namespace".to_string(),
        ));
    }

    let address = std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port.get()));
    let stream = std::net::TcpStream::connect_timeout(&address, timeout).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to connect to Sandbox loopback port {}: {error}",
            port.get()
        ))
    })?;
    stream.set_nonblocking(true).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to configure Sandbox port stream: {error}"
        ))
    })?;
    Ok(stream)
}
