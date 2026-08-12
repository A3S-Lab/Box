//! Network management for container-to-container communication.
//!
//! Provides `NetworkStore` for persisting network state and
//! platform-specific network backend managers for virtio-net networking:
//! - Linux: `PasstManager` (passt Unix stream socket)
//! - Unix: `NetProxyManager` (inherited pure-Rust userspace gateway)
//!
//! Linux bridge networking uses passt. Restricted MicroVM egress on Linux and
//! built-in macOS networking use the inherited netproxy.

#[cfg(any(target_os = "linux", test))]
mod passt;
mod store;

#[cfg(unix)]
pub use a3s_box_netproxy::NetProxyManager;
#[cfg(any(target_os = "linux", test))]
pub use passt::{terminate_passt, PasstManager};
pub use store::NetworkStore;

/// Platform-agnostic handle to a running network backend process or thread.
pub trait NetworkBackend: Send + Sync {
    /// Path to the Unix socket used to communicate with this backend.
    fn socket_path(&self) -> &std::path::Path;
    /// Stop the backend and clean up the socket.
    fn stop(&mut self);
}

#[cfg(unix)]
impl NetworkBackend for NetProxyManager {
    fn socket_path(&self) -> &std::path::Path {
        self.socket_path()
    }

    fn stop(&mut self) {
        self.stop();
    }
}
