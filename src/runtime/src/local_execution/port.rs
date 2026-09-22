use std::num::NonZeroU16;
#[cfg(target_os = "linux")]
use std::path::Path;
use std::time::Duration;

#[cfg(target_os = "linux")]
use a3s_box_core::ExecutionBackend;
#[cfg(target_os = "linux")]
use a3s_box_core::ExecutionUdpPortIo;
use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionManagerError, ExecutionManagerResult,
    ExecutionPortConnector, ExecutionPortStream, ExecutionUdpPort,
};
use async_trait::async_trait;
#[cfg(target_os = "linux")]
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
#[cfg(target_os = "linux")]
use tokio::net::UnixStream;

use super::LocalExecutionManager;
#[cfg(target_os = "linux")]
use crate::BoxRecord;

#[cfg(target_os = "linux")]
const PORT_FORWARD_STREAM_ID: u32 = 1;
#[cfg(target_os = "linux")]
const PORT_FORWARD_FRAME_OPEN: u8 = 1;
#[cfg(target_os = "linux")]
const PORT_FORWARD_FRAME_OPEN_ACK: u8 = 2;
#[cfg(target_os = "linux")]
const PORT_FORWARD_FRAME_DATA: u8 = 3;
#[cfg(target_os = "linux")]
const PORT_FORWARD_FRAME_CLOSE: u8 = 4;
#[cfg(target_os = "linux")]
const PORT_FORWARD_FRAME_OPEN_UDP: u8 = 7;
#[cfg(target_os = "linux")]
const PORT_FORWARD_BUFFER_BYTES: usize = 16 * 1024;
#[cfg(target_os = "linux")]
const PORT_FORWARD_MAX_FRAME_BYTES: usize = 64 * 1024;

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
            let (record, backend) = self.require_connectable(execution_id, generation).await?;
            let pid = record
                .pid
                .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
            let pid_start_time = record.pid_start_time;
            if !crate::process::is_process_alive_with_identity(pid, pid_start_time) {
                return Err(ExecutionManagerError::NotFound(execution_id.clone()));
            }

            let stream: ExecutionPortStream = if backend.is_sandbox() {
                Box::pin(
                    connect_in_network_namespace(
                        execution_id.clone(),
                        pid,
                        pid_start_time,
                        port,
                        timeout,
                    )
                    .await?,
                )
            } else {
                let socket_path = record.exec_socket_path.with_file_name("portfwd.sock");
                connect_microvm_port(execution_id, &socket_path, port, timeout).await?
            };

            // The lifecycle may have advanced while the blocking connect was in
            // flight. Re-read the canonical record before publishing the stream.
            let (current, current_backend) =
                self.require_connectable(execution_id, generation).await?;
            if current.pid != Some(pid)
                || current.pid_start_time != pid_start_time
                || current_backend != backend
                || current.exec_socket_path != record.exec_socket_path
                || !crate::process::is_process_alive_with_identity(pid, pid_start_time)
            {
                return Err(ExecutionManagerError::Conflict {
                    execution_id: execution_id.clone(),
                    message: "runtime generation changed while connecting its data plane"
                        .to_string(),
                });
            }
            return Ok(stream);
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = (execution_id, generation, port, timeout);
            Err(ExecutionManagerError::Unavailable(
                "Sandbox port connections require Linux network namespaces".to_string(),
            ))
        }
    }

    async fn connect_udp_port(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        port: NonZeroU16,
        timeout: Duration,
    ) -> ExecutionManagerResult<ExecutionUdpPort> {
        if timeout.is_zero() {
            return Err(ExecutionManagerError::InvalidRequest(
                "UDP port connection timeout must be non-zero".to_string(),
            ));
        }

        #[cfg(target_os = "linux")]
        {
            let (record, backend) = self.require_connectable(execution_id, generation).await?;
            let pid = record
                .pid
                .ok_or_else(|| ExecutionManagerError::NotFound(execution_id.clone()))?;
            let pid_start_time = record.pid_start_time;
            if !crate::process::is_process_alive_with_identity(pid, pid_start_time) {
                return Err(ExecutionManagerError::NotFound(execution_id.clone()));
            }

            let session: ExecutionUdpPort = if backend.is_sandbox() {
                Box::new(
                    connect_udp_in_network_namespace(
                        execution_id.clone(),
                        pid,
                        pid_start_time,
                        port,
                        timeout,
                    )
                    .await?,
                )
            } else {
                let socket_path = record.exec_socket_path.with_file_name("portfwd.sock");
                connect_microvm_udp_port(execution_id, &socket_path, port, timeout).await?
            };

            let (current, current_backend) =
                self.require_connectable(execution_id, generation).await?;
            if current.pid != Some(pid)
                || current.pid_start_time != pid_start_time
                || current_backend != backend
                || current.exec_socket_path != record.exec_socket_path
                || !crate::process::is_process_alive_with_identity(pid, pid_start_time)
            {
                return Err(ExecutionManagerError::Conflict {
                    execution_id: execution_id.clone(),
                    message: "runtime generation changed while connecting its UDP data plane"
                        .to_string(),
                });
            }
            return Ok(session);
        }

        #[cfg(not(target_os = "linux"))]
        {
            let _ = (execution_id, generation, port, timeout);
            Err(ExecutionManagerError::Unavailable(
                "UDP port connections require Linux".to_string(),
            ))
        }
    }
}

