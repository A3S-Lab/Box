//! Exec types for host-to-guest command execution.
//!
//! Shared request/response types used by both the guest exec server
//! and the host exec client.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Vsock port for the exec server.
pub const EXEC_VSOCK_PORT: u32 = a3s_transport::ports::EXEC_SERVER;

/// Vsock port for the Windows host-port forward control channel.
pub const PORT_FWD_VSOCK_PORT: u32 = 4093;

/// Host-control frame that asks guest init to signal the container main process.
///
/// Windows shares the existing long-lived port-forward channel because WHPX
/// named-pipe mappings are guest-initiated. The payload is one big-endian `i32`
/// Linux signal number.
pub const WINDOWS_CONTROL_SIGNAL_FRAME: u8 = 5;

/// Host-only request file watched by the Windows control worker.
pub const WINDOWS_STOP_REQUEST_FILE: &str = "stop.signal";

/// Temporary sibling used to publish a Windows stop request atomically.
pub const WINDOWS_STOP_REQUEST_TEMP_FILE: &str = "stop.signal.tmp";

/// Default exec timeout: 5 seconds.
pub const DEFAULT_EXEC_TIMEOUT_NS: u64 = 5_000_000_000;

/// Maximum buffered streaming output size per stream (stdout/stderr): 16 MiB.
pub const MAX_OUTPUT_BYTES: usize = 16 * 1024 * 1024;

/// Maximum captured one-shot output size per stream: 1 MiB.
///
/// One-shot responses retain the legacy JSON `Vec<u8>` representation, whose
/// worst-case encoding uses four bytes per input byte. Bounding both streams
/// at 1 MiB guarantees the complete response fits the transport's 16 MiB
/// frame without breaking older host or guest binaries.
pub const MAX_ONE_SHOT_OUTPUT_BYTES: usize = 1024 * 1024;

/// Frame type byte for streaming exec chunks.
pub const FRAME_EXEC_CHUNK: u8 = 0x01;

/// Frame type byte for streaming exec exit.
pub const FRAME_EXEC_EXIT: u8 = 0x02;

/// Request to execute a command in the guest.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecRequest {
    /// Optional idempotency key for one-shot execution.
    ///
    /// A guest that supports replay must execute the same keyed request at
    /// most once while the result remains in its bounded replay cache. The key
    /// is deliberately optional for wire compatibility with older clients.
    /// Streaming execution cannot provide an exact one-shot result replay and
    /// therefore must not set this field.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
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
    /// Optional guest-visible rootfs path to chroot into before executing.
    #[serde(default)]
    pub rootfs: Option<String>,
    /// Optional stdin data to pipe to the command.
    #[serde(default)]
    pub stdin: Option<Vec<u8>>,
    /// Keep stdin open for subsequent streaming data frames.
    #[serde(default)]
    pub stdin_streaming: bool,
    /// User to run the command as (supported: "root", "1000", "1000:1000").
    #[serde(default)]
    pub user: Option<String>,
    /// Enable streaming mode (receive output chunks as they arrive).
    #[serde(default)]
    pub streaming: bool,
}

/// Output from an executed command.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ExecOutput {
    /// Captured stdout bytes.
    pub stdout: Vec<u8>,
    /// Captured stderr bytes.
    pub stderr: Vec<u8>,
    /// Process exit code.
    pub exit_code: i32,
    /// Whether either captured stream exceeded its bound and was truncated.
    /// Defaults to `false` when reading responses from older guest binaries.
    #[serde(default)]
    pub truncated: bool,
}

/// Which output stream a chunk belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum StreamType {
    /// Standard output.
    Stdout,
    /// Standard error.
    Stderr,
}

impl std::fmt::Display for StreamType {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StreamType::Stdout => write!(f, "stdout"),
            StreamType::Stderr => write!(f, "stderr"),
        }
    }
}

/// A chunk of streaming output from a running command.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecChunk {
    /// Which stream this chunk belongs to.
    pub stream: StreamType,
    /// Raw output bytes.
    pub data: Vec<u8>,
}

