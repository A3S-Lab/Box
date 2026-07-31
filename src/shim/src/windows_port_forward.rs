#![cfg(target_os = "windows")]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::fs;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex, MutexGuard};
use std::thread;
use std::time::{Duration, Instant};

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::exec::{
    windows_exec_pipe_path, WINDOWS_CONTROL_EXEC_FRAME, WINDOWS_CONTROL_SIGNAL_FRAME,
    WINDOWS_GUEST_CONTROL_READY_FILE,
};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, HANDLE,
    INVALID_HANDLE_VALUE, WAIT_OBJECT_0,
};
use windows_sys::Win32::Storage::FileSystem::{
    FlushFileBuffers, ReadFile, WriteFile, FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX,
};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PeekNamedPipe, PIPE_READMODE_BYTE,
    PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};
use windows_sys::Win32::System::Threading::{OpenProcess, WaitForSingleObject, INFINITE};

const FRAME_OPEN: u8 = 1;
const FRAME_OPEN_ACK: u8 = 2;
const FRAME_DATA: u8 = 3;
const FRAME_CLOSE: u8 = 4;
const OPEN_ACK_TIMEOUT: Duration = Duration::from_secs(10);
const OPEN_RETRY_WINDOW: Duration = Duration::from_secs(60);
const OPEN_RETRY_BACKOFF: Duration = Duration::from_millis(250);
const PORT_FWD_READY_TIMEOUT: Duration = Duration::from_secs(5);
const PORT_REBIND_TIMEOUT: Duration = Duration::from_secs(2);
const PORT_REBIND_BACKOFF: Duration = Duration::from_millis(25);
const STOP_CONTROL_WAIT: Duration = Duration::from_secs(1);
const STOP_REQUEST_POLL: Duration = Duration::from_millis(50);
const MAX_STOP_REQUEST_BYTES: u64 = 16;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;
const PROCESS_SYNCHRONIZE_ACCESS: u32 = 0x0010_0000;

#[derive(Clone, Copy, Debug)]
struct PortMapping {
    host_port: u16,
    guest_port: u16,
}

struct SharedControlState {
    control: Mutex<Option<Arc<ControlConnection>>>,
    cvar: Condvar,
    next_stream_id: AtomicU32,
}

type SharedControl = Arc<SharedControlState>;

pub struct PortForwardManager {
    pipe_base_name: String,
    child: Child,
}

impl PortForwardManager {
    pub fn pipe_name(&self) -> &str {
        &self.pipe_base_name
    }
}

impl Drop for PortForwardManager {
    fn drop(&mut self) {
        if self.child.try_wait().ok().flatten().is_none() {
            if let Err(error) = self.child.kill() {
                tracing::warn!(
                    worker_pid = self.child.id(),
                    error = %error,
                    "Failed to terminate Windows port-forward worker"
                );
            }
        }
        if let Err(error) = self.child.wait() {
            tracing::warn!(
                worker_pid = self.child.id(),
                error = %error,
                "Failed to wait for Windows port-forward worker"
            );
        }
    }
}

