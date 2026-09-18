#[cfg(target_os = "linux")]
use std::collections::HashMap;
use std::io::{self, Read, Write};
#[cfg(target_os = "linux")]
use std::net::{Shutdown, TcpStream, UdpSocket};
#[cfg(target_os = "linux")]
use std::os::fd::{AsRawFd, OwnedFd};
#[cfg(target_os = "linux")]
use std::os::unix::net::UnixStream;
use std::sync::{Arc, Mutex};
#[cfg(target_os = "linux")]
use std::thread;
#[cfg(target_os = "linux")]
use std::time::Duration;

#[cfg(target_os = "linux")]
use a3s_box_core::exec::{WINDOWS_CONTROL_EXEC_FRAME, WINDOWS_CONTROL_SIGNAL_FRAME};
#[cfg(target_os = "linux")]
use a3s_box_core::PORT_FWD_VSOCK_PORT;
#[cfg(target_os = "linux")]
use nix::sys::socket::{connect, socket, AddressFamily, SockFlag, SockType, VsockAddr};
use tracing::info;
#[cfg(target_os = "linux")]
use tracing::{debug, warn};

const HOST_CID: u32 = 2;
const ENV_WINDOWS_ENABLED: &str = "BOX_WINDOWS_PORT_FWD";
const ENV_CRI_ENABLED: &str = "BOX_CRI_PORT_FWD";

const FRAME_OPEN: u8 = 1;
const FRAME_OPEN_ACK: u8 = 2;
const FRAME_DATA: u8 = 3;
const FRAME_CLOSE: u8 = 4;
// Must not collide with WINDOWS_CONTROL_SIGNAL_FRAME (5) or
// WINDOWS_CONTROL_EXEC_FRAME (6) on the shared Windows control channel.
const FRAME_OPEN_UDP: u8 = 7;

fn decode_stop_signal_payload(payload: &[u8]) -> Option<i32> {
    let bytes: [u8; 4] = payload.try_into().ok()?;
    let signal = i32::from_be_bytes(bytes);
    (1..=64).contains(&signal).then_some(signal)
}

type SharedWriter = Arc<Mutex<std::fs::File>>;
#[cfg(target_os = "linux")]
type StreamMap = Arc<Mutex<HashMap<u32, GuestTargetStream>>>;

#[cfg(target_os = "linux")]
enum GuestTargetStream {
    Tcp(TcpStream),
    Udp(UdpSocket),
    Exec(UnixStream),
}

#[cfg(target_os = "linux")]
impl GuestTargetStream {
    fn try_clone(&self) -> io::Result<Self> {
        match self {
            Self::Tcp(stream) => stream.try_clone().map(Self::Tcp),
            Self::Udp(socket) => socket.try_clone().map(Self::Udp),
            Self::Exec(stream) => stream.try_clone().map(Self::Exec),
        }
    }

    fn shutdown(&self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.shutdown(Shutdown::Both),
            // UDP has no stream shutdown; drop/close happens when the map entry
            // is removed. Connected datagrams still deliver until then.
            Self::Udp(_) => Ok(()),
            Self::Exec(stream) => stream.shutdown(Shutdown::Both),
        }
    }

    fn as_raw_fd(&self) -> std::os::fd::RawFd {
        match self {
            Self::Tcp(stream) => stream.as_raw_fd(),
            Self::Udp(socket) => socket.as_raw_fd(),
            Self::Exec(stream) => stream.as_raw_fd(),
        }
    }

    fn set_nonblocking(&self, nonblocking: bool) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.set_nonblocking(nonblocking),
            Self::Udp(socket) => socket.set_nonblocking(nonblocking),
            Self::Exec(stream) => stream.set_nonblocking(nonblocking),
        }
    }
}

#[cfg(target_os = "linux")]
impl Read for GuestTargetStream {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.read(buf),
            Self::Udp(socket) => socket.recv(buf),
            Self::Exec(stream) => stream.read(buf),
        }
    }
}

#[cfg(target_os = "linux")]
impl Write for GuestTargetStream {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Tcp(stream) => stream.write(buf),
            Self::Udp(socket) => socket.send(buf),
            Self::Exec(stream) => stream.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Tcp(stream) => stream.flush(),
            Self::Udp(_) => Ok(()),
            Self::Exec(stream) => stream.flush(),
        }
    }
}