/// Final exit notification from a streaming exec.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecExit {
    /// Process exit code.
    pub exit_code: i32,
    /// Set when the process (or its memory cgroup) was killed by the
    /// out-of-memory killer. Carried back so the CRI can report the container
    /// exit reason as `OOMKilled`. Defaults to `false` for wire compatibility.
    #[serde(default)]
    pub oom_killed: bool,
}

/// A streaming exec event — a chunk of output, a flush acknowledgement, or the
/// final exit.
#[derive(Debug, Clone)]
pub enum ExecEvent {
    /// A chunk of stdout or stderr data.
    Chunk(ExecChunk),
    /// Acknowledgement of a flush request: every output chunk the guest had
    /// buffered when it received the flush has been sent ahead of this marker.
    /// Used to establish a definitive pre/post boundary for log rotation
    /// (`ReopenContainerLog`) without racing in-flight output.
    FlushAck,
    /// The command has exited.
    Exit(ExecExit),
}

/// Metrics collected during command execution.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ExecMetrics {
    /// Wall-clock duration in milliseconds.
    pub duration_ms: u64,
    /// Peak memory usage in bytes (if available).
    #[serde(default)]
    pub peak_memory_bytes: Option<u64>,
    /// Total stdout bytes produced.
    pub stdout_bytes: u64,
    /// Total stderr bytes produced.
    pub stderr_bytes: u64,
}

/// File transfer request for upload/download between host and guest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileRequest {
    /// Operation type.
    pub op: FileOp,
    /// Path inside the guest.
    pub guest_path: String,
    /// File content (for upload only, base64-encoded).
    #[serde(default)]
    pub data: Option<String>,
    /// User that owns newly created files and parent directories.
    #[serde(default)]
    pub user: Option<String>,
}

/// File transfer operation type.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FileOp {
    /// Upload a file from host to guest.
    Upload,
    /// Download a file from guest to host.
    Download,
}

/// File transfer response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileResponse {
    /// Whether the operation succeeded.
    pub success: bool,
    /// File content (for download only, base64-encoded).
    #[serde(default)]
    pub data: Option<String>,
    /// File size in bytes.
    #[serde(default)]
    pub size: u64,
    /// Error message if the operation failed.
    #[serde(default)]
    pub error: Option<String>,
}

/// Metadata operation performed inside a managed workload filesystem.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilesystemOp {
    /// Inspect one path without modifying it.
    Stat,
    /// Recursively create one directory.
    MakeDir,
    /// Rename one entry, creating destination parents when necessary.
    Move,
    /// List descendants to a bounded depth.
    ListDir,
    /// Recursively remove one entry.
    Remove,
}

/// Generation-fenced filesystem request sent to the workload guest.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesystemRequest {
    /// Requested operation.
    pub op: FilesystemOp,
    /// Source or primary path.
    pub path: String,
    /// Destination path for [`FilesystemOp::Move`].
    #[serde(default)]
    pub destination: Option<String>,
    /// Requested descendant depth for [`FilesystemOp::ListDir`].
    #[serde(default)]
    pub depth: u32,
    /// Guest user used for home expansion and ownership.
    #[serde(default)]
    pub user: Option<String>,
}

/// Entry type returned by a workload filesystem operation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FilesystemEntryKind {
    /// Entry type could not be represented by the pinned contract.
    Unspecified,
    /// Regular file, or a symlink whose target is a regular file.
    File,
    /// Directory, or a symlink whose target is a directory.
    Directory,
}

/// Portable guest metadata used by compatibility protocol adapters.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FilesystemEntry {
    pub name: String,
    pub kind: FilesystemEntryKind,
    pub path: String,
    pub size: i64,
    pub mode: u32,
    pub permissions: String,
    pub owner: String,
    pub group: String,
    pub modified_seconds: i64,
    pub modified_nanos: i32,
    #[serde(default)]
    pub symlink_target: Option<String>,
    #[serde(default)]
    pub metadata: BTreeMap<String, String>,
}

/// Result of one workload filesystem metadata or mutation operation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FilesystemResponse {
    pub success: bool,
    #[serde(default)]
    pub entry: Option<FilesystemEntry>,
    #[serde(default)]
    pub entries: Vec<FilesystemEntry>,
    #[serde(default)]
    pub error: Option<String>,
}

