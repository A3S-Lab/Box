//! Command execution and streaming clients.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::ExecutionProcessSignal;
use tokio::io::AsyncWriteExt;
use tokio::sync::Mutex;

#[cfg(unix)]
type ExecStream = tokio::net::UnixStream;
#[cfg(windows)]
type ExecStream = tokio::net::windows::named_pipe::NamedPipeClient;

const EXEC_CONTROL_CANCEL: &[u8] = b"cancel";
const EXEC_CONTROL_STDIN_CLOSE: &[u8] = b"stdin-close";
const EXEC_CONTROL_SIGNAL: &[u8] = b"signal:";
/// Host→guest control: flush all buffered output and reply with a flush-ack.
const EXEC_CONTROL_FLUSH: &[u8] = b"flush";
/// Host→guest control: stream a tar archive of the guest-visible rootfs.
const EXEC_CONTROL_ARCHIVE_ROOTFS: &[u8] = b"archive-rootfs-v1";
const EXEC_CONTROL_ARCHIVE_ROOTFS_PAUSE: &[u8] = b"archive-rootfs-v1:pause";
/// Guest→host marker after every archive data frame has been sent.
const EXEC_ARCHIVE_ROOTFS_DONE: &[u8] = b"archive-rootfs-v1-done";
/// Host→trusted-maintenance-PID1 request to unmount its rootfs disk and exit.
const EXEC_CONTROL_SHUTDOWN_MAINTENANCE: &[u8] = b"shutdown-rootfs-maintenance-v1";
const EXEC_SHUTDOWN_MAINTENANCE_ACK: &[u8] = b"shutdown-rootfs-maintenance-v1-ack";
/// Guest→host marker (carried in a Control frame) acknowledging a flush. Kept
/// distinct from an `ExecExit` JSON payload so `next_event` can tell them apart.
/// Must match the guest's `EXEC_FLUSH_ACK` in `guest/init/src/exec_server.rs`.
const EXEC_FLUSH_ACK: &[u8] = b"flush-ack";
/// Guest→host acknowledgement that a `signal-main:<N>` graceful-stop control was
/// received and the signal delivered. Must match the guest's
/// `EXEC_SIGNAL_MAIN_ACK` in `guest/init/src/exec_server.rs`.
const EXEC_SIGNAL_MAIN_ACK: &[u8] = b"signal-main-ack";
/// Guest→host acknowledgement that a `spawn-main` deferred-main control was
/// received and the container main spawned. Matches the guest's
/// `EXEC_SPAWN_MAIN_ACK` in `guest/init/src/exec_server.rs`.
const EXEC_SPAWN_MAIN_ACK: &[u8] = b"spawn-main-ack";
/// Guest→host negative acknowledgement for `spawn-main`, followed by a UTF-8-ish
/// diagnostic string from guest-init.
const EXEC_SPAWN_MAIN_NACK: &[u8] = b"spawn-main-nack:";

/// Host-side slack added to a one-shot exec's in-guest `timeout_ns` before the
/// host gives up reading the reply. The in-guest timeout cannot fire if the
/// guest is wedged, so the host needs its own ceiling.
const EXEC_HOST_SLACK_SECS: u64 = 10;
/// Host-side deadline for a `signal-main` ACK. Signal delivery + the ACK are
/// fast; a wedged guest that never replies must not block the caller's
/// force-kill fallback.
const SIGNAL_MAIN_ACK_TIMEOUT_SECS: u64 = 10;

#[derive(Debug, Clone, PartialEq, Eq)]
enum ControlAckAttempt {
    /// Guest sent the expected control ACK.
    Acked,
    /// Connect failed; the control frame never left the host.
    NotReached,
    /// Write may have reached the guest, or the ACK was lost/timed out.
    Ambiguous,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum SpawnMainAttempt {
    /// Guest sent `spawn-main-ack`.
    Acked,
    /// Connect failed; the control frame never left the host.
    NotReached,
    /// Write may have reached the guest, or the ACK was lost/timed out.
    Ambiguous,
    /// Guest reported the main is already present (idempotent success).
    AlreadySpawned,
    /// Guest rejected the spawn with a non-idempotent reason.
    Rejected(String),
}

type ExecFrameReader = a3s_transport::FrameReader<tokio::io::ReadHalf<ExecStream>>;
type ExecFrameWriter = a3s_transport::FrameWriter<tokio::io::WriteHalf<ExecStream>>;

async fn connect_exec_stream(path: &Path) -> std::io::Result<ExecStream> {
    #[cfg(unix)]
    {
        tokio::net::UnixStream::connect(path).await
    }

    #[cfg(windows)]
    {
        const ERROR_FILE_NOT_FOUND: i32 = 2;
        const ERROR_PIPE_BUSY: i32 = 231;
        const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(5);
        const RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(10);

        let deadline = tokio::time::Instant::now() + CONNECT_TIMEOUT;
        loop {
            match tokio::net::windows::named_pipe::ClientOptions::new().open(path) {
                Ok(stream) => return Ok(stream),
                Err(error)
                    if matches!(
                        error.raw_os_error(),
                        Some(ERROR_FILE_NOT_FOUND | ERROR_PIPE_BUSY)
                    ) && tokio::time::Instant::now() < deadline =>
                {
                    tokio::time::sleep(RETRY_DELAY).await;
                }
                Err(error) => return Err(error),
            }
        }
    }
}

/// True when the failure may have occurred after the guest already claimed or
/// completed a keyed one-shot exec (lost response / broken connection).
pub(crate) fn is_ambiguous_guest_exec_transport(error: &BoxError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("unavailable")
        || message.contains("closed without response")
        || message.contains("response timed out")
        || message.contains("response read failed")
        || message.contains("connection failed")
}

/// True when a filesystem call may have completed in the guest but the host
/// lost the reply (or the write may have partially reached the guest).
fn is_ambiguous_filesystem_transport(error: &BoxError) -> bool {
    let message = error.to_string().to_ascii_lowercase();
    message.contains("unavailable")
        || message.contains("closed without")
        || message.contains("response timed out")
        || message.contains("response read failed")
        || message.contains("request write failed")
        || message.contains("connection failed")
}

fn is_idempotent_filesystem_op(op: &a3s_box_core::FilesystemOp) -> bool {
    matches!(
        op,
        a3s_box_core::FilesystemOp::Stat | a3s_box_core::FilesystemOp::ListDir
    )
}

/// Keyed one-shot exec can replay the guest journal with the same `request_id`.
pub(crate) fn should_retry_keyed_guest_exec(
    request: &a3s_box_core::exec::ExecRequest,
    error: &BoxError,
) -> bool {
    request
        .request_id
        .as_ref()
        .is_some_and(|request_id| !request_id.is_empty())
        && is_ambiguous_guest_exec_transport(error)
}

/// Client for executing commands through the platform-local guest channel.
///
/// Uses the Frame wire protocol: sends a Data frame with JSON ExecRequest,
/// receives a Data frame with JSON ExecOutput.
#[derive(Debug)]
pub struct ExecClient {
    socket_path: PathBuf,
}

impl ExecClient {
    pub(crate) fn for_socket(socket_path: &Path) -> Self {
        Self {
            socket_path: socket_path.to_path_buf(),
        }
    }

    /// Connect to the exec server via a Unix socket or Windows named pipe.
    ///
    /// Verifies the socket is connectable.
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        let client = Self::for_socket(socket_path);
        let _stream = client.open_stream().await?;
        Ok(client)
    }

