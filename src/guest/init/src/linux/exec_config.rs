//! Workload execution, boot bundle, Secret, and sidecar configuration.

use super::*;

/// Container entrypoint configuration parsed from environment variables.
pub(super) struct ExecConfig {
    /// Container executable path
    pub(super) executable: String,
    /// Container arguments
    pub(super) args: Vec<String>,
    /// Container environment variables
    pub(super) env: Vec<(String, String)>,
    /// Working directory
    pub(super) workdir: String,
    /// Container user (`uid`, `uid:gid`, `root`, or a name resolved via the
    /// image `/etc/passwd`). Applied to the main process before exec.
    pub(super) user: Option<String>,
    /// Whether stdin should be connected to `/dev/null`.
    pub(super) stdin_null: bool,
}

impl Drop for ExecConfig {
    fn drop(&mut self) {
        for (_, value) in &mut self.env {
            value.zeroize();
        }
    }
}

impl ExecConfig {
    pub(super) fn from_guest_boot_config(
        config: GuestExecConfig,
        env: Vec<(String, String)>,
    ) -> Self {
        Self {
            executable: config.executable,
            args: config.args,
            env,
            workdir: config.workdir,
            user: config.user,
            stdin_null: config.stdin_null,
        }
    }

    /// Parse container entrypoint configuration from environment variables.
    ///
    /// Expected environment variables:
    /// - BOX_EXEC_CONFIG_FILE: fixed runtime-owned JSON process configuration
    /// - BOX_EXEC_ENV_*: container environment variables
    ///
    /// Legacy runtimes may instead pass BOX_EXEC_EXEC, BOX_EXEC_ARGC,
    /// BOX_EXEC_ARG_<n>, BOX_EXEC_WORKDIR, BOX_EXEC_USER, and BOX_EXEC_STDIN.
    pub(super) fn from_env() -> Result<Self, Box<dyn std::error::Error>> {
        Self::from_env_with_staged_file_consumption(true)
    }

    pub(super) fn from_env_without_consuming_staged_file(
    ) -> Result<Self, Box<dyn std::error::Error>> {
        Self::from_env_with_staged_file_consumption(false)
    }

