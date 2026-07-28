//! Linux passt peer-network multiplexer.
//!
//! passt provides excellent host egress but owns one guest connection and does
//! not switch Ethernet frames between separate passt instances. This adapter
//! stays between libkrun and passt: gateway/non-peer traffic keeps flowing to
//! passt, while frames addressed to another box on the same A3S network use the
//! shared Unix-datagram switch already used by the macOS netproxy backend.

use std::io::{self, Read, Write};
use std::os::fd::{AsRawFd, FromRawFd, RawFd};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};
use std::time::Duration;

use crate::device::BridgePort;

const MIN_ETHERNET_FRAME: usize = 14;
const MAX_ETHERNET_FRAME: usize = 65_550;
const MAX_PENDING_BYTES: usize = 4 * 1024 * 1024;
const IO_BURST: usize = 64;
const POLL_TIMEOUT_MS: libc::c_int = 100;

/// Start a shim-owned adapter for an inherited libkrun stream socket.
pub fn spawn_inherited_passt_bridge(
    proxy_fd: RawFd,
    passt_socket_path: PathBuf,
    bridge_socket_dir: PathBuf,
    own_mac: [u8; 6],
) -> a3s_box_core::error::Result<()> {
    if proxy_fd < 0 {
        return Err(a3s_box_core::error::BoxError::NetworkError(
            "invalid inherited passt bridge descriptor".to_string(),
        ));
    }

    // SAFETY: the runtime transfers sole ownership of this inherited endpoint
    // to the shim through NetworkInstanceConfig.
    let guest = unsafe { UnixStream::from_raw_fd(proxy_fd) };
    let passt = connect_passt(&passt_socket_path).map_err(|error| {
        a3s_box_core::error::BoxError::NetworkError(format!(
            "failed to connect passt bridge to {}: {error}",
            passt_socket_path.display()
        ))
    })?;
    let bridge = BridgePort::bind(&bridge_socket_dir, own_mac).map_err(|error| {
        a3s_box_core::error::BoxError::NetworkError(format!(
            "failed to join bridge Ethernet switch: {error}"
        ))
    })?;

    std::thread::Builder::new()
        .name("a3s-passt-bridge".to_string())
        .spawn(move || {
            if let Err(error) = run_passt_bridge(guest, passt, bridge) {
                tracing::warn!(%error, "passt peer bridge stopped");
            }
        })
        .map_err(|error| {
            a3s_box_core::error::BoxError::NetworkError(format!(
                "failed to spawn passt peer bridge: {error}"
            ))
        })?;
    Ok(())
}

fn connect_passt(path: &Path) -> io::Result<UnixStream> {
    let deadline = std::time::Instant::now() + Duration::from_secs(2);
    loop {
        match UnixStream::connect(path) {
            Ok(stream) => return Ok(stream),
            Err(error) if std::time::Instant::now() < deadline => {
                tracing::debug!(%error, path = %path.display(), "Waiting for passt socket");
                std::thread::sleep(Duration::from_millis(20));
            }
            Err(error) => return Err(error),
        }
    }
}