#[cfg(target_os = "linux")]
pub fn run_port_forward_client(
    request_shutdown: fn(i32),
) -> Result<(), Box<dyn std::error::Error>> {
    if std::env::var(ENV_WINDOWS_ENABLED).as_deref() == Ok("1") {
        return run_windows_port_forward_client(request_shutdown);
    }

    if std::env::var(ENV_CRI_ENABLED).as_deref() == Ok("1") {
        return run_cri_port_forward_server();
    }

    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub fn run_port_forward_client(
    _request_shutdown: fn(i32),
) -> Result<(), Box<dyn std::error::Error>> {
    info!("Guest port forwarding is unavailable on non-Linux development hosts");
    Ok(())
}

#[cfg(target_os = "linux")]
fn run_windows_port_forward_client(
    request_shutdown: fn(i32),
) -> Result<(), Box<dyn std::error::Error>> {
    let mut backoff = Duration::from_millis(250);
    loop {
        match connect_control() {
            Ok(control) => {
                info!(
                    host_cid = HOST_CID,
                    host_port = PORT_FWD_VSOCK_PORT,
                    "Windows port-forward control channel connected"
                );
                backoff = Duration::from_millis(250);
                if let Err(err) = serve_control(control, Some(request_shutdown)) {
                    warn!(error = %err, "Windows port-forward control channel dropped");
                }
            }
            Err(err) => {
                warn!(
                    error = %err,
                    retry_ms = backoff.as_millis(),
                    "Windows port-forward control connect failed"
                );
                thread::sleep(backoff);
                backoff = (backoff * 2).min(Duration::from_secs(5));
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn run_cri_port_forward_server() -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::socket::{
        accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, VsockAddr,
    };
    use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};

    info!(
        guest_port = PORT_FWD_VSOCK_PORT,
        "Starting CRI port-forward server"
    );

    let sock_fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )?;

    unsafe {
        libc::fcntl(sock_fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }

    let addr = VsockAddr::new(libc::VMADDR_CID_ANY, PORT_FWD_VSOCK_PORT);
    bind(sock_fd.as_raw_fd(), &addr)?;
    listen(&sock_fd, Backlog::new(4)?)?;

    loop {
        match accept(sock_fd.as_raw_fd()) {
            Ok(client_fd) => {
                let client = unsafe { OwnedFd::from_raw_fd(client_fd) };
                std::thread::spawn(move || {
                    let file = std::fs::File::from(client);
                    if let Err(err) = serve_control(file, None) {
                        warn!(error = %err, "CRI port-forward control connection dropped");
                    }
                });
            }
            Err(err) => {
                warn!(error = %err, "CRI port-forward accept failed");
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

fn guest_loopback_addrs(port: u16) -> [std::net::SocketAddr; 2] {
    [
        std::net::SocketAddr::from((std::net::Ipv4Addr::LOCALHOST, port)),
        std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, port)),
    ]
}

#[cfg(target_os = "linux")]
fn connect_guest_loopback(port: u16) -> io::Result<TcpStream> {
    let mut last_error = None;
    for addr in guest_loopback_addrs(port) {
        match TcpStream::connect(addr) {
            Ok(stream) => return Ok(stream),
            Err(error) => last_error = Some(error),
        }
    }
    Err(last_error.unwrap_or_else(|| {
        io::Error::new(
            io::ErrorKind::ConnectionRefused,
            "guest loopback connect failed",
        )
    }))
}

#[cfg(target_os = "linux")]
fn connect_guest_loopback_udp(port: u16) -> io::Result<UdpSocket> {
    let socket = UdpSocket::bind(std::net::SocketAddr::from((
        std::net::Ipv4Addr::LOCALHOST,
        0,
    )))?;
    socket.connect(std::net::SocketAddr::from((
        std::net::Ipv4Addr::LOCALHOST,
        port,
    )))?;
    Ok(socket)
}

#[cfg(target_os = "linux")]
fn connect_control() -> io::Result<std::fs::File> {
    let fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::empty(),
        None,
    )
    .map_err(io::Error::other)?;

    // Set CLOEXEC manually since SOCK_CLOEXEC isn't available in nix 0.29 on macOS
    unsafe {
        libc::fcntl(fd.as_raw_fd(), libc::F_SETFD, libc::FD_CLOEXEC);
    }

    let addr = VsockAddr::new(HOST_CID, PORT_FWD_VSOCK_PORT);
    connect(fd.as_raw_fd(), &addr).map_err(io::Error::other)?;

    let owned: OwnedFd = fd;
    Ok(std::fs::File::from(owned))
}

#[cfg(target_os = "linux")]
fn serve_control(control: std::fs::File, request_shutdown: Option<fn(i32)>) -> io::Result<()> {
    let writer = Arc::new(Mutex::new(control.try_clone()?));
    let streams: StreamMap = Arc::new(Mutex::new(HashMap::new()));
    let mut reader = control;

    loop {
        let frame = match read_frame(&mut reader)? {
            Some(frame) => frame,
            None => {
                debug!("pf: serve_control read EOF, connection closing");
                return Ok(());
            }
        };
        debug!(
            kind = frame.kind,
            stream_id = frame.stream_id,
            payload_len = frame.payload.len(),
            "pf: serve_control received frame"
        );

        match frame.kind {
            FRAME_OPEN => {
                if frame.payload.len() != 2 {
                    write_frame(&writer, FRAME_OPEN_ACK, frame.stream_id, &[1])?;
                    continue;
                }

                let guest_port = u16::from_be_bytes([frame.payload[0], frame.payload[1]]);
                match connect_guest_loopback(guest_port) {
                    Ok(stream) => {
                        let _ = stream.set_nodelay(true);
                        let peer = stream.peer_addr().ok();
                        let local = stream.local_addr().ok();
                        let stream = GuestTargetStream::Tcp(stream);
                        let read_stream = stream.try_clone()?;
                        debug!(
                            stream_id = frame.stream_id,
                            guest_port,
                            peer = ?peer,
                            local = ?local,
                            "pf: connected guest TCP target, spawned reader"
                        );
                        streams.lock().unwrap().insert(frame.stream_id, stream);
                        spawn_guest_reader(
                            frame.stream_id,
                            read_stream,
                            writer.clone(),
                            streams.clone(),
                        );
                        write_frame(&writer, FRAME_OPEN_ACK, frame.stream_id, &[0])?;
                    }
                    Err(err) => {
                        debug!(
                            error = %err,
                            stream_id = frame.stream_id,
                            guest_port,
                            "Failed to connect guest TCP target"
                        );
                        write_frame(&writer, FRAME_OPEN_ACK, frame.stream_id, &[1])?;
                    }
                }
            }
            FRAME_OPEN_UDP => {
                if frame.payload.len() != 2 {
                    write_frame(&writer, FRAME_OPEN_ACK, frame.stream_id, &[1])?;
                    continue;
                }

                let guest_port = u16::from_be_bytes([frame.payload[0], frame.payload[1]]);
                match connect_guest_loopback_udp(guest_port) {
                    Ok(socket) => {
                        let peer = socket.peer_addr().ok();
                        let local = socket.local_addr().ok();
                        let stream = GuestTargetStream::Udp(socket);
                        let read_stream = stream.try_clone()?;
                        debug!(
                            stream_id = frame.stream_id,
                            guest_port,
                            peer = ?peer,
                            local = ?local,
                            "pf: connected guest UDP target, spawned reader"
                        );
                        streams.lock().unwrap().insert(frame.stream_id, stream);
                        spawn_guest_reader(
                            frame.stream_id,
                            read_stream,
                            writer.clone(),
                            streams.clone(),
                        );
                        write_frame(&writer, FRAME_OPEN_ACK, frame.stream_id, &[0])?;
                    }
                    Err(err) => {
                        debug!(
                            error = %err,
                            stream_id = frame.stream_id,
                            guest_port,
                            "Failed to connect guest UDP target"
                        );
                        write_frame(&writer, FRAME_OPEN_ACK, frame.stream_id, &[1])?;
                    }
                }
            }
            WINDOWS_CONTROL_EXEC_FRAME => {
                if !frame.payload.is_empty() {
                    write_frame(&writer, FRAME_OPEN_ACK, frame.stream_id, &[1])?;
                    continue;
                }

                match UnixStream::pair() {
                    Ok((handler_stream, relay_stream)) => {
                        let handler_fd: OwnedFd = handler_stream.into();
                        thread::spawn(move || {
                            if let Err(error) = crate::exec_server::handle_connection(handler_fd) {
                                warn!(
                                    error = %error,
                                    "Windows tunneled exec handler failed"
                                );
                            }
                        });

                        let stream = GuestTargetStream::Exec(relay_stream);
                        let read_stream = stream.try_clone()?;
                        streams.lock().unwrap().insert(frame.stream_id, stream);
                        spawn_guest_reader(
                            frame.stream_id,
                            read_stream,
                            writer.clone(),
                            streams.clone(),
                        );
                        write_frame(&writer, FRAME_OPEN_ACK, frame.stream_id, &[0])?;
                    }
                    Err(error) => {
                        warn!(
                            error = %error,
                            stream_id = frame.stream_id,
                            "Failed to create Windows tunneled exec session"
                        );
                        write_frame(&writer, FRAME_OPEN_ACK, frame.stream_id, &[1])?;
                    }
                }
            }
            FRAME_DATA => {
                // Clone under the map lock, then write outside it. Holding the
                // map mutex across a guest write wedged every later OPEN/DATA
                // when TSI stalled one stream (#446).
                let mut stream = {
                    let guard = streams.lock().unwrap();
                    match guard.get(&frame.stream_id) {
                        Some(stream) => match stream.try_clone() {
                            Ok(cloned) => cloned,
                            Err(err) => {
                                warn!(
                                    stream_id = frame.stream_id,
                                    error = %err,
                                    "pf: clone guest target for DATA write failed"
                                );
                                drop(guard);
                                close_stream(frame.stream_id, &streams);
                                let _ = write_frame(&writer, FRAME_CLOSE, frame.stream_id, &[]);
                                continue;
                            }
                        },
                        None => {
                            warn!(stream_id = frame.stream_id, "pf: DATA for unknown stream");
                            continue;
                        }
                    }
                };
                match stream
                    .write_all(&frame.payload)
                    .and_then(|_| stream.flush())
                {
                    Ok(()) => debug!(
                        stream_id = frame.stream_id,
                        len = frame.payload.len(),
                        "pf: wrote client data to guest target"
                    ),
                    Err(err) => {
                        warn!(stream_id = frame.stream_id, error = %err, "pf: write to guest target failed");
                        close_stream(frame.stream_id, &streams);
                        let _ = write_frame(&writer, FRAME_CLOSE, frame.stream_id, &[]);
                    }
                }
            }
            FRAME_CLOSE => {
                close_stream(frame.stream_id, &streams);
            }
            WINDOWS_CONTROL_SIGNAL_FRAME if request_shutdown.is_some() => {
                let Some(signal) = decode_stop_signal_payload(&frame.payload) else {
                    warn!(
                        stream_id = frame.stream_id,
                        payload_len = frame.payload.len(),
                        "Ignoring invalid Windows stop control frame"
                    );
                    continue;
                };
                if frame.stream_id != 0 {
                    warn!(
                        stream_id = frame.stream_id,
                        signal, "Ignoring Windows stop control frame with a nonzero stream ID"
                    );
                    continue;
                }

                request_shutdown.expect("guarded by is_some")(signal);
                info!(signal, "Windows host stop signal accepted by guest init");
            }
            WINDOWS_CONTROL_SIGNAL_FRAME => {
                debug!("Ignoring Windows lifecycle frame on CRI port-forward channel");
            }
            _ => {
                debug!(kind = frame.kind, "Ignoring unknown port-forward frame");
            }
        }
    }
}

#[cfg(target_os = "linux")]
fn spawn_guest_reader(
    stream_id: u32,
    mut stream: GuestTargetStream,
    writer: SharedWriter,
    streams: StreamMap,
) {
    thread::spawn(move || {
        // Under libkrun TSI, a blocking recv() on an AF_INET socket blocks a
        // concurrent send() on another fd of the same connection. Wait with
        // poll(POLLIN) and read with O_NONBLOCK so OPEN→DATA relays complete
        // (#446). Write half stays blocking.
        if let Err(err) = stream.set_nonblocking(true) {
            warn!(
                stream_id,
                error = %err,
                "pf: failed to set guest target reader non-blocking"
            );
            close_stream(stream_id, &streams);
            let _ = write_frame(&writer, FRAME_CLOSE, stream_id, &[]);
            return;
        }

        let mut buf = [0u8; 16 * 1024];
        debug!(stream_id, "pf: guest reader thread started");
        loop {
            if let Err(err) = wait_fd_readable(stream.as_raw_fd()) {
                warn!(stream_id, error = %err, "pf: guest target poll failed");
                break;
            }

            match stream.read(&mut buf) {
                Ok(0) => {
                    debug!(stream_id, "pf: guest target read EOF");
                    break;
                }
                Ok(n) => {
                    debug!(stream_id, n, "pf: read from guest target -> FRAME_DATA");
                    if write_frame(&writer, FRAME_DATA, stream_id, &buf[..n]).is_err() {
                        warn!(stream_id, "pf: relay FRAME_DATA to host failed");
                        break;
                    }
                }
                Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
                Err(err) if err.kind() == io::ErrorKind::WouldBlock => continue,
                Err(err) => {
                    warn!(stream_id, error = %err, "pf: guest target read error");
                    break;
                }
            }
        }

        debug!(stream_id, "pf: guest reader thread exiting");
        close_stream(stream_id, &streams);
        let _ = write_frame(&writer, FRAME_CLOSE, stream_id, &[]);
    });
}

/// Block until `fd` is readable, errored, or hung up.
///
/// Prefer this over a blocking `recv()` whenever another thread may `send()` on
/// the same TSI TCP connection (#446).
#[cfg(target_os = "linux")]
fn wait_fd_readable(fd: std::os::fd::RawFd) -> io::Result<()> {
    loop {
        let mut fds = [libc::pollfd {
            fd,
            events: libc::POLLIN | libc::POLLERR | libc::POLLHUP,
            revents: 0,
        }];
        let rc = unsafe { libc::poll(fds.as_mut_ptr(), 1, -1) };
        if rc < 0 {
            let err = io::Error::last_os_error();
            if err.kind() == io::ErrorKind::Interrupted {
                continue;
            }
            return Err(err);
        }
        if rc == 0 {
            continue;
        }
        return Ok(());
    }
}

#[cfg(target_os = "linux")]
fn close_stream(stream_id: u32, streams: &StreamMap) {
    if let Some(stream) = streams.lock().unwrap().remove(&stream_id) {
        let _ = stream.shutdown();
    }
}

struct Frame {
    kind: u8,
    stream_id: u32,
    payload: Vec<u8>,
}

fn read_frame(reader: &mut impl Read) -> io::Result<Option<Frame>> {
    let mut header = [0u8; 9];
    match reader.read_exact(&mut header) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err),
    }

    let len = u32::from_be_bytes([header[5], header[6], header[7], header[8]]) as usize;
    let mut payload = vec![0u8; len];
    if len > 0 {
        reader.read_exact(&mut payload)?;
    }

    Ok(Some(Frame {
        kind: header[0],
        stream_id: u32::from_be_bytes([header[1], header[2], header[3], header[4]]),
        payload,
    }))
}

fn write_frame(writer: &SharedWriter, kind: u8, stream_id: u32, payload: &[u8]) -> io::Result<()> {
    let mut guard = writer.lock().unwrap();
    guard.write_all(&[kind])?;
    guard.write_all(&stream_id.to_be_bytes())?;
    guard.write_all(&(payload.len() as u32).to_be_bytes())?;
    if !payload.is_empty() {
        guard.write_all(payload)?;
    }
    guard.flush()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Cursor, ErrorKind, Seek, SeekFrom};

    #[test]
    fn write_then_read_frame_round_trips_kind_stream_and_payload() {
        let backing_file = tempfile::NamedTempFile::new().unwrap();
        let file = backing_file.reopen().unwrap();
        let writer = Arc::new(Mutex::new(file));

        write_frame(&writer, FRAME_DATA, 0x0102_0304, b"hello").unwrap();

        let mut guard = writer.lock().unwrap();
        guard.seek(SeekFrom::Start(0)).unwrap();
        let frame = read_frame(&mut *guard).unwrap().unwrap();

        assert_eq!(frame.kind, FRAME_DATA);
        assert_eq!(frame.stream_id, 0x0102_0304);
        assert_eq!(frame.payload, b"hello");
    }

    #[test]
    fn write_then_read_frame_supports_empty_payload() {
        let backing_file = tempfile::NamedTempFile::new().unwrap();
        let file = backing_file.reopen().unwrap();
        let writer = Arc::new(Mutex::new(file));

        write_frame(&writer, FRAME_CLOSE, 7, &[]).unwrap();

        let mut guard = writer.lock().unwrap();
        guard.seek(SeekFrom::Start(0)).unwrap();
        let frame = read_frame(&mut *guard).unwrap().unwrap();

        assert_eq!(frame.kind, FRAME_CLOSE);
        assert_eq!(frame.stream_id, 7);
        assert!(frame.payload.is_empty());
    }

    #[test]
    fn read_frame_returns_none_on_clean_eof_before_header() {
        let mut cursor = Cursor::new(Vec::<u8>::new());

        assert!(read_frame(&mut cursor).unwrap().is_none());
    }

    #[test]
    fn read_frame_errors_on_truncated_payload() {
        let bytes = vec![
            FRAME_DATA, 0, 0, 0, 9, // stream id
            0, 0, 0, 4, // payload length
            b'o', b'k',
        ];
        let mut cursor = Cursor::new(bytes);

        let err = match read_frame(&mut cursor) {
            Ok(_) => panic!("truncated payload should return an error"),
            Err(err) => err,
        };

        assert_eq!(err.kind(), ErrorKind::UnexpectedEof);
    }

    #[test]
    fn read_frame_consumes_coalesced_frames_without_an_extra_readiness_edge() {
        let mut bytes = Vec::new();
        bytes.extend_from_slice(&[FRAME_CLOSE]);
        bytes.extend_from_slice(&1_u32.to_be_bytes());
        bytes.extend_from_slice(&0_u32.to_be_bytes());
        bytes.extend_from_slice(&[FRAME_OPEN]);
        bytes.extend_from_slice(&2_u32.to_be_bytes());
        bytes.extend_from_slice(&2_u32.to_be_bytes());
        bytes.extend_from_slice(&8080_u16.to_be_bytes());
        let mut cursor = Cursor::new(bytes);

        let close = read_frame(&mut cursor).unwrap().unwrap();
        let open = read_frame(&mut cursor).unwrap().unwrap();

        assert_eq!(close.kind, FRAME_CLOSE);
        assert_eq!(close.stream_id, 1);
        assert!(close.payload.is_empty());
        assert_eq!(open.kind, FRAME_OPEN);
        assert_eq!(open.stream_id, 2);
        assert_eq!(open.payload, 8080_u16.to_be_bytes());
    }

    #[test]
    fn guest_loopback_addrs_try_ipv4_then_ipv6() {
        let addrs = guest_loopback_addrs(8080);
        assert_eq!(addrs[0], std::net::SocketAddr::from(([127, 0, 0, 1], 8080)));
        assert_eq!(
            addrs[1],
            std::net::SocketAddr::from((std::net::Ipv6Addr::LOCALHOST, 8080))
        );
    }

    #[test]
    fn stop_signal_payload_requires_one_valid_big_endian_signal() {
        assert_eq!(decode_stop_signal_payload(&15_i32.to_be_bytes()), Some(15));
        assert_eq!(decode_stop_signal_payload(&64_i32.to_be_bytes()), Some(64));
        assert_eq!(decode_stop_signal_payload(&0_i32.to_be_bytes()), None);
        assert_eq!(decode_stop_signal_payload(&65_i32.to_be_bytes()), None);
        assert_eq!(decode_stop_signal_payload(&[15]), None);
    }
}