pub fn spawn_port_forward_manager(
    box_id: &str,
    port_map: &[String],
    stop_request: &Path,
) -> Result<PortForwardManager> {
    parse_port_map(port_map)?;
    let pipe_base_name = format!("a3s-box-portfwd-{}", box_id.replace('-', ""));
    let ready_file = std::env::temp_dir().join(format!(
        "a3s-box-portfwd-{}-{}.ready",
        box_id.replace('-', ""),
        std::process::id()
    ));
    let _ = fs::remove_file(&ready_file);

    let current_exe = std::env::current_exe().map_err(|err| {
        BoxError::NetworkError(format!(
            "failed to resolve shim executable for Windows port-forward worker: {}",
            err
        ))
    })?;
    let mut cmd = Command::new(current_exe);
    cmd.arg("--port-fwd-worker")
        .arg("--box-id")
        .arg(box_id)
        .arg("--parent-pid")
        .arg(std::process::id().to_string())
        .arg("--ready-file")
        .arg(&ready_file)
        .arg("--stop-request")
        .arg(stop_request)
        .stdin(Stdio::null())
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .creation_flags(CREATE_NO_WINDOW);
    for mapping in port_map {
        cmd.arg("--port-map").arg(mapping);
    }

    let mut child = cmd.spawn().map_err(|err| {
        BoxError::NetworkError(format!(
            "failed to spawn Windows port-forward worker: {}",
            err
        ))
    })?;

    let ready_started = Instant::now();
    loop {
        if let Ok(contents) = fs::read_to_string(&ready_file) {
            let _ = fs::remove_file(&ready_file);
            let trimmed = contents.trim();
            if trimmed.eq_ignore_ascii_case("ok") {
                tracing::info!(
                    box_id,
                    pipe = %pipe_base_name,
                    worker_pid = child.id(),
                    "Windows port-forward worker ready"
                );
                return Ok(PortForwardManager {
                    pipe_base_name,
                    child,
                });
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(BoxError::NetworkError(format!(
                "Windows port-forward worker failed: {}",
                trimmed
            )));
        }

        if let Ok(Some(status)) = child.try_wait() {
            let _ = fs::remove_file(&ready_file);
            return Err(BoxError::NetworkError(format!(
                "Windows port-forward worker exited before readiness (status: {})",
                status
            )));
        }

        if ready_started.elapsed() >= PORT_FWD_READY_TIMEOUT {
            let _ = child.kill();
            let _ = child.wait();
            let _ = fs::remove_file(&ready_file);
            return Err(BoxError::NetworkError(
                "timed out waiting for Windows port-forward worker readiness".to_string(),
            ));
        }

        thread::sleep(Duration::from_millis(50));
    }
}

pub fn run_port_forward_worker(
    box_id: &str,
    port_map: &[String],
    parent_pid: u32,
    ready_file: &Path,
    stop_request: &Path,
) -> Result<()> {
    let mappings = parse_port_map(port_map)?;

    spawn_parent_watchdog(parent_pid);

    let pipe_base_name = format!("a3s-box-portfwd-{}", box_id.replace('-', ""));
    let pipe_path = format!(r"\\.\pipe\{}", pipe_base_name);
    let exec_pipe_path = windows_exec_pipe_path(box_id);
    let guest_control_ready_file = stop_request.with_file_name(WINDOWS_GUEST_CONTROL_READY_FILE);
    let _ = fs::remove_file(&guest_control_ready_file);
    let shared_control: SharedControl = Arc::new(SharedControlState {
        control: Mutex::new(None),
        cvar: Condvar::new(),
        next_stream_id: AtomicU32::new(1),
    });
    spawn_stop_request_forwarder(stop_request.to_path_buf(), shared_control.clone());

    let initial_server = match NamedPipeServer::create(&pipe_path, true) {
        Ok(server) => server,
        Err(err) => {
            write_ready_file(
                ready_file,
                &format!(
                    "failed to create Windows port-forward pipe {}: {}",
                    pipe_path, err
                ),
            );
            return Err(BoxError::NetworkError(format!(
                "failed to create Windows port-forward pipe {}: {}",
                pipe_path, err
            )));
        }
    };
    tracing::info!(pipe = %pipe_path, "Windows published-port control pipe ready");

    let initial_exec_server = match NamedPipeServer::create(&exec_pipe_path, true) {
        Ok(server) => server,
        Err(err) => {
            write_ready_file(
                ready_file,
                &format!(
                    "failed to create Windows exec pipe {}: {}",
                    exec_pipe_path, err
                ),
            );
            return Err(BoxError::NetworkError(format!(
                "failed to create Windows exec pipe {}: {}",
                exec_pipe_path, err
            )));
        }
    };
    tracing::info!(pipe = %exec_pipe_path, "Windows host exec pipe ready");

    for mapping in mappings {
        let listener = match bind_published_port(mapping, PORT_REBIND_TIMEOUT) {
            Ok(listener) => listener,
            Err(err) => {
                write_ready_file(
                    ready_file,
                    &format!(
                        "failed to bind Windows published port 0.0.0.0:{} -> {}: {}",
                        mapping.host_port, mapping.guest_port, err
                    ),
                );
                return Err(BoxError::NetworkError(format!(
                    "failed to bind Windows published port 0.0.0.0:{} -> {}: {}",
                    mapping.host_port, mapping.guest_port, err
                )));
            }
        };
        tracing::info!(
            host_port = mapping.host_port,
            guest_port = mapping.guest_port,
            "Windows published port listener ready"
        );

        let shared_control = shared_control.clone();
        thread::spawn(move || listen_host_port_loop(listener, mapping, shared_control));
    }

    let exec_control = shared_control.clone();
    thread::spawn(move || exec_pipe_server_loop(initial_exec_server, exec_pipe_path, exec_control));

    write_ready_file(ready_file, "ok");
    pipe_server_loop(
        initial_server,
        pipe_path,
        guest_control_ready_file,
        shared_control,
    );
    Ok(())
}

fn bind_published_port(mapping: PortMapping, timeout: Duration) -> io::Result<TcpListener> {
    let started = Instant::now();
    loop {
        match TcpListener::bind(("0.0.0.0", mapping.host_port)) {
            Ok(listener) => return Ok(listener),
            Err(error)
                if error.kind() == io::ErrorKind::AddrInUse && started.elapsed() < timeout =>
            {
                thread::sleep(PORT_REBIND_BACKOFF.min(timeout.saturating_sub(started.elapsed())));
            }
            Err(error) => return Err(error),
        }
    }
}

fn decode_stop_signal(bytes: &[u8]) -> io::Result<i32> {
    let value = std::str::from_utf8(bytes)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?
        .trim()
        .parse::<i32>()
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    if !(1..=64).contains(&value) {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!("Windows stop signal must be between 1 and 64, got {value}"),
        ));
    }
    Ok(value)
}