#[cfg(target_os = "linux")]
impl LocalExecutionManager {
    async fn require_connectable(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<(BoxRecord, ExecutionBackend)> {
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
        Ok((record, backend))
    }
}

#[cfg(target_os = "linux")]
#[derive(Debug)]
struct PortForwardFrame {
    kind: u8,
    stream_id: u32,
    payload: Vec<u8>,
}

#[cfg(target_os = "linux")]
async fn connect_microvm_port(
    execution_id: &ExecutionId,
    socket_path: &Path,
    port: NonZeroU16,
    timeout: Duration,
) -> ExecutionManagerResult<ExecutionPortStream> {
    let connect = async {
        let mut control = UnixStream::connect(socket_path).await.map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to connect to MicroVM port channel for {execution_id}: {error}"
            ))
        })?;
        write_port_forward_frame(
            &mut control,
            PORT_FORWARD_FRAME_OPEN,
            PORT_FORWARD_STREAM_ID,
            &port.get().to_be_bytes(),
        )
        .await
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to request MicroVM port {} for {execution_id}: {error}",
                port.get()
            ))
        })?;
        let acknowledgement = read_port_forward_frame(&mut control)
            .await
            .map_err(|error| {
                ExecutionManagerError::Unavailable(format!(
                    "failed to open MicroVM port {} for {execution_id}: {error}",
                    port.get()
                ))
            })?;
        let accepted = acknowledgement.as_ref().is_some_and(|frame| {
            frame.kind == PORT_FORWARD_FRAME_OPEN_ACK
                && frame.stream_id == PORT_FORWARD_STREAM_ID
                && frame.payload.as_slice() == [0]
        });
        if !accepted {
            return Err(ExecutionManagerError::Unavailable(format!(
                "MicroVM port {} rejected the connection for {execution_id}",
                port.get()
            )));
        }

        let (application, relay) = tokio::io::duplex(PORT_FORWARD_MAX_FRAME_BYTES);
        let relay_execution_id = execution_id.clone();
        tokio::spawn(async move {
            if let Err(error) = relay_microvm_port(control, relay).await {
                tracing::warn!(
                    execution_id = %relay_execution_id,
                    guest_port = port.get(),
                    error = %error,
                    "MicroVM port relay failed"
                );
            }
        });
        Ok(Box::pin(application) as ExecutionPortStream)
    };

    tokio::time::timeout(timeout, connect).await.map_err(|_| {
        ExecutionManagerError::Unavailable(format!(
            "timed out connecting MicroVM port {} for {execution_id}",
            port.get()
        ))
    })?
}

#[cfg(target_os = "linux")]
async fn relay_microvm_port(
    control: UnixStream,
    relay: tokio::io::DuplexStream,
) -> std::io::Result<()> {
    let (mut control_read, mut control_write) = control.into_split();
    let (mut relay_read, mut relay_write) = tokio::io::split(relay);
    let mut buffer = [0_u8; PORT_FORWARD_BUFFER_BYTES];

    loop {
        tokio::select! {
            read = relay_read.read(&mut buffer) => match read? {
                0 => {
                    write_port_forward_frame(
                        &mut control_write,
                        PORT_FORWARD_FRAME_CLOSE,
                        PORT_FORWARD_STREAM_ID,
                        &[],
                    )
                    .await?;
                    return Ok(());
                }
                count => {
                    write_port_forward_frame(
                        &mut control_write,
                        PORT_FORWARD_FRAME_DATA,
                        PORT_FORWARD_STREAM_ID,
                        &buffer[..count],
                    )
                    .await?;
                }
            },
            frame = read_port_forward_frame(&mut control_read) => {
                let Some(frame) = frame? else {
                    relay_write.shutdown().await?;
                    return Ok(());
                };
                if frame.stream_id != PORT_FORWARD_STREAM_ID {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "MicroVM port channel returned an unexpected stream ID",
                    ));
                }
                match frame.kind {
                    PORT_FORWARD_FRAME_DATA => relay_write.write_all(&frame.payload).await?,
                    PORT_FORWARD_FRAME_CLOSE => {
                        relay_write.shutdown().await?;
                        return Ok(());
                    }
                    _ => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "MicroVM port channel returned an unexpected frame",
                        ));
                    }
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
async fn write_port_forward_frame<W>(
    stream: &mut W,
    kind: u8,
    stream_id: u32,
    payload: &[u8],
) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let payload_len = u32::try_from(payload.len()).map_err(|_| {
        std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "MicroVM port frame payload exceeds u32",
        )
    })?;
    stream.write_all(&[kind]).await?;
    stream.write_all(&stream_id.to_be_bytes()).await?;
    stream.write_all(&payload_len.to_be_bytes()).await?;
    stream.write_all(payload).await?;
    stream.flush().await
}