    /// Get the socket path this client is connected to.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    pub(crate) async fn open_stream(&self) -> Result<ExecStream> {
        connect_exec_stream(&self.socket_path).await.map_err(|e| {
            BoxError::ExecError(format!(
                "Exec connection failed to {}: {}",
                self.socket_path.display(),
                e,
            ))
        })
    }

    /// Execute a command in the guest.
    ///
    /// Sends a Data frame with JSON ExecRequest, reads a Data frame with JSON ExecOutput.
    /// When `request_id` is non-empty, retries once on ambiguous transport loss
    /// with a fresh stream so the guest replay cache can reconcile a lost reply.
    pub async fn exec_command(
        &self,
        request: &a3s_box_core::exec::ExecRequest,
    ) -> Result<a3s_box_core::exec::ExecOutput> {
        let stream = self.open_stream().await?;
        match self.exec_command_on_stream(stream, request).await {
            Ok(output) => Ok(output),
            Err(error) if should_retry_keyed_guest_exec(request, &error) => {
                let stream = self.open_stream().await?;
                self.exec_command_on_stream(stream, request).await
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) async fn exec_command_on_stream(
        &self,
        mut stream: ExecStream,
        request: &a3s_box_core::exec::ExecRequest,
    ) -> Result<a3s_box_core::exec::ExecOutput> {
        let payload = serde_json::to_vec(request)
            .map_err(|e| BoxError::ExecError(format!("Failed to serialize exec request: {}", e)))?;

        // Send request as Data frame
        let request_frame = a3s_transport::Frame::data(payload);
        let encoded = request_frame.encode().map_err(|e| {
            BoxError::ExecError(format!("Failed to encode exec request frame: {}", e))
        })?;
        stream
            .write_all(&encoded)
            .await
            .map_err(|e| BoxError::ExecError(format!("Exec request write failed: {}", e)))?;

        // Read response frame, bounded by a HOST-side deadline of the request's
        // timeout plus slack. The request's timeout_ns is only enforced INSIDE
        // the guest; a wedged guest (kernel hang, OOM thrash, frozen VM) can
        // still complete the host connect handshake but never reply, which would
        // block this read forever and stall every caller (health probes, the
        // monitor poll loop, CLI exec).
        let (r, _w) = tokio::io::split(stream);
        let mut reader = a3s_transport::FrameReader::new(r);
        let host_deadline = std::time::Duration::from_nanos(request.timeout_ns)
            .saturating_add(std::time::Duration::from_secs(EXEC_HOST_SLACK_SECS));
        let frame = tokio::time::timeout(host_deadline, reader.read_frame())
            .await
            .map_err(|_| {
                BoxError::ExecError(format!(
                    "Exec response timed out after {host_deadline:?} (guest may be wedged)"
                ))
            })?
            .map_err(|e| BoxError::ExecError(format!("Exec response read failed: {}", e)))?
            .ok_or_else(|| {
                BoxError::ExecError("Exec server closed without response".to_string())
            })?;

        match frame.frame_type {
            a3s_transport::FrameType::Data => {
                let output: a3s_box_core::exec::ExecOutput = serde_json::from_slice(&frame.payload)
                    .map_err(|e| {
                        BoxError::ExecError(format!("Failed to parse exec response: {}", e))
                    })?;
                Ok(output)
            }
            a3s_transport::FrameType::Error => {
                let msg = String::from_utf8_lossy(&frame.payload);
                Err(BoxError::ExecError(format!("Exec server error: {}", msg)))
            }
            _ => Err(BoxError::ExecError(format!(
                "Unexpected frame type: {:?}",
                frame.frame_type
            ))),
        }
    }

    /// Execute a command in streaming mode.
    ///
    /// Sends a Data frame with JSON ExecRequest (streaming=true), then reads
    /// multiple frames: ExecChunk frames for stdout/stderr data, and a final
    /// ExecExit frame with the exit code.
    ///
    /// Returns a `StreamingExec` handle for reading events.
    pub async fn exec_stream(
        &self,
        request: &a3s_box_core::exec::ExecRequest,
    ) -> Result<StreamingExec> {
        let stream = self.open_stream().await?;
        self.exec_stream_on_stream(stream, request).await
    }

    pub(crate) async fn exec_stream_on_stream(
        &self,
        stream: ExecStream,
        request: &a3s_box_core::exec::ExecRequest,
    ) -> Result<StreamingExec> {
        let mut req = request.clone();
        req.streaming = true;

        let payload = serde_json::to_vec(&req)
            .map_err(|e| BoxError::ExecError(format!("Failed to serialize exec request: {}", e)))?;

        let (r, w) = tokio::io::split(stream);
        let mut writer = a3s_transport::FrameWriter::new(w);
        writer
            .write_data(&payload)
            .await
            .map_err(|e| BoxError::ExecError(format!("Exec request write failed: {}", e)))?;

        let reader = a3s_transport::FrameReader::new(r);
        let started = std::time::Instant::now();

        Ok(StreamingExec {
            reader,
            writer: Arc::new(Mutex::new(writer)),
            started,
            stdout_bytes: 0,
            stderr_bytes: 0,
            done: false,
        })
    }

    /// Stream a guest-created rootfs tar archive into `output`.
    ///
    /// The guest performs `stat` and tar-header creation, preserving Linux
    /// uid/gid/mode even when the host virtio-fs backing directory exposes
    /// different macOS metadata. Mounted subtrees are excluded by guest-init.
    pub async fn archive_rootfs<W>(&self, output: &mut W, pause: bool) -> Result<u64>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut stream = connect_exec_stream(&self.socket_path)
            .await
            .map_err(|error| {
                BoxError::ExecError(format!(
                    "Rootfs archive connection failed to {}: {error}",
                    self.socket_path.display()
                ))
            })?;

        let control = if pause {
            EXEC_CONTROL_ARCHIVE_ROOTFS_PAUSE
        } else {
            EXEC_CONTROL_ARCHIVE_ROOTFS
        };
        let request = a3s_transport::Frame::control(control.to_vec());
        stream
            .write_all(&request.encode().map_err(|error| {
                BoxError::ExecError(format!("Rootfs archive request encode failed: {error}"))
            })?)
            .await
            .map_err(|error| {
                BoxError::ExecError(format!("Rootfs archive request write failed: {error}"))
            })?;

        let (reader, _writer) = tokio::io::split(stream);
        let mut reader = a3s_transport::FrameReader::new(reader);
        let mut written = 0u64;
        loop {
            let frame = reader
                .read_frame()
                .await
                .map_err(|error| {
                    BoxError::ExecError(format!("Rootfs archive read failed: {error}"))
                })?
                .ok_or_else(|| {
                    BoxError::ExecError(
                        "Rootfs archive stream closed before completion".to_string(),
                    )
                })?;

            match frame.frame_type {
                a3s_transport::FrameType::Data => {
                    output.write_all(&frame.payload).await.map_err(|error| {
                        BoxError::ExecError(format!("Rootfs archive output write failed: {error}"))
                    })?;
                    written = written.saturating_add(frame.payload.len() as u64);
                }
                a3s_transport::FrameType::Control if frame.payload == EXEC_ARCHIVE_ROOTFS_DONE => {
                    output.flush().await.map_err(|error| {
                        BoxError::ExecError(format!("Rootfs archive output flush failed: {error}"))
                    })?;
                    return Ok(written);
                }
                a3s_transport::FrameType::Error => {
                    return Err(BoxError::ExecError(format!(
                        "Guest rootfs archive failed: {}",
                        String::from_utf8_lossy(&frame.payload)
                    )));
                }
                other => {
                    return Err(BoxError::ExecError(format!(
                        "Unexpected rootfs archive frame: {other:?}"
                    )));
                }
            }
        }
    }

    /// Transfer a file to/from the guest.
    ///
    /// Sends a discriminated JSON file request and reads a JSON FileResponse.
    pub async fn file_transfer(
        &self,
        request: &a3s_box_core::exec::FileRequest,
    ) -> Result<a3s_box_core::exec::FileResponse> {
        let stream = self.open_stream().await?;
        self.file_transfer_on_stream(stream, request).await
    }

    pub(crate) async fn file_transfer_on_stream(
        &self,
        mut stream: ExecStream,
        request: &a3s_box_core::exec::FileRequest,
    ) -> Result<a3s_box_core::exec::FileResponse> {
        let payload = serde_json::to_vec(&a3s_box_core::GuestSessionRequest::File(request.clone()))
            .map_err(|e| BoxError::ExecError(format!("Failed to serialize file request: {}", e)))?;

        let request_frame = a3s_transport::Frame::data(payload);
        let encoded = request_frame.encode().map_err(|e| {
            BoxError::ExecError(format!("Failed to encode file request frame: {}", e))
        })?;
        stream
            .write_all(&encoded)
            .await
            .map_err(|e| BoxError::ExecError(format!("File request write failed: {}", e)))?;

        let (r, _w) = tokio::io::split(stream);
        let mut reader = a3s_transport::FrameReader::new(r);
        let frame = reader
            .read_frame()
            .await
            .map_err(|e| BoxError::ExecError(format!("File response read failed: {}", e)))?
            .ok_or_else(|| {
                BoxError::ExecError("Exec server closed without response".to_string())
            })?;

        match frame.frame_type {
            a3s_transport::FrameType::Data => {
                let response: a3s_box_core::exec::FileResponse =
                    serde_json::from_slice(&frame.payload).map_err(|e| {
                        BoxError::ExecError(format!("Failed to parse file response: {}", e))
                    })?;
                Ok(response)
            }
            a3s_transport::FrameType::Error => {
                let msg = String::from_utf8_lossy(&frame.payload);
                Err(BoxError::ExecError(format!("File transfer error: {}", msg)))
            }
            _ => Err(BoxError::ExecError(format!(
                "Unexpected frame type: {:?}",
                frame.frame_type
            ))),
        }
    }

    /// Perform a filesystem metadata or mutation operation inside the guest.
    ///
    /// Read-only ops (`Stat`, `ListDir`) retry once on a fresh stream when the
    /// transport is ambiguous — they are naturally idempotent. Mutating ops
    /// (`MakeDir`, `Move`, `Remove`) stay single-shot until guest journals exist.
    pub async fn filesystem(
        &self,
        request: &a3s_box_core::FilesystemRequest,
    ) -> Result<a3s_box_core::FilesystemResponse> {
        match self.filesystem_once(request).await {
            Ok(response) => Ok(response),
            Err(error)
                if is_idempotent_filesystem_op(&request.op)
                    && is_ambiguous_filesystem_transport(&error) =>
            {
                self.filesystem_once(request).await
            }
            Err(error) => Err(error),
        }
    }

    async fn filesystem_once(
        &self,
        request: &a3s_box_core::FilesystemRequest,
    ) -> Result<a3s_box_core::FilesystemResponse> {
        let stream = self.open_stream().await?;
        self.filesystem_on_stream(stream, request).await
    }

    pub(crate) async fn filesystem_on_stream(
        &self,
        mut stream: ExecStream,
        request: &a3s_box_core::FilesystemRequest,
    ) -> Result<a3s_box_core::FilesystemResponse> {
        let payload = serde_json::to_vec(&a3s_box_core::GuestSessionRequest::Filesystem(
            request.clone(),
        ))
        .map_err(|error| {
            BoxError::ExecError(format!("Failed to serialize filesystem request: {error}"))
        })?;
        let encoded = a3s_transport::Frame::data(payload)
            .encode()
            .map_err(|error| {
                BoxError::ExecError(format!("Failed to encode filesystem request: {error}"))
            })?;
        stream.write_all(&encoded).await.map_err(|error| {
            BoxError::ExecError(format!("Filesystem request write failed: {error}"))
        })?;

        let (read, _write) = tokio::io::split(stream);
        let mut reader = a3s_transport::FrameReader::new(read);
        let frame = reader
            .read_frame()
            .await
            .map_err(|error| {
                BoxError::ExecError(format!("Filesystem response read failed: {error}"))
            })?
            .ok_or_else(|| {
                BoxError::ExecError("Exec server closed without filesystem response".to_string())
            })?;
        match frame.frame_type {
            a3s_transport::FrameType::Data => {
                serde_json::from_slice(&frame.payload).map_err(|error| {
                    BoxError::ExecError(format!("Failed to parse filesystem response: {error}"))
                })
            }
            a3s_transport::FrameType::Error => Err(BoxError::ExecError(format!(
                "Filesystem operation failed: {}",
                String::from_utf8_lossy(&frame.payload)
            ))),
            other => Err(BoxError::ExecError(format!(
                "Unexpected filesystem response frame: {other:?}"
            ))),
        }
    }

    /// Send a Heartbeat frame and wait for a Heartbeat response.
    ///
    /// Returns `true` if the exec server responds, `false` otherwise.
    pub async fn heartbeat(&self) -> Result<bool> {
        let mut stream = match connect_exec_stream(&self.socket_path).await {
            Ok(s) => s,
            Err(_) => return Ok(false),
        };

        let frame = a3s_transport::Frame::heartbeat();
        let encoded = match frame.encode() {
            Ok(e) => e,
            Err(_) => return Ok(false),
        };

        if stream.write_all(&encoded).await.is_err() {
            return Ok(false);
        }

        let (r, _w) = tokio::io::split(stream);
        let mut reader = a3s_transport::FrameReader::new(r);
        match reader.read_frame().await {
            Ok(Some(f)) if f.frame_type == a3s_transport::FrameType::Heartbeat => Ok(true),
            _ => Ok(false),
        }
    }

    /// Ask the guest to deliver `signal` (a signal number, e.g. 15 for SIGTERM)
    /// to the main container process for graceful shutdown. The guest runs the
    /// container's own stop handler; when it exits, guest init exits and the VM
    /// stops cleanly. Returns `Ok(true)` if the guest acknowledged, `Ok(false)`
    /// if it did not respond (caller should fall back to a hard stop).
    ///
    /// Connect failures mean the guest was never reached (`Ok(false)`, no retry).
    /// After a write may have reached the guest, a lost ACK is ambiguous: the
    /// signal is naturally idempotent, so one fresh-stream retry is safe and
    /// avoids false-negative force-kills (parity with OCI kill/delete).
    pub async fn signal_main(&self, signal: i32) -> Result<bool> {
        let payload = format!("signal-main:{}", signal).into_bytes();
        self.control_ack_with_ambiguous_retry(&payload, EXEC_SIGNAL_MAIN_ACK, "signal-main")
            .await
    }

    /// Ask the restricted rootfs maintenance guest to unmount its read-only
    /// auxiliary disk and let PID 1 return. Returns false on any transport or
    /// protocol failure so teardown can use its bounded shim fallback.
    ///
    /// Guest shutdown is naturally idempotent (`IDLE→SHUTTING_DOWN` or already
    /// `SHUTTING_DOWN` both ACK). Connect failures stay `Ok(false)` with no
    /// retry; ambiguous write/ACK loss retries once on a fresh stream.
    pub async fn shutdown_rootfs_maintenance(&self) -> Result<bool> {
        self.control_ack_with_ambiguous_retry(
            EXEC_CONTROL_SHUTDOWN_MAINTENANCE,
            EXEC_SHUTDOWN_MAINTENANCE_ACK,
            "maintenance shutdown",
        )
        .await
    }

    async fn control_ack_with_ambiguous_retry(
        &self,
        control_payload: &[u8],
        ack_payload: &[u8],
        label: &str,
    ) -> Result<bool> {
        match self
            .control_ack_once(control_payload, ack_payload, label)
            .await?
        {
            ControlAckAttempt::Acked => Ok(true),
            ControlAckAttempt::NotReached => Ok(false),
            ControlAckAttempt::Ambiguous => {
                match self
                    .control_ack_once(control_payload, ack_payload, label)
                    .await?
                {
                    ControlAckAttempt::Acked => Ok(true),
                    ControlAckAttempt::NotReached | ControlAckAttempt::Ambiguous => Ok(false),
                }
            }
        }
    }

    async fn control_ack_once(
        &self,
        control_payload: &[u8],
        ack_payload: &[u8],
        label: &str,
    ) -> Result<ControlAckAttempt> {
        let mut stream = match connect_exec_stream(&self.socket_path).await {
            Ok(s) => s,
            Err(_) => return Ok(ControlAckAttempt::NotReached),
        };

        let frame = a3s_transport::Frame::control(control_payload.to_vec());
        let encoded = frame
            .encode()
            .map_err(|e| BoxError::ExecError(format!("{label} frame encode failed: {e}")))?;

        if stream.write_all(&encoded).await.is_err() {
            // Write may have partially reached the guest; treat as ambiguous.
            return Ok(ControlAckAttempt::Ambiguous);
        }

        // Host-side deadline: a wedged guest can complete the connect handshake
        // (listen backlog) but never write the ACK, which would hang this read
        // forever — and stop/restart deliver these controls BEFORE their
        // force-kill / shim fallback, so the fallback would never run.
        let (r, _w) = tokio::io::split(stream);
        let mut reader = a3s_transport::FrameReader::new(r);
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(SIGNAL_MAIN_ACK_TIMEOUT_SECS),
            reader.read_frame(),
        )
        .await;
        match read {
            Ok(Ok(Some(f)))
                if f.frame_type == a3s_transport::FrameType::Control
                    && f.payload == ack_payload =>
            {
                Ok(ControlAckAttempt::Acked)
            }
            _ => Ok(ControlAckAttempt::Ambiguous),
        }
    }

    /// Ask a guest that booted IDLE (`BOX_DEFERRED_MAIN=1`) to spawn its container
    /// command — already known to the guest via BOX_EXEC_* — as the MAIN process.
    /// The spawned main inherits the console (so its output reaches the json-file
    /// logs) and drives the VM lifecycle. Returns `Ok(true)` if acknowledged.
    ///
    /// Connect failures stay `Ok(false)` with no retry. After a write may have
    /// reached the guest, a lost ACK is ambiguous: the guest publishes at most
    /// one main and ACKs repeat spawn-main once the pid is published, so one
    /// fresh-stream retry is safe. An `already spawned` NACK is treated as
    /// success (observe-after-ambiguity).
    pub async fn spawn_main(&self, spec_json: Option<&[u8]>) -> Result<bool> {
        match self.spawn_main_once(spec_json).await? {
            SpawnMainAttempt::Acked => Ok(true),
            SpawnMainAttempt::NotReached => Ok(false),
            SpawnMainAttempt::AlreadySpawned => Ok(true),
            SpawnMainAttempt::Rejected(reason) => Err(BoxError::ExecError(format!(
                "spawn-main rejected by guest: {reason}"
            ))),
            SpawnMainAttempt::Ambiguous => match self.spawn_main_once(spec_json).await? {
                SpawnMainAttempt::Acked | SpawnMainAttempt::AlreadySpawned => Ok(true),
                SpawnMainAttempt::NotReached | SpawnMainAttempt::Ambiguous => Ok(false),
                SpawnMainAttempt::Rejected(reason) => Err(BoxError::ExecError(format!(
                    "spawn-main rejected by guest: {reason}"
                ))),
            },
        }
    }

    async fn spawn_main_once(&self, spec_json: Option<&[u8]>) -> Result<SpawnMainAttempt> {
        let mut stream = match connect_exec_stream(&self.socket_path).await {
            Ok(s) => s,
            Err(_) => return Ok(SpawnMainAttempt::NotReached),
        };

        let mut payload = b"spawn-main:".to_vec();
        if let Some(json) = spec_json {
            payload.extend_from_slice(json);
        }
        let frame = a3s_transport::Frame::control(payload);
        let encoded = frame
            .encode()
            .map_err(|e| BoxError::ExecError(format!("spawn-main frame encode failed: {}", e)))?;

        if stream.write_all(&encoded).await.is_err() {
            return Ok(SpawnMainAttempt::Ambiguous);
        }

        let (r, _w) = tokio::io::split(stream);
        let mut reader = a3s_transport::FrameReader::new(r);
        let read = tokio::time::timeout(
            std::time::Duration::from_secs(SIGNAL_MAIN_ACK_TIMEOUT_SECS),
            reader.read_frame(),
        )
        .await;
        match read {
            Ok(Ok(Some(f)))
                if f.frame_type == a3s_transport::FrameType::Control
                    && f.payload == EXEC_SPAWN_MAIN_ACK =>
            {
                Ok(SpawnMainAttempt::Acked)
            }
            Ok(Ok(Some(f)))
                if f.frame_type == a3s_transport::FrameType::Control
                    && f.payload.starts_with(EXEC_SPAWN_MAIN_NACK) =>
            {
                let reason = String::from_utf8_lossy(&f.payload[EXEC_SPAWN_MAIN_NACK.len()..]);
                if reason.contains("already spawned") {
                    Ok(SpawnMainAttempt::AlreadySpawned)
                } else {
                    Ok(SpawnMainAttempt::Rejected(reason.into_owned()))
                }
            }
            _ => Ok(SpawnMainAttempt::Ambiguous),
        }
    }
}

