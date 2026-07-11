//! `a3s-box build` command — Build an image from a Dockerfile or Containerfile.
//!
//! Parses a Dockerfile/Containerfile, pulls the base image, executes instructions,
//! and produces an OCI image stored in the local image store.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use clap::{Args, ValueEnum};

#[path = "build_buildkit_vm.rs"]
mod buildkit_vm;

const BUILD_RUN_POOL_SOCKET_ENV: &str = "A3S_BOX_BUILD_RUN_POOL_SOCKET";
const BUILD_RUN_CACHE_DIR_ENV: &str = "A3S_BOX_BUILD_RUN_CACHE_DIR";
const DEFAULT_BUILD_RUN_POOL_GUEST_ROOTFS: &str = "/run/a3s/build-rootfs";

#[derive(Clone, Copy, Debug, PartialEq, Eq, ValueEnum)]
pub enum BuildBackend {
    /// Use the default backend for the host and Dockerfile.
    Auto,
    /// Use the built-in host-side A3S build engine.
    Host,
    /// Run BuildKit inside an A3S Linux VM and load its OCI output.
    BuildkitVm,
}

#[derive(Args)]
pub struct BuildArgs {
    /// Build context directory (contains Dockerfile/Containerfile and source files)
    #[arg(default_value = ".")]
    pub path: String,

    /// Name and optionally tag for the image (e.g., "myimage:latest")
    #[arg(short = 't', long = "tag")]
    pub tag: Option<String>,

    /// Path to Dockerfile/Containerfile (default: <PATH>/Dockerfile, then <PATH>/Containerfile)
    #[arg(short = 'f', long = "file")]
    pub file: Option<String>,

    /// Set build-time variables (KEY=VALUE), can be repeated
    #[arg(long = "build-arg")]
    pub build_arg: Vec<String>,

    /// Suppress build output
    #[arg(short, long)]
    pub quiet: bool,

    /// Target platform for the build (e.g., "linux/amd64").
    ///
    /// Multi-platform image indexes are not supported yet.
    #[arg(long)]
    pub platform: Option<String>,

    /// Build only up to the named (or indexed) stage in a multi-stage build.
    #[arg(long)]
    pub target: Option<String>,

    /// Do not use the layer build cache; rebuild every layer.
    #[arg(long = "no-cache")]
    pub no_cache: bool,

    /// Build backend: auto, host, or buildkit-vm.
    ///
    /// On macOS, auto delegates Dockerfiles containing RUN to BuildKit in an A3S VM.
    #[arg(long, value_enum, default_value_t = BuildBackend::Auto)]
    pub builder: BuildBackend,

    /// BuildKit image to run when --builder=buildkit-vm is selected.
    #[arg(long = "buildkit-image", value_name = "IMAGE")]
    pub buildkit_image: Option<String>,

    /// CPUs for the BuildKit VM helper box.
    #[arg(long = "buildkit-cpus", value_name = "N")]
    pub buildkit_cpus: Option<String>,

    /// Memory for the BuildKit VM helper box.
    #[arg(long = "buildkit-memory", value_name = "SIZE")]
    pub buildkit_memory: Option<String>,

    /// Push the built tag directly from the BuildKit VM.
    ///
    /// Currently supported only with --builder=buildkit-vm and requires --tag.
    #[arg(long)]
    pub push: bool,

    /// Use plain HTTP when pushing from the BuildKit VM to a trusted registry.
    #[arg(long, alias = "insecure")]
    pub plain_http: bool,

    /// Execute Dockerfile RUN instructions through the warm-pool daemon.
    #[arg(long = "run-pool")]
    pub run_pool: bool,

    /// Warm-pool daemon socket for Dockerfile RUN execution.
    #[arg(long = "run-pool-socket", value_name = "PATH")]
    pub run_pool_socket: Option<String>,

    /// Start the Dockerfile RUN warm-pool daemon when one is not already running.
    ///
    /// Requires --run-pool-image so build leases use an explicit helper VM image.
    #[arg(long = "run-pool-autostart")]
    pub run_pool_autostart: bool,

    /// Helper VM image for Dockerfile RUN pool leases; omitted uses daemon default.
    #[arg(long = "run-pool-image", value_name = "IMAGE")]
    pub run_pool_image: Option<String>,

    /// CPUs for lazily-created Dockerfile RUN pool helper VMs.
    #[arg(long = "run-pool-cpus", default_value_t = 2)]
    pub run_pool_cpus: u32,