/// Versioned non-exec request sent over the guest execution session.
///
/// Exec requests predate this envelope and remain bare JSON for wire
/// compatibility. File requests use an explicit discriminator so the guest
/// never attempts to deserialize them as commands.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "request_type", content = "request", rename_all = "snake_case")]
pub enum GuestSessionRequest {
    /// Upload or download one file.
    File(FileRequest),
    /// Inspect or mutate workload filesystem metadata.
    Filesystem(FilesystemRequest),
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exec_request_serialization_roundtrip() {
        let req = ExecRequest {
            request_id: Some("exec-1".to_string()),
            cmd: vec!["ls".to_string(), "-la".to_string()],
            timeout_ns: 3_000_000_000,
            env: vec!["FOO=bar".to_string()],
            working_dir: Some("/tmp".to_string()),
            rootfs: Some("/run/a3s/cri/rootfs/sb/c/rootfs".to_string()),
            stdin: None,
            stdin_streaming: false,
            user: None,
            streaming: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ExecRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.cmd, vec!["ls", "-la"]);
        assert_eq!(parsed.request_id.as_deref(), Some("exec-1"));
        assert_eq!(parsed.timeout_ns, 3_000_000_000);
        assert_eq!(parsed.env, vec!["FOO=bar"]);
        assert_eq!(parsed.working_dir, Some("/tmp".to_string()));
        assert_eq!(
            parsed.rootfs,
            Some("/run/a3s/cri/rootfs/sb/c/rootfs".to_string())
        );
        assert!(parsed.stdin.is_none());
        assert!(!parsed.stdin_streaming);
        assert!(parsed.user.is_none());
        assert!(!parsed.streaming);
    }