/// Handle for reading streaming exec events.
///
/// Reads frames from the exec server: Data frames contain `ExecChunk` (stdout/stderr),
/// Control frames contain `ExecExit` (final exit code).
pub struct StreamingExec {
    reader: ExecFrameReader,
    writer: Arc<Mutex<ExecFrameWriter>>,
    started: std::time::Instant,
    stdout_bytes: u64,
    stderr_bytes: u64,
    done: bool,
}

/// Cloneable input side for a running streaming exec workload.
#[derive(Clone, Debug)]
pub struct StreamingExecInput {
    writer: Arc<Mutex<ExecFrameWriter>>,
}

impl StreamingExecInput {
    /// Write bytes to the running command's stdin.
    pub async fn write_stdin(&self, data: &[u8]) -> Result<()> {
        self.writer
            .lock()
            .await
            .write_data(data)
            .await
            .map_err(|e| BoxError::ExecError(format!("Streaming exec stdin write failed: {}", e)))
    }

    /// Close the running command's stdin without stopping the process.
    pub async fn close_stdin(&self) -> Result<()> {
        self.writer
            .lock()
            .await
            .write_control(EXEC_CONTROL_STDIN_CLOSE)
            .await
            .map_err(|e| {
                BoxError::ExecError(format!("Streaming exec stdin close write failed: {}", e))
            })
    }