    /// Memory for lazily-created Dockerfile RUN pool helper VMs.
    #[arg(long = "run-pool-memory", default_value = "512m")]
    pub run_pool_memory: String,

    /// Timeout for each Dockerfile RUN command when using --run-pool.
    #[arg(long = "run-pool-timeout", default_value = "1h", value_parser = crate::output::parse_duration_secs)]
    pub run_pool_timeout: u64,

    /// Persistent cache directory for Dockerfile RUN --mount=type=cache with --run-pool.
    #[arg(long = "run-cache-dir", value_name = "PATH")]
    pub run_cache_dir: Option<String>,
}

pub async fn execute(args: BuildArgs) -> Result<(), Box<dyn std::error::Error>> {
    let context_dir = PathBuf::from(&args.path)
        .canonicalize()
        .map_err(|e| format!("Invalid build context path '{}': {}", args.path, e))?;

    if !context_dir.is_dir() {
        return Err(format!(
            "Build context '{}' is not a directory",
            context_dir.display()
        )
        .into());
    }

    let dockerfile_path = resolve_build_file(&context_dir, args.file.as_deref())?;

    // Parse build args
    let build_args = parse_build_args(&args.build_arg)?;

    let platforms = parse_platforms(args.platform.as_deref())?;

    let run_pool = resolve_run_pool_config(&args)?;
    if run_pool.is_some() && args.builder == BuildBackend::BuildkitVm {
        return Err("--run-pool cannot be combined with --builder=buildkit-vm".into());
    }
    if args.run_pool_autostart {
        if let Some(config) = &run_pool {
            super::pool::ensure_pool_daemon_running(&pool_autostart_config_for_build(config)?)
                .await?;
        }
    }

    let use_buildkit_vm = if run_pool.is_some() {
        false
    } else {
        should_use_buildkit_vm(args.builder, &dockerfile_path)?
    };
    if args.push && !use_buildkit_vm {
        return Err("--push is currently supported only with --builder=buildkit-vm".into());
    }

    if use_buildkit_vm {
        return buildkit_vm::execute(buildkit_vm::Build {
            context_dir,
            dockerfile_path,
            tag: args.tag.clone(),
            build_args: args.build_arg.clone(),
            quiet: args.quiet,
            platform: args.platform.clone(),
            target: args.target.clone(),
            no_cache: args.no_cache,
            push: args.push,
            plain_http: args.plain_http,
            image: args
                .buildkit_image
                .clone()
                .unwrap_or_else(buildkit_vm::default_image),
            cpus: args
                .buildkit_cpus
                .clone()
                .unwrap_or_else(buildkit_vm::default_cpus),
            memory: args
                .buildkit_memory
                .clone()
                .unwrap_or_else(buildkit_vm::default_memory),
        })
        .await;
    }

    // Open image store
    let store = Arc::new(super::open_image_store()?);

    let config = a3s_box_runtime::BuildConfig {
        context_dir,
        dockerfile_path,
        tag: args.tag.clone(),
        build_args,
        quiet: args.quiet,
        platforms,
        target: args.target.clone(),
        no_cache: args.no_cache,
        metrics: None,
        run_pool,
    };

    let result = a3s_box_runtime::oci::build::engine::build(config, store).await?;

    if args.quiet {
        println!("{}", result.digest);
    }

    Ok(())
}

fn resolve_run_pool_config(
    args: &BuildArgs,
) -> Result<Option<a3s_box_runtime::BuildRunPoolConfig>, Box<dyn std::error::Error>> {
    let env_socket = std::env::var(BUILD_RUN_POOL_SOCKET_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    let env_cache_dir = std::env::var(BUILD_RUN_CACHE_DIR_ENV)
        .ok()
        .filter(|value| !value.trim().is_empty());
    let enabled = args.run_pool
        || args.run_pool_autostart
        || args.run_pool_socket.is_some()
        || env_socket.is_some()
        || args.run_cache_dir.is_some()
        || env_cache_dir.is_some();
    if !enabled {
        return Ok(None);
    }

    if args.run_pool_timeout == 0 {
        return Err("--run-pool-timeout must be greater than 0".into());
    }

    let socket = args
        .run_pool_socket
        .clone()
        .or(env_socket)
        .unwrap_or_else(|| super::pool::DEFAULT_SOCKET.to_string());
    let memory_mb = crate::output::parse_memory(&args.run_pool_memory)
        .map_err(|e| format!("Invalid --run-pool-memory: {e}"))?;
    if args.run_pool_autostart && args.run_pool_image.is_none() {
        return Err(
            "--run-pool-autostart requires --run-pool-image so the helper VM image is explicit"
                .into(),
        );
    }
    let run_cache_dir = args
        .run_cache_dir
        .clone()
        .or(env_cache_dir)
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            a3s_box_core::dirs_home()
                .join("buildcache")
                .join("run-cache")
        });

    Ok(Some(a3s_box_runtime::BuildRunPoolConfig {
        socket,
        image: args.run_pool_image.clone(),
        vcpus: args.run_pool_cpus,
        memory_mb,
        guest_rootfs: DEFAULT_BUILD_RUN_POOL_GUEST_ROOTFS.to_string(),
        timeout_ns: args.run_pool_timeout.saturating_mul(1_000_000_000),
        run_cache_dir,
    }))
}