    fn from_env_with_staged_file_consumption(
        consume_staged_file: bool,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        // Legacy BOX_EXEC_* values are base64-encoded (URL-safe, no pad) by
        // the runtime when BOX_EXEC_B64=1. Some libkrun init builds import
        // those values from /proc/cmdline but miss the marker; infer the old
        // encoded form from BOX_EXEC_EXEC so old runtimes still boot.
        let b64 = should_decode_box_exec_values();
        let decode = |s: String| decode_box_exec_value(s, b64);

        let (executable, args, workdir, user, stdin_null) =
            match std::env::var("BOX_EXEC_CONFIG_FILE") {
                Ok(path) => {
                    let staged = read_staged_exec_config(&path, consume_staged_file)?;
                    (
                        staged.executable,
                        staged.args,
                        staged.workdir,
                        staged.user,
                        staged.stdin_null,
                    )
                }
                Err(std::env::VarError::NotPresent) => {
                    let executable = std::env::var("BOX_EXEC_EXEC")
                        .map(&decode)
                        .unwrap_or_else(|_| "/bin/sh".to_string());
                    let args = match std::env::var("BOX_EXEC_ARGC")
                        .ok()
                        .and_then(|value| value.parse::<usize>().ok())
                    {
                        Some(argc) => (0..argc)
                            .filter_map(|index| {
                                std::env::var(format!("BOX_EXEC_ARG_{index}"))
                                    .ok()
                                    .map(&decode)
                            })
                            .collect(),
                        None => vec![],
                    };
                    let workdir = std::env::var("BOX_EXEC_WORKDIR")
                        .map(&decode)
                        .unwrap_or_else(|_| "/".to_string());
                    let user = std::env::var("BOX_EXEC_USER")
                        .ok()
                        .map(&decode)
                        .filter(|value| !value.is_empty());
                    let stdin_null = std::env::var("BOX_EXEC_STDIN")
                        .map(|value| value.eq_ignore_ascii_case("null"))
                        .unwrap_or(false);
                    (executable, args, workdir, user, stdin_null)
                }
                Err(error) => {
                    return Err(format!("BOX_EXEC_CONFIG_FILE is invalid: {error}").into());
                }
            };

        // Collect BOX_EXEC_ENV_* variables (values decoded as above). Skip
        // BOX_EXEC_ENV_FILE — it's the pointer to the staged env file, not a
        // container variable. Kept for backward compatibility with a runtime that
        // still passes container env inline.
        let mut env: Vec<(String, String)> = std::env::vars()
            .filter_map(|(key, value)| {
                key.strip_prefix("BOX_EXEC_ENV_")
                    .filter(|stripped| *stripped != "FILE")
                    .map(|stripped| (stripped.to_string(), decode(value)))
            })
            .collect();

        // Bulk container env is staged in a file (runtime/src/vm/spec.rs): K8s
        // injects ~150 service env vars, which overflow the guest kernel cmdline
        // if passed inline, so the runtime writes them to a file and points here.
        // Each line is `KEY=base64(value)`; the key may itself contain `=`-free
        // bytes only (env names are a safe charset), so split on the first `=`.
        if let Ok(path) = std::env::var("BOX_EXEC_ENV_FILE") {
            match std::fs::read_to_string(&path) {
                Ok(contents) => {
                    for line in contents.lines() {
                        if let Some((k, v)) = line.split_once('=') {
                            env.push((k.to_string(), decode(v.to_string())));
                        }
                    }
                }
                Err(e) => eprintln!("init.krun: failed to read BOX_EXEC_ENV_FILE {path}: {e}"),
            }
        }

        Ok(Self {
            executable,
            args,
            env,
            workdir,
            user,
            stdin_null,
        })
    }

    /// Replace the non-sensitive Runtime binding manifest with exact
    /// values read from read-only files that Box mounted from node tmpfs.
    /// The manifest itself never reaches the workload environment.
    pub(super) fn materialize_secret_environment(
        &mut self,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.materialize_secret_environment_from(std::path::Path::new("/.a3s-box-secrets"))
    }

    pub(super) fn materialize_secret_environment_from(
        &mut self,
        internal_root: &std::path::Path,
    ) -> Result<(), Box<dyn std::error::Error>> {
        let manifests = self
            .env
            .iter()
            .enumerate()
            .filter(|(_, (key, _))| key == SECRET_ENVIRONMENT_MANIFEST)
            .map(|(index, _)| index)
            .collect::<Vec<_>>();
        if manifests.is_empty() {
            return Ok(());
        }
        if manifests.len() != 1 {
            return Err("duplicate Box Secret environment manifests".into());
        }
        let (_, encoded) = self.env.remove(manifests[0]);
        let encoded = Zeroizing::new(encoded);
        let bindings: Vec<SecretEnvironmentBinding> = serde_json::from_str(&encoded)
            .map_err(|_| "Box Secret environment manifest is not valid version-1 JSON")?;
        if bindings.is_empty() || bindings.len() > 128 {
            return Err("Box Secret environment manifest has an invalid binding count".into());
        }

        let mut variables = std::collections::BTreeSet::new();
        let mut paths = std::collections::BTreeSet::new();
        for binding in bindings {
            binding.validate()?;
            if !variables.insert(binding.variable.clone()) || !paths.insert(binding.path.clone()) {
                return Err("Box Secret environment manifest contains duplicate bindings".into());
            }
            let path = std::path::Path::new(&binding.path);
            let relative = path
                .strip_prefix(internal_root)
                .map_err(|_| "Box Secret environment file escaped the reserved guest directory")?;
            let components = relative
                .components()
                .filter_map(|component| match component {
                    std::path::Component::Normal(value) => value.to_str(),
                    _ => None,
                })
                .collect::<Vec<_>>();
            if components.len() != 2
                || components[0].len() != 64
                || !components[0].bytes().all(|byte| byte.is_ascii_hexdigit())
                || components[1].len() != 10
                || !components[1].as_bytes()[..3]
                    .iter()
                    .all(|byte| byte.is_ascii_digit())
                || &components[1].as_bytes()[3..] != b".secret"
                || components[1][..3]
                    .parse::<usize>()
                    .ok()
                    .is_none_or(|index| index >= 128)
            {
                return Err("Box Secret environment file has an invalid reserved identity".into());
            }
            let metadata = std::fs::symlink_metadata(path)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::{MetadataExt, PermissionsExt};
                if !metadata.file_type().is_file()
                    || metadata.file_type().is_symlink()
                    || metadata.nlink() != 1
                    || metadata.len() == 0
                    || metadata.len() > 1024 * 1024
                    || metadata.permissions().mode() & 0o777 != 0o400
                {
                    return Err("Box Secret environment file violates its regular-file, size, or mode contract".into());
                }
            }
            let bytes = Zeroizing::new(std::fs::read(path)?);
            if bytes.is_empty() || bytes.len() > 1024 * 1024 {
                return Err("Box Secret environment value has an invalid size".into());
            }
            let mut value = std::str::from_utf8(bytes.as_slice())
                .map_err(|_| "Box Secret environment value is not UTF-8")?
                .to_owned();
            if value.contains('\0') {
                value.zeroize();
                return Err("Box Secret environment value contains a NUL byte".into());
            }
            if self.env.iter().any(|(key, _)| key == &binding.variable) {
                value.zeroize();
                return Err(
                    "Box Secret environment binding conflicts with an existing value".into(),
                );
            }
            self.env.push((binding.variable, value));
        }
        Ok(())
    }
}