    /// Request cancellation of the running command.
    pub async fn cancel(&self) -> Result<()> {
        self.writer
            .lock()
            .await
            .write_control(EXEC_CONTROL_CANCEL)
            .await
            .map_err(|e| BoxError::ExecError(format!("Streaming exec cancel write failed: {}", e)))
    }

    /// Deliver one of the typed Linux workload signals to the command's
    /// process group. SIGKILL retains the established cancel control for
    /// compatibility with older guests; SIGTERM uses the signal control.
    pub async fn send_signal(&self, signal: ExecutionProcessSignal) -> Result<()> {
        if signal == ExecutionProcessSignal::Kill {
            return self.cancel().await;
        }
        let mut payload = EXEC_CONTROL_SIGNAL.to_vec();
        payload.extend_from_slice(signal.linux_number().to_string().as_bytes());
        self.writer
            .lock()
            .await
            .write_control(&payload)
            .await
            .map_err(|e| BoxError::ExecError(format!("Streaming exec signal write failed: {}", e)))
    }

    /// Request a flush of the guest's buffered output. The guest replies with a
    /// flush-ack (`ExecEvent::FlushAck`) once every chunk it had buffered at
    /// flush time has been sent, establishing a clean log-rotation boundary.
    pub async fn flush(&self) -> Result<()> {
        self.writer
            .lock()
            .await
            .write_control(EXEC_CONTROL_FLUSH)
            .await
            .map_err(|e| BoxError::ExecError(format!("Streaming exec flush write failed: {}", e)))
    }
}

impl StreamingExec {
    /// Return a cloneable input handle for this running stream.
    pub fn input(&self) -> StreamingExecInput {
        StreamingExecInput {
            writer: self.writer.clone(),
        }
    }

    /// Write bytes to the running command's stdin.
    pub async fn write_stdin(&self, data: &[u8]) -> Result<()> {
        self.input().write_stdin(data).await
    }

    /// Close the running command's stdin without stopping the process.
    pub async fn close_stdin(&self) -> Result<()> {
        self.input().close_stdin().await
    }

    /// Request a flush of the guest's buffered output (see
    /// [`StreamingExecInput::flush`]).
    pub async fn flush(&self) -> Result<()> {
        self.input().flush().await
    }