    #[test]
    fn test_exec_request_streaming_flag() {
        let req = ExecRequest {
            request_id: None,
            cmd: vec!["tail".to_string(), "-f".to_string()],
            timeout_ns: 0,
            env: vec![],
            working_dir: None,
            rootfs: None,
            stdin: None,
            stdin_streaming: false,
            user: None,
            streaming: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ExecRequest = serde_json::from_str(&json).unwrap();
        assert!(parsed.streaming);
        assert!(!parsed.stdin_streaming);
    }

    #[test]
    fn test_exec_request_stdin_streaming_flag() {
        let req = ExecRequest {
            request_id: None,
            cmd: vec!["cat".to_string()],
            timeout_ns: 0,
            env: vec![],
            working_dir: None,
            rootfs: None,
            stdin: None,
            stdin_streaming: true,
            user: None,
            streaming: true,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ExecRequest = serde_json::from_str(&json).unwrap();
        assert!(parsed.stdin_streaming);
    }

    #[test]
    fn test_exec_output_serialization_roundtrip() {
        let output = ExecOutput {
            stdout: b"hello\n".to_vec(),
            stderr: b"warning\n".to_vec(),
            exit_code: 0,
            truncated: true,
        };
        let json = serde_json::to_string(&output).unwrap();
        let parsed: ExecOutput = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.stdout, b"hello\n");
        assert_eq!(parsed.stderr, b"warning\n");
        assert_eq!(parsed.exit_code, 0);
        assert!(parsed.truncated);
    }

    #[test]
    fn test_exec_output_non_zero_exit() {
        let output = ExecOutput {
            stdout: vec![],
            stderr: b"not found\n".to_vec(),
            exit_code: 127,
            truncated: false,
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
        assert_eq!(MAX_ONE_SHOT_OUTPUT_BYTES, 1024 * 1024);
    }

    #[test]
    fn maximum_one_shot_output_fits_one_transport_frame() {
        let output = ExecOutput {
            stdout: vec![u8::MAX; MAX_ONE_SHOT_OUTPUT_BYTES],
            stderr: vec![u8::MAX; MAX_ONE_SHOT_OUTPUT_BYTES],
            exit_code: i32::MIN,
            truncated: true,
        };
        let encoded = serde_json::to_vec(&output).unwrap();
        assert!(encoded.len() <= a3s_transport::MAX_PAYLOAD_SIZE as usize);
    }

    #[test]
    fn test_exec_request_empty_cmd() {
        let req = ExecRequest {
            request_id: None,
            cmd: vec![],
            timeout_ns: 0,
            env: vec![],
            working_dir: None,
            rootfs: None,
            stdin: None,
            stdin_streaming: false,
            user: None,
            streaming: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ExecRequest = serde_json::from_str(&json).unwrap();
        assert!(parsed.cmd.is_empty());
        assert!(parsed.request_id.is_none());
        assert_eq!(parsed.timeout_ns, 0);
        assert!(parsed.env.is_empty());
        assert!(parsed.working_dir.is_none());
        assert!(parsed.rootfs.is_none());
        assert!(!parsed.stdin_streaming);
        assert!(parsed.user.is_none());
    }

    #[test]
    fn test_exec_request_backward_compatible_deserialization() {
        // Old format without rootfs or streaming fields should still parse.
        let json = r#"{"cmd":["ls"],"timeout_ns":0}"#;
        let parsed: ExecRequest = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.cmd, vec!["ls"]);
        assert!(parsed.request_id.is_none());
        assert!(parsed.env.is_empty());
        assert!(parsed.working_dir.is_none());
        assert!(parsed.rootfs.is_none());
        assert!(parsed.stdin.is_none());
        assert!(!parsed.stdin_streaming);
        assert!(parsed.user.is_none());
        assert!(!parsed.streaming);
    }

    #[test]
    fn test_exec_request_with_stdin() {
        let req = ExecRequest {
            request_id: None,
            cmd: vec!["sh".to_string()],
            timeout_ns: 0,
            env: vec![],
            working_dir: None,
            rootfs: None,
            stdin: Some(b"echo hello\n".to_vec()),
            stdin_streaming: false,
            user: None,
            streaming: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ExecRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.stdin, Some(b"echo hello\n".to_vec()));
        assert!(!parsed.stdin_streaming);
    }

    #[test]
    fn test_exec_request_with_user() {
        let req = ExecRequest {
            request_id: None,
            cmd: vec!["whoami".to_string()],
            timeout_ns: 0,
            env: vec![],
            working_dir: None,
            rootfs: None,
            stdin: None,
            stdin_streaming: false,
            user: Some("root".to_string()),
            streaming: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ExecRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.user, Some("root".to_string()));
    }

    #[test]
    fn test_exec_request_with_user_uid_gid() {
        let req = ExecRequest {
            request_id: None,
            cmd: vec!["id".to_string()],
            timeout_ns: 0,
            env: vec![],
            working_dir: None,
            rootfs: None,
            stdin: None,
            stdin_streaming: false,
            user: Some("1000:1000".to_string()),
            streaming: false,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: ExecRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.user, Some("1000:1000".to_string()));
    }

    #[test]
    fn test_exec_output_empty() {
        let output = ExecOutput {
            stdout: vec![],
            stderr: vec![],
            exit_code: 0,
            truncated: false,
        };
        assert!(output.stdout.is_empty());
        assert!(output.stderr.is_empty());
        assert_eq!(output.exit_code, 0);
        assert!(!output.truncated);
    }

    #[test]
    fn test_exec_output_backward_compatible_deserialization() {
        let parsed: ExecOutput =
            serde_json::from_str(r#"{"stdout":[],"stderr":[],"exit_code":0}"#).unwrap();
        assert!(!parsed.truncated);
    }

    // --- Streaming types ---

    #[test]
    fn test_stream_type_display() {
        assert_eq!(StreamType::Stdout.to_string(), "stdout");
        assert_eq!(StreamType::Stderr.to_string(), "stderr");
    }

    #[test]
    fn test_exec_chunk_serde_roundtrip() {
        let chunk = ExecChunk {
            stream: StreamType::Stdout,
            data: b"hello world\n".to_vec(),
        };
        let json = serde_json::to_string(&chunk).unwrap();
        let parsed: ExecChunk = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.stream, StreamType::Stdout);
        assert_eq!(parsed.data, b"hello world\n");
    }

    #[test]
    fn test_exec_chunk_stderr() {
        let chunk = ExecChunk {
            stream: StreamType::Stderr,
            data: b"error: not found\n".to_vec(),
        };
        let json = serde_json::to_string(&chunk).unwrap();
        let parsed: ExecChunk = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.stream, StreamType::Stderr);
    }

    #[test]
    fn test_exec_exit_serde_roundtrip() {
        let exit = ExecExit {
            exit_code: 42,
            oom_killed: false,
        };
        let json = serde_json::to_string(&exit).unwrap();
        let parsed: ExecExit = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.exit_code, 42);
    }

