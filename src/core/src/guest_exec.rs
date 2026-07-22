//! Bounded guest entrypoint configuration staged outside the kernel command line.

use serde::{Deserialize, Serialize};

/// Fixed in-guest location of the runtime-owned entrypoint configuration.
pub const RUNTIME_EXEC_CONFIG_PATH: &str = "/.a3s-box-exec.json";
/// Versioned schema written by the runtime and consumed by guest-init.
pub const RUNTIME_EXEC_CONFIG_SCHEMA: &str = "a3s.box.guest-exec.v1";
/// Maximum serialized entrypoint configuration accepted by either side.
pub const MAX_RUNTIME_EXEC_CONFIG_BYTES: usize = 1024 * 1024;
/// Match libkrun's maximum argument pointer count.
pub const MAX_RUNTIME_EXEC_ARGS: usize = 4096;

/// Runtime-owned process configuration consumed by guest-init before launch.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct GuestExecConfig {
    pub schema: String,
    pub executable: String,
    pub args: Vec<String>,
    pub workdir: String,
    pub user: Option<String>,
    pub stdin_null: bool,
}

impl GuestExecConfig {
    pub fn new(
        executable: String,
        args: Vec<String>,
        workdir: String,
        user: Option<String>,
        stdin_null: bool,
    ) -> Self {
        Self {
            schema: RUNTIME_EXEC_CONFIG_SCHEMA.to_string(),
            executable,
            args,
            workdir,
            user,
            stdin_null,
        }
    }

    pub fn validate(&self) -> Result<(), String> {
        if self.schema != RUNTIME_EXEC_CONFIG_SCHEMA {
            return Err(format!("unsupported guest exec schema: {}", self.schema));
        }
        if self.executable.is_empty() {
            return Err("guest executable must not be empty".to_string());
        }
        if self.args.len() > MAX_RUNTIME_EXEC_ARGS {
            return Err(format!(
                "guest argument count {} exceeds limit {}",
                self.args.len(),
                MAX_RUNTIME_EXEC_ARGS
            ));
        }
        if self.workdir.is_empty() {
            return Err("guest working directory must not be empty".to_string());
        }
        if self.executable.contains('\0')
            || self.workdir.contains('\0')
            || self.args.iter().any(|argument| argument.contains('\0'))
            || self.user.as_ref().is_some_and(|user| user.contains('\0'))
        {
            return Err("guest exec configuration contains NUL".to_string());
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn guest_exec_config_validates_schema_bounds_and_nul() {
        let valid = GuestExecConfig::new(
            "/bin/sh".to_string(),
            vec!["-c".to_string(), "printf ok".to_string()],
            "/".to_string(),
            Some("123:456".to_string()),
            true,
        );
        valid.validate().unwrap();

        let mut wrong_schema = valid.clone();
        wrong_schema.schema = "a3s.box.guest-exec.v2".to_string();
        assert!(wrong_schema.validate().is_err());

        let mut too_many = valid.clone();
        too_many.args = vec![String::new(); MAX_RUNTIME_EXEC_ARGS + 1];
        assert!(too_many.validate().is_err());

        let mut nul = valid;
        nul.args.push("bad\0arg".to_string());
        assert!(nul.validate().is_err());
    }
}