fn read_stop_signal(path: &Path) -> io::Result<Option<i32>> {
    let metadata = match fs::metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    if metadata.len() > MAX_STOP_REQUEST_BYTES {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            format!(
                "Windows stop request {} exceeds {MAX_STOP_REQUEST_BYTES} bytes",
                path.display()
            ),
        ));
    }
    fs::read(path).and_then(|bytes| decode_stop_signal(&bytes).map(Some))
}

fn spawn_stop_request_forwarder(path: PathBuf, shared_control: SharedControl) {
    thread::spawn(move || {
        let mut last_error = None;
        loop {
            match read_stop_signal(&path) {
                Ok(None) => last_error = None,
                Ok(Some(signal)) => match wait_for_control(&shared_control, STOP_CONTROL_WAIT) {
                    Ok(control) => match control.send_frame(
                        WINDOWS_CONTROL_SIGNAL_FRAME,
                        0,
                        &signal.to_be_bytes(),
                    ) {
                        Ok(()) => {
                            match fs::remove_file(&path) {
                                Ok(()) => {}
                                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                                Err(error) => tracing::warn!(
                                    error = %error,
                                    path = %path.display(),
                                    "Failed to remove delivered Windows stop request"
                                ),
                            }
                            tracing::info!(
                                signal,
                                path = %path.display(),
                                "Forwarded Windows stop request to guest init"
                            );
                            return;
                        }
                        Err(error) => {
                            tracing::debug!(
                                error = %error,
                                signal,
                                "Failed to send Windows stop request; retrying"
                            );
                        }
                    },
                    Err(error) => tracing::debug!(
                        error = %error,
                        signal,
                        "Windows guest control channel is not ready for stop request"
                    ),
                },
                Err(error) => {
                    let message = error.to_string();
                    if last_error.as_deref() != Some(message.as_str()) {
                        tracing::warn!(
                            error = %error,
                            path = %path.display(),
                            "Invalid Windows stop request"
                        );
                        last_error = Some(message);
                    }
                }
            }
            thread::sleep(STOP_REQUEST_POLL);
        }
    });
}

fn write_ready_file(path: &Path, contents: &str) {
    if let Err(err) = fs::write(path, contents) {
        tracing::warn!(error = %err, path = %path.display(), "Failed to write Windows port-forward readiness file");
    }
}

fn spawn_parent_watchdog(parent_pid: u32) {
    thread::spawn(move || {
        let handle = unsafe { OpenProcess(PROCESS_SYNCHRONIZE_ACCESS, 0, parent_pid) };
        if handle == 0 {
            tracing::info!(
                parent_pid,
                "Windows port-forward worker exiting because shim parent is unavailable"
            );
            std::process::exit(0);
        }

        let wait_status = unsafe { WaitForSingleObject(handle, INFINITE) };
        unsafe {
            CloseHandle(handle);
        }
        if wait_status != WAIT_OBJECT_0 {
            tracing::warn!(
                parent_pid,
                wait_status,
                "Windows port-forward worker parent wait returned an unexpected status"
            );
        }
        tracing::info!(
            parent_pid,
            "Windows port-forward worker exiting because shim parent is gone"
        );
        std::process::exit(0);
    });
}

fn parse_port_map(port_map: &[String]) -> Result<Vec<PortMapping>> {
    port_map
        .iter()
        .map(|mapping| {
            let (host, guest) = mapping.split_once(':').ok_or_else(|| {
                BoxError::NetworkError(format!(
                    "invalid port mapping '{}' (expected host:guest)",
                    mapping
                ))
            })?;

            let host_port = host.parse::<u16>().map_err(|_| {
                BoxError::NetworkError(format!("invalid host port in mapping '{}'", mapping))
            })?;
            let guest_port = guest.parse::<u16>().map_err(|_| {
                BoxError::NetworkError(format!("invalid guest port in mapping '{}'", mapping))
            })?;

            Ok(PortMapping {
                host_port,
                guest_port,
            })
        })
        .collect()
}

