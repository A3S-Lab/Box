#![cfg(target_os = "windows")]

use std::collections::HashMap;
use std::ffi::OsStr;
use std::io::{self, Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::os::windows::ffi::OsStrExt;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{mpsc, Arc, Condvar, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use a3s_box_core::error::{BoxError, Result};
use windows_sys::Win32::Foundation::{
    CloseHandle, GetLastError, ERROR_BROKEN_PIPE, ERROR_NO_DATA, ERROR_PIPE_CONNECTED, HANDLE,
    INVALID_HANDLE_VALUE,
};
use windows_sys::Win32::Storage::FileSystem::{ReadFile, WriteFile, PIPE_ACCESS_DUPLEX};
use windows_sys::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, DisconnectNamedPipe, PeekNamedPipe, PIPE_READMODE_BYTE,
    PIPE_TYPE_BYTE, PIPE_UNLIMITED_INSTANCES, PIPE_WAIT,
};

const FRAME_OPEN: u8 = 1;
const FRAME_OPEN_ACK: u8 = 2;
const FRAME_DATA: u8 = 3;
const FRAME_CLOSE: u8 = 4;
const OPEN_ACK_TIMEOUT: Duration = Duration::from_secs(10);
const OPEN_RETRY_WINDOW: Duration = Duration::from_secs(60);
const OPEN_RETRY_BACKOFF: Duration = Duration::from_millis(250);

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

pub fn spawn_port_forward_manager(box_id: &str, port_map: &[String]) -> Result<String> {
    let mappings = parse_port_map(port_map)?;
    if mappings.is_empty() {
        return Err(BoxError::NetworkError(
            "Windows port-forward manager requires at least one mapping".to_string(),
        ));
    }

    let pipe_base_name = format!("a3s-box-portfwd-{}", box_id.replace('-', ""));
    let pipe_path = format!(r"\\.\pipe\{}", pipe_base_name);
    let shared_control: SharedControl = Arc::new(SharedControlState {
        control: Mutex::new(None),
        cvar: Condvar::new(),
        next_stream_id: AtomicU32::new(1),
    });
    tracing::debug!(pipe = %pipe_path, mappings = ?port_map, "Spawning Windows port-forward manager");

    {
        let pipe_path = pipe_path.clone();
        let shared_control = shared_control.clone();
        thread::spawn(move || pipe_server_loop(pipe_path, shared_control));
    }

    for mapping in mappings {
        let shared_control = shared_control.clone();
        thread::spawn(move || listen_host_port(mapping, shared_control));
    }

    Ok(pipe_base_name)
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

fn listen_host_port(mapping: PortMapping, shared_control: SharedControl) {
    let listener = match TcpListener::bind(("0.0.0.0", mapping.host_port)) {
        Ok(listener) => listener,
        Err(err) => {
            tracing::error!(
                error = %err,
                host_port = mapping.host_port,
                guest_port = mapping.guest_port,
                "Failed to bind Windows published port"
            );
            return;
        }
    };

    tracing::info!(
        host_port = mapping.host_port,
        guest_port = mapping.guest_port,
        "Windows published port listener ready"
    );

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

fn handle_host_client(
    mut stream: TcpStream,
    guest_port: u16,
    shared_control: SharedControl,
) -> io::Result<()> {
    let mut control = wait_for_control(&shared_control, Duration::from_secs(60))?;
    let stream_id = shared_control
        .next_stream_id
        .fetch_add(1, Ordering::Relaxed);
    let writer_stream = stream.try_clone()?;
    control.register_stream(stream_id, writer_stream);

    let open_deadline = Instant::now() + OPEN_RETRY_WINDOW;
    let mut attempt = 0u32;
    loop {
        attempt = attempt.saturating_add(1);
        let open_rx = control.register_open_waiter(stream_id);

        match control.send_frame(FRAME_OPEN, stream_id, &guest_port.to_be_bytes()) {
            Ok(()) => {}
            Err(_) => {
                let remaining = open_deadline.saturating_duration_since(Instant::now());
                if remaining.is_zero() {
                    control.unregister_stream(stream_id);
                    return Err(io::Error::new(
                        io::ErrorKind::TimedOut,
                        format!(
                            "timed out waiting for guest port-forward open ack for port {}",
                            guest_port
                        ),
                    ));
                }

                match wait_for_control(&shared_control, remaining) {
                    Ok(new_control) if !Arc::ptr_eq(&control, &new_control) => {
                        control.unregister_stream(stream_id);
                        let writer_stream = stream.try_clone()?;
                        new_control.register_stream(stream_id, writer_stream);
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
            Ok(true) => break,
            Ok(false) | Err(_) => {}
        }

        if Instant::now() >= open_deadline {
            control.unregister_stream(stream_id);
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                format!(
                    "timed out waiting for guest port-forward open ack for port {}",
                    guest_port
                ),
            ));
        }

        thread::sleep(OPEN_RETRY_BACKOFF);
    }

    let mut buf = [0u8; 16 * 1024];
    loop {
        match stream.read(&mut buf) {
            Ok(0) => break,
            Ok(n) => control.send_frame(FRAME_DATA, stream_id, &buf[..n])?,
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

fn wait_for_control(
    shared_control: &SharedControl,
    timeout: Duration,
) -> io::Result<Arc<ControlConnection>> {
    let deadline = Instant::now() + timeout;
    let mut guard = shared_control.control.lock().unwrap();

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
        let (new_guard, result) = shared_control.cvar.wait_timeout(guard, wait).unwrap();
        guard = new_guard;
        if result.timed_out() {
            return Err(io::Error::new(
                io::ErrorKind::TimedOut,
                "guest port-forward control channel is not connected",
            ));
        }
    }
}

fn pipe_server_loop(pipe_path: String, shared_control: SharedControl) {
    loop {
        let server = match NamedPipeServer::create(&pipe_path) {
            Ok(server) => server,
            Err(err) => {
                tracing::error!(error = %err, pipe = %pipe_path, "Failed to create port-forward pipe");
                thread::sleep(Duration::from_secs(1));
                continue;
            }
        };

        if let Err(err) = server.connect() {
            tracing::warn!(error = %err, pipe = %pipe_path, "Failed to accept guest pipe connection");
            thread::sleep(Duration::from_millis(200));
            continue;
        }

        let control = Arc::new(ControlConnection::new(server));
        {
            let mut guard = shared_control.control.lock().unwrap();
            *guard = Some(control.clone());
            shared_control.cvar.notify_all();
        }

        tracing::info!(pipe = %pipe_path, "Windows guest port-forward control channel connected");
        if let Err(err) = control.read_loop() {
            tracing::warn!(error = %err, pipe = %pipe_path, "Windows guest port-forward control channel closed");
        }
        control.close_all_streams();

        let mut guard = shared_control.control.lock().unwrap();
        if guard
            .as_ref()
            .map(|existing| Arc::ptr_eq(existing, &control))
            .unwrap_or(false)
        {
            *guard = None;
        }
    }
}

struct ControlConnection {
    pipe: Arc<NamedPipeServer>,
    write_lock: Mutex<()>,
    streams: Mutex<HashMap<u32, TcpStream>>,
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

    fn register_stream(&self, stream_id: u32, stream: TcpStream) {
        self.streams.lock().unwrap().insert(stream_id, stream);
    }

    fn unregister_stream(&self, stream_id: u32) {
        if let Some(stream) = self.streams.lock().unwrap().remove(&stream_id) {
            let _ = stream.shutdown(Shutdown::Both);
        }
        self.pending_open.lock().unwrap().remove(&stream_id);
    }

    fn register_open_waiter(&self, stream_id: u32) -> mpsc::Receiver<bool> {
        let (tx, rx) = mpsc::channel();
        self.pending_open.lock().unwrap().insert(stream_id, tx);
        rx
    }

    fn send_frame(&self, kind: u8, stream_id: u32, payload: &[u8]) -> io::Result<()> {
        let _guard = self.write_lock.lock().unwrap();
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
                    if let Some(tx) = self.pending_open.lock().unwrap().remove(&frame.stream_id) {
                        let _ = tx.send(ok);
                    }
                }
                FRAME_DATA => {
                    let mut remove = false;
                    {
                        let mut streams = self.streams.lock().unwrap();
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
                FRAME_CLOSE => self.unregister_stream(frame.stream_id),
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
        let mut streams = self.streams.lock().unwrap();
        for (_, stream) in streams.drain() {
            let _ = stream.shutdown(Shutdown::Both);
        }
        let mut pending = self.pending_open.lock().unwrap();
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
    fn create(path: &str) -> io::Result<Self> {
        let path_w = wide(path);
        let handle = unsafe {
            CreateNamedPipeW(
                path_w.as_ptr(),
                PIPE_ACCESS_DUPLEX,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
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
