//! Guest exec server for executing commands inside the VM.
//!
//! Listens on vsock port 4089 and accepts HTTP POST /exec requests
//! with JSON-encoded ExecRequest bodies. Returns ExecOutput as JSON.

#[cfg(target_os = "linux")]
use std::io::Write;
use std::io::Read;
use std::time::Duration;

use a3s_box_core::exec::{ExecOutput, DEFAULT_EXEC_TIMEOUT_NS, MAX_OUTPUT_BYTES};
use tracing::{info, warn};

/// Vsock port for the exec server.
pub const EXEC_VSOCK_PORT: u32 = 4089;

/// Run the exec server, listening on vsock port 4089.
///
/// On Linux, binds to `AF_VSOCK` with `VMADDR_CID_ANY`.
/// On non-Linux platforms, this is a no-op (development stub).
pub fn run_exec_server() -> Result<(), Box<dyn std::error::Error>> {
    info!("Starting exec server on vsock port {}", EXEC_VSOCK_PORT);

    #[cfg(target_os = "linux")]
    {
        run_vsock_server()?;
    }

    #[cfg(not(target_os = "linux"))]
    {
        info!("Exec server not available on non-Linux platform (development mode)");
    }

    Ok(())
}

/// Linux vsock server implementation.
#[cfg(target_os = "linux")]
fn run_vsock_server() -> Result<(), Box<dyn std::error::Error>> {
    use nix::sys::socket::{
        accept, bind, listen, socket, AddressFamily, Backlog, SockFlag, SockType, VsockAddr,
    };
    use std::os::fd::{FromRawFd, OwnedFd};
    use tracing::error;

    // Create vsock socket
    let sock_fd = socket(
        AddressFamily::Vsock,
        SockType::Stream,
        SockFlag::SOCK_CLOEXEC,
        None,
    )?;

    // Bind to VMADDR_CID_ANY (accept from any CID) on exec port
    let addr = VsockAddr::new(libc::VMADDR_CID_ANY, EXEC_VSOCK_PORT);
    bind(sock_fd.as_raw_fd(), &addr)?;

    // Listen with small backlog (exec is sequential)
    listen(&sock_fd, Backlog::new(4)?)?;

    info!("Exec server listening on vsock port {}", EXEC_VSOCK_PORT);

    // Accept loop
    loop {
        match accept(sock_fd.as_raw_fd()) {
            Ok(client_fd) => {
                // Safety: accept returns a valid fd
                let client = unsafe { OwnedFd::from_raw_fd(client_fd) };
                if let Err(e) = handle_connection(client) {
                    warn!("Failed to handle exec connection: {}", e);
                }
            }
            Err(e) => {
                error!("Accept failed: {}", e);
                std::thread::sleep(Duration::from_millis(100));
            }
        }
    }
}

/// Handle a single connection: read HTTP request, execute command, send response.
#[cfg(target_os = "linux")]
fn handle_connection(fd: std::os::fd::OwnedFd) -> Result<(), Box<dyn std::error::Error>> {
    use std::os::fd::AsRawFd;
    use a3s_box_core::exec::ExecRequest;
    use tracing::debug;

    let raw_fd = fd.as_raw_fd();

    // Wrap in a File for Read/Write
    let mut stream = unsafe { std::fs::File::from_raw_fd(raw_fd) };

    // Read the HTTP request (up to 64 KiB should be plenty)
    let mut buf = vec![0u8; 65536];
    let n = stream.read(&mut buf)?;
    if n == 0 {
        return Ok(());
    }

    let request_str = String::from_utf8_lossy(&buf[..n]);
    debug!("Exec request received ({} bytes)", n);

    // Parse HTTP body (find the blank line separating headers from body)
    let body = match request_str.find("\r\n\r\n") {
        Some(pos) => &request_str[pos + 4..],
        None => {
            send_error_response(&mut stream, 400, "Malformed HTTP request")?;
            // Prevent double-close: forget the fd since stream owns it
            std::mem::forget(fd);
            return Ok(());
        }
    };

    // Parse ExecRequest from JSON body
    let exec_req: ExecRequest = match serde_json::from_str(body) {
        Ok(req) => req,
        Err(e) => {
            send_error_response(&mut stream, 400, &format!("Invalid JSON: {}", e))?;
            std::mem::forget(fd);
            return Ok(());
        }
    };

    // Execute the command
    let output = execute_command(&exec_req.cmd, exec_req.timeout_ns, &exec_req.env, exec_req.working_dir.as_deref());

    // Send HTTP response with JSON body
    let response_body = serde_json::to_string(&output)?;
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        response_body.len(),
        response_body,
    );
    stream.write_all(response.as_bytes())?;

    // Prevent double-close: stream already owns the fd
    std::mem::forget(fd);

    Ok(())
}

