use std::collections::VecDeque;
use std::io;
use std::os::unix::net::UnixDatagram;
use std::path::{Path, PathBuf};
use std::sync::{
    atomic::{AtomicU64, Ordering},
    Arc,
};

use smoltcp::time::Instant;
use smoltcp::wire::EthernetAddress;

/// MAC address we assign to the virtual gateway interface.
pub(super) const GATEWAY_MAC: EthernetAddress =
    EthernetAddress([0x02, 0x00, 0x00, 0x00, 0x00, 0x01]);
/// Maximum Ethernet frame size (header + MTU).
pub(super) const MAX_FRAME: usize = 1514;

#[derive(Default)]
pub(super) struct NetStats {
    rx_bytes: AtomicU64,
    tx_bytes: AtomicU64,
    rx_packets: AtomicU64,
    tx_packets: AtomicU64,
}

pub(super) struct NetStatsSnapshot {
    pub(super) rx_bytes: u64,
    pub(super) tx_bytes: u64,
    pub(super) rx_packets: u64,
    pub(super) tx_packets: u64,
}

impl NetStats {
    pub(super) fn record_rx(&self, bytes: usize) {
        self.rx_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        self.rx_packets.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn record_tx(&self, bytes: usize) {
        self.tx_bytes.fetch_add(bytes as u64, Ordering::Relaxed);
        self.tx_packets.fetch_add(1, Ordering::Relaxed);
    }

    pub(super) fn snapshot(&self) -> NetStatsSnapshot {
        NetStatsSnapshot {
            rx_bytes: self.rx_bytes.load(Ordering::Relaxed),
            tx_bytes: self.tx_bytes.load(Ordering::Relaxed),
            rx_packets: self.rx_packets.load(Ordering::Relaxed),
            tx_packets: self.tx_packets.load(Ordering::Relaxed),
        }
    }
}

// ── smoltcp phy::Device ───────────────────────────────────────────────────────

/// smoltcp physical-layer device backed by a connected Unix datagram socket.
///
/// Frames from the VM arrive via `recv()` and are queued in `rx_queue`.
/// smoltcp reads them through `receive()`. Frames smoltcp wants to transmit
/// are sent directly to the peer via `transmit()` / `TxToken::consume()`.
///
/// The socket MUST be connected to the peer (via `UnixDatagram::connect`) before
/// use so that `send()` works without a destination address. On macOS, using
/// `send_to()` on a socket whose peer has called `connect()` to us causes
/// ECONNRESET / EDESTADDRREQ in the peer's receive path.
pub(super) struct UnixgramDevice {
    pub(super) socket: UnixDatagram,
    pub(super) bridge: Option<BridgePort>,
    pub(super) rx_queue: VecDeque<Vec<u8>>,
    pub(super) stats: Arc<NetStats>,
}

impl UnixgramDevice {
    /// Drain the socket into `rx_queue` (non-blocking, batch up to 64 frames).
    pub(super) fn drain(&mut self) {
        if let Some(bridge) = &self.bridge {
            bridge.drain_to_guest(&self.socket, &self.stats);
        }
        let mut buf = vec![0u8; MAX_FRAME];
        for _ in 0..64 {
            match self.socket.recv(&mut buf) {
                Ok(n) => {
                    tracing::trace!(
                        bytes = n,
                        "NetProxy received ethernet frame from guest/libkrun"
                    );
                    self.stats.record_tx(n);
                    let frame = &buf[..n];
                    let deliver_locally = self
                        .bridge
                        .as_ref()
                        .map(|bridge| bridge.forward_from_guest(frame))
                        .unwrap_or(true);
                    if deliver_locally {
                        self.rx_queue.push_back(frame.to_vec());
                    }
                }
                Err(e) if e.kind() == io::ErrorKind::WouldBlock => break,
                Err(e) => {
                    tracing::warn!(error = %e, "NetProxy: recv from libkrun failed");
                    break;
                }
            }
        }
    }
}

pub(super) struct BridgePort {
    socket: UnixDatagram,
    directory: PathBuf,
    own_path: PathBuf,
}

impl BridgePort {
    pub(super) fn bind(directory: &Path, own_mac: [u8; 6]) -> io::Result<Self> {
        std::fs::create_dir_all(directory)?;
        let own_path = directory.join(mac_socket_name(own_mac));
        match std::fs::remove_file(&own_path) {
            Ok(()) => {}
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
        let socket = UnixDatagram::bind(&own_path)?;
        socket.set_nonblocking(true)?;
        Ok(Self {
            socket,
            directory: directory.to_path_buf(),
            own_path,
        })
    }

    /// Forward one guest frame. Returns whether the local gateway must also
    /// receive it (broadcast/multicast or non-peer traffic).
    pub(super) fn forward_from_guest(&self, frame: &[u8]) -> bool {
        let Some(destination) = ethernet_destination(frame) else {
            return true;
        };
        if destination == GATEWAY_MAC.0 {
            return true;
        }
        if is_group_mac(destination) {
            self.flood(frame);
            return true;
        }

        let peer = self.directory.join(mac_socket_name(destination));
        if peer != self.own_path && peer.exists() {
            if let Err(error) = self.socket.send_to(frame, &peer) {
                tracing::debug!(%error, peer = %peer.display(), "Bridge peer send failed");
            }
            return false;
        }

        // Unknown unicast uses normal switch flooding while still allowing the
        // local gateway stack to inspect traffic addressed outside this switch.
        self.flood(frame);
        true
    }

    fn flood(&self, frame: &[u8]) {
        let Ok(entries) = std::fs::read_dir(&self.directory) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path == self.own_path || path.extension().and_then(|v| v.to_str()) != Some("sock") {
                continue;
            }
            let _ = self.socket.send_to(frame, path);
        }
    }