    #[test]
    fn test_exec_metrics_default() {
        let m = ExecMetrics::default();
        assert_eq!(m.duration_ms, 0);
        assert!(m.peak_memory_bytes.is_none());
        assert_eq!(m.stdout_bytes, 0);
        assert_eq!(m.stderr_bytes, 0);
    }

    #[test]
    fn test_exec_metrics_serde_roundtrip() {
        let m = ExecMetrics {
            duration_ms: 1234,
            peak_memory_bytes: Some(65536),
            stdout_bytes: 100,
            stderr_bytes: 50,
        };
        let json = serde_json::to_string(&m).unwrap();
        let parsed: ExecMetrics = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.duration_ms, 1234);
        assert_eq!(parsed.peak_memory_bytes, Some(65536));
        assert_eq!(parsed.stdout_bytes, 100);
        assert_eq!(parsed.stderr_bytes, 50);
    }

    // --- File transfer types ---

    #[test]
    fn test_file_request_upload() {
        let req = FileRequest {
            op: FileOp::Upload,
            guest_path: "/tmp/test.txt".to_string(),
            data: Some("aGVsbG8=".to_string()),
            user: Some("1000:1000".to_string()),
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: FileRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.op, FileOp::Upload);
        assert_eq!(parsed.guest_path, "/tmp/test.txt");
        assert_eq!(parsed.data.as_deref(), Some("aGVsbG8="));
    }

    #[test]
    fn test_file_request_download() {
        let req = FileRequest {
            op: FileOp::Download,
            guest_path: "/etc/hostname".to_string(),
            data: None,
            user: None,
        };
        let json = serde_json::to_string(&req).unwrap();
        let parsed: FileRequest = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.op, FileOp::Download);
        assert!(parsed.data.is_none());
    }

    #[test]
    fn test_file_response_success() {
        let resp = FileResponse {
            success: true,
            data: Some("Y29udGVudA==".to_string()),
            size: 7,
            error: None,
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: FileResponse = serde_json::from_str(&json).unwrap();
        assert!(parsed.success);
        assert_eq!(parsed.size, 7);
        assert!(parsed.error.is_none());
    }

    #[test]
    fn test_file_response_error() {
        let resp = FileResponse {
            success: false,
            data: None,
            size: 0,
            error: Some("file not found".to_string()),
        };
        let json = serde_json::to_string(&resp).unwrap();
        let parsed: FileResponse = serde_json::from_str(&json).unwrap();
        assert!(!parsed.success);
        assert_eq!(parsed.error.as_deref(), Some("file not found"));
    }

    #[test]
    fn file_session_request_has_an_unambiguous_wire_discriminator() {
        let request = GuestSessionRequest::File(FileRequest {
            op: FileOp::Download,
            guest_path: "/tmp/data.bin".to_string(),
            data: None,
            user: None,
        });

        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["request_type"], "file");
        assert_eq!(value["request"]["op"], "Download");
        assert_eq!(value["request"]["guest_path"], "/tmp/data.bin");
        assert!(serde_json::from_value::<GuestSessionRequest>(value).is_ok());
    }

    #[test]
    fn filesystem_session_request_has_an_unambiguous_wire_discriminator() {
        let request = GuestSessionRequest::Filesystem(FilesystemRequest {
            op: FilesystemOp::Move,
            path: "~/before".to_string(),
            destination: Some("~/after".to_string()),
            depth: 0,
            user: Some("user".to_string()),
        });

        let value = serde_json::to_value(&request).unwrap();
        assert_eq!(value["request_type"], "filesystem");
        assert_eq!(value["request"]["op"], "Move");
        assert_eq!(value["request"]["destination"], "~/after");
        assert!(serde_json::from_value::<GuestSessionRequest>(value).is_ok());
    }

    #[test]
    fn test_frame_exec_constants() {
        assert_eq!(FRAME_EXEC_CHUNK, 0x01);
        assert_eq!(FRAME_EXEC_EXIT, 0x02);
    }
}