    /// Read the next event from the stream.
    ///
    /// Returns `None` when the command has exited and all output has been read.
    pub async fn next_event(&mut self) -> Result<Option<a3s_box_core::exec::ExecEvent>> {
        use a3s_box_core::exec::{ExecChunk, ExecEvent, ExecExit};

        if self.done {
            return Ok(None);
        }

        let frame = match self.reader.read_frame().await {
            Ok(Some(f)) => f,
            Ok(None) => {
                self.done = true;
                return Ok(None);
            }
            Err(e) => {
                self.done = true;
                return Err(BoxError::ExecError(format!(
                    "Streaming exec read failed: {}",
                    e
                )));
            }
        };

        match frame.frame_type {
            a3s_transport::FrameType::Data => {
                // Data frame = ExecChunk (stdout/stderr)
                let chunk: ExecChunk = serde_json::from_slice(&frame.payload).map_err(|e| {
                    BoxError::ExecError(format!("Failed to parse exec chunk: {}", e))
                })?;
                match chunk.stream {
                    a3s_box_core::exec::StreamType::Stdout => {
                        self.stdout_bytes += chunk.data.len() as u64;
                    }
                    a3s_box_core::exec::StreamType::Stderr => {
                        self.stderr_bytes += chunk.data.len() as u64;
                    }
                }
                Ok(Some(ExecEvent::Chunk(chunk)))
            }
            a3s_transport::FrameType::Control => {
                // A Control frame is either a flush-ack marker or an ExecExit.
                if frame.payload == EXEC_FLUSH_ACK {
                    // Boundary marker for log rotation — the stream continues.
                    return Ok(Some(ExecEvent::FlushAck));
                }
                let exit: ExecExit = serde_json::from_slice(&frame.payload).map_err(|e| {
                    BoxError::ExecError(format!("Failed to parse exec exit: {}", e))
                })?;
                self.done = true;
                Ok(Some(ExecEvent::Exit(exit)))
            }
            a3s_transport::FrameType::Error => {
                let msg = String::from_utf8_lossy(&frame.payload);
                self.done = true;
                Err(BoxError::ExecError(format!(
                    "Streaming exec error: {}",
                    msg
                )))
            }
            _ => Err(BoxError::ExecError(format!(
                "Unexpected frame type in stream: {:?}",
                frame.frame_type
            ))),
        }
    }

    /// Request cancellation of the running streaming exec workload.
    ///
    /// The guest exec server treats this as a best-effort container stop signal
    /// and should emit a final exit frame after terminating the child process.
    pub async fn cancel(&mut self) -> Result<()> {
        self.input().cancel().await
    }

    /// Collect all remaining output and return the final result with metrics.
    ///
    /// Consumes the stream, buffering all stdout/stderr until the command exits.
    pub async fn collect(
        mut self,
    ) -> Result<(
        a3s_box_core::exec::ExecOutput,
        a3s_box_core::exec::ExecMetrics,
    )> {
        use a3s_box_core::exec::{ExecEvent, ExecMetrics, ExecOutput};

        let mut stdout = Vec::new();
        let mut stderr = Vec::new();
        let mut exit_code = -1;
        let mut truncated = false;

        while let Some(event) = self.next_event().await? {
            match event {
                ExecEvent::Chunk(chunk) => {
                    let target = match chunk.stream {
                        a3s_box_core::exec::StreamType::Stdout => &mut stdout,
                        a3s_box_core::exec::StreamType::Stderr => &mut stderr,
                    };
                    let remaining =
                        a3s_box_core::exec::MAX_OUTPUT_BYTES.saturating_sub(target.len());
                    if chunk.data.len() > remaining {
                        truncated = true;
                    }
                    target.extend_from_slice(&chunk.data[..chunk.data.len().min(remaining)]);
                }
                ExecEvent::FlushAck => {}
                ExecEvent::Exit(exit) => {
                    exit_code = exit.exit_code;
                }
            }
        }

        let metrics = ExecMetrics {
            duration_ms: self.started.elapsed().as_millis() as u64,
            peak_memory_bytes: None,
            stdout_bytes: self.stdout_bytes,
            stderr_bytes: self.stderr_bytes,
        };

        let output = ExecOutput {
            stdout,
            stderr,
            exit_code,
            truncated,
        };

        Ok((output, metrics))
    }

    /// Whether the stream has finished (command exited or connection closed).
    pub fn is_done(&self) -> bool {
        self.done
    }

    /// Get execution metrics so far.
    pub fn metrics(&self) -> a3s_box_core::exec::ExecMetrics {
        a3s_box_core::exec::ExecMetrics {
            duration_ms: self.started.elapsed().as_millis() as u64,
            peak_memory_bytes: None,
            stdout_bytes: self.stdout_bytes,
            stderr_bytes: self.stderr_bytes,
        }
    }
}

impl std::fmt::Debug for StreamingExec {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("StreamingExec")
            .field("done", &self.done)
            .field("stdout_bytes", &self.stdout_bytes)
            .field("stderr_bytes", &self.stderr_bytes)
            .finish()
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::net::UnixListener;

    fn bind_test_listener(path: &Path) -> Option<UnixListener> {
        match UnixListener::bind(path) {
            Ok(listener) => Some(listener),
            Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => {
                eprintln!(
                    "skipping Unix socket test; sandbox denied bind at {}: {}",
                    path.display(),
                    e
                );
                None
            }
            Err(e) => panic!("failed to bind test socket {}: {}", path.display(), e),
        }
    }