fn run_passt_bridge(
    mut guest: UnixStream,
    mut passt: UnixStream,
    bridge: BridgePort,
) -> io::Result<()> {
    guest.set_nonblocking(true)?;
    passt.set_nonblocking(true)?;

    let mut guest_input = Vec::new();
    let mut passt_input = Vec::new();
    let mut to_guest = PendingBytes::default();
    let mut to_passt = PendingBytes::default();

    loop {
        let mut progressed = false;
        progressed |= to_guest.flush(&mut guest)?;
        progressed |= to_passt.flush(&mut passt)?;

        match read_available(&mut guest, &mut guest_input)? {
            ReadState::Progress => progressed = true,
            ReadState::Eof => return Ok(()),
            ReadState::Idle => {}
        }
        for frame in decode_frames(&mut guest_input)? {
            progressed = true;
            if bridge.forward_from_guest(&frame) {
                to_passt.push_frame(&frame)?;
            }
        }

        match read_available(&mut passt, &mut passt_input)? {
            ReadState::Progress => progressed = true,
            ReadState::Eof => {
                return Err(io::Error::new(
                    io::ErrorKind::BrokenPipe,
                    "passt closed its guest connection",
                ));
            }
            ReadState::Idle => {}
        }
        // Buffer complete passt frames before forwarding them. Passing through
        // arbitrary stream fragments would let a peer frame land in the middle
        // of a partially-read passt frame and corrupt libkrun's framing.
        for frame in decode_frames(&mut passt_input)? {
            progressed = true;
            to_guest.push_frame(&frame)?;
        }

        let mut peer_frames = Vec::new();
        bridge.drain_frames(&mut peer_frames, IO_BURST);
        for frame in peer_frames {
            progressed = true;
            to_guest.push_frame(&frame)?;
        }

        progressed |= to_guest.flush(&mut guest)?;
        progressed |= to_passt.flush(&mut passt)?;
        if !progressed {
            poll_network(
                &guest,
                &passt,
                bridge.raw_fd(),
                !to_guest.is_empty(),
                !to_passt.is_empty(),
            )?;
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ReadState {
    Idle,
    Progress,
    Eof,
}

fn read_available(stream: &mut UnixStream, output: &mut Vec<u8>) -> io::Result<ReadState> {
    let mut buffer = [0u8; 64 * 1024];
    let mut state = ReadState::Idle;
    for _ in 0..IO_BURST {
        match stream.read(&mut buffer) {
            Ok(0) => return Ok(ReadState::Eof),
            Ok(size) => {
                if output.len().saturating_add(size) > MAX_PENDING_BYTES {
                    return Err(io::Error::new(
                        io::ErrorKind::OutOfMemory,
                        "passt bridge input buffer limit exceeded",
                    ));
                }
                output.extend_from_slice(&buffer[..size]);
                state = ReadState::Progress;
            }
            Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
            Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
            Err(error) => return Err(error),
        }
    }
    Ok(state)
}

fn decode_frames(buffer: &mut Vec<u8>) -> io::Result<Vec<Vec<u8>>> {
    let mut frames = Vec::new();
    let mut offset = 0usize;
    while buffer.len().saturating_sub(offset) >= 4 {
        let length = u32::from_be_bytes(buffer[offset..offset + 4].try_into().unwrap()) as usize;
        if !(MIN_ETHERNET_FRAME..=MAX_ETHERNET_FRAME).contains(&length) {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                format!("invalid passt Ethernet frame length {length}"),
            ));
        }
        let framed_length = 4 + length;
        if buffer.len().saturating_sub(offset) < framed_length {
            break;
        }
        frames.push(buffer[offset + 4..offset + framed_length].to_vec());
        offset += framed_length;
    }
    if offset != 0 {
        buffer.drain(..offset);
    }
    Ok(frames)
}

#[derive(Default)]
struct PendingBytes {
    bytes: Vec<u8>,
    offset: usize,
}

impl PendingBytes {
    fn is_empty(&self) -> bool {
        self.offset == self.bytes.len()
    }

    fn push(&mut self, bytes: &[u8]) -> io::Result<()> {
        self.compact();
        if self.bytes.len().saturating_add(bytes.len()) > MAX_PENDING_BYTES {
            return Err(io::Error::new(
                io::ErrorKind::OutOfMemory,
                "passt bridge output buffer limit exceeded",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        Ok(())
    }

    fn push_frame(&mut self, frame: &[u8]) -> io::Result<()> {
        let length = u32::try_from(frame.len()).map_err(|_| {
            io::Error::new(io::ErrorKind::InvalidData, "Ethernet frame is too large")
        })?;
        self.push(&length.to_be_bytes())?;
        self.push(frame)
    }

    fn flush(&mut self, stream: &mut UnixStream) -> io::Result<bool> {
        let mut progressed = false;
        for _ in 0..IO_BURST {
            if self.is_empty() {
                self.bytes.clear();
                self.offset = 0;
                break;
            }
            match stream.write(&self.bytes[self.offset..]) {
                Ok(0) => {
                    return Err(io::Error::new(
                        io::ErrorKind::WriteZero,
                        "passt bridge stream returned a zero-length write",
                    ));
                }
                Ok(size) => {
                    self.offset += size;
                    progressed = true;
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => return Err(error),
            }
        }
        Ok(progressed)
    }

    fn compact(&mut self) {
        if self.offset == 0 {
            return;
        }
        if self.offset == self.bytes.len() {
            self.bytes.clear();
        } else {
            self.bytes.drain(..self.offset);
        }
        self.offset = 0;
    }
}

fn poll_network(
    guest: &UnixStream,
    passt: &UnixStream,
    bridge_fd: RawFd,
    guest_writable: bool,
    passt_writable: bool,
) -> io::Result<()> {
    let mut descriptors = [
        libc::pollfd {
            fd: guest.as_raw_fd(),
            events: libc::POLLIN | if guest_writable { libc::POLLOUT } else { 0 },
            revents: 0,
        },
        libc::pollfd {
            fd: passt.as_raw_fd(),
            events: libc::POLLIN | if passt_writable { libc::POLLOUT } else { 0 },
            revents: 0,
        },
        libc::pollfd {
            fd: bridge_fd,
            events: libc::POLLIN,
            revents: 0,
        },
    ];
    let result = unsafe {
        libc::poll(
            descriptors.as_mut_ptr(),
            descriptors.len() as libc::nfds_t,
            POLL_TIMEOUT_MS,
        )
    };
    if result < 0 {
        let error = io::Error::last_os_error();
        if error.kind() != io::ErrorKind::Interrupted {
            return Err(error);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ethernet_frame(destination: [u8; 6], source: [u8; 6], marker: u8) -> Vec<u8> {
        let mut frame = vec![0u8; 60];
        frame[..6].copy_from_slice(&destination);
        frame[6..12].copy_from_slice(&source);
        frame[12..14].copy_from_slice(&[0x08, 0x00]);
        frame[14] = marker;
        frame
    }

    fn write_frame(stream: &mut UnixStream, frame: &[u8]) {
        stream
            .write_all(&(frame.len() as u32).to_be_bytes())
            .unwrap();
        stream.write_all(frame).unwrap();
    }

    fn read_frame(stream: &mut UnixStream) -> Vec<u8> {
        let mut length = [0u8; 4];
        stream.read_exact(&mut length).unwrap();
        let mut frame = vec![0u8; u32::from_be_bytes(length) as usize];
        stream.read_exact(&mut frame).unwrap();
        frame
    }

    #[test]
    fn peer_unicast_bypasses_passt_while_gateway_traffic_reaches_it() {
        let directory = tempfile::tempdir().unwrap();
        let mac_a = [0x02, 0x42, 10, 91, 0, 2];
        let mac_b = [0x02, 0x42, 10, 91, 0, 3];
        let bridge_a = BridgePort::bind(directory.path(), mac_a).unwrap();
        let bridge_b = BridgePort::bind(directory.path(), mac_b).unwrap();
        let (mut guest_a, proxy_a) = UnixStream::pair().unwrap();
        let (mut guest_b, proxy_b) = UnixStream::pair().unwrap();
        let (passt_a, mut backend_a) = UnixStream::pair().unwrap();
        let (passt_b, _backend_b) = UnixStream::pair().unwrap();
        guest_b
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        backend_a
            .set_read_timeout(Some(Duration::from_millis(100)))
            .unwrap();
        guest_a
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();

        let thread_a = std::thread::spawn(move || run_passt_bridge(proxy_a, passt_a, bridge_a));
        let thread_b = std::thread::spawn(move || run_passt_bridge(proxy_b, passt_b, bridge_b));

        let peer = ethernet_frame(mac_b, mac_a, 0x11);
        write_frame(&mut guest_a, &peer);
        assert_eq!(read_frame(&mut guest_b), peer);
        let mut byte = [0u8; 1];
        assert!(matches!(
            backend_a.read(&mut byte),
            Err(error)
                if matches!(
                    error.kind(),
                    io::ErrorKind::WouldBlock | io::ErrorKind::TimedOut
                )
        ));

        let gateway = ethernet_frame([0x02, 0, 0, 0, 0, 1], mac_a, 0x22);
        write_frame(&mut guest_a, &gateway);
        backend_a
            .set_read_timeout(Some(Duration::from_secs(2)))
            .unwrap();
        assert_eq!(read_frame(&mut backend_a), gateway);

        // A partial passt frame must stay buffered while a complete peer frame
        // passes it. Otherwise the two stream records become interleaved and
        // libkrun can no longer decode either one.
        let from_passt = ethernet_frame(mac_a, [0x02, 0, 0, 0, 0, 1], 0x33);
        let mut encoded = (from_passt.len() as u32).to_be_bytes().to_vec();
        encoded.extend_from_slice(&from_passt);
        backend_a.write_all(&encoded[..2]).unwrap();
        std::thread::sleep(Duration::from_millis(150));

        let peer_reply = ethernet_frame(mac_a, mac_b, 0x44);
        write_frame(&mut guest_b, &peer_reply);
        assert_eq!(read_frame(&mut guest_a), peer_reply);

        backend_a.write_all(&encoded[2..]).unwrap();
        assert_eq!(read_frame(&mut guest_a), from_passt);

        drop(guest_a);
        drop(guest_b);
        assert!(thread_a.join().unwrap().is_ok());
        assert!(thread_b.join().unwrap().is_ok());
    }

    #[test]
    fn decoder_retains_partial_frames_and_rejects_invalid_lengths() {
        let frame = ethernet_frame([0xff; 6], [1, 2, 3, 4, 5, 6], 0x33);
        let mut encoded = (frame.len() as u32).to_be_bytes().to_vec();
        encoded.extend_from_slice(&frame);
        let tail = encoded.split_off(7);
        assert!(decode_frames(&mut encoded).unwrap().is_empty());
        encoded.extend_from_slice(&tail);
        assert_eq!(decode_frames(&mut encoded).unwrap(), vec![frame]);
        assert!(encoded.is_empty());

        let mut invalid = 1u32.to_be_bytes().to_vec();
        assert_eq!(
            decode_frames(&mut invalid).unwrap_err().kind(),
            io::ErrorKind::InvalidData
        );
    }
}
