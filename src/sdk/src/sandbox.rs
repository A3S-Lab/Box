//! Sandbox — a running MicroVM instance.

use std::path::PathBuf;

use a3s_box_core::error::Result;
use a3s_box_core::exec::{ExecOutput, ExecRequest};
use a3s_box_runtime::{ExecClient, PtyClient, VmManager};

/// Result of executing a command in a sandbox.
#[derive(Debug, Clone)]
pub struct ExecResult {
    /// Standard output (lossy UTF-8 conversion from raw bytes).
    pub stdout: String,
    /// Standard error (lossy UTF-8 conversion from raw bytes).
    pub stderr: String,
    /// Exit code (0 = success).
    pub exit_code: i32,
}

impl From<ExecOutput> for ExecResult {
    fn from(output: ExecOutput) -> Self {
        Self {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.exit_code,
        }
    }
}

/// A running MicroVM sandbox.
///
/// Provides methods to execute commands, open PTY sessions,
/// and manage the sandbox lifecycle.
pub struct Sandbox {
    /// Unique sandbox identifier.
    id: String,
    /// Human-readable name.
    name: String,
    /// VM manager (owns the VM lifecycle).
    vm: VmManager,
    /// Path to the exec Unix socket.
    exec_socket: PathBuf,
    /// Path to the PTY Unix socket.
    pty_socket: PathBuf,
}

impl Sandbox {
    /// Create a new Sandbox handle (called by BoxSdk::create).
    pub(crate) fn new(
        id: String,
        name: String,
        vm: VmManager,
        exec_socket: PathBuf,
        pty_socket: PathBuf,
    ) -> Self {
        Self {
            id,
            name,
            vm,
            exec_socket,
            pty_socket,
        }
    }

    /// Get the sandbox ID.
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Get the sandbox name.
    pub fn name(&self) -> &str {
        &self.name
    }

    /// Get the current sandbox state.
    pub async fn state(&self) -> a3s_box_runtime::BoxState {
        self.vm.state().await
    }

    /// Execute a command in the sandbox.
    ///
    /// # Arguments
    /// * `cmd` - Command to execute
    /// * `args` - Command arguments
    ///
    /// # Returns
    /// * `ExecResult` with stdout, stderr, and exit code
    pub async fn exec(&self, cmd: &str, args: &[&str]) -> Result<ExecResult> {
        let mut cmd_parts = vec![cmd.to_string()];
        cmd_parts.extend(args.iter().map(|a| a.to_string()));

        let request = ExecRequest {
            cmd: cmd_parts,
            timeout_ns: 0,
            env: Vec::new(),
            working_dir: None,
            stdin: None,
            user: None,
        };

        let client = ExecClient::connect(&self.exec_socket).await?;
        let output = client.exec_command(&request).await?;
        Ok(ExecResult::from(output))
    }

    /// Execute a command with environment variables and working directory.
    pub async fn exec_with_options(
        &self,
        cmd: Vec<String>,
        env: Vec<String>,
        working_dir: Option<String>,
        stdin: Option<Vec<u8>>,
    ) -> Result<ExecResult> {
        let request = ExecRequest {
            cmd,
            timeout_ns: 0,
            env,
            working_dir,
            stdin,
            user: None,
        };

        let client = ExecClient::connect(&self.exec_socket).await?;
        let output = client.exec_command(&request).await?;
        Ok(ExecResult::from(output))
    }

    /// Open an interactive PTY session.
    ///
    /// Returns a `PtyClient` for bidirectional terminal I/O.
    pub async fn pty(&self, shell: &str, cols: u16, rows: u16) -> Result<PtyClient> {
        let mut client = PtyClient::connect(&self.pty_socket).await?;

        let request = a3s_box_core::pty::PtyRequest {
            cmd: vec![shell.to_string()],
            env: Vec::new(),
            working_dir: None,
            user: None,
            cols,
            rows,
        };
        client.send_request(&request).await?;

        Ok(client)
    }

    /// Stop the sandbox and release resources.
    pub async fn stop(mut self) -> Result<()> {
        tracing::info!(sandbox_id = %self.id, "Stopping sandbox");
        self.vm.destroy().await
    }

    /// Check if the sandbox is running.
    pub async fn is_running(&self) -> bool {
        matches!(
            self.vm.state().await,
            a3s_box_runtime::BoxState::Ready | a3s_box_runtime::BoxState::Busy
        )
    }
}

impl std::fmt::Debug for Sandbox {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Sandbox")
            .field("id", &self.id)
            .field("name", &self.name)
            .finish()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_exec_result_from_exec_output() {
        let output = ExecOutput {
            stdout: b"hello\n".to_vec(),
            stderr: Vec::new(),
            exit_code: 0,
        };
        let result = ExecResult::from(output);
        assert_eq!(result.stdout, "hello\n");
        assert_eq!(result.stderr, "");
        assert_eq!(result.exit_code, 0);
    }

    #[test]
    fn test_exec_result_nonzero_exit() {
        let output = ExecOutput {
            stdout: Vec::new(),
            stderr: b"not found\n".to_vec(),
            exit_code: 127,
        };
        let result = ExecResult::from(output);
        assert_eq!(result.exit_code, 127);
        assert_eq!(result.stderr, "not found\n");
    }
}