fn listen_host_port_loop(
    listener: TcpListener,
    mapping: PortMapping,
    shared_control: SharedControl,
) {
    for incoming in listener.incoming() {
        match incoming {
            Ok(stream) => {
                let shared_control = shared_control.clone();
                thread::spawn(move || {
                    if let Err(err) = handle_host_client(stream, mapping.guest_port, shared_control)
                    {
                        tracing::debug!(
                            error = %err,
                            host_port = mapping.host_port,
                            guest_port = mapping.guest_port,
                            "Published port session ended"
                        );
                    }
                });
            }
            Err(err) => {
                tracing::warn!(
                    error = %err,
                    host_port = mapping.host_port,
                    "Failed to accept published port connection"
                );
                thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

enum HostClient {
    Tcp(TcpStream),
    Pipe(Arc<NamedPipeServer>),
}

impl HostClient {
    fn writer(&self) -> io::Result<HostWriter> {
        match self {
            Self::Tcp(stream) => stream.try_clone().map(HostWriter::Tcp),
            Self::Pipe(stream) => Ok(HostWriter::Pipe(stream.clone())),
        }
    }

    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.read(buf),
            Self::Pipe(stream) => stream.read(buf),
        }
    }
}

enum HostWriter {
    Tcp(TcpStream),
    Pipe(Arc<NamedPipeServer>),
}

impl HostWriter {
    fn write_all(&mut self, payload: &[u8]) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.write_all(payload),
            Self::Pipe(stream) => stream.write_all(payload),
        }
    }

    fn shutdown(&self) {
        match self {
            Self::Tcp(stream) => {
                let _ = stream.shutdown(Shutdown::Both);
            }
            Self::Pipe(stream) => stream.disconnect(),
        }
    }
}

fn handle_host_client(
    stream: TcpStream,
    guest_port: u16,
    shared_control: SharedControl,
) -> io::Result<()> {
    handle_host_stream(
        HostClient::Tcp(stream),
        FRAME_OPEN,
        guest_port.to_be_bytes().to_vec(),
        format!("guest TCP port {guest_port}"),
        shared_control,
    )
}

fn handle_exec_pipe_client(
    stream: NamedPipeServer,
    shared_control: SharedControl,
) -> io::Result<()> {
    handle_host_stream(
        HostClient::Pipe(Arc::new(stream)),
        WINDOWS_CONTROL_EXEC_FRAME,
        Vec::new(),
        "guest exec service".to_string(),
        shared_control,
    )
}

fn handle_host_stream(
    mut stream: HostClient,
    open_frame: u8,
    open_payload: Vec<u8>,
    target: String,
    shared_control: SharedControl,
) -> io::Result<()> {
    let mut control = wait_for_control(&shared_control, Duration::from_secs(60))?;
    let stream_id = shared_control
        .next_stream_id
        .fetch_add(1, Ordering::Relaxed);
    control.register_stream(stream_id, stream.writer()?);

    let open_deadline = Instant::now() + OPEN_RETRY_WINDOW;
    let mut attempt = 0u32;
    loop {
        attempt = attempt.saturating_add(1);
        let open_rx = control.register_open_waiter(stream_id);

        match control.send_frame(open_frame, stream_id, &open_payload) {
            Ok(()) => {}
            Err(_) => {
                let remaining = open_deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    control.unregister_stream(stream_id);
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!("timed out waiting for open ack from {target}"),
                    ));
                }

                match wait_for_control(&shared_control, remaining) {
                    Ok(new_control) if !Arc::ptr_eq(&control, &new_control) => {
                        control.detach_stream(stream_id);
                        new_control.register_stream(stream_id, stream.writer()?);
                        control = new_control;
                    }
                    Ok(_) => {
                        thread::sleep(OPEN_RETRY_BACKOFF);
                    }
                    Err(wait_err) => {
                        control.unregister_stream(stream_id);
                        return Err(wait_err);
                    }
                }
                continue;
            }
        }

        match open_rx.recv_timeout(OPEN_ACK_TIMEOUT) {
            Ok(true) => {
                tracing::debug!(stream_id, attempt, target, "Guest tunnel opened");
                break;
            }
            Ok(false) => tracing::debug!(
                stream_id,
                attempt,
                target,
                "Guest rejected tunnel open; retrying"
            ),
            Err(error) => tracing::debug!(
                stream_id,
                attempt,
                target,
                error = %error,
                "Guest tunnel open acknowledgement timed out; retrying"
            ),
        }

        if Instant::now() >= open_deadline {
            control.unregister_stream(stream_id);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!("timed out waiting for open ack from {target}"),
            ));
        }

        thread::sleep(OPEN_RETRY_BACKOFF);
    }

    let mut buf = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => {
                tracing::debug!(stream_id, len = n, target, "Forwarding host tunnel data");
                control.send_frame(FRAME_DATA, stream_id, &buf[..n])?;
            }
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => {
                thread::sleep(Duration::from_millis(5));
                continue;
            }
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            Err(err) => {
                control.unregister_stream(stream_id);
                let _ = control.send_frame(FRAME_CLOSE, stream_id, &[]);
                return Err(err);
            }
        }
    }

    control.unregister_stream(stream_id);
    let _ = control.send_frame(FRAME_CLOSE, stream_id, &[]);
    Ok(())
}

