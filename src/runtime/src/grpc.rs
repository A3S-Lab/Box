//! gRPC client for host-guest communication over Unix socket.
//!
//! Provides a minimal client for health-checking the guest agent.
//! Agent-level operations (sessions, generation, skills) are handled
//! by the a3s-code crate, not the Box runtime.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use tokio::net::UnixStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

/// Client for communicating with the guest agent over Unix socket.
///
/// This client only supports health checking. Agent-level operations
/// (sessions, generation, skills) belong in the a3s-code crate.
pub struct AgentClient {
    socket_path: PathBuf,
}

impl AgentClient {
    /// Connect to the guest agent via Unix socket.
    ///
    /// Verifies the socket is connectable but does not perform a health check.
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        // Verify we can connect to the socket
        let _stream = UnixStream::connect(socket_path).await.map_err(|e| {
            BoxError::Other(format!(
                "Failed to connect to agent at {}: {}",
                socket_path.display(),
                e,
            ))
        })?;

        Ok(Self {
            socket_path: socket_path.to_path_buf(),
        })
    }

    /// Get the socket path this client is connected to.
    pub fn socket_path(&self) -> &Path {
        &self.socket_path
    }

    /// Perform a health check on the guest agent.
    ///
    /// Connects to the Unix socket and sends a minimal HTTP request.
    /// Returns `true` if the agent responds, `false` otherwise.
    pub async fn health_check(&self) -> Result<bool> {
        let mut stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
            BoxError::Other(format!(
                "Health check failed: cannot connect to {}: {}",
                self.socket_path.display(),
                e,
            ))
        })?;

        // Send a minimal HTTP/1.1 health check request.
        // The guest agent exposes a /healthz endpoint for this purpose.
        let request = b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n";
        stream.write_all(request).await.map_err(|e| {
            BoxError::Other(format!("Health check write failed: {}", e))
        })?;

        let mut response = vec![0u8; 1024];
        let n = stream.read(&mut response).await.map_err(|e| {
            BoxError::Other(format!("Health check read failed: {}", e))
        })?;

        if n == 0 {
            return Ok(false);
        }

        // Check for HTTP 200 response
        let response_str = String::from_utf8_lossy(&response[..n]);
        Ok(response_str.contains("200"))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_connect_nonexistent_socket() {
        let result = AgentClient::connect(Path::new("/tmp/nonexistent-a3s-test.sock")).await;
        assert!(result.is_err());
    }
}