    #[tokio::test]
    async fn test_exec_connect_nonexistent_socket() {
        let result = ExecClient::connect(Path::new("/tmp/nonexistent-a3s-exec-test.sock")).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, BoxError::ExecError(_)));
    }

    #[tokio::test]
    async fn test_exec_connect_and_socket_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("exec.sock");
        let Some(_listener) = bind_test_listener(&sock_path) else {
            return;
        };

        let client = ExecClient::connect(&sock_path).await.unwrap();
        assert_eq!(client.socket_path(), sock_path);
    }

    #[tokio::test]
    async fn test_exec_heartbeat_with_echo_server() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("hb_echo.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            // Accept connect verification
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
            // Accept heartbeat connection and echo back
            let (mut stream, _) = listener.accept().await.unwrap();
            // Read frame header
            let mut header = [0u8; 5];
            stream.read_exact(&mut header).await.unwrap();
            let len = u32::from_be_bytes([header[1], header[2], header[3], header[4]]) as usize;
            let mut payload = vec![0u8; len];
            if len > 0 {
                stream.read_exact(&mut payload).await.unwrap();
            }
            // Respond with Heartbeat frame
            let response = a3s_transport::Frame::heartbeat();
            let encoded = response.encode().unwrap();
            stream.write_all(&encoded).await.unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let client = ExecClient::connect(&sock_path).await.unwrap();
        let result = client.heartbeat().await.unwrap();
        assert!(result);
    }

    #[tokio::test]
    async fn test_exec_heartbeat_no_response() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("hb_close.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            // Accept connect verification
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
            // Accept heartbeat connection, read request, then close
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 1024];
            let _ = stream.read(&mut buf).await;
            drop(stream);
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let client = ExecClient::connect(&sock_path).await.unwrap();
        let result = client.heartbeat().await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn test_exec_heartbeat_nonexistent_socket() {
        // heartbeat() on a non-connectable socket should return false, not error
        let client = ExecClient {
            socket_path: PathBuf::from("/tmp/nonexistent-hb-test.sock"),
        };
        let result = client.heartbeat().await.unwrap();
        assert!(!result);
    }

    #[tokio::test]
    async fn file_transfer_uses_the_discriminated_guest_request() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("file_transfer.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            // ExecClient::connect performs one reachability connection first.
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
            let (stream, _) = listener.accept().await.unwrap();
            let (read, write) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(read);
            let mut writer = a3s_transport::FrameWriter::new(write);
            let frame = reader.read_frame().await.unwrap().unwrap();
            let request: a3s_box_core::GuestSessionRequest =
                serde_json::from_slice(&frame.payload).unwrap();
            match request {
                a3s_box_core::GuestSessionRequest::File(request) => {
                    assert_eq!(request.op, a3s_box_core::FileOp::Upload);
                    assert_eq!(request.guest_path, "~/data.bin");
                    assert_eq!(request.data.as_deref(), Some("AAEC"));
                    assert_eq!(request.user.as_deref(), Some("user"));
                }
                other => panic!("unexpected guest request: {other:?}"),
            }
            writer
                .write_data(
                    &serde_json::to_vec(&a3s_box_core::FileResponse {
                        success: true,
                        data: None,
                        size: 3,
                        error: None,
                    })
                    .unwrap(),
                )
                .await
                .unwrap();
        });

        let client = ExecClient::connect(&sock_path).await.unwrap();
        let response = client
            .file_transfer(&a3s_box_core::FileRequest {
                op: a3s_box_core::FileOp::Upload,
                guest_path: "~/data.bin".to_string(),
                data: Some("AAEC".to_string()),
                user: Some("user".to_string()),
                max_bytes: None,
            })
            .await
            .unwrap();
        assert!(response.success);
        assert_eq!(response.size, 3);
    }

    #[tokio::test]
    async fn filesystem_uses_the_discriminated_guest_request() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("filesystem.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
            let (stream, _) = listener.accept().await.unwrap();
            let (read, write) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(read);
            let mut writer = a3s_transport::FrameWriter::new(write);
            let frame = reader.read_frame().await.unwrap().unwrap();
            let request: a3s_box_core::GuestSessionRequest =
                serde_json::from_slice(&frame.payload).unwrap();
            match request {
                a3s_box_core::GuestSessionRequest::Filesystem(request) => {
                    assert_eq!(request.op, a3s_box_core::FilesystemOp::ListDir);
                    assert_eq!(request.path, "~/data");
                    assert_eq!(request.depth, 2);
                    assert_eq!(request.user.as_deref(), Some("user"));
                }
                other => panic!("unexpected guest request: {other:?}"),
            }
            writer
                .write_data(
                    &serde_json::to_vec(&a3s_box_core::FilesystemResponse {
                        success: true,
                        entry: None,
                        entries: Vec::new(),
                        error: None,
                    })
                    .unwrap(),
                )
                .await
                .unwrap();
        });

        let client = ExecClient::connect(&sock_path).await.unwrap();
        let response = client
            .filesystem(&a3s_box_core::FilesystemRequest {
                op: a3s_box_core::FilesystemOp::ListDir,
                path: "~/data".to_string(),
                destination: None,
                depth: 2,
                user: Some("user".to_string()),
            })
            .await
            .unwrap();
        assert!(response.success);
        assert!(response.entries.is_empty());
    }

    #[tokio::test]
    async fn filesystem_stat_retries_once_after_lost_response() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("filesystem-stat-retry.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };
        let frames = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let frames_server = frames.clone();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);

            // First Stat: read request then drop both halves (lost response).
            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let _ = reader.read_frame().await.unwrap().unwrap();
            frames_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(reader);
            drop(w);

            // Second Stat: respond.
            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);
            let _ = reader.read_frame().await.unwrap().unwrap();
            frames_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            writer
                .write_data(
                    &serde_json::to_vec(&a3s_box_core::FilesystemResponse {
                        success: true,
                        entry: None,
                        entries: Vec::new(),
                        error: None,
                    })
                    .unwrap(),
                )
                .await
                .unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        let client = ExecClient::connect(&sock_path).await.unwrap();
        let response = client
            .filesystem(&a3s_box_core::FilesystemRequest {
                op: a3s_box_core::FilesystemOp::Stat,
                path: "/tmp".to_string(),
                destination: None,
                depth: 0,
                user: None,
            })
            .await
            .unwrap();
        assert!(response.success);
        assert_eq!(frames.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn filesystem_mkdir_does_not_retry_after_lost_response() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("filesystem-mkdir-no-retry.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };
        let frames = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let frames_server = frames.clone();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);

            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let _ = reader.read_frame().await.unwrap().unwrap();
            frames_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(reader);
            drop(w);
            // No second accept — mutating ops must not retry without journals.
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        let client = ExecClient::connect(&sock_path).await.unwrap();
        let err = client
            .filesystem(&a3s_box_core::FilesystemRequest {
                op: a3s_box_core::FilesystemOp::MakeDir,
                path: "/tmp/new-dir".to_string(),
                destination: None,
                depth: 0,
                user: None,
            })
            .await
            .unwrap_err();
        assert!(
            is_ambiguous_filesystem_transport(&err),
            "expected ambiguous transport error, got {err}"
        );
        assert_eq!(frames.load(std::sync::atomic::Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn test_archive_rootfs_streams_data_until_done_marker() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("archive.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            // ExecClient::connect performs one reachability connection first.
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut header = [0u8; 5];
            stream.read_exact(&mut header).await.unwrap();
            assert_eq!(header[0], a3s_transport::FrameType::Control as u8);
            let length = u32::from_be_bytes(header[1..5].try_into().unwrap()) as usize;
            let mut payload = vec![0u8; length];
            stream.read_exact(&mut payload).await.unwrap();
            assert_eq!(payload, EXEC_CONTROL_ARCHIVE_ROOTFS);

            for payload in [b"first".as_slice(), b"-second".as_slice()] {
                let frame = a3s_transport::Frame::data(payload.to_vec());
                stream.write_all(&frame.encode().unwrap()).await.unwrap();
            }
            let done = a3s_transport::Frame::control(EXEC_ARCHIVE_ROOTFS_DONE.to_vec());
            stream.write_all(&done.encode().unwrap()).await.unwrap();
        });

        let client = ExecClient::connect(&sock_path).await.unwrap();
        let output_path = tmp.path().join("rootfs.tar");
        let mut output = tokio::fs::File::create(&output_path).await.unwrap();
        let written = client.archive_rootfs(&mut output, false).await.unwrap();
        drop(output);

        assert_eq!(written, 12);
        assert_eq!(std::fs::read(output_path).unwrap(), b"first-second");
    }

    #[tokio::test]
    async fn test_exec_signal_main_round_trip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("signal_main.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            // Accept connect verification
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
            // Accept signal-main connection: read the Control frame, ack it.
            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);

            let frame = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(frame.frame_type, a3s_transport::FrameType::Control);
            assert_eq!(frame.payload, b"signal-main:2");

            writer.write_control(EXEC_SIGNAL_MAIN_ACK).await.unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let client = ExecClient::connect(&sock_path).await.unwrap();
        // SIGINT = 2 (image STOPSIGNAL example)
        let acked = client.signal_main(2).await.unwrap();
        assert!(acked);
    }

    #[tokio::test]
    async fn signal_main_retries_once_after_lost_ack() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("signal_main_retry.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };
        let frames = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let frames_server = frames.clone();

        tokio::spawn(async move {
            // Accept connect verification
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);

            // First signal-main: apply (read frame) then drop the whole stream
            // so the host sees closed-without-ACK (not the 10s ACK timeout).
            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let frame = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(frame.frame_type, a3s_transport::FrameType::Control);
            assert_eq!(frame.payload, b"signal-main:15");
            frames_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(reader);
            drop(w);

            // Second signal-main: ACK.
            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);
            let frame = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(frame.payload, b"signal-main:15");
            frames_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            writer.write_control(EXEC_SIGNAL_MAIN_ACK).await.unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        let client = ExecClient::connect(&sock_path).await.unwrap();
        let acked = client.signal_main(15).await.unwrap();
        assert!(acked, "lost ACK must recover via one fresh-stream retry");
        assert_eq!(
            frames.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "guest must see the same signal twice (natural idempotency)"
        );
    }

    #[tokio::test]
    async fn test_rootfs_maintenance_shutdown_round_trip() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("maintenance-shutdown.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
            let (stream, _) = listener.accept().await.unwrap();
            let (read, write) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(read);
            let mut writer = a3s_transport::FrameWriter::new(write);
            let frame = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(frame.frame_type, a3s_transport::FrameType::Control);
            assert_eq!(frame.payload, EXEC_CONTROL_SHUTDOWN_MAINTENANCE);
            writer
                .write_control(EXEC_SHUTDOWN_MAINTENANCE_ACK)
                .await
                .unwrap();
        });

        let client = ExecClient::connect(&sock_path).await.unwrap();
        assert!(client.shutdown_rootfs_maintenance().await.unwrap());
    }

    #[tokio::test]
    async fn shutdown_rootfs_maintenance_retries_once_after_lost_ack() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("maintenance-shutdown-retry.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };
        let frames = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let frames_server = frames.clone();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);

            // First shutdown: guest applies (read frame) then drop both halves.
            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let frame = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(frame.frame_type, a3s_transport::FrameType::Control);
            assert_eq!(frame.payload, EXEC_CONTROL_SHUTDOWN_MAINTENANCE);
            frames_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(reader);
            drop(w);

            // Second shutdown: ACK (guest already SHUTTING_DOWN is idempotent).
            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);
            let frame = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(frame.payload, EXEC_CONTROL_SHUTDOWN_MAINTENANCE);
            frames_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            writer
                .write_control(EXEC_SHUTDOWN_MAINTENANCE_ACK)
                .await
                .unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        let client = ExecClient::connect(&sock_path).await.unwrap();
        assert!(
            client.shutdown_rootfs_maintenance().await.unwrap(),
            "lost ACK must recover via one fresh-stream retry"
        );
        assert_eq!(
            frames.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "guest must see shutdown twice (natural idempotency)"
        );
    }

    #[tokio::test]
    async fn spawn_main_retries_once_after_lost_ack() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("spawn_main_retry.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };
        let frames = Arc::new(std::sync::atomic::AtomicUsize::new(0));
        let frames_server = frames.clone();

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);

            // First spawn-main: guest applies then drops both halves (lost ACK).
            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let frame = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(frame.frame_type, a3s_transport::FrameType::Control);
            assert!(frame.payload.starts_with(b"spawn-main:"));
            frames_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            drop(reader);
            drop(w);

            // Second spawn-main: ACK (guest published-pid idempotency).
            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);
            let frame = reader.read_frame().await.unwrap().unwrap();
            assert!(frame.payload.starts_with(b"spawn-main:"));
            frames_server.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            writer.write_control(EXEC_SPAWN_MAIN_ACK).await.unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        let client = ExecClient::connect(&sock_path).await.unwrap();
        assert!(
            client.spawn_main(None).await.unwrap(),
            "lost ACK must recover via one fresh-stream retry"
        );
        assert_eq!(frames.load(std::sync::atomic::Ordering::SeqCst), 2);
    }

    #[tokio::test]
    async fn spawn_main_treats_already_spawned_nack_as_success() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("spawn_main_already.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);
            let _ = reader.read_frame().await.unwrap().unwrap();
            let mut nack = EXEC_SPAWN_MAIN_NACK.to_vec();
            nack.extend_from_slice(b"container main already spawned");
            writer.write_control(&nack).await.unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
        let client = ExecClient::connect(&sock_path).await.unwrap();
        assert!(
            client.spawn_main(None).await.unwrap(),
            "already-spawned NACK is observe-after-ambiguity success"
        );
    }

    #[tokio::test]
    async fn test_exec_signal_main_nonexistent_socket() {
        // signal_main on a non-connectable socket returns false, not an error,
        // so the caller can fall back to a hard stop.
        let client = ExecClient {
            socket_path: PathBuf::from("/tmp/nonexistent-signal-main-test.sock"),
        };
        let acked = client.signal_main(15).await.unwrap();
        assert!(!acked);
    }

    #[tokio::test]
    async fn test_exec_client_exec_command() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("exec_cmd.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            // Accept connect verification
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
            // Accept exec request — read Frame, respond with Frame
            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);

            // Read request frame
            let _frame = reader.read_frame().await.unwrap().unwrap();

            // Send response as Data frame
            let output = a3s_box_core::exec::ExecOutput {
                stdout: b"hello\n".to_vec(),
                stderr: vec![],
                exit_code: 0,
                truncated: false,
            };
            let payload = serde_json::to_vec(&output).unwrap();
            writer.write_data(&payload).await.unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let client = ExecClient::connect(&sock_path).await.unwrap();
        let req = a3s_box_core::exec::ExecRequest {
            request_id: None,
            cmd: vec!["echo".to_string(), "hello".to_string()],
            env: vec![],
            working_dir: None,
            rootfs: None,
            user: None,
            stdin: None,
            stdin_streaming: false,
            timeout_ns: 0,
            streaming: false,
        };
        let output = client.exec_command(&req).await.unwrap();
        assert_eq!(output.exit_code, 0);
        assert_eq!(&output.stdout[..], b"hello\n");
        assert!(output.stderr.is_empty());
    }

    #[tokio::test]
    async fn test_exec_client_exec_stream_collect() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("exec_stream.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);

            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);

            let frame = reader.read_frame().await.unwrap().unwrap();
            let request: a3s_box_core::exec::ExecRequest =
                serde_json::from_slice(&frame.payload).unwrap();
            assert!(request.streaming);

            let stdout = a3s_box_core::exec::ExecChunk {
                stream: a3s_box_core::exec::StreamType::Stdout,
                data: b"hello ".to_vec(),
            };
            writer
                .write_data(&serde_json::to_vec(&stdout).unwrap())
                .await
                .unwrap();

            let stderr = a3s_box_core::exec::ExecChunk {
                stream: a3s_box_core::exec::StreamType::Stderr,
                data: b"warn".to_vec(),
            };
            writer
                .write_data(&serde_json::to_vec(&stderr).unwrap())
                .await
                .unwrap();

            let exit = a3s_box_core::exec::ExecExit {
                exit_code: 17,
                oom_killed: false,
            };
            writer
                .write_control(&serde_json::to_vec(&exit).unwrap())
                .await
                .unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let client = ExecClient::connect(&sock_path).await.unwrap();
        let req = a3s_box_core::exec::ExecRequest {
            request_id: None,
            cmd: vec!["echo".to_string(), "hello".to_string()],
            env: vec![],
            working_dir: None,
            rootfs: None,
            user: None,
            stdin: None,
            stdin_streaming: false,
            timeout_ns: 0,
            streaming: false,
        };

        let stream = client.exec_stream(&req).await.unwrap();
        let (output, metrics) = stream.collect().await.unwrap();
        assert_eq!(output.stdout, b"hello ");
        assert_eq!(output.stderr, b"warn");
        assert_eq!(output.exit_code, 17);
        assert_eq!(metrics.stdout_bytes, 6);
        assert_eq!(metrics.stderr_bytes, 4);
    }

    #[tokio::test]
    async fn test_exec_client_exec_stream_cancel_writes_control_frame() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("exec_stream_cancel.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);

            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);

            let frame = reader.read_frame().await.unwrap().unwrap();
            let request: a3s_box_core::exec::ExecRequest =
                serde_json::from_slice(&frame.payload).unwrap();
            assert!(request.streaming);

            let cancel = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(cancel.frame_type, a3s_transport::FrameType::Control);
            assert_eq!(cancel.payload, b"cancel");

            let exit = a3s_box_core::exec::ExecExit {
                exit_code: 137,
                oom_killed: false,
            };
            writer
                .write_control(&serde_json::to_vec(&exit).unwrap())
                .await
                .unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let client = ExecClient::connect(&sock_path).await.unwrap();
        let req = a3s_box_core::exec::ExecRequest {
            request_id: None,
            cmd: vec!["sleep".to_string(), "60".to_string()],
            env: vec![],
            working_dir: None,
            rootfs: None,
            user: None,
            stdin: None,
            stdin_streaming: false,
            timeout_ns: 0,
            streaming: false,
        };

        let mut stream = client.exec_stream(&req).await.unwrap();
        stream.cancel().await.unwrap();
        let event = stream.next_event().await.unwrap().unwrap();
        match event {
            a3s_box_core::exec::ExecEvent::Exit(exit) => assert_eq!(exit.exit_code, 137),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_exec_client_exec_stream_input_writes_stdin_close_and_signals() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("exec_stream_stdin.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);

            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);

            let frame = reader.read_frame().await.unwrap().unwrap();
            let request: a3s_box_core::exec::ExecRequest =
                serde_json::from_slice(&frame.payload).unwrap();
            assert!(request.streaming);

            let stdin = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(stdin.frame_type, a3s_transport::FrameType::Data);
            assert_eq!(stdin.payload, b"hello stdin\n");

            let close = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(close.frame_type, a3s_transport::FrameType::Control);
            assert_eq!(close.payload, EXEC_CONTROL_STDIN_CLOSE);

            let terminate = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(terminate.frame_type, a3s_transport::FrameType::Control);
            assert_eq!(terminate.payload, b"signal:15");

            let kill = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(kill.frame_type, a3s_transport::FrameType::Control);
            assert_eq!(kill.payload, EXEC_CONTROL_CANCEL);

            let exit = a3s_box_core::exec::ExecExit {
                exit_code: 0,
                oom_killed: false,
            };
            writer
                .write_control(&serde_json::to_vec(&exit).unwrap())
                .await
                .unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let client = ExecClient::connect(&sock_path).await.unwrap();
        let req = a3s_box_core::exec::ExecRequest {
            request_id: None,
            cmd: vec!["cat".to_string()],
            env: vec![],
            working_dir: None,
            rootfs: None,
            user: None,
            stdin: None,
            stdin_streaming: true,
            timeout_ns: 0,
            streaming: false,
        };

        let mut stream = client.exec_stream(&req).await.unwrap();
        let input = stream.input();
        input.write_stdin(b"hello stdin\n").await.unwrap();
        input.close_stdin().await.unwrap();
        input
            .send_signal(ExecutionProcessSignal::Terminate)
            .await
            .unwrap();
        input
            .send_signal(ExecutionProcessSignal::Kill)
            .await
            .unwrap();
        let event = stream.next_event().await.unwrap().unwrap();
        match event {
            a3s_box_core::exec::ExecEvent::Exit(exit) => assert_eq!(exit.exit_code, 0),
            other => panic!("unexpected event: {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_exec_client_flush_sends_control_and_parses_ack_then_exit() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("exec_stream_flush.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);

            let (stream, _) = listener.accept().await.unwrap();
            let (r, w) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(r);
            let mut writer = a3s_transport::FrameWriter::new(w);

            // Consume the streaming request, then the flush control frame.
            let _req = reader.read_frame().await.unwrap().unwrap();
            let flush = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(flush.frame_type, a3s_transport::FrameType::Control);
            assert_eq!(flush.payload, EXEC_CONTROL_FLUSH);

            // Reply: a buffered chunk, the flush-ack marker, then exit.
            let chunk = a3s_box_core::exec::ExecChunk {
                stream: a3s_box_core::exec::StreamType::Stdout,
                data: b"pre-rotation\n".to_vec(),
            };
            writer
                .write_data(&serde_json::to_vec(&chunk).unwrap())
                .await
                .unwrap();
            writer.write_control(EXEC_FLUSH_ACK).await.unwrap();
            let exit = a3s_box_core::exec::ExecExit {
                exit_code: 0,
                oom_killed: false,
            };
            writer
                .write_control(&serde_json::to_vec(&exit).unwrap())
                .await
                .unwrap();
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let client = ExecClient::connect(&sock_path).await.unwrap();
        let req = a3s_box_core::exec::ExecRequest {
            request_id: None,
            cmd: vec!["sh".to_string()],
            env: vec![],
            working_dir: None,
            rootfs: None,
            user: None,
            stdin: None,
            stdin_streaming: false,
            timeout_ns: 0,
            streaming: false,
        };

        let mut stream = client.exec_stream(&req).await.unwrap();
        stream.flush().await.unwrap();

        use a3s_box_core::exec::ExecEvent;
        match stream.next_event().await.unwrap().unwrap() {
            ExecEvent::Chunk(c) => assert_eq!(c.data, b"pre-rotation\n"),
            other => panic!("expected chunk, got {other:?}"),
        }
        // The flush-ack must parse as FlushAck, NOT as an exit (which would
        // wrongly end the stream).
        match stream.next_event().await.unwrap().unwrap() {
            ExecEvent::FlushAck => {}
            other => panic!("expected flush-ack, got {other:?}"),
        }
        match stream.next_event().await.unwrap().unwrap() {
            ExecEvent::Exit(exit) => assert_eq!(exit.exit_code, 0),
            other => panic!("expected exit, got {other:?}"),
        }
    }

    #[tokio::test]
    async fn test_exec_client_malformed_response() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("exec_bad.sock");
        let Some(listener) = bind_test_listener(&sock_path) else {
            return;
        };

        tokio::spawn(async move {
            let (stream, _) = listener.accept().await.unwrap();
            drop(stream);
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut buf = vec![0u8; 4096];
            let _ = stream.read(&mut buf).await;
            // Send garbage — not a valid frame
            stream.write_all(b"garbage").await.unwrap();
            drop(stream);
        });

        tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;

        let client = ExecClient::connect(&sock_path).await.unwrap();
        let req = a3s_box_core::exec::ExecRequest {
            request_id: None,
            cmd: vec!["test".to_string()],
            env: vec![],
            working_dir: None,
            rootfs: None,
            user: None,
            stdin: None,
            stdin_streaming: false,
            timeout_ns: 0,
            streaming: false,
        };
        let result = client.exec_command(&req).await;
        assert!(result.is_err());
    }
}