fn exec_pipe_server_loop(
    initial_server: NamedPipeServer,
    pipe_path: String,
    shared_control: SharedControl,
) {
    let mut next_server = Some(initial_server);
    loop {
        let server = match next_server.take() {
            Some(server) => server,
            None => match NamedPipeServer::create(&pipe_path, false) {
                Ok(server) => server,
                Err(error) => {
                    tracing::error!(
                        error = %error,
                        pipe = %pipe_path,
                        "Failed to create Windows exec pipe instance"
                    );
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }
            },
        };

        if let Err(error) = server.connect() {
            tracing::warn!(
                error = %error,
                pipe = %pipe_path,
                "Failed to accept Windows exec pipe client"
            );
            thread::sleep(Duration::from_millis(100));
            continue;
        }

        next_server = NamedPipeServer::create(&pipe_path, false).ok();
        let control = shared_control.clone();
        thread::spawn(move || {
            if let Err(error) = handle_exec_pipe_client(server, control) {
                tracing::debug!(error = %error, "Windows tunneled exec session ended");
            }
        });
    }
}

fn wait_for_control(
    shared_control: &SharedControl,
    timeout: Duration,
) -> io::Result<Arc<ControlConnection>> {
    let deadline = Instant::now() + timeout;
    let mut guard = lock_or_recover(&shared_control.control, "shared port-forward control");

    loop {
        if let Some(control) = guard.as_ref() {
            return Ok(control.clone());
        }

        let now = Instant::now();
        if now >= deadline {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "guest port-forward control channel is not connected",
            ));
        }

        let wait = deadline.saturating_duration_since(now);
        let (new_guard, timed_out) = match shared_control.cvar.wait_timeout(guard, wait) {
            Ok((new_guard, result)) => (new_guard, result.timed_out()),
            Err(poisoned) => {
                tracing::warn!(
                    "Recovered poisoned shared port-forward control mutex while waiting"
                );
                let (new_guard, result) = poisoned.into_inner();
                (new_guard, result.timed_out())
            }
        };
        guard = new_guard;
        if timed_out {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "guest port-forward control channel is not connected",
            ));
        }
    }
}

fn pipe_server_loop(
    initial_server: NamedPipeServer,
    pipe_path: String,
    guest_control_ready_file: PathBuf,
    shared_control: SharedControl,
) {
    let mut next_server = Some(initial_server);
    loop {
        let server = match next_server.take() {
            Some(server) => server,
            None => match NamedPipeServer::create(&pipe_path, false) {
                Ok(server) => server,
                Err(err) => {
                    tracing::error!(error = %err, pipe = %pipe_path, "Failed to create port-forward pipe");
                    thread::sleep(Duration::from_secs(1));
                    continue;
                }
            },
        };

        if let Err(err) = server.connect() {
            tracing::warn!(error = %err, pipe = %pipe_path, "Failed to accept guest pipe connection");
            thread::sleep(Duration::from_millis(200));
            continue;
        }

        let control = Arc::new(ControlConnection::new(server));
        {
            let mut guard = lock_or_recover(&shared_control.control, "shared port-forward control");
            *guard = Some(control.clone());
            shared_control.cvar.notify_all();
        }

        tracing::info!(pipe = %pipe_path, "Windows guest port-forward control channel connected");
        write_ready_file(&guest_control_ready_file, "ok");
        if let Err(err) = control.read_loop() {
            tracing::warn!(error = %err, pipe = %pipe_path, "Windows guest port-forward control channel closed");
        }
        if let Err(error) = fs::remove_file(&guest_control_ready_file) {
            if error.kind() != io::ErrorKind::NotFound {
                tracing::warn!(
                    error = %error,
                    path = %guest_control_ready_file.display(),
                    "Failed to remove Windows guest control readiness marker"
                );
            }
        }
        control.close_all_streams();

        let mut guard = lock_or_recover(&shared_control.control, "shared port-forward control");
        if guard
            .as_ref()
            .map(|existing| Arc::ptr_eq(existing, &control))
            .unwrap_or(false)
        {
            *guard = None;
        }
    }
}

