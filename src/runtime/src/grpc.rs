//! Host-guest communication clients over Unix socket.
//!
//! - `AgentClient`: Health-checking the guest agent (port 4088).
//! - `ExecClient`: Executing commands in the guest (port 4089).
//!
//! Agent-level operations (sessions, generation, skills) are handled
//! by the a3s-code crate, not the Box runtime.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};
use tokio::net::UnixStream;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::tee::attestation::{AttestationReport, AttestationRequest};

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

/// Client for executing commands in the guest over Unix socket.
///
/// Sends HTTP POST /exec requests with JSON-encoded ExecRequest bodies
/// and parses JSON ExecOutput responses.
#[derive(Debug)]
pub struct ExecClient {
    socket_path: PathBuf,
}

impl ExecClient {
    /// Connect to the exec server via Unix socket.
    ///
    /// Verifies the socket is connectable.
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        let _stream = UnixStream::connect(socket_path).await.map_err(|e| {
            BoxError::ExecError(format!(
                "Failed to connect to exec server at {}: {}",
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

    /// Execute a command in the guest.
    ///
    /// Sends an HTTP POST /exec request over the Unix socket and returns
    /// the captured stdout, stderr, and exit code.
    pub async fn exec_command(
        &self,
        request: &a3s_box_core::exec::ExecRequest,
    ) -> Result<a3s_box_core::exec::ExecOutput> {
        let body = serde_json::to_string(request).map_err(|e| {
            BoxError::ExecError(format!("Failed to serialize exec request: {}", e))
        })?;

        let http_request = format!(
            "POST /exec HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body,
        );

        let mut stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
            BoxError::ExecError(format!(
                "Exec connection failed to {}: {}",
                self.socket_path.display(),
                e,
            ))
        })?;

        stream.write_all(http_request.as_bytes()).await.map_err(|e| {
            BoxError::ExecError(format!("Exec request write failed: {}", e))
        })?;

        // Read full response (up to 32 MiB + headers)
        let mut response = Vec::with_capacity(4096);
        let mut buf = vec![0u8; 65536];
        loop {
            let n = stream.read(&mut buf).await.map_err(|e| {
                BoxError::ExecError(format!("Exec response read failed: {}", e))
            })?;
            if n == 0 {
                break;
            }
            response.extend_from_slice(&buf[..n]);
            // Safety limit: 33 MiB (16 MiB stdout + 16 MiB stderr + headers)
            if response.len() > 33 * 1024 * 1024 {
                break;
            }
        }

        let response_str = String::from_utf8_lossy(&response);

        // Find the JSON body after the HTTP headers
        let body_str = response_str
            .find("\r\n\r\n")
            .map(|pos| &response_str[pos + 4..])
            .ok_or_else(|| {
                BoxError::ExecError("Malformed exec response: no HTTP body".to_string())
            })?;

        let output: a3s_box_core::exec::ExecOutput =
            serde_json::from_str(body_str).map_err(|e| {
                BoxError::ExecError(format!("Failed to parse exec response: {}", e))
            })?;

        Ok(output)
    }
}

/// Client for requesting attestation reports from the guest VM.
///
/// Sends HTTP POST /attest requests over the Unix socket to the guest agent,
/// which calls the SNP_GET_REPORT ioctl and returns the hardware-signed report.
#[derive(Debug)]
pub struct AttestationClient {
    socket_path: PathBuf,
}

impl AttestationClient {
    /// Connect to the guest agent for attestation requests.
    pub async fn connect(socket_path: &Path) -> Result<Self> {
        let _stream = UnixStream::connect(socket_path).await.map_err(|e| {
            BoxError::AttestationError(format!(
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

    /// Request an attestation report from the guest VM.
    ///
    /// The guest agent receives the request, calls `SNP_GET_REPORT` via
    /// `/dev/sev-guest`, and returns the hardware-signed report with
    /// the certificate chain.
    ///
    /// # Arguments
    /// * `request` - Attestation request containing the verifier's nonce
    ///
    /// # Returns
    /// * `Ok(AttestationReport)` - Hardware-signed report with cert chain
    /// * `Err(...)` - If the guest agent is unreachable or SNP is unavailable
    pub async fn get_report(
        &self,
        request: &AttestationRequest,
    ) -> Result<AttestationReport> {
        let body = serde_json::to_string(request).map_err(|e| {
            BoxError::AttestationError(format!("Failed to serialize attestation request: {}", e))
        })?;

        let http_request = format!(
            "POST /attest HTTP/1.1\r\nHost: localhost\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
            body.len(),
            body,
        );

        let mut stream = UnixStream::connect(&self.socket_path).await.map_err(|e| {
            BoxError::AttestationError(format!(
                "Attestation connection failed to {}: {}",
                self.socket_path.display(),
                e,
            ))
        })?;

        stream.write_all(http_request.as_bytes()).await.map_err(|e| {
            BoxError::AttestationError(format!("Attestation request write failed: {}", e))
        })?;

        // Read full response (report + certs can be several KB)
        let mut response = Vec::with_capacity(8192);
        let mut buf = vec![0u8; 8192];
        loop {
            let n = stream.read(&mut buf).await.map_err(|e| {
                BoxError::AttestationError(format!("Attestation response read failed: {}", e))
            })?;
            if n == 0 {
                break;
            }
            response.extend_from_slice(&buf[..n]);
            // Safety limit: 1 MiB (report + full cert chain)
            if response.len() > 1024 * 1024 {
                break;
            }
        }

        let response_str = String::from_utf8_lossy(&response);

        // Find the JSON body after the HTTP headers
        let body_str = response_str
            .find("\r\n\r\n")
            .map(|pos| &response_str[pos + 4..])
            .ok_or_else(|| {
                BoxError::AttestationError(
                    "Malformed attestation response: no HTTP body".to_string(),
                )
            })?;

        // Check for HTTP error status
        if !response_str.starts_with("HTTP/1.1 200") && !response_str.starts_with("HTTP/1.0 200") {
            return Err(BoxError::AttestationError(format!(
                "Attestation request failed: {}",
                body_str.chars().take(200).collect::<String>(),
            )));
        }

        let report: AttestationReport = serde_json::from_str(body_str).map_err(|e| {
            BoxError::AttestationError(format!("Failed to parse attestation response: {}", e))
        })?;

        Ok(report)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_agent_connect_nonexistent_socket() {
        let result = AgentClient::connect(Path::new("/tmp/nonexistent-a3s-test.sock")).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_exec_connect_nonexistent_socket() {
        let result = ExecClient::connect(Path::new("/tmp/nonexistent-a3s-exec-test.sock")).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, BoxError::ExecError(_)));
    }

    #[tokio::test]
    async fn test_attestation_connect_nonexistent_socket() {
        let result =
            AttestationClient::connect(Path::new("/tmp/nonexistent-a3s-attest-test.sock")).await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(matches!(err, BoxError::AttestationError(_)));
    }
}