/// Send an HTTP error response.
#[cfg(target_os = "linux")]
fn send_error_response(
    stream: &mut impl Write,
    status: u16,
    message: &str,
) -> Result<(), std::io::Error> {
    let body = format!(r#"{{"error":"{}"}}"#, message);
    let response = format!(
        "HTTP/1.1 {} Error\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        status,
        body.len(),
        body,
    );
    stream.write_all(response.as_bytes())
}

/// Execute a command with timeout, environment variables, and working directory.
///
/// Returns ExecOutput with stdout, stderr, and exit code.
fn execute_command(cmd: &[String], timeout_ns: u64, env: &[String], working_dir: Option<&str>) -> ExecOutput {
    if cmd.is_empty() {
        return ExecOutput {
            stdout: vec![],
            stderr: b"Empty command".to_vec(),
            exit_code: 1,
        };
    }

    let timeout_ns = if timeout_ns == 0 {
        DEFAULT_EXEC_TIMEOUT_NS
    } else {
        timeout_ns
    };
    let timeout = Duration::from_nanos(timeout_ns);

    let mut command = std::process::Command::new(&cmd[0]);
    command
        .args(&cmd[1..])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());

    // Apply environment variables (KEY=VALUE format)
    for entry in env {
        if let Some((key, value)) = entry.split_once('=') {
            command.env(key, value);
        }
    }

    // Apply working directory
    if let Some(dir) = working_dir {
        command.current_dir(dir);
    }

    let mut child = match command.spawn()
    {
        Ok(child) => child,
        Err(e) => {
            return ExecOutput {
                stdout: vec![],
                stderr: format!("Failed to spawn command '{}': {}", cmd[0], e).into_bytes(),
                exit_code: 127,
            };
        }
    };

    // Wait with timeout using a polling loop
    let start = std::time::Instant::now();
    let poll_interval = Duration::from_millis(50);

    loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                // Process exited
                let mut stdout = Vec::new();
                let mut stderr = Vec::new();
                if let Some(ref mut out) = child.stdout {
                    let _ = out.read_to_end(&mut stdout);
                }
                if let Some(ref mut err) = child.stderr {
                    let _ = err.read_to_end(&mut stderr);
                }

                return ExecOutput {
                    stdout: truncate_output(stdout),
                    stderr: truncate_output(stderr),
                    exit_code: status.code().unwrap_or(1),
                };
            }
            Ok(None) => {
                // Still running
                if start.elapsed() >= timeout {
                    // Timeout — kill the process
                    warn!("Exec command timed out after {:?}, killing", timeout);
                    let _ = child.kill();
                    let _ = child.wait();

                    let mut stdout = Vec::new();
                    let mut stderr = Vec::new();
                    if let Some(ref mut out) = child.stdout {
                        let _ = out.read_to_end(&mut stdout);
                    }
                    if let Some(ref mut err) = child.stderr {
                        let _ = err.read_to_end(&mut stderr);
                    }

                    stderr.extend_from_slice(b"\nProcess killed: timeout exceeded");

                    return ExecOutput {
                        stdout: truncate_output(stdout),
                        stderr: truncate_output(stderr),
                        exit_code: 137, // SIGKILL
                    };
                }
                std::thread::sleep(poll_interval);
            }
            Err(e) => {
                return ExecOutput {
                    stdout: vec![],
                    stderr: format!("Failed to wait for command: {}", e).into_bytes(),
                    exit_code: 1,
                };
            }
        }
    }
}