#[cfg(all(test, windows))]
mod windows_tests {
    use std::sync::atomic::{AtomicU64, Ordering};

    use tokio::net::windows::named_pipe::ServerOptions;

    use super::*;

    static NEXT_PIPE: AtomicU64 = AtomicU64::new(1);

    #[tokio::test]
    async fn exec_command_round_trips_over_a_real_named_pipe() {
        let pipe_path = format!(
            r"\\.\pipe\a3s-box-exec-client-test-{}-{}",
            std::process::id(),
            NEXT_PIPE.fetch_add(1, Ordering::Relaxed)
        );
        let first_server = ServerOptions::new()
            .first_pipe_instance(true)
            .max_instances(254)
            .create(&pipe_path)
            .expect("create first exec pipe instance");
        let server_path = pipe_path.clone();
        let server = tokio::spawn(async move {
            first_server
                .connect()
                .await
                .expect("accept reachability connection");
            let second_server = ServerOptions::new()
                .max_instances(254)
                .create(&server_path)
                .expect("create request pipe instance");
            drop(first_server);

            second_server
                .connect()
                .await
                .expect("accept exec request connection");
            let (read, write) = tokio::io::split(second_server);
            let mut reader = a3s_transport::FrameReader::new(read);
            let mut writer = a3s_transport::FrameWriter::new(write);
            let request = reader
                .read_frame()
                .await
                .expect("read request frame")
                .expect("request stream stays open");
            let request: a3s_box_core::exec::ExecRequest =
                serde_json::from_slice(&request.payload).expect("decode request");
            assert_eq!(request.cmd, ["echo", "windows"]);

            let output = a3s_box_core::exec::ExecOutput {
                stdout: b"windows\n".to_vec(),
                stderr: Vec::new(),
                exit_code: 0,
                truncated: false,
            };
            writer
                .write_data(&serde_json::to_vec(&output).expect("encode response"))
                .await
                .expect("write response");
        });

        let client = ExecClient::connect(Path::new(&pipe_path))
            .await
            .expect("connect exec client");
        let output = client
            .exec_command(&a3s_box_core::exec::ExecRequest {
                request_id: None,
                cmd: vec!["echo".to_string(), "windows".to_string()],
                timeout_ns: 5_000_000_000,
                env: Vec::new(),
                working_dir: None,
                rootfs: None,
                stdin: None,
                stdin_streaming: false,
                user: None,
                streaming: false,
            })
            .await
            .expect("execute over named pipe");

        assert_eq!(output.stdout, b"windows\n");
        assert_eq!(output.exit_code, 0);
        server.await.expect("server task joins");
    }
}