pub(super) fn read_staged_exec_config(
    path: &str,
    consume: bool,
) -> Result<GuestExecConfig, Box<dyn std::error::Error>> {
    if path != RUNTIME_EXEC_CONFIG_PATH {
        return Err(format!("unsupported BOX_EXEC_CONFIG_FILE path {path:?}").into());
    }
    let path = std::path::Path::new(path);
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() {
        return Err(format!(
            "guest exec config is not a regular file: {}",
            path.display()
        )
        .into());
    }
    if metadata.len() > MAX_RUNTIME_EXEC_CONFIG_BYTES as u64 {
        return Err(format!(
            "guest exec config is {} bytes; limit is {} bytes",
            metadata.len(),
            MAX_RUNTIME_EXEC_CONFIG_BYTES
        )
        .into());
    }
    let bytes = std::fs::read(path)?;
    if bytes.len() > MAX_RUNTIME_EXEC_CONFIG_BYTES {
        return Err(format!(
            "guest exec config grew to {} bytes; limit is {} bytes",
            bytes.len(),
            MAX_RUNTIME_EXEC_CONFIG_BYTES
        )
        .into());
    }
    let config = parse_staged_exec_config(&bytes)?;
    if consume {
        std::fs::remove_file(path)?;
    }
    Ok(config)
}

pub(super) fn parse_staged_exec_config(
    bytes: &[u8],
) -> Result<GuestExecConfig, Box<dyn std::error::Error>> {
    if bytes.len() > MAX_RUNTIME_EXEC_CONFIG_BYTES {
        return Err(format!(
            "guest exec config is {} bytes; limit is {} bytes",
            bytes.len(),
            MAX_RUNTIME_EXEC_CONFIG_BYTES
        )
        .into());
    }
    let config: GuestExecConfig = serde_json::from_slice(bytes)?;
    config
        .validate()
        .map_err(|message| format!("invalid guest exec config: {message}"))?;
    Ok(config)
}