    pub(super) fn drain_to_guest(&self, guest: &UnixDatagram, stats: &NetStats) {
        let mut buf = [0u8; MAX_FRAME];
        for _ in 0..64 {
            match self.socket.recv(&mut buf) {
                Ok(size) => {
                    if guest.send(&buf[..size]).is_ok() {
                        stats.record_rx(size);
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => break,
                Err(error) => {
                    tracing::debug!(%error, "Bridge peer receive failed");
                    break;
                }
            }
        }
    }
}

impl Drop for BridgePort {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.own_path);
    }
}

fn ethernet_destination(frame: &[u8]) -> Option<[u8; 6]> {
    frame.get(..6)?.try_into().ok()
}

fn is_group_mac(mac: [u8; 6]) -> bool {
    mac[0] & 1 == 1
}

fn mac_socket_name(mac: [u8; 6]) -> String {
    format!(
        "{:02x}-{:02x}-{:02x}-{:02x}-{:02x}-{:02x}.sock",
        mac[0], mac[1], mac[2], mac[3], mac[4], mac[5]
    )
}

/// Owned received frame — consumed by smoltcp's interface layer.
pub(super) struct OwnedRxToken(Vec<u8>);

impl smoltcp::phy::RxToken for OwnedRxToken {
    fn consume<R, F>(mut self, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        f(&mut self.0)
    }
}

/// Transmit token — smoltcp writes a frame into `buf`, which we then send.
///
/// The socket must already be connected to the peer so `send()` works without
/// an explicit destination address.
pub(super) struct TxToken {
    socket: UnixDatagram,
    stats: Arc<NetStats>,
}

impl smoltcp::phy::TxToken for TxToken {
    fn consume<R, F>(self, len: usize, f: F) -> R
    where
        F: FnOnce(&mut [u8]) -> R,
    {
        let mut buf = vec![0u8; len];
        let result = f(&mut buf);
        tracing::trace!(
            bytes = len,
            "NetProxy sending ethernet frame to guest/libkrun"
        );
        if let Err(e) = self.socket.send(&buf) {
            tracing::warn!(error = %e, len, "NetProxy: send to libkrun failed");
        } else {
            self.stats.record_rx(len);
        }
        result
    }
}

impl smoltcp::phy::Device for UnixgramDevice {
    type RxToken<'a>
        = OwnedRxToken
    where
        Self: 'a;
    type TxToken<'a>
        = TxToken
    where
        Self: 'a;

    fn receive(&mut self, _ts: Instant) -> Option<(Self::RxToken<'_>, Self::TxToken<'_>)> {
        let frame = self.rx_queue.pop_front()?;
        let tx = TxToken {
            socket: self.socket.try_clone().ok()?,
            stats: Arc::clone(&self.stats),
        };
        Some((OwnedRxToken(frame), tx))
    }

    fn transmit(&mut self, _ts: Instant) -> Option<Self::TxToken<'_>> {
        Some(TxToken {
            socket: self.socket.try_clone().ok()?,
            stats: Arc::clone(&self.stats),
        })
    }

    fn capabilities(&self) -> smoltcp::phy::DeviceCapabilities {
        let mut caps = smoltcp::phy::DeviceCapabilities::default();
        caps.medium = smoltcp::phy::Medium::Ethernet;
        caps.max_transmission_unit = MAX_FRAME;
        caps
    }
}