#[cfg(test)]
mod keyed_exec_tests {
    use super::*;

    #[test]
    fn ambiguous_transport_detects_lost_response_paths() {
        assert!(is_ambiguous_guest_exec_transport(&BoxError::ExecError(
            "Exec server closed without response".to_string()
        )));
        assert!(is_ambiguous_guest_exec_transport(&BoxError::ExecError(
            "Exec response timed out after 15s".to_string()
        )));
        assert!(is_ambiguous_guest_exec_transport(&BoxError::ExecError(
            "Exec connection failed to /tmp/x".to_string()
        )));
        assert!(!is_ambiguous_guest_exec_transport(&BoxError::ExecError(
            "command rejected by guest policy".to_string()
        )));
    }

    #[test]
    fn keyed_retry_requires_non_empty_request_id() {
        let keyed = a3s_box_core::exec::ExecRequest {
            request_id: Some("cli-exec-abc".to_string()),
            cmd: vec!["true".to_string()],
            timeout_ns: 1,
            env: vec![],
            working_dir: None,
            rootfs: None,
            stdin: None,
            stdin_streaming: false,
            user: None,
            streaming: false,
        };
        let unkeyed = a3s_box_core::exec::ExecRequest {
            request_id: None,
            ..keyed.clone()
        };
        let empty = a3s_box_core::exec::ExecRequest {
            request_id: Some(String::new()),
            ..keyed.clone()
        };
        let lost = BoxError::ExecError("Exec server closed without response".to_string());
        assert!(should_retry_keyed_guest_exec(&keyed, &lost));
        assert!(!should_retry_keyed_guest_exec(&unkeyed, &lost));
        assert!(!should_retry_keyed_guest_exec(&empty, &lost));
        assert!(!should_retry_keyed_guest_exec(
            &keyed,
            &BoxError::ExecError("policy denied".to_string())
        ));
    }
}