pub(super) fn read_guest_boot_config_from_env(
) -> Result<Option<GuestBootConfig>, Box<dyn std::error::Error>> {
    let path = match std::env::var(GUEST_BOOT_CONFIG_ENV) {
        Ok(path) => path,
        Err(std::env::VarError::NotPresent) => return Ok(None),
        Err(error) => {
            return Err(format!("{GUEST_BOOT_CONFIG_ENV} is invalid: {error}").into());
        }
    };
    if path != GUEST_BOOT_CONFIG_PATH {
        return Err(format!("unsupported {GUEST_BOOT_CONFIG_ENV} path {path:?}").into());
    }

    let path = std::path::Path::new(&path);
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
        return Err(format!(
            "guest boot config is not a regular file: {}",
            path.display()
        )
        .into());
    }
    if metadata.len() > MAX_GUEST_BOOT_CONFIG_BYTES as u64 {
        return Err(format!(
            "guest boot config is {} bytes; limit is {} bytes",
            metadata.len(),
            MAX_GUEST_BOOT_CONFIG_BYTES
        )
        .into());
    }
    let bytes = std::fs::read(path)?;
    parse_guest_boot_config(&bytes).map(Some)
}

pub(super) fn parse_guest_boot_config(
    bytes: &[u8],
) -> Result<GuestBootConfig, Box<dyn std::error::Error>> {
    if bytes.len() > MAX_GUEST_BOOT_CONFIG_BYTES {
        return Err(format!(
            "guest boot config is {} bytes; limit is {} bytes",
            bytes.len(),
            MAX_GUEST_BOOT_CONFIG_BYTES
        )
        .into());
    }
    let config: GuestBootConfig = serde_json::from_slice(bytes)?;
    config
        .validate()
        .map_err(|message| format!("invalid guest boot config: {message}"))?;
    Ok(config)
}

pub(super) fn should_decode_box_exec_values() -> bool {
    if std::env::var("BOX_EXEC_B64")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        return true;
    }

    std::env::var("BOX_EXEC_EXEC")
        .ok()
        .and_then(|raw| decode_box_exec_value_if_valid(&raw))
        .as_deref()
        .is_some_and(is_plausible_exec)
}

pub(super) fn decode_box_exec_value(value: String, decode: bool) -> String {
    if decode {
        decode_box_exec_value_if_valid(&value).unwrap_or(value)
    } else {
        value
    }
}

pub(super) fn decode_box_exec_value_if_valid(value: &str) -> Option<String> {
    use base64::Engine;

    base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value.as_bytes())
        .ok()
        .and_then(|bytes| String::from_utf8(bytes).ok())
        .filter(|decoded| !decoded.is_empty() && !decoded.contains('\0'))
}

pub(super) fn is_plausible_exec(value: &str) -> bool {
    !value.is_empty()
        && (value.starts_with('/')
            || value.starts_with("./")
            || value.starts_with("../")
            || value
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.' | '+' | ':')))
}

/// Sidecar process configuration parsed from environment variables.
pub(super) struct SidecarConfig {
    /// Sidecar image name (informational only inside the VM — binary is already in rootfs)
    pub(super) image: String,
    /// Vsock port the sidecar listens on
    pub(super) vsock_port: u32,
    /// Environment variables for the sidecar
    pub(super) env: Vec<(String, String)>,
}

impl SidecarConfig {
    /// Parse sidecar configuration from environment variables.
    ///
    /// Returns `None` if `BOX_SIDECAR_IMAGE` is not set.
    pub(super) fn from_env() -> Option<Self> {
        let image = std::env::var("BOX_SIDECAR_IMAGE").ok()?;
        if image.is_empty() {
            return None;
        }

        let vsock_port = std::env::var("BOX_SIDECAR_VSOCK_PORT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(4092u32);

        let env_count: usize = std::env::var("BOX_SIDECAR_ENV_COUNT")
            .ok()
            .and_then(|s| s.parse().ok())
            .unwrap_or(0);

        let env: Vec<(String, String)> = (0..env_count)
            .filter_map(|i| {
                let raw = std::env::var(format!("BOX_SIDECAR_ENV_{}", i)).ok()?;
                let (key, value) = raw.split_once('=')?;
                Some((key.to_string(), value.to_string()))
            })
            .collect();

        Some(Self {
            image,
            vsock_port,
            env,
        })
    }
}