fn lock_or_recover<'a, T>(mutex: &'a Mutex<T>, name: &str) -> MutexGuard<'a, T> {
    match mutex.lock() {
        Ok(guard) => guard,
        Err(poisoned) => {
            tracing::warn!(
                mutex = name,
                "Recovered poisoned Windows port-forward mutex"
            );
            poisoned.into_inner()
        }
    }
}

struct ControlConnection {
    pipe: Arc<NamedPipeServer>,
    write_lock: Mutex<()>,
    streams: Mutex<HashMap<u32, HostWriter>>,
    pending_open: Mutex<HashMap<u32, mpsc::Sender<bool>>>,
}

impl ControlConnection {
    fn new(pipe: NamedPipeServer) -> Self {
        Self {
            pipe: Arc::new(pipe),
            write_lock: Mutex::new(()),
            streams: Mutex::new(HashMap::new()),
            pending_open: Mutex::new(HashMap::new()),
        }
    }

    fn register_stream(&self, stream_id: u32, stream: HostWriter) {
        lock_or_recover(&self.streams, "port-forward streams").insert(stream_id, stream);
    }

    fn detach_stream(&self, stream_id: u32) {
        lock_or_recover(&self.streams, "port-forward streams").remove(&stream_id);
        lock_or_recover(&self.pending_open, "port-forward pending open").remove(&stream_id);
    }

    fn unregister_stream(&self, stream_id: u32) {
        if let Some(stream) =
            lock_or_recover(&self.streams, "port-forward streams").remove(&stream_id)
        {
            stream.shutdown();
        }
        lock_or_recover(&self.pending_open, "port-forward pending open").remove(&stream_id);
    }

    fn register_open_waiter(&self, stream_id: u32) -> mpsc::Receiver<bool> {
        let (tx, rx) = mpsc::channel();
        lock_or_recover(&self.pending_open, "port-forward pending open").insert(stream_id, tx);
        rx
    }

    fn send_frame(&self, kind: u8, stream_id: u32, payload: &[u8]) -> io::Result<()> {
        let _guard = lock_or_recover(&self.write_lock, "port-forward write lock");
        self.pipe.write_frame(kind, stream_id, payload)
    }

    fn read_loop(&self) -> io::Result<()> {
        loop {
            let frame = match self.pipe.read_frame() {
                Ok(frame) => frame,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => {
                    thread::sleep(Duration::from_millis(5));
                    continue;
                }
                Err(err) => return Err(err),
            };

            let frame = match frame {
                Some(frame) => frame,
                None => return Ok(()),
            };

            match frame.kind {
                FRAME_OPEN_ACK => {
                    let ok = frame.payload.first().copied().unwrap_or(1) == 0;
                    tracing::debug!(
                        stream_id = frame.stream_id,
                        ok,
                        "Received guest tunnel open acknowledgement"
                    );
                    if let Some(tx) =
                        lock_or_recover(&self.pending_open, "port-forward pending open")
                            .remove(&frame.stream_id)
                    {
                        let _ = tx.send(ok);
                    }
                }
                FRAME_DATA => {
                    tracing::debug!(
                        stream_id = frame.stream_id,
                        len = frame.payload.len(),
                        "Forwarding guest tunnel data to host client"
                    );
                    let mut remove = false;
                    {
                        let mut streams = lock_or_recover(&self.streams, "port-forward streams");
                        if let Some(stream) = streams.get_mut(&frame.stream_id) {
                            if stream.write_all(&frame.payload).is_err() {
                                remove = true;
                            }
                        }
                    }
                    if remove {
                        self.unregister_stream(frame.stream_id);
                    }
                }
                FRAME_CLOSE => {
                    tracing::debug!(stream_id = frame.stream_id, "Guest closed tunneled stream");
                    self.unregister_stream(frame.stream_id)
                }
                _ => {
                    tracing::debug!(
                        kind = frame.kind,
                        "Ignoring unknown Windows port-forward frame"
                    );
                }
            }
        }
    }

    fn close_all_streams(&self) {
        let mut streams = lock_or_recover(&self.streams, "port-forward streams");
        for (_, stream) in streams.drain() {
            stream.shutdown();
        }
        let mut pending = lock_or_recover(&self.pending_open, "port-forward pending open");
        for (_, tx) in pending.drain() {
            let _ = tx.send(false);
        }
    }
}

struct Frame {
    kind: u8,
    stream_id: u32,
    payload: Vec<u8>,
}

struct NamedPipeServer {
    handle: HANDLE,
}

