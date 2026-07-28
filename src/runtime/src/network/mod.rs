//! Network management for container-to-container communication.
//!
//! Provides `NetworkStore` for persisting network state and
//! platform-specific network backend managers for bridge networking:
//! - Linux: `PasstManager` (passt Unix stream socket)
//! - macOS: `NetProxyManager` (pure-Rust vfkit server, no external binary)

#[cfg(any(target_os = "linux", test))]
mod passt;
mod store;

#[cfg(target_os = "macos")]
pub use a3s_box_netproxy::NetProxyManager;
#[cfg(any(target_os = "linux", test))]
pub use passt::{terminate_passt, PasstManager};
pub use store::NetworkStore;

/// Stable per-user switch directory for one logical bridge network.
#[cfg(unix)]
pub fn bridge_socket_dir(home: &std::path::Path, network_name: &str) -> std::path::PathBuf {
    use sha2::{Digest, Sha256};

    let mut digest = Sha256::new();
    digest.update(home.as_os_str().as_encoded_bytes());
    digest.update([0]);
    digest.update(network_name.as_bytes());
    let key = hex::encode(digest.finalize());
    let uid = unsafe { libc::getuid() };
    let temporary = if cfg!(target_os = "macos") {
        std::path::PathBuf::from("/private/tmp")
    } else {
        std::path::PathBuf::from("/tmp")
    };
    temporary
        .join("a3s-box-switches")
        .join(uid.to_string())
        .join(&key[..24])
}

#[cfg(unix)]
fn cleanup_bridge_socket_dir(home: &std::path::Path, network_name: &str) {
    let directory = bridge_socket_dir(home, network_name);
    match std::fs::remove_dir_all(&directory) {
        Ok(()) => {}
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => tracing::warn!(
            path = %directory.display(),
            %error,
            "Failed to remove bridge switch directory"
        ),
    }
}

/// Platform-agnostic handle to a running network backend process or thread.
pub trait NetworkBackend: Send + Sync {
    /// Path to the Unix socket used to communicate with this backend.
    fn socket_path(&self) -> &std::path::Path;
    /// Stop the backend and clean up the socket.
    fn stop(&mut self);
}

#[cfg(target_os = "macos")]
impl NetworkBackend for NetProxyManager {
    fn socket_path(&self) -> &std::path::Path {
        self.socket_path()
    }

    fn stop(&mut self) {
        self.stop();
    }
}
