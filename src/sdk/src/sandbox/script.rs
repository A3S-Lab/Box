use std::time::Duration;

use super::{CommandResult, CommandRunOptions, Commands, SandboxCommand};
use crate::{ClientError, Result};

/// Fluent builder for a script sent through stdin to an explicit interpreter.
///
/// The source is not interpolated into a shell command, so arbitrary script
/// contents do not require host-side quoting or a temporary host file.
#[derive(Debug, Clone)]
pub struct ScriptBuilder {
    commands: Commands,
    source: Vec<u8>,
    interpreter: Vec<String>,
    options: CommandRunOptions,
}

impl ScriptBuilder {
    pub(crate) fn new(commands: Commands, source: impl AsRef<[u8]>) -> Self {
        Self {
            commands,
            source: source.as_ref().to_vec(),
            interpreter: vec!["/bin/sh".to_string(), "-se".to_string()],
            options: CommandRunOptions::default(),
        }
    }

    /// Select the interpreter argv, for example `["python", "-"]`.
    pub fn interpreter(mut self, command: impl IntoIterator<Item = impl Into<String>>) -> Self {
        self.interpreter = command.into_iter().map(Into::into).collect();
        self
    }

    pub const fn timeout(mut self, timeout: Duration) -> Self {
        self.options.timeout = Some(timeout);
        self
    }

    pub fn env(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.options.envs.insert(key.into(), value.into());
        self
    }

    pub fn cwd(mut self, cwd: impl Into<String>) -> Self {
        self.options.cwd = Some(cwd.into());
        self
    }

    pub fn user(mut self, user: impl Into<String>) -> Self {
        self.options.user = Some(user.into());
        self
    }

    pub async fn run(mut self) -> Result<CommandResult> {
        if self.source.is_empty() {
            return Err(ClientError::Validation(
                "script source cannot be empty".to_string(),
            ));
        }
        if self.interpreter.is_empty() {
            return Err(ClientError::Validation(
                "script interpreter cannot be empty".to_string(),
            ));
        }
        self.options.stdin = Some(self.source);
        self.commands
            .run_with_options(SandboxCommand::Argv(self.interpreter), self.options)
            .await
    }
}

impl Commands {
    /// Start a fluent, stdin-backed script request.
    pub fn script(&self, source: impl AsRef<[u8]>) -> ScriptBuilder {
        ScriptBuilder::new(self.clone(), source)
    }
}
