//! Guest agent connection client.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use tokio::net::UnixStream;

/// Client for communicating with the guest agent over Unix socket.
///
/// This client only supports connection testing. Agent-level operations
/// (sessions, generation, skills) belong in the a3s-code crate.
/// Health checking is done via `ExecClient::heartbeat()` on the exec server.
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
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::UnixListener;

    #[tokio::test]
    async fn test_agent_connect_nonexistent_socket() {
        let result = AgentClient::connect(Path::new("/tmp/nonexistent-a3s-test.sock")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_agent_connect_and_socket_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let sock_path = tmp.path().join("agent.sock");
        let _listener = UnixListener::bind(&sock_path).unwrap();

        let client = AgentClient::connect(&sock_path).await.unwrap();
        assert_eq!(client.socket_path(), sock_path);
    }
}