#[cfg(target_os = "linux")]
async fn read_port_forward_frame<R>(stream: &mut R) -> std::io::Result<Option<PortForwardFrame>>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0_u8; 9];
    match stream.read_exact(&mut header).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let payload_len = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;
    if payload_len > PORT_FORWARD_MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "MicroVM port frame exceeds the bounded payload limit",
        ));
    }
    let mut payload = vec![0_u8; payload_len];
    stream.read_exact(&mut payload).await?;
    Ok(Some(PortForwardFrame {
        kind: header[0],
        stream_id: u32::from_be_bytes([header[1], header[2], header[3], header[4]]),
        payload,
    }))
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

#[cfg(target_os = "linux")]
struct NamespaceUdpPort {
    socket: tokio::net::UdpSocket,
}

#[cfg(target_os = "linux")]
#[async_trait]
impl ExecutionUdpPortIo for NamespaceUdpPort {
    async fn send_datagram(&mut self, payload: &[u8]) -> std::io::Result<()> {
        self.socket.send(payload).await.map(|_| ())
    }

    async fn recv_datagram(&mut self, max_len: usize) -> std::io::Result<Vec<u8>> {
        let mut buf = vec![0_u8; max_len];
        let n = self.socket.recv(&mut buf).await?;
        buf.truncate(n);
        Ok(buf)
    }
}

#[cfg(target_os = "linux")]
async fn connect_udp_in_network_namespace(
    execution_id: ExecutionId,
    pid: u32,
    pid_start_time: Option<u64>,
    port: NonZeroU16,
    timeout: Duration,
) -> ExecutionManagerResult<NamespaceUdpPort> {
    let (sender, receiver) = tokio::sync::oneshot::channel();
    std::thread::Builder::new()
        .name(format!("a3s-udp-{pid}-{}", port.get()))
        .spawn(move || {
            let result = connect_udp_in_network_namespace_blocking(
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
                "failed to start Sandbox UDP connector: {error}"
            ))
        })?;

    let socket = receiver.await.map_err(|_| {
        ExecutionManagerError::Internal("Sandbox UDP connector exited without a result".to_string())
    })??;
    let socket = tokio::net::UdpSocket::from_std(socket).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to register Sandbox UDP socket with Tokio: {error}"
        ))
    })?;
    Ok(NamespaceUdpPort { socket })
}

#[cfg(target_os = "linux")]
fn connect_udp_in_network_namespace_blocking(
    execution_id: &ExecutionId,
    pid: u32,
    pid_start_time: Option<u64>,
    port: NonZeroU16,
    timeout: Duration,
) -> ExecutionManagerResult<std::net::UdpSocket> {
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
    let socket = std::net::UdpSocket::bind(std::net::SocketAddr::from((
        std::net::Ipv4Addr::UNSPECIFIED,
        0,
    )))
    .map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to bind Sandbox UDP client socket: {error}"
        ))
    })?;
    socket.set_read_timeout(Some(timeout)).ok();
    socket.set_write_timeout(Some(timeout)).ok();
    socket.connect(address).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to connect Sandbox UDP to loopback port {}: {error}",
            port.get()
        ))
    })?;
    socket.set_nonblocking(true).map_err(|error| {
        ExecutionManagerError::Unavailable(format!(
            "failed to configure Sandbox UDP socket: {error}"
        ))
    })?;
    Ok(socket)
}

#[cfg(target_os = "linux")]
struct MicroVmUdpPort {
    to_guest: tokio::sync::mpsc::Sender<Vec<u8>>,
    from_guest: tokio::sync::mpsc::Receiver<Vec<u8>>,
}