/// Truncate output to MAX_OUTPUT_BYTES if it exceeds the limit.
fn truncate_output(mut data: Vec<u8>) -> Vec<u8> {
    if data.len() > MAX_OUTPUT_BYTES {
        data.truncate(MAX_OUTPUT_BYTES);
    }
    data
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_truncate_output_within_limit() {
        let data = vec![0u8; 100];
        let result = truncate_output(data.clone());
        assert_eq!(result.len(), 100);
    }

    #[test]
    fn test_truncate_output_exceeds_limit() {
        let data = vec![0u8; MAX_OUTPUT_BYTES + 1000];
        let result = truncate_output(data);
        assert_eq!(result.len(), MAX_OUTPUT_BYTES);
    }

    #[test]
    fn test_truncate_output_at_limit() {
        let data = vec![0u8; MAX_OUTPUT_BYTES];
        let result = truncate_output(data);
        assert_eq!(result.len(), MAX_OUTPUT_BYTES);
    }

    #[test]
    fn test_truncate_output_empty() {
        let data = vec![];
        let result = truncate_output(data);
        assert!(result.is_empty());
    }

    #[test]
    fn test_execute_command_echo() {
        let output = execute_command(&["echo".to_string(), "hello".to_string()], 0, &[], None);
        assert_eq!(output.exit_code, 0);
        assert_eq!(String::from_utf8_lossy(&output.stdout).trim(), "hello");
        assert!(output.stderr.is_empty());
    }

    #[test]
    fn test_execute_command_nonexistent() {
        let output = execute_command(
            &["this_command_does_not_exist_a3s_test".to_string()],
            0,
            &[],
            None,
        );
        assert_ne!(output.exit_code, 0);
        assert!(!output.stderr.is_empty());
    }

    #[test]
    fn test_execute_command_empty() {
        let output = execute_command(&[], 0, &[], None);
        assert_eq!(output.exit_code, 1);
        assert_eq!(output.stderr, b"Empty command");
    }

    #[test]
    fn test_execute_command_non_zero_exit() {
        let output = execute_command(
            &["sh".to_string(), "-c".to_string(), "exit 42".to_string()],
            0,
            &[],
            None,
        );
        assert_eq!(output.exit_code, 42);
    }

    #[test]
    fn test_execute_command_stderr_output() {
        let output = execute_command(
            &[
                "sh".to_string(),
                "-c".to_string(),
                "echo error >&2".to_string(),
            ],
            0,
            &[],
            None,
        );
        assert_eq!(output.exit_code, 0);
        assert!(String::from_utf8_lossy(&output.stderr).contains("error"));
    }

    #[test]
    fn test_execute_command_with_env() {
        let output = execute_command(
            &["sh".to_string(), "-c".to_string(), "echo $TEST_VAR".to_string()],
            0,
            &["TEST_VAR=hello_from_env".to_string()],
            None,
        );
        assert_eq!(output.exit_code, 0);
        assert_eq!(
            String::from_utf8_lossy(&output.stdout).trim(),
            "hello_from_env"
        );
    }

    #[test]
    fn test_execute_command_with_working_dir() {
        let output = execute_command(
            &["pwd".to_string()],
            0,
            &[],
            Some("/tmp"),
        );
        assert_eq!(output.exit_code, 0);
        // On macOS /tmp is a symlink to /private/tmp
        let pwd = String::from_utf8_lossy(&output.stdout).trim().to_string();
        assert!(pwd == "/tmp" || pwd == "/private/tmp");
    }

    #[test]
    fn test_exec_vsock_port_constant() {
        assert_eq!(EXEC_VSOCK_PORT, 4089);
    }
}