impl NamedPipeServer {
    fn create(path: &str, first_instance: bool) -> io::Result<Self> {
        let path_w = wide(path);
        let open_mode = PIPE_ACCESS_DUPLEX
            | if first_instance {
                FILE_FLAG_FIRST_PIPE_INSTANCE
            } else {
                0
            };
        let handle = unsafe {
            CreateNamedPipeW(
                path_w.as_ptr(),
                open_mode,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT | PIPE_REJECT_REMOTE_CLIENTS,
                PIPE_UNLIMITED_INSTANCES,
                64 * 1024,
                64 * 1024,
                0,
                std::ptr::null(),
            )
        };

        if handle == INVALID_HANDLE_VALUE {
            return Err(io::Error::last_os_error());
        }

        Ok(Self { handle })
    }

    fn disconnect(&self) {
        unsafe {
            // A guest response and FRAME_CLOSE commonly arrive back-to-back.
            // Wait until the named-pipe client has consumed every byte already
            // written before disconnecting, otherwise Windows may discard the
            // final response and make the client fail with ERROR_PIPE_NOT_CONNECTED
            // (233). A departed client makes FlushFileBuffers fail immediately;
            // disconnect remains best-effort in either case.
            FlushFileBuffers(self.handle);
            DisconnectNamedPipe(self.handle);
        }
    }

    fn connect(&self) -> io::Result<()> {
        let result = unsafe { ConnectNamedPipe(self.handle, std::ptr::null_mut()) };
        if result != 0 {
            return Ok(());
        }

        let err = unsafe { GetLastError() };
        if err == ERROR_PIPE_CONNECTED {
            Ok(())
        } else {
            Err(io::Error::from_raw_os_error(err as i32))
        }
    }

    fn read_frame(&self) -> io::Result<Option<Frame>> {
        let mut preview = [0u8; 9];
        let mut preview_read = 0u32;
        let mut bytes_available = 0u32;
        let ok = unsafe {
            PeekNamedPipe(
                self.handle,
                preview.as_mut_ptr() as *mut _,
                preview.len() as u32,
                &mut preview_read,
                &mut bytes_available,
                std::ptr::null_mut(),
            )
        };

        if ok == 0 {
            let err = io::Error::last_os_error();
            if matches!(
                err.raw_os_error(),
                Some(code) if code == ERROR_BROKEN_PIPE as i32 || code == ERROR_NO_DATA as i32
            ) {
                return Ok(None);
            }
            return Err(err);
        }

        if preview_read < preview.len() as u32 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "pipe frame header not ready",
            ));
        }

        let header = preview;
        let len = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;
        let frame_size = header.len() + len;
        if bytes_available < frame_size as u32 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "pipe frame payload not ready",
            ));
        }

        let mut header = [0u8; 9];
        self.read_exact(&mut header)?;

        let mut payload = vec![0u8; len];
        if len > 0 {
            self.read_exact(&mut payload)?;
        }

        Ok(Some(Frame {
            kind: header[0],
            stream_id: u32::from_be_bytes([header[1], header[2], header[3], header[4]]),
            payload,
        }))
    }

    fn write_frame(&self, kind: u8, stream_id: u32, payload: &[u8]) -> io::Result<()> {
        self.write_all(&[kind])?;
        self.write_all(&stream_id.to_be_bytes())?;
        self.write_all(&(payload.len() as u32).to_be_bytes())?;
        if !payload.is_empty() {
            self.write_all(payload)?;
        }
        Ok(())
    }

    fn read_exact(&self, buf: &mut [u8]) -> io::Result<()> {
        let mut offset = 0usize;
        while offset < buf.len() {
            let mut read = 0u32;
            let ok = unsafe {
                ReadFile(
                    self.handle,
                    buf[offset..].as_mut_ptr() as *mut _,
                    (buf.len() - offset) as u32,
                    &mut read,
                    std::ptr::null_mut(),
                )
            };

            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            if read == 0 {
                return Err(io::Error::new(io::ErrorKind::UnexpectedEof, "pipe closed"));
            }
            offset += read as usize;
        }
        Ok(())
    }

    fn read(&self, buf: &mut [u8]) -> io::Result<usize> {
        let mut bytes_available = 0u32;
        let peek_ok = unsafe {
            PeekNamedPipe(
                self.handle,
                std::ptr::null_mut(),
                0,
                std::ptr::null_mut(),
                &mut bytes_available,
                std::ptr::null_mut(),
            )
        };
        if peek_ok == 0 {
            return Err(io::Error::last_os_error());
        }
        if bytes_available == 0 {
            return Err(io::Error::new(
                io::ErrorKind::WouldBlock,
                "named pipe has no client data available",
            ));
        }

        let mut read = 0u32;
        let read_len = buf.len().min(bytes_available as usize);
        let ok = unsafe {
            ReadFile(
                self.handle,
                buf.as_mut_ptr() as *mut _,
                read_len as u32,
                &mut read,
                std::ptr::null_mut(),
            )
        };
        if ok == 0 {
            return Err(io::Error::last_os_error());
        }
        Ok(read as usize)
    }

    fn write_all(&self, buf: &[u8]) -> io::Result<()> {
        let mut offset = 0usize;
        while offset < buf.len() {
            let mut written = 0u32;
            let ok = unsafe {
                WriteFile(
                    self.handle,
                    buf[offset..].as_ptr() as *const _,
                    (buf.len() - offset) as u32,
                    &mut written,
                    std::ptr::null_mut(),
                )
            };

            if ok == 0 {
                return Err(io::Error::last_os_error());
            }
            offset += written as usize;
        }
        Ok(())
    }
}