#[cfg(target_os = "linux")]
#[async_trait]
impl ExecutionUdpPortIo for MicroVmUdpPort {
    async fn send_datagram(&mut self, payload: &[u8]) -> std::io::Result<()> {
        self.to_guest.send(payload.to_vec()).await.map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::BrokenPipe,
                "MicroVM UDP association closed",
            )
        })
    }

    async fn recv_datagram(&mut self, max_len: usize) -> std::io::Result<Vec<u8>> {
        let payload = self.from_guest.recv().await.ok_or_else(|| {
            std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "MicroVM UDP association closed",
            )
        })?;
        if payload.len() > max_len {
            return Err(std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "MicroVM UDP datagram exceeds receiver bound",
            ));
        }
        Ok(payload)
    }
}

#[cfg(target_os = "linux")]
async fn connect_microvm_udp_port(
    execution_id: &ExecutionId,
    socket_path: &Path,
    port: NonZeroU16,
    timeout: Duration,
) -> ExecutionManagerResult<ExecutionUdpPort> {
    let connect = async {
        let mut control = UnixStream::connect(socket_path).await.map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to connect to MicroVM UDP port channel for {execution_id}: {error}"
            ))
        })?;
        write_port_forward_frame(
            &mut control,
            PORT_FORWARD_FRAME_OPEN_UDP,
            PORT_FORWARD_STREAM_ID,
            &port.get().to_be_bytes(),
        )
        .await
        .map_err(|error| {
            ExecutionManagerError::Unavailable(format!(
                "failed to request MicroVM UDP port {} for {execution_id}: {error}",
                port.get()
            ))
        })?;
        let acknowledgement = read_port_forward_frame(&mut control)
            .await
            .map_err(|error| {
                ExecutionManagerError::Unavailable(format!(
                    "failed to open MicroVM UDP port {} for {execution_id}: {error}",
                    port.get()
                ))
            })?;
        let accepted = acknowledgement.as_ref().is_some_and(|frame| {
            frame.kind == PORT_FORWARD_FRAME_OPEN_ACK
                && frame.stream_id == PORT_FORWARD_STREAM_ID
                && frame.payload.as_slice() == [0]
        });
        if !accepted {
            return Err(ExecutionManagerError::Unavailable(format!(
                "MicroVM UDP port {} rejected the association for {execution_id}",
                port.get()
            )));
        }

        let (to_guest_tx, mut to_guest_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
        let (from_guest_tx, from_guest_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
        let relay_execution_id = execution_id.clone();
        tokio::spawn(async move {
            if let Err(error) = relay_microvm_udp(control, &mut to_guest_rx, from_guest_tx).await {
                tracing::warn!(
                    execution_id = %relay_execution_id,
                    guest_port = port.get(),
                    error = %error,
                    "MicroVM UDP relay failed"
                );
            }
        });
        Ok(Box::new(MicroVmUdpPort {
            to_guest: to_guest_tx,
            from_guest: from_guest_rx,
        }) as ExecutionUdpPort)
    };

    tokio::time::timeout(timeout, connect).await.map_err(|_| {
        ExecutionManagerError::Unavailable(format!(
            "timed out connecting MicroVM UDP port {} for {execution_id}",
            port.get()
        ))
    })?
}

#[cfg(target_os = "linux")]
async fn relay_microvm_udp(
    control: UnixStream,
    to_guest: &mut tokio::sync::mpsc::Receiver<Vec<u8>>,
    from_guest: tokio::sync::mpsc::Sender<Vec<u8>>,
) -> std::io::Result<()> {
    let (mut control_read, mut control_write) = control.into_split();
    loop {
        tokio::select! {
            host = to_guest.recv() => {
                let Some(payload) = host else {
                    write_port_forward_frame(
                        &mut control_write,
                        PORT_FORWARD_FRAME_CLOSE,
                        PORT_FORWARD_STREAM_ID,
                        &[],
                    )
                    .await?;
                    return Ok(());
                };
                write_port_forward_frame(
                    &mut control_write,
                    PORT_FORWARD_FRAME_DATA,
                    PORT_FORWARD_STREAM_ID,
                    &payload,
                )
                .await?;
            }
            frame = read_port_forward_frame(&mut control_read) => {
                let Some(frame) = frame? else {
                    return Ok(());
                };
                if frame.stream_id != PORT_FORWARD_STREAM_ID {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidData,
                        "MicroVM UDP channel returned an unexpected stream ID",
                    ));
                }
                match frame.kind {
                    PORT_FORWARD_FRAME_DATA => {
                        if from_guest.send(frame.payload).await.is_err() {
                            return Ok(());
                        }
                    }
                    PORT_FORWARD_FRAME_CLOSE => return Ok(()),
                    _ => {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "MicroVM UDP channel returned an unexpected frame",
                        ));
                    }
                }
            }
        }
    }
}

#[cfg(all(test, target_os = "linux"))]
#[path = "port_tests.rs"]
mod tests;
