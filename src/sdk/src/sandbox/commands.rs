use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use a3s_box_core::ExecRequest;

use super::SandboxInner;
use crate::{ClientError, Result};

/// A shell command or an explicit argument vector.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SandboxCommand {
    Shell(String),
    Argv(Vec<String>),
}

impl SandboxCommand {
    pub fn shell(command: impl Into<String>) -> Self {
        Self::Shell(command.into())
    }

    pub fn argv(command: impl IntoIterator<Item = impl Into<String>>) -> Self {
        Self::Argv(command.into_iter().map(Into::into).collect())
    }

    fn into_argv(self) -> Result<Vec<String>> {
        let argv = match self {
            Self::Shell(command) => {
                if command.trim().is_empty() {
                    return Err(ClientError::Validation(
                        "sandbox command cannot be empty".to_string(),
                    ));
                }
                vec!["/bin/sh".to_string(), "-lc".to_string(), command]
            }
            Self::Argv(argv) => argv,
        };
        if argv.is_empty() {
            return Err(ClientError::Validation(
                "sandbox command cannot be empty".to_string(),
            ));
        }
        Ok(argv)
    }
}

impl From<String> for SandboxCommand {
    fn from(command: String) -> Self {
        Self::Shell(command)
    }
}

impl From<&str> for SandboxCommand {
    fn from(command: &str) -> Self {
        Self::Shell(command.to_string())
    }
}

impl From<Vec<String>> for SandboxCommand {
    fn from(command: Vec<String>) -> Self {
        Self::Argv(command)
    }
}

/// Optional controls for one Sandbox command.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CommandRunOptions {
    pub timeout: Option<Duration>,
    pub envs: BTreeMap<String, String>,
    pub cwd: Option<String>,
    pub user: Option<String>,
    pub stdin: Option<Vec<u8>>,
    /// Stable one-shot exec identity for replay-safe retries after retryable
    /// `Unavailable`. When omitted, the SDK mints a fresh `sdk-command-*` id.
    /// Callers that retry the same command after a partial prepare-exec must
    /// reuse the same id (do not append `.retry-N`).
    pub request_id: Option<String>,
}

impl CommandRunOptions {
    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.timeout = Some(timeout);
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.envs.insert(key.into(), value.into());
        self
    }

    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.cwd = Some(cwd.into());
        self
    }

    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.user = Some(user.into());
        self
    }

    pub fn stdin(mut self, stdin: impl Into<Vec<u8>>) -> Self {
        self.stdin = Some(stdin.into());
        self
    }

    pub fn request_id(mut self, request_id: impl Into<String>) -> Self {
        self.request_id = Some(request_id.into());
        self
    }
}

/// Captured result of one local Sandbox command execution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandResult {
    pub stdout: String,
    pub stderr: String,
    pub exit_code: i32,
    pub truncated: bool,
    /// Process-journal identity used for this one-shot exec (caller-provided or
    /// SDK-minted). Reuse on retryable `Unavailable` retries.
    pub request_id: String,
    pub(crate) stdout_bytes: Vec<u8>,
    pub(crate) stderr_bytes: Vec<u8>,
}

/// Command namespace attached to a local [`super::Sandbox`].
#[derive(Clone)]
pub struct Commands {
    pub(crate) inner: Arc<SandboxInner>,
}

impl std::fmt::Debug for Commands {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("Commands")
            .field("sandbox_id", &self.inner.execution_id)
            .finish()
    }
}

impl Commands {
    pub async fn run(&self, command: impl Into<SandboxCommand>) -> Result<CommandResult> {
        self.run_with_options(command, CommandRunOptions::default())
            .await
    }

    pub async fn run_with_options(
        &self,
        command: impl Into<SandboxCommand>,
        options: CommandRunOptions,
    ) -> Result<CommandResult> {
        let timeout_ns = match options.timeout {
            Some(timeout) if timeout.is_zero() => {
                return Err(ClientError::Validation(
                    "sandbox command timeout must be greater than zero".to_string(),
                ))
            }
            Some(timeout) => u64::try_from(timeout.as_nanos()).unwrap_or(u64::MAX),
            None => 0,
        };
        let request_id = match options.request_id {
            Some(request_id) => {
                validate_command_request_id(&request_id)?;
                request_id
            }
            None => format!("sdk-command-{}", uuid::Uuid::new_v4()),
        };
        let (_, generation) = self.inner.active_execution()?;
        let request = ExecRequest {
            request_id: Some(request_id.clone()),
            cmd: command.into().into_argv()?,
            timeout_ns,
            env: options
                .envs
                .into_iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect(),
            working_dir: options.cwd,
            rootfs: None,
            stdin: options.stdin,
            stdin_streaming: false,
            user: options.user,
            streaming: false,
        };

        let output = match self
            .inner
            .client
            .execute_execution(&self.inner.execution_id, generation, request)
            .await
        {
            Ok(output) => output,
            Err(ClientError::Execution(a3s_box_core::ExecutionManagerError::Unavailable(
                message,
            ))) => {
                return Err(ClientError::CommandUnavailable {
                    request_id,
                    message,
                });
            }
            Err(error) => return Err(error),
        };

        Ok(CommandResult {
            stdout: String::from_utf8_lossy(&output.stdout).into_owned(),
            stderr: String::from_utf8_lossy(&output.stderr).into_owned(),
            exit_code: output.exit_code,
            truncated: output.truncated,
            request_id,
            stdout_bytes: output.stdout,
            stderr_bytes: output.stderr,
        })
    }
}

const MAX_COMMAND_REQUEST_ID_BYTES: usize = 512;

fn validate_command_request_id(request_id: &str) -> Result<()> {
    if request_id.is_empty()
        || request_id.len() > MAX_COMMAND_REQUEST_ID_BYTES
        || request_id.contains('\0')
    {
        return Err(ClientError::Validation(
            "sandbox command request_id must be a non-empty UTF-8 string of at most 512 bytes without NUL".to_string(),
        ));
    }
    Ok(())
}