impl Drop for NamedPipeServer {
    fn drop(&mut self) {
        unsafe {
            DisconnectNamedPipe(self.handle);
            CloseHandle(self.handle);
        }
    }
}

unsafe impl Send for NamedPipeServer {}
unsafe impl Sync for NamedPipeServer {}

fn wide(s: &str) -> Vec<u16> {
    OsStr::new(s)
        .encode_wide()
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs::OpenOptions;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn empty_port_map_is_valid_for_lifecycle_only_control() {
        assert!(parse_port_map(&[]).unwrap().is_empty());
    }

    #[test]
    fn stop_signal_decoder_accepts_the_linux_signal_range() {
        assert_eq!(decode_stop_signal(b"15").unwrap(), 15);
        assert_eq!(decode_stop_signal(b"64\n").unwrap(), 64);
    }

    #[test]
    fn stop_signal_decoder_rejects_invalid_values() {
        for value in [b"0".as_slice(), b"65", b"term", b""] {
            assert_eq!(
                decode_stop_signal(value).unwrap_err().kind(),
                io::ErrorKind::InvalidData
            );
        }
    }

    #[test]
    fn stop_signal_reader_distinguishes_missing_and_valid_requests() {
        let directory = tempfile::tempdir().unwrap();
        let request = directory.path().join("stop.signal");

        assert_eq!(read_stop_signal(&request).unwrap(), None);
        fs::write(&request, "2").unwrap();
        assert_eq!(read_stop_signal(&request).unwrap(), Some(2));
    }

    #[test]
    fn stop_signal_reader_rejects_oversized_requests() {
        let directory = tempfile::tempdir().unwrap();
        let request = directory.path().join("stop.signal");
        fs::write(&request, vec![b'1'; MAX_STOP_REQUEST_BYTES as usize + 1]).unwrap();

        assert_eq!(
            read_stop_signal(&request).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn published_port_bind_retries_until_the_previous_listener_exits() {
        let previous = TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let host_port = previous.local_addr().unwrap().port();
        let release = thread::spawn(move || {
            thread::sleep(Duration::from_millis(100));
            drop(previous);
        });

        let listener = bind_published_port(
            PortMapping {
                host_port,
                guest_port: 8080,
            },
            Duration::from_secs(1),
        )
        .unwrap();

        release.join().unwrap();
        assert_eq!(listener.local_addr().unwrap().port(), host_port);
    }

    #[test]
    fn published_port_bind_reports_a_persistent_conflict() {
        let previous = TcpListener::bind(("0.0.0.0", 0)).unwrap();
        let host_port = previous.local_addr().unwrap().port();

        let error = bind_published_port(
            PortMapping {
                host_port,
                guest_port: 8080,
            },
            Duration::from_millis(75),
        )
        .unwrap_err();

        assert_eq!(error.kind(), io::ErrorKind::AddrInUse);
    }

    #[test]
    fn named_pipe_disconnect_flushes_the_complete_response() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos();
        let path = format!(
            r"\\.\pipe\a3s-box-flush-test-{}-{nonce}",
            std::process::id()
        );
        let server = NamedPipeServer::create(&path, true).unwrap();
        let client_path = path.clone();
        let payload = b"response-before-close";

        let client = thread::spawn(move || {
            let mut pipe = OpenOptions::new()
                .read(true)
                .write(true)
                .open(client_path)
                .unwrap();
            let mut received = vec![0u8; payload.len()];
            pipe.read_exact(&mut received).unwrap();
            received
        });

        server.connect().unwrap();
        server.write_all(payload).unwrap();
        server.disconnect();

        assert_eq!(client.join().unwrap(), payload);
    }
}