fn pool_autostart_config_for_build(
    config: &a3s_box_runtime::BuildRunPoolConfig,
) -> Result<super::pool::PoolAutoStartConfig, Box<dyn std::error::Error>> {
    Ok(super::pool::PoolAutoStartConfig {
        socket: config.socket.clone(),
        image: None,
        size: super::pool::DEFAULT_AUTOSTART_POOL_SIZE,
        max: super::pool::DEFAULT_AUTOSTART_POOL_MAX,
    })
}

fn should_use_buildkit_vm(
    backend: BuildBackend,
    dockerfile_path: &std::path::Path,
) -> Result<bool, Box<dyn std::error::Error>> {
    match backend {
        BuildBackend::BuildkitVm => Ok(true),
        BuildBackend::Host => Ok(false),
        BuildBackend::Auto => {
            #[cfg(target_os = "macos")]
            {
                Ok(dockerfile_has_run(dockerfile_path)? && !unsafe_host_run_enabled())
            }

            #[cfg(not(target_os = "macos"))]
            {
                let _ = dockerfile_path;
                Ok(false)
            }
        }
    }
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn unsafe_host_run_enabled() -> bool {
    std::env::var("A3S_BOX_UNSAFE_HOST_RUN")
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

#[cfg_attr(not(target_os = "macos"), allow(dead_code))]
fn dockerfile_has_run(
    dockerfile_path: &std::path::Path,
) -> Result<bool, Box<dyn std::error::Error>> {
    let dockerfile = a3s_box_runtime::Dockerfile::from_file(dockerfile_path)?;
    Ok(dockerfile
        .instructions
        .iter()
        .any(|instruction| matches!(instruction, a3s_box_runtime::Instruction::Run { .. })))
}

/// Parse KEY=VALUE pairs into a HashMap.
fn parse_build_args(args: &[String]) -> Result<HashMap<String, String>, String> {
    let mut map = HashMap::new();
    for arg in args {
        let (key, value) = arg
            .split_once('=')
            .ok_or_else(|| format!("Invalid build arg (expected KEY=VALUE): {arg}"))?;
        map.insert(key.to_string(), value.to_string());
    }
    Ok(map)
}

fn parse_platforms(
    platform: Option<&str>,
) -> Result<Vec<a3s_box_core::platform::Platform>, Box<dyn std::error::Error>> {
    let Some(platform) = platform else {
        return Ok(vec![]);
    };

    let platforms = a3s_box_core::platform::Platform::parse_list(platform)
        .map_err(|e| format!("Invalid --platform: {e}"))?;
    if platforms.len() > 1 {
        return Err(
            "Multi-platform builds are not implemented yet; pass a single --platform value".into(),
        );
    }
    if platforms.iter().any(|p| p.os != "linux") {
        return Err("Only linux target platforms are supported for builds".into());
    }

    Ok(platforms)
}

fn resolve_build_file(
    context_dir: &std::path::Path,
    file: Option<&str>,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    if let Some(file) = file {
        let path = PathBuf::from(file);
        let build_file = if path.is_absolute() {
            path
        } else {
            context_dir.join(path)
        };

        if build_file.exists() {
            return Ok(build_file);
        }

        return Err(format!("Build file not found at {}", build_file.display()).into());
    }

    for candidate in ["Dockerfile", "Containerfile"] {
        let path = context_dir.join(candidate);
        if path.exists() {
            return Ok(path);
        }
    }

    Err(format!(
        "Build file not found: expected Dockerfile or Containerfile in {}",
        context_dir.display()
    )
    .into())
}

#[cfg(test)]
mod tests {
    use super::*;

    struct EnvGuard {
        key: &'static str,
        previous: Option<std::ffi::OsString>,
    }

    impl EnvGuard {
        fn set(key: &'static str, value: impl AsRef<std::ffi::OsStr>) -> Self {
            let previous = std::env::var_os(key);
            std::env::set_var(key, value);
            Self { key, previous }
        }
    }

    impl Drop for EnvGuard {
        fn drop(&mut self) {
            match &self.previous {
                Some(value) => std::env::set_var(self.key, value),
                None => std::env::remove_var(self.key),
            }
        }
    }

    fn build_args() -> BuildArgs {
        BuildArgs {
            path: ".".to_string(),
            tag: None,
            file: None,
            build_arg: vec![],
            quiet: false,
            platform: None,
            target: None,
            no_cache: false,
            builder: BuildBackend::Auto,
            buildkit_image: None,
            buildkit_cpus: None,
            buildkit_memory: None,
            push: false,
            plain_http: false,
            run_pool: false,
            run_pool_socket: None,
            run_pool_autostart: false,
            run_pool_image: None,
            run_pool_cpus: 2,
            run_pool_memory: "512m".to_string(),
            run_pool_timeout: 3600,
            run_cache_dir: None,
        }
    }

    #[test]
    fn test_parse_build_args_valid() {
        let args = vec!["VERSION=1.0".to_string(), "DEBUG=true".to_string()];
        let result = parse_build_args(&args).unwrap();
        assert_eq!(result.get("VERSION"), Some(&"1.0".to_string()));
        assert_eq!(result.get("DEBUG"), Some(&"true".to_string()));
    }

    #[test]
    fn test_parse_build_args_empty() {
        let result = parse_build_args(&[]).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_build_args_invalid() {
        let args = vec!["NOEQUALS".to_string()];
        assert!(parse_build_args(&args).is_err());
    }

    #[test]
    fn test_parse_build_args_value_with_equals() {
        let args = vec!["URL=http://example.com?a=1".to_string()];
        let result = parse_build_args(&args).unwrap();
        assert_eq!(
            result.get("URL"),
            Some(&"http://example.com?a=1".to_string())
        );
    }

    #[test]
    fn test_should_use_buildkit_vm_respects_explicit_backend() {
        let tmp = tempfile::tempdir().unwrap();
        let dockerfile = tmp.path().join("Dockerfile");
        std::fs::write(&dockerfile, "FROM scratch\nRUN echo hi\n").unwrap();

        assert!(should_use_buildkit_vm(BuildBackend::BuildkitVm, &dockerfile).unwrap());
        assert!(!should_use_buildkit_vm(BuildBackend::Host, &dockerfile).unwrap());
    }

    #[test]
    fn test_resolve_run_pool_config_explicit_socket() {
        let mut args = build_args();
        args.run_pool = true;
        args.run_pool_socket = Some("/tmp/a3s-build-pool.sock".to_string());
        args.run_pool_image = Some("alpine:latest".to_string());
        args.run_pool_cpus = 4;
        args.run_pool_memory = "1g".to_string();
        args.run_pool_timeout = 90;
        args.run_cache_dir = Some("/tmp/a3s-run-cache".to_string());

        let config = resolve_run_pool_config(&args).unwrap().unwrap();

        assert_eq!(config.socket, "/tmp/a3s-build-pool.sock");
        assert_eq!(config.image.as_deref(), Some("alpine:latest"));
        assert_eq!(config.vcpus, 4);
        assert_eq!(config.memory_mb, 1024);
        assert_eq!(config.guest_rootfs, DEFAULT_BUILD_RUN_POOL_GUEST_ROOTFS);
        assert_eq!(config.timeout_ns, 90_000_000_000);
        assert_eq!(config.run_cache_dir, PathBuf::from("/tmp/a3s-run-cache"));
    }

    #[test]
    fn test_resolve_run_pool_config_rejects_zero_timeout() {
        let mut args = build_args();
        args.run_pool = true;
        args.run_pool_timeout = 0;

        let err = resolve_run_pool_config(&args).unwrap_err().to_string();

        assert!(err.contains("--run-pool-timeout"));
    }

    #[test]
    fn test_resolve_run_pool_config_autostart_requires_image() {
        let mut args = build_args();
        args.run_pool_autostart = true;

        let err = resolve_run_pool_config(&args).unwrap_err().to_string();

        assert!(err.contains("--run-pool-autostart"));
        assert!(err.contains("--run-pool-image"));
    }

    #[test]
    fn test_pool_autostart_config_for_build_starts_lazy_helper_daemon() {
        let mut args = build_args();
        args.run_pool = true;
        args.run_pool_autostart = true;
        args.run_pool_socket = Some("/tmp/a3s-build-pool.sock".to_string());
        args.run_pool_image = Some("alpine:latest".to_string());

        let config = resolve_run_pool_config(&args).unwrap().unwrap();
        let autostart = pool_autostart_config_for_build(&config).unwrap();

        assert_eq!(config.image.as_deref(), Some("alpine:latest"));
        assert_eq!(autostart.socket, "/tmp/a3s-build-pool.sock");
        assert!(autostart.image.is_none());
        assert_eq!(
            autostart.size,
            crate::commands::pool::DEFAULT_AUTOSTART_POOL_SIZE
        );
        assert_eq!(
            autostart.max,
            crate::commands::pool::DEFAULT_AUTOSTART_POOL_MAX
        );
    }

    #[test]
    fn test_resolve_run_pool_config_env_cache_dir_enables_pool() {
        let tmp = tempfile::tempdir().unwrap();
        let cache_dir = tmp.path().join("run-cache");
        let _guard = EnvGuard::set(BUILD_RUN_CACHE_DIR_ENV, cache_dir.as_os_str());
        let args = build_args();

        let config = resolve_run_pool_config(&args).unwrap().unwrap();

        assert_eq!(config.socket, crate::commands::pool::DEFAULT_SOCKET);
        assert_eq!(config.run_cache_dir, cache_dir);
    }

    #[test]
    fn test_dockerfile_has_run_detects_run_instruction() {
        let tmp = tempfile::tempdir().unwrap();
        let dockerfile = tmp.path().join("Dockerfile");
        std::fs::write(&dockerfile, "FROM scratch\nRUN echo hi\n").unwrap();

        assert!(dockerfile_has_run(&dockerfile).unwrap());

        std::fs::write(&dockerfile, "FROM scratch\nCOPY . /app\n").unwrap();
        assert!(!dockerfile_has_run(&dockerfile).unwrap());
    }

    #[test]
    fn test_parse_platforms_empty() {
        let result = parse_platforms(None).unwrap();
        assert!(result.is_empty());
    }

    #[test]
    fn test_parse_platforms_single() {
        let result = parse_platforms(Some("linux/amd64")).unwrap();
        assert_eq!(result.len(), 1);
        assert_eq!(result[0].to_string(), "linux/amd64");
    }

    #[test]
    fn test_parse_platforms_rejects_multiple() {
        let err = parse_platforms(Some("linux/amd64,linux/arm64"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Multi-platform builds are not implemented yet"));
    }

    #[test]
    fn test_parse_platforms_rejects_non_linux() {
        let err = parse_platforms(Some("windows/amd64"))
            .unwrap_err()
            .to_string();
        assert!(err.contains("Only linux target platforms"));
    }

    #[test]
    fn test_resolve_build_file_prefers_dockerfile() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Dockerfile"), "FROM scratch\n").unwrap();
        std::fs::write(tmp.path().join("Containerfile"), "FROM scratch\n").unwrap();

        let path = resolve_build_file(tmp.path(), None).unwrap();
        assert_eq!(path.file_name().unwrap(), "Dockerfile");
    }

    #[test]
    fn test_resolve_build_file_falls_back_to_containerfile() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Containerfile"), "FROM scratch\n").unwrap();

        let path = resolve_build_file(tmp.path(), None).unwrap();
        assert_eq!(path.file_name().unwrap(), "Containerfile");
    }

    #[test]
    fn test_resolve_build_file_explicit_file() {
        let tmp = tempfile::tempdir().unwrap();
        std::fs::write(tmp.path().join("Customfile"), "FROM scratch\n").unwrap();

        let path = resolve_build_file(tmp.path(), Some("Customfile")).unwrap();
        assert_eq!(path.file_name().unwrap(), "Customfile");
    }

    #[test]
    fn test_resolve_build_file_missing() {
        let tmp = tempfile::tempdir().unwrap();
        let err = resolve_build_file(tmp.path(), None)
            .unwrap_err()
            .to_string();
        assert!(err.contains("Dockerfile or Containerfile"));
    }
}
