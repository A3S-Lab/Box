//! Exec types for host-to-guest command execution.
//!
//! Shared request/response types used by both the guest exec server
//! and the host exec client.

use serde::{Deserialize, Serialize};

/// Default exec timeout: 5 seconds.
pub const DEFAULT_EXEC_TIMEOUT_NS: u64 = 5_000_000_000;

/// Maximum output size per stream (stdout/stderr): 16 MiB.
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// Request to execute a command in the guest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecRequest {
    /// Command and arguments (e.g., ["ls", "-la"]).
    pub cmd: Vec<String>,
    /// Timeout in nanoseconds. 0 means use the default.
    pub timeout_ns: u64,
    /// Additional environment variables (KEY=VALUE pairs).
    #[serde(default)]
    pub env: Vec<String>,
    /// Working directory for the command.
    #[serde(default)]
    pub working_dir: Option<String>,
}

/// Output from an executed command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecOutput {
    /// Captured stdout bytes.
    pub stdout: Vec<u8>,
    /// Captured stderr bytes.
    pub stderr: Vec<u8>,
    /// Process exit code.
    pub exit_code: i32,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exec_request_serialization_roundtrip() {
        let req = ExecRequest {
            cmd: vec!["ls".to_string(), "-la".to_string()],
            timeout_ns: 3_000_000_000,
            env: vec!["FOO=bar".to_string()],
            working_dir: Some("/tmp".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ExecRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.cmd, vec!["ls", "-la"]);
        assert_eq!(parsed.timeout_ns, 3_000_000_000);
        assert_eq!(parsed.env, vec!["FOO=bar"]);
        assert_eq!(parsed.working_dir, Some("/tmp".to_string()));
    }

    #[test]
    fn test_exec_output_serialization_roundtrip() {
        let output = ExecOutput {
            stdout: b"hello\n".to_vec(),
            stderr: b"warning\n".to_vec(),
            exit_code: 0,
        };
        let json = serde_json::to_string(&output).unwrap();
        let parsed: ExecOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.stdout, b"hello\n");
        assert_eq!(parsed.stderr, b"warning\n");
        assert_eq!(parsed.exit_code, 0);
    }

    #[test]
    fn test_exec_output_non_zero_exit() {
        let output = ExecOutput {
            stdout: vec![],
            stderr: b"not found\n".to_vec(),
            exit_code: 127,
        };
        let json = serde_json::to_string(&output).unwrap();
        let parsed: ExecOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.exit_code, 127);
        assert!(parsed.stdout.is_empty());
    }

    #[test]
    fn test_default_timeout_constant() {
        assert_eq!(DEFAULT_EXEC_TIMEOUT_NS, 5_000_000_000);
    }

    #[test]
    fn test_max_output_bytes_constant() {
        assert_eq!(MAX_OUTPUT_BYTES, 16 * 1024 * 1024);
    }

    #[test]
    fn test_exec_request_empty_cmd() {
        let req = ExecRequest {
            cmd: vec![],
            timeout_ns: 0,
            env: vec![],
            working_dir: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ExecRequest = serde_json::from_str(&json).unwrap();
        assert!(parsed.cmd.is_empty());
        assert_eq!(parsed.timeout_ns, 0);
        assert!(parsed.env.is_empty());
        assert!(parsed.working_dir.is_none());
    }

    #[test]
    fn test_exec_request_backward_compatible_deserialization() {
        // Old format without env/working_dir should still parse
        let json = r#"{"cmd":["ls"],"timeout_ns":0}"#;
        let parsed: ExecRequest = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.cmd, vec!["ls"]);
        assert!(parsed.env.is_empty());
        assert!(parsed.working_dir.is_none());
    }

    #[test]
    fn test_exec_output_empty() {
        let output = ExecOutput {
            stdout: vec![],
            stderr: vec![],
            exit_code: 0,
        };
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        assert_eq!(output.exit_code, 0);
    }
}
