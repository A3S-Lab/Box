//! Instruction handlers for the build engine.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};

use super::super::dockerfile::{
    Instruction, RunBindMount, RunCacheMount, RunCommand, RunTmpfsMount,
};
use super::super::dockerignore::DockerIgnore;
use super::super::layer::{
    create_layer_from_dir_with_chown, create_layer_with_chown, create_layer_with_deletions,
    sha256_bytes, LayerInfo,
};
use super::stages::resolve_stage_rootfs;
use super::utils::{
    assert_within, copy_dir_filtered, expand_args, extract_tar_to_dst, is_tar_archive,
    reject_path_traversal, resolve_chown, resolve_path,
};
use super::BuildState;

#[cfg(target_os = "macos")]
const UNSAFE_HOST_RUN_ENV: &str = "A3S_BOX_UNSAFE_HOST_RUN";
const RUN_OUTPUT_CONTEXT_BYTES: usize = 16 * 1024;

/// Whether a COPY/ADD source contains shell glob metacharacters.
fn has_glob_meta(s: &str) -> bool {
    s.contains('*') || s.contains('?') || s.contains('[')
}

/// Match a single path-segment glob (`*` = any run, `?` = one char) against a
/// name. Used for COPY/ADD wildcard expansion (the final segment of the source).
fn glob_segment_match(pattern: &str, name: &str) -> bool {
    let p: Vec<char> = pattern.chars().collect();
    let n: Vec<char> = name.chars().collect();
    // Classic two-pointer wildcard match with `*` backtracking.
    let (mut pi, mut ni) = (0usize, 0usize);
    let (mut star, mut mark) = (None, 0usize);
    while ni < n.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == n[ni]) {
            pi += 1;
            ni += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            mark = ni;
            pi += 1;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ni = mark;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == '*' {
        pi += 1;
    }
    pi == p.len()
}

/// Expand a COPY/ADD source pattern against `base_dir` into concrete relative
/// paths. Globs are honored in the final path segment (the common Docker case,
/// e.g. `*.conf` or `src/*.txt`). Returns the matches sorted; empty if none.
fn expand_glob_sources(base_dir: &Path, pattern: &str) -> Vec<String> {
    let p = pattern.trim_start_matches('/');
    let (dir_part, name_pat) = match p.rsplit_once('/') {
        Some((d, n)) => (d, n),
        None => ("", p),
    };
    let search_dir = if dir_part.is_empty() {
        base_dir.to_path_buf()
    } else {
        base_dir.join(dir_part)
    };
    let mut matches = Vec::new();
    if let Ok(entries) = std::fs::read_dir(&search_dir) {
        for entry in entries.flatten() {
            let fname = entry.file_name();
            let fname = fname.to_string_lossy();
            if glob_segment_match(name_pat, &fname) {
                matches.push(if dir_part.is_empty() {
                    fname.into_owned()
                } else {
                    format!("{}/{}", dir_part, fname)
                });
            }
        }
    }
    matches.sort();
    matches
}

/// Resolve COPY/ADD source patterns, expanding any globs against `base_dir`.
/// A non-glob source is passed through verbatim; a glob with no matches errors
/// like Docker ("no source files were specified").
fn resolve_source_patterns(base_dir: &Path, src_patterns: &[String]) -> Result<Vec<String>> {
    let mut resolved = Vec::new();
    for src in src_patterns {
        if src.starts_with("http://") || src.starts_with("https://") {
            // Remote ADD sources are never globbed (and may contain `?` query
            // strings); pass them through untouched.
            resolved.push(src.clone());
        } else if has_glob_meta(src) {
            let matches = expand_glob_sources(base_dir, src);
            if matches.is_empty() {
                return Err(BoxError::BuildError(format!(
                    "COPY/ADD source not found: no matches for pattern '{}'",
                    src
                )));
            }
            resolved.extend(matches);
        } else {
            resolved.push(src.clone());
        }
    }
    Ok(resolved)
}

/// Handle COPY: copy files from build context into rootfs, create a layer.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_copy(
    src_patterns: &[String],
    dst: &str,
    chown: Option<&str>,
    context_dir: &Path,
    rootfs_dir: &Path,
    layers_dir: &Path,
    workdir: &str,
    layer_index: usize,
    ignore: Option<&DockerIgnore>,
) -> Result<LayerInfo> {
    // Expand any glob source patterns against the context (Docker semantics).
    let src_patterns = &resolve_source_patterns(context_dir, src_patterns)?;

    // Resolve destination path
    let resolved_dst = resolve_path(workdir, dst);
    reject_path_traversal(&resolved_dst)?;
    let dst_in_rootfs = rootfs_dir.join(resolved_dst.trim_start_matches('/'));
    // The destination must not escape the rootfs via `..` or a base-image
    // symlink whose target leaves it (a write would land on the host).
    assert_within(rootfs_dir, &dst_in_rootfs)?;

    // Ensure destination directory exists
    if dst.ends_with('/') || src_patterns.len() > 1 {
        std::fs::create_dir_all(&dst_in_rootfs).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to create COPY destination {}: {}",
                dst_in_rootfs.display(),
                e
            ))
        })?;
    } else if let Some(parent) = dst_in_rootfs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            BoxError::BuildError(format!("Failed to create parent directory: {}", e))
        })?;
    }

    // Copy each source
    for src in src_patterns {
        // Resolve the source relative to the context (or, for COPY --from, the
        // source stage's rootfs). A leading "/" must NOT be treated as a host
        // absolute path: `Path::join` discards the base for an absolute arg, so
        // `rootfs.join("/run.sh")` would wrongly become "/run.sh". COPY --from
        // sources are conventionally absolute, so strip the leading slash.
        reject_path_traversal(src)?;
        let rel = PathBuf::from(if src == "." {
            ""
        } else {
            src.trim_start_matches('/')
        });
        let src_path = context_dir.join(src.trim_start_matches('/'));
        if !src_path.exists() {
            return Err(BoxError::BuildError(format!(
                "COPY source not found: {} (in context {})",
                src,
                context_dir.display()
            )));
        }
        // A source must resolve inside the build context (no `..`/symlink escape
        // that would bake a host file outside the context into the image).
        assert_within(context_dir, &src_path)?;

        // A single source excluded by .dockerignore is not in the build context.
        if let Some(ign) = ignore {
            if !rel.as_os_str().is_empty() && src_path.is_file() && ign.is_excluded(&rel) {
                return Err(BoxError::BuildError(format!(
                    "COPY source not found: {} (excluded by .dockerignore)",
                    src
                )));
            }
        }

        if src_path.is_dir() {
            copy_dir_filtered(&src_path, &dst_in_rootfs, &rel, ignore)?;
        } else {
            // If dst ends with / or is a directory, copy into it
            let target = if dst_in_rootfs.is_dir() {
                dst_in_rootfs.join(
                    src_path
                        .file_name()
                        .unwrap_or_else(|| std::ffi::OsStr::new(src)),
                )
            } else {
                dst_in_rootfs.clone()
            };
            std::fs::copy(&src_path, &target).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to copy {} to {}: {}",
                    src_path.display(),
                    target.display(),
                    e
                ))
            })?;
        }
    }

    // Resolve --chown uid/gid (header-level, no host filesystem ownership change
    // required — Docker BuildKit sets tar headers rather than calling chown).
    let chown_ids = if let Some(spec) = chown {
        Some(resolve_chown(spec, rootfs_dir)?)
    } else {
        None
    };

    // Create a layer from the copied files
    let layer_path = layers_dir.join(format!("layer_{}.tar.gz", layer_index));
    let target_prefix = Path::new(resolved_dst.trim_start_matches('/'));
    if dst_in_rootfs.is_dir() {
        create_layer_from_dir_with_chown(&dst_in_rootfs, target_prefix, &layer_path, chown_ids)
    } else if dst_in_rootfs.parent().is_some() {
        let changed = vec![PathBuf::from(
            dst_in_rootfs
                .strip_prefix(rootfs_dir)
                .unwrap_or(target_prefix),
        )];
        create_layer_with_chown(rootfs_dir, &changed, &[], &layer_path, chown_ids)
    } else {
        Err(BoxError::BuildError("Invalid COPY destination".to_string()))
    }
}

/// Handle RUN: execute a command in the rootfs.
///
/// On Linux, uses chroot. On macOS, isolated RUN execution is not implemented yet.
/// Returns Some(LayerInfo) if a layer was created, None if skipped.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_run(
    command: &RunCommand,
    cache_mounts: &[RunCacheMount],
    bind_mounts: &[RunBindMount],
    tmpfs_mounts: &[RunTmpfsMount],
    context_dir: &Path,
    completed_stages: &[(Option<String>, PathBuf)],
    rootfs_dir: &Path,
    layers_dir: &Path,
    workdir: &str,
    env: &[(String, String)],
    shell: &[String],
    layer_index: usize,
    quiet: bool,
    ignore: Option<&DockerIgnore>,
) -> Result<Option<LayerInfo>> {
    #[cfg(target_os = "macos")]
    {
        if !unsafe_host_run_enabled() {
            return Err(BoxError::BuildError(format!(
                "Dockerfile RUN is not supported on macOS yet because isolated Linux build \
                 execution is not implemented locally. Re-run on Linux, delegate with \
                 `a3s-box build --builder=buildkit-vm`, or set {UNSAFE_HOST_RUN_ENV}=1 \
                 to opt into unsafe host-side execution for local experiments."
            )));
        }

        handle_run_on_host_unsafe(
            command,
            cache_mounts,
            bind_mounts,
            tmpfs_mounts,
            context_dir,
            completed_stages,
            rootfs_dir,
            layers_dir,
            workdir,
            env,
            shell,
            layer_index,
            quiet,
            ignore,
        )
    }

    // Linux: execute via chroot
    #[cfg(target_os = "linux")]
    {
        use super::super::layer::DirSnapshot;

        validate_linux_run_preconditions(rootfs_dir, command, shell, linux_effective_uid())?;
        prepare_linux_run_filesystem(rootfs_dir)?;
        ensure_linux_run_workdir(rootfs_dir, workdir)?;
        ensure_run_cache_mount_targets(rootfs_dir, cache_mounts)?;

        let before = DirSnapshot::capture(rootfs_dir)?;
        let bind_mount_guard = RunBindMountOverlays::activate(
            rootfs_dir,
            context_dir,
            completed_stages,
            bind_mounts,
            workdir,
            ignore,
        )?;
        let tmpfs_mount_guard = RunTmpfsMountOverlays::activate(rootfs_dir, tmpfs_mounts, workdir)?;
        let run_mounts = LinuxRunMounts::mount(rootfs_dir)?;
        let run_mounts =
            run_mounts.with_cache_mounts(rootfs_dir, cache_mounts, completed_stages)?;

        let output = execute_linux_run_command(rootfs_dir, command, workdir, env, shell)?;

        if !output.status.success() {
            return Err(run_command_failed_error(
                &run_command_to_string(command),
                &output,
            ));
        }
        print_run_output(&output, quiet);
        run_mounts.unmount()?;
        tmpfs_mount_guard.restore()?;
        bind_mount_guard.restore()?;

        // Capture diff
        let after = DirSnapshot::capture(rootfs_dir)?;
        let changed = filter_run_mount_paths(
            before.diff(&after),
            cache_mounts,
            bind_mounts,
            tmpfs_mounts,
            workdir,
        );
        let deleted = filter_run_mount_paths(
            before.deletions(&after),
            cache_mounts,
            bind_mounts,
            tmpfs_mounts,
            workdir,
        );

        if changed.is_empty() && deleted.is_empty() {
            return Ok(None);
        }

        let layer_path = layers_dir.join(format!("layer_{}.tar.gz", layer_index));
        let layer_info = create_layer_with_deletions(rootfs_dir, &changed, &deleted, &layer_path)?;
        Ok(Some(layer_info))
    }

    // Other platforms: not supported
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (
            rootfs_dir,
            layers_dir,
            cache_mounts,
            bind_mounts,
            tmpfs_mounts,
            context_dir,
            completed_stages,
            workdir,
            env,
            shell,
            layer_index,
            quiet,
            ignore,
        );
        Err(BoxError::BuildError(format!(
            "Dockerfile RUN is not supported on this platform yet because isolated Linux build execution is not implemented: {}",
            run_command_to_string(command)
        )))
    }
}

/// Handle RUN through a warm-pool VM lease.
///
/// The build stage rootfs is mounted into the leased helper VM and the command
/// executes with `ExecRequest.rootfs`, so mutations land back in `rootfs_dir`.
#[cfg(feature = "pool")]
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_run_with_pool(
    command: &RunCommand,
    cache_mounts: &[RunCacheMount],
    bind_mounts: &[RunBindMount],
    tmpfs_mounts: &[RunTmpfsMount],
    context_dir: &Path,
    completed_stages: &[(Option<String>, PathBuf)],
    rootfs_dir: &Path,
    layers_dir: &Path,
    workdir: &str,
    env: &[(String, String)],
    shell: &[String],
    user: Option<&str>,
    layer_index: usize,
    quiet: bool,
    session: &super::BuildRunPoolSession,
    ignore: Option<&DockerIgnore>,
) -> Result<Option<LayerInfo>> {
    use super::super::layer::DirSnapshot;

    validate_run_command_preconditions(rootfs_dir, command, shell)?;
    prepare_pool_run_filesystem(rootfs_dir)?;
    ensure_linux_run_workdir(rootfs_dir, workdir)?;
    ensure_run_cache_mount_targets(rootfs_dir, cache_mounts)?;

    let before = DirSnapshot::capture(rootfs_dir)?;
    let bind_mount_guard = RunBindMountOverlays::activate(
        rootfs_dir,
        context_dir,
        completed_stages,
        bind_mounts,
        workdir,
        ignore,
    )?;
    let tmpfs_mount_guard = RunTmpfsMountOverlays::activate(rootfs_dir, tmpfs_mounts, workdir)?;
    let cache_mount_guard = PoolRunCacheMounts::activate_with_cache_root(
        rootfs_dir,
        cache_mounts,
        &session.run_cache_dir,
        completed_stages,
    )?;
    let output = match session
        .lease
        .exec(crate::pool::PoolLeaseExec {
            cmd: build_pool_run_cmd(command, shell, workdir),
            timeout_ns: Some(session.timeout_ns),
            env: run_env_entries(env),
            working_dir: build_pool_run_workdir(command, workdir),
            rootfs: Some(session.guest_rootfs.clone()),
            stdin: None,
            user: user.map(str::to_string),
        })
        .await
    {
        Ok(output) => output,
        Err(error) => {
            cache_mount_guard.restore_without_sync()?;
            tmpfs_mount_guard.restore()?;
            bind_mount_guard.restore()?;
            return Err(BoxError::BuildError(format!(
                "Failed to execute RUN in warm pool: {error}"
            )));
        }
    };

    if output.exit_code != 0 {
        cache_mount_guard.restore_without_sync()?;
        tmpfs_mount_guard.restore()?;
        bind_mount_guard.restore()?;
        return Err(run_command_failed_error_parts(
            &run_command_to_string(command),
            output.exit_code,
            &output.stdout,
            &output.stderr,
        ));
    }
    cache_mount_guard.restore()?;
    tmpfs_mount_guard.restore()?;
    bind_mount_guard.restore()?;
    print_output_parts(&output.stdout, &output.stderr, quiet);

    let after = DirSnapshot::capture(rootfs_dir)?;
    let changed = filter_run_mount_paths(
        before.diff(&after),
        cache_mounts,
        bind_mounts,
        tmpfs_mounts,
        workdir,
    );
    let deleted = filter_run_mount_paths(
        before.deletions(&after),
        cache_mounts,
        bind_mounts,
        tmpfs_mounts,
        workdir,
    );

    if changed.is_empty() && deleted.is_empty() {
        return Ok(None);
    }

    let layer_path = layers_dir.join(format!("layer_{}.tar.gz", layer_index));
    let layer_info = create_layer_with_deletions(rootfs_dir, &changed, &deleted, &layer_path)?;
    Ok(Some(layer_info))
}

#[cfg(not(feature = "pool"))]
#[allow(clippy::too_many_arguments)]
pub(super) async fn handle_run_with_pool(
    command: &RunCommand,
    _cache_mounts: &[RunCacheMount],
    _bind_mounts: &[RunBindMount],
    _tmpfs_mounts: &[RunTmpfsMount],
    _context_dir: &Path,
    _completed_stages: &[(Option<String>, PathBuf)],
    _rootfs_dir: &Path,
    _layers_dir: &Path,
    _workdir: &str,
    _env: &[(String, String)],
    _shell: &[String],
    _user: Option<&str>,
    _layer_index: usize,
    _quiet: bool,
    _session: &super::BuildRunPoolSession,
    _ignore: Option<&DockerIgnore>,
) -> Result<Option<LayerInfo>> {
    Err(BoxError::BuildError(format!(
        "Dockerfile RUN warm-pool execution requires the runtime 'pool' feature: {}",
        run_command_to_string(command)
    )))
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn shell_command_in_workdir(workdir: &str, command: &str) -> String {
    let workdir = if workdir.trim().is_empty() {
        "/"
    } else {
        workdir
    };
    if workdir == "/" {
        command.to_string()
    } else {
        format!("cd {} && {}", shell_quote(workdir), command)
    }
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn shell_quote(value: &str) -> String {
    let mut out = String::from("'");
    for ch in value.chars() {
        if ch == '\'' {
            out.push_str("'\\''");
        } else {
            out.push(ch);
        }
    }
    out.push('\'');
    out
}

#[cfg_attr(all(not(feature = "pool"), not(target_os = "linux")), allow(dead_code))]
fn normalized_run_workdir(workdir: &str) -> &str {
    if workdir.trim().is_empty() {
        "/"
    } else {
        workdir
    }
}

fn run_command_to_string(command: &RunCommand) -> String {
    match command {
        RunCommand::Shell(command) => command.clone(),
        RunCommand::Exec(exec) => serde_json::to_string(exec).unwrap_or_else(|_| {
            exec.iter()
                .map(String::as_str)
                .collect::<Vec<_>>()
                .join(" ")
        }),
    }
}

fn linux_run_shell_path(shell: &[String]) -> &str {
    shell.first().map(String::as_str).unwrap_or("/bin/sh")
}

fn validate_run_command_preconditions(
    rootfs_dir: &Path,
    command: &RunCommand,
    shell: &[String],
) -> Result<()> {
    match command {
        RunCommand::Shell(_) => validate_run_shell_preconditions(rootfs_dir, shell),
        RunCommand::Exec(exec) => validate_run_exec_preconditions(rootfs_dir, exec),
    }
}

fn validate_run_shell_preconditions(rootfs_dir: &Path, shell: &[String]) -> Result<()> {
    let shell_path = linux_run_shell_path(shell);
    if !shell_path.starts_with('/') {
        return Err(BoxError::BuildError(format!(
            "Dockerfile RUN shell '{}' is not absolute; SHELL must name an absolute in-rootfs executable",
            shell_path
        )));
    }
    let shell_in_rootfs = rootfs_dir.join(shell_path.trim_start_matches('/'));
    if std::fs::symlink_metadata(&shell_in_rootfs).is_err() {
        return Err(BoxError::BuildError(format!(
            "Dockerfile RUN shell '{}' was not found in rootfs at {}; the base image must contain the configured shell",
            shell_path,
            shell_in_rootfs.display()
        )));
    }

    Ok(())
}

fn validate_run_exec_preconditions(rootfs_dir: &Path, exec: &[String]) -> Result<()> {
    let executable = exec.first().ok_or_else(|| {
        BoxError::BuildError("Dockerfile RUN exec form requires at least one argument".to_string())
    })?;
    if executable.is_empty() {
        return Err(BoxError::BuildError(
            "Dockerfile RUN exec form executable cannot be empty".to_string(),
        ));
    }
    if executable.starts_with('/') {
        let executable_in_rootfs = rootfs_dir.join(executable.trim_start_matches('/'));
        if std::fs::symlink_metadata(&executable_in_rootfs).is_err() {
            return Err(BoxError::BuildError(format!(
                "Dockerfile RUN exec form executable '{}' was not found in rootfs at {}",
                executable,
                executable_in_rootfs.display()
            )));
        }
    }

    Ok(())
}

#[cfg_attr(not(target_os = "linux"), allow(dead_code))]
fn validate_linux_run_preconditions(
    rootfs_dir: &Path,
    command: &RunCommand,
    shell: &[String],
    effective_uid: u32,
) -> Result<()> {
    if effective_uid != 0 {
        return Err(BoxError::BuildError(
            "Dockerfile RUN on Linux requires root privileges because the current isolated build path uses chroot. Re-run as root or build on a root-capable builder.".to_string(),
        ));
    }

    validate_run_command_preconditions(rootfs_dir, command, shell)
}

#[cfg(target_os = "linux")]
fn linux_effective_uid() -> u32 {
    unsafe { libc::geteuid() }
}

#[cfg_attr(
    all(not(feature = "pool"), not(target_os = "linux"), not(test)),
    allow(dead_code)
)]
fn ensure_linux_run_workdir(rootfs_dir: &Path, workdir: &str) -> Result<PathBuf> {
    let workdir = if workdir.trim().is_empty() {
        "/"
    } else {
        workdir
    };
    if !workdir.starts_with('/') {
        return Err(BoxError::BuildError(format!(
            "Dockerfile RUN workdir '{}' is not absolute",
            workdir
        )));
    }

    let workdir_path = rootfs_dir.join(workdir.trim_start_matches('/'));
    std::fs::create_dir_all(&workdir_path).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to create RUN workdir {}: {}",
            workdir_path.display(),
            e
        ))
    })?;
    Ok(workdir_path)
}

#[cfg_attr(all(not(feature = "pool"), not(test)), allow(dead_code))]
fn build_run_shell_cmd(shell: &[String], workdir: &str, command: &str) -> Vec<String> {
    let run_command = shell_command_in_workdir(workdir, command);
    if shell.len() >= 2 {
        let mut cmd = shell.to_vec();
        cmd.push(run_command);
        cmd
    } else if shell.len() == 1 {
        vec![shell[0].clone(), run_command]
    } else {
        vec!["/bin/sh".to_string(), "-c".to_string(), run_command]
    }
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
fn build_pool_run_cmd(command: &RunCommand, shell: &[String], workdir: &str) -> Vec<String> {
    match command {
        RunCommand::Shell(command) => build_run_shell_cmd(shell, workdir, command),
        RunCommand::Exec(exec) => exec.to_vec(),
    }
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
fn build_pool_run_workdir(command: &RunCommand, workdir: &str) -> Option<String> {
    match command {
        RunCommand::Shell(_) => Some("/".to_string()),
        RunCommand::Exec(_) => Some(normalized_run_workdir(workdir).to_string()),
    }
}

#[cfg_attr(all(not(feature = "pool"), not(test)), allow(dead_code))]
fn run_env_entries(env: &[(String, String)]) -> Vec<String> {
    let mut entries = vec![
        "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin".to_string(),
        "HOME=/root".to_string(),
    ];
    entries.extend(env.iter().map(|(key, value)| format!("{key}={value}")));
    entries
}

#[cfg(target_os = "linux")]
fn execute_linux_run_command(
    rootfs_dir: &Path,
    command: &RunCommand,
    workdir: &str,
    env: &[(String, String)],
    shell: &[String],
) -> Result<std::process::Output> {
    match command {
        RunCommand::Shell(command) => {
            let mut cmd = std::process::Command::new("chroot");
            cmd.arg(rootfs_dir);
            if shell.len() >= 2 {
                cmd.arg(&shell[0]);
                for arg in &shell[1..] {
                    cmd.arg(arg);
                }
            } else if shell.len() == 1 {
                cmd.arg(&shell[0]);
            } else {
                cmd.arg("/bin/sh");
                cmd.arg("-c");
            }
            let run_command = shell_command_in_workdir(workdir, command);
            cmd.arg(&run_command);
            configure_run_command_env(&mut cmd, env);
            cmd.output()
                .map_err(|e| BoxError::BuildError(format!("Failed to execute RUN command: {}", e)))
        }
        RunCommand::Exec(exec) => execute_linux_run_exec_form(rootfs_dir, exec, workdir, env),
    }
}

#[cfg(target_os = "linux")]
fn execute_linux_run_exec_form(
    rootfs_dir: &Path,
    exec: &[String],
    workdir: &str,
    env: &[(String, String)],
) -> Result<std::process::Output> {
    use std::os::unix::process::CommandExt;

    validate_run_exec_preconditions(rootfs_dir, exec)?;
    let mut cmd = std::process::Command::new(&exec[0]);
    cmd.args(&exec[1..]);
    configure_run_command_env(&mut cmd, env);

    let rootfs = path_cstring(rootfs_dir, "RUN rootfs")?;
    let workdir = std::ffi::CString::new(normalized_run_workdir(workdir))
        .map_err(|_| BoxError::BuildError("Dockerfile RUN workdir contains NUL".to_string()))?;
    unsafe {
        cmd.pre_exec(move || {
            if libc::chroot(rootfs.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            if libc::chdir(workdir.as_ptr()) != 0 {
                return Err(std::io::Error::last_os_error());
            }
            Ok(())
        });
    }

    cmd.output()
        .map_err(|e| BoxError::BuildError(format!("Failed to execute RUN exec form: {}", e)))
}

#[cfg(target_os = "linux")]
fn configure_run_command_env(cmd: &mut std::process::Command, env: &[(String, String)]) {
    cmd.env_clear();
    cmd.env(
        "PATH",
        "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
    );
    cmd.env("HOME", "/root");
    for (key, value) in env {
        cmd.env(key, value);
    }
}

fn ensure_run_cache_mount_targets(rootfs_dir: &Path, cache_mounts: &[RunCacheMount]) -> Result<()> {
    for mount in cache_mounts {
        let target = run_cache_mount_target(rootfs_dir, mount)?;
        std::fs::create_dir_all(&target).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to create RUN cache mount target {}: {}",
                target.display(),
                e
            ))
        })?;
    }
    Ok(())
}

fn run_bind_mount_source(
    source_root: &Path,
    source_label: &str,
    mount: &RunBindMount,
    ignore: Option<&DockerIgnore>,
) -> Result<(PathBuf, PathBuf)> {
    reject_path_traversal(&mount.source)?;
    let rel = normalized_context_rel(&mount.source);
    let source_path = source_root.join(&rel);
    if !source_path.exists() {
        return Err(BoxError::BuildError(format!(
            "RUN bind mount source not found: {} (in {})",
            mount.source, source_label
        )));
    }
    assert_within(source_root, &source_path)?;
    if let Some(ign) = ignore {
        if !rel.as_os_str().is_empty() && ign.is_excluded(&rel) {
            return Err(BoxError::BuildError(format!(
                "RUN bind mount source not found: {} (excluded by .dockerignore)",
                mount.source
            )));
        }
    }
    Ok((source_path, rel))
}

fn run_bind_mount_target(
    rootfs_dir: &Path,
    workdir: &str,
    mount: &RunBindMount,
) -> Result<(PathBuf, PathBuf)> {
    let resolved = resolve_path(normalized_run_workdir(workdir), &mount.target);
    reject_path_traversal(&resolved)?;
    let rel = normalized_rootfs_rel(&resolved);
    if rel.as_os_str().is_empty() {
        return Err(BoxError::BuildError(format!(
            "RUN bind mount target '{}' resolves to /, which is not supported by the warm-pool build overlay",
            mount.target
        )));
    }
    let target = rootfs_dir.join(&rel);
    assert_within(rootfs_dir, &target)?;
    Ok((target, rel))
}

fn run_tmpfs_mount_target(
    rootfs_dir: &Path,
    workdir: &str,
    mount: &RunTmpfsMount,
) -> Result<(PathBuf, PathBuf)> {
    let resolved = resolve_path(normalized_run_workdir(workdir), &mount.target);
    reject_path_traversal(&resolved)?;
    let rel = normalized_rootfs_rel(&resolved);
    if rel.as_os_str().is_empty() {
        return Err(BoxError::BuildError(format!(
            "RUN tmpfs mount target '{}' resolves to /, which is not supported by the warm-pool build overlay",
            mount.target
        )));
    }
    let target = rootfs_dir.join(&rel);
    assert_within(rootfs_dir, &target)?;
    Ok((target, rel))
}

fn normalized_context_rel(path: &str) -> PathBuf {
    let trimmed = path.trim_start_matches('/');
    if trimmed == "." {
        PathBuf::new()
    } else {
        normalize_rel_components(Path::new(trimmed))
    }
}

fn normalized_rootfs_rel(path: &str) -> PathBuf {
    normalize_rel_components(Path::new(path.trim_start_matches('/')))
}

fn normalize_rel_components(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        if let std::path::Component::Normal(part) = component {
            out.push(part);
        }
    }
    out
}

fn copy_run_bind_mount_source(
    source: &Path,
    source_rel: &Path,
    target: &Path,
    ignore: Option<&DockerIgnore>,
) -> Result<()> {
    let meta = std::fs::symlink_metadata(source).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to inspect RUN bind mount source {}: {}",
            source.display(),
            e
        ))
    })?;

    if meta.is_dir() {
        copy_dir_filtered(source, target, source_rel, ignore)?;
        return Ok(());
    }

    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to create RUN bind mount target parent {}: {}",
                parent.display(),
                e
            ))
        })?;
    }

    if meta.file_type().is_symlink() {
        copy_symlink(source, target)
    } else {
        std::fs::copy(source, target).map(|_| ()).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to copy RUN bind mount source {} to {}: {}",
                source.display(),
                target.display(),
                e
            ))
        })
    }
}

fn copy_symlink(source: &Path, target: &Path) -> Result<()> {
    let link_target = std::fs::read_link(source).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to read RUN bind mount symlink {}: {}",
            source.display(),
            e
        ))
    })?;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&link_target, target).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to create RUN bind mount symlink {} -> {}: {}",
                target.display(),
                link_target.display(),
                e
            ))
        })
    }
    #[cfg(not(unix))]
    {
        std::fs::write(target, Vec::new()).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to create RUN bind mount symlink placeholder {} -> {}: {}",
                target.display(),
                link_target.display(),
                e
            ))
        })
    }
}

fn run_cache_mount_target(rootfs_dir: &Path, mount: &RunCacheMount) -> Result<PathBuf> {
    reject_path_traversal(&mount.target)?;
    let target = rootfs_dir.join(mount.target.trim_start_matches('/'));
    assert_within(rootfs_dir, &target)?;
    Ok(target)
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
fn filter_run_mount_paths(
    paths: Vec<PathBuf>,
    cache_mounts: &[RunCacheMount],
    bind_mounts: &[RunBindMount],
    tmpfs_mounts: &[RunTmpfsMount],
    workdir: &str,
) -> Vec<PathBuf> {
    if cache_mounts.is_empty() && bind_mounts.is_empty() && tmpfs_mounts.is_empty() {
        return paths;
    }

    let mut exact_paths = Vec::new();
    let mut subtree_paths = Vec::new();
    for mount in cache_mounts {
        let mount_path = PathBuf::from(mount.target.trim_start_matches('/'));
        subtree_paths.push(mount_path.clone());
        for ancestor in mount_path.ancestors() {
            if !ancestor.as_os_str().is_empty() {
                exact_paths.push(ancestor.to_path_buf());
            }
        }
    }
    for mount in bind_mounts {
        let resolved = resolve_path(normalized_run_workdir(workdir), &mount.target);
        let mount_path = normalized_rootfs_rel(&resolved);
        subtree_paths.push(mount_path.clone());
        for ancestor in mount_path.ancestors() {
            if !ancestor.as_os_str().is_empty() {
                exact_paths.push(ancestor.to_path_buf());
            }
        }
    }
    for mount in tmpfs_mounts {
        let resolved = resolve_path(normalized_run_workdir(workdir), &mount.target);
        let mount_path = normalized_rootfs_rel(&resolved);
        subtree_paths.push(mount_path.clone());
        for ancestor in mount_path.ancestors() {
            if !ancestor.as_os_str().is_empty() {
                exact_paths.push(ancestor.to_path_buf());
            }
        }
    }
    exact_paths.sort();
    exact_paths.dedup();
    subtree_paths.sort();
    subtree_paths.dedup();
    paths
        .into_iter()
        .filter(|path| {
            !exact_paths.iter().any(|mount_path| path == mount_path)
                && !subtree_paths
                    .iter()
                    .any(|mount_path| path == mount_path || path.starts_with(mount_path))
        })
        .collect()
}

fn run_cache_mount_dir(cache_root: &Path, mount: &RunCacheMount) -> PathBuf {
    let id = mount.id.as_deref().unwrap_or(&mount.target);
    cache_root.join(sha256_bytes(id.as_bytes()))
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
fn run_cache_mount_seed_source(
    completed_stages: &[(Option<String>, PathBuf)],
    mount: &RunCacheMount,
) -> Result<Option<PathBuf>> {
    let Some(from_ref) = mount.from.as_deref() else {
        return Ok(None);
    };

    reject_path_traversal(&mount.source)?;
    let rel = normalized_context_rel(&mount.source);
    let source_root = resolve_stage_rootfs(from_ref, completed_stages)?;
    let source = source_root.join(&rel);
    if !source.exists() {
        return Err(BoxError::BuildError(format!(
            "RUN cache mount seed source not found: {} (in from={})",
            mount.source, from_ref
        )));
    }
    assert_within(source_root, &source)?;
    if !source.is_dir() {
        return Err(BoxError::BuildError(format!(
            "RUN cache mount seed source must be a directory: {} (in from={})",
            mount.source, from_ref
        )));
    }
    Ok(Some(source))
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
fn seed_run_cache_mount(
    cache_dir: &Path,
    mount: &RunCacheMount,
    completed_stages: &[(Option<String>, PathBuf)],
) -> Result<()> {
    if cache_dir.exists() {
        return Ok(());
    }

    let Some(seed_source) = run_cache_mount_seed_source(completed_stages, mount)? else {
        return Ok(());
    };

    let parent = cache_dir.parent().ok_or_else(|| {
        BoxError::BuildError(format!(
            "RUN cache directory has no parent: {}",
            cache_dir.display()
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to create RUN cache parent {}: {}",
            parent.display(),
            e
        ))
    })?;
    copy_run_cache_seed_to(&seed_source, cache_dir)
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
fn copy_run_cache_seed_to(seed_source: &Path, cache_dir: &Path) -> Result<()> {
    crate::cache::layer_cache::copy_dir_recursive(seed_source, cache_dir).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to seed RUN cache mount {} from {}: {}",
            cache_dir.display(),
            seed_source.display(),
            e
        ))
    })
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
fn hydrate_run_cache_mount(cache_dir: &Path, target: &Path) -> Result<()> {
    if !cache_dir.exists() {
        return Ok(());
    }
    if !cache_dir.is_dir() {
        return Err(BoxError::BuildError(format!(
            "RUN cache mount {} is not a directory",
            cache_dir.display()
        )));
    }
    remove_path_any(target)?;
    crate::cache::layer_cache::copy_dir_recursive(cache_dir, target).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to hydrate RUN cache mount {} from {}: {}",
            target.display(),
            cache_dir.display(),
            e
        ))
    })
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
fn sync_run_cache_mount(target: &Path, cache_dir: &Path) -> Result<()> {
    let parent = cache_dir.parent().ok_or_else(|| {
        BoxError::BuildError(format!(
            "RUN cache directory has no parent: {}",
            cache_dir.display()
        ))
    })?;
    std::fs::create_dir_all(parent).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to create RUN cache parent {}: {}",
            parent.display(),
            e
        ))
    })?;

    let staging = tempfile::Builder::new()
        .prefix(".run-cache-staging-")
        .tempdir_in(parent)
        .map_err(|e| {
            BoxError::BuildError(format!("Failed to create RUN cache staging dir: {e}"))
        })?;
    let staged = staging.path().join("cache");
    if target.exists() {
        crate::cache::layer_cache::copy_dir_recursive(target, &staged).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to stage RUN cache mount {} into {}: {}",
                target.display(),
                staged.display(),
                e
            ))
        })?;
    } else {
        std::fs::create_dir_all(&staged).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to create empty RUN cache staging dir {}: {}",
                staged.display(),
                e
            ))
        })?;
    }

    remove_path_any(cache_dir)?;
    std::fs::rename(&staged, cache_dir).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to publish RUN cache mount {}: {}",
            cache_dir.display(),
            e
        ))
    })
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
struct PoolRunCacheMounts {
    staging_dir: Option<PathBuf>,
    overlays: Vec<PoolRunCacheMountOverlay>,
    restored: bool,
    sync_cache: bool,
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
struct PoolRunCacheMountOverlay {
    target: PathBuf,
    backup: PathBuf,
    cache_dir: PathBuf,
    _lock: crate::file_lock::FileLock,
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
impl PoolRunCacheMounts {
    fn activate_with_cache_root(
        rootfs_dir: &Path,
        cache_mounts: &[RunCacheMount],
        cache_root: &Path,
        completed_stages: &[(Option<String>, PathBuf)],
    ) -> Result<Self> {
        let mut mounts = Self {
            staging_dir: None,
            overlays: Vec::new(),
            restored: false,
            sync_cache: false,
        };

        if cache_mounts.is_empty() {
            return Ok(mounts);
        }

        std::fs::create_dir_all(cache_root).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to create RUN cache root {}: {}",
                cache_root.display(),
                e
            ))
        })?;
        let staging_dir = rootfs_dir
            .join(".a3s-box-run-cache-overlays")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&staging_dir).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to create RUN cache mount staging dir {}: {}",
                staging_dir.display(),
                e
            ))
        })?;
        mounts.staging_dir = Some(staging_dir.clone());

        for (idx, mount) in cache_mounts.iter().enumerate() {
            let target = run_cache_mount_target(rootfs_dir, mount)?;
            let backup = staging_dir.join(format!("target-{idx}"));
            let cache_dir = run_cache_mount_dir(cache_root, mount);
            if mounts
                .overlays
                .iter()
                .any(|overlay| overlay.cache_dir == cache_dir)
            {
                return Err(BoxError::BuildError(format!(
                    "Duplicate RUN cache mount id/target for {}",
                    mount.raw
                )));
            }
            // The warm-pool cache mount is a host-side hydrate/publish overlay,
            // not BuildKit's live shared directory, so even `sharing=shared`
            // serializes one cache key to avoid losing concurrent writeback.
            let lock = crate::file_lock::FileLock::acquire(&cache_dir).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to lock RUN cache mount {}: {}",
                    cache_dir.display(),
                    e
                ))
            })?;
            seed_run_cache_mount(&cache_dir, mount, completed_stages)?;
            std::fs::rename(&target, &backup).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to hide RUN cache mount target {}: {}",
                    target.display(),
                    e
                ))
            })?;
            mounts.overlays.push(PoolRunCacheMountOverlay {
                target: target.clone(),
                backup,
                cache_dir: cache_dir.clone(),
                _lock: lock,
            });
            std::fs::create_dir_all(&target).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to activate RUN cache mount target {}: {}",
                    target.display(),
                    e
                ))
            })?;
            hydrate_run_cache_mount(&cache_dir, &target)?;
            apply_run_cache_mount_metadata(&target, mount)?;
        }

        mounts.sync_cache = true;
        Ok(mounts)
    }

    fn restore(mut self) -> Result<()> {
        let result = self.restore_inner();
        if result.is_ok() {
            self.restored = true;
        }
        result
    }

    fn restore_without_sync(mut self) -> Result<()> {
        self.sync_cache = false;
        let result = self.restore_inner();
        if result.is_ok() {
            self.restored = true;
        }
        result
    }

    fn restore_inner(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }

        let mut first_error = None;
        for overlay in self.overlays.iter().rev() {
            if self.sync_cache {
                if let Err(error) = sync_run_cache_mount(&overlay.target, &overlay.cache_dir) {
                    first_error.get_or_insert(error);
                }
            }
            if let Err(error) = remove_path_any(&overlay.target) {
                first_error.get_or_insert(error);
                continue;
            }
            if let Err(error) = std::fs::rename(&overlay.backup, &overlay.target).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to restore RUN cache mount target {}: {}",
                    overlay.target.display(),
                    e
                ))
            }) {
                first_error.get_or_insert(error);
            }
        }

        if let Some(staging_dir) = &self.staging_dir {
            if let Err(error) = std::fs::remove_dir_all(staging_dir).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to remove RUN cache mount staging dir {}: {}",
                    staging_dir.display(),
                    e
                ))
            }) {
                first_error.get_or_insert(error);
            }
            if let Some(parent) = staging_dir.parent() {
                match std::fs::remove_dir(parent) {
                    Ok(()) => {}
                    Err(err)
                        if matches!(
                            err.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                        ) => {}
                    Err(err) => {
                        first_error.get_or_insert(BoxError::BuildError(format!(
                            "Failed to remove RUN cache mount staging parent {}: {}",
                            parent.display(),
                            err
                        )));
                    }
                }
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => {
                self.restored = true;
                Ok(())
            }
        }
    }
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
impl Drop for PoolRunCacheMounts {
    fn drop(&mut self) {
        let _ = self.restore_inner();
    }
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
struct RunBindMountOverlays {
    staging_dir: Option<PathBuf>,
    overlays: Vec<RunBindMountOverlay>,
    restored: bool,
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
struct RunBindMountOverlay {
    target: PathBuf,
    backup: Option<PathBuf>,
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
impl RunBindMountOverlays {
    #[cfg(test)]
    fn activate_context(
        rootfs_dir: &Path,
        context_dir: &Path,
        bind_mounts: &[RunBindMount],
        workdir: &str,
        ignore: Option<&DockerIgnore>,
    ) -> Result<Self> {
        Self::activate(rootfs_dir, context_dir, &[], bind_mounts, workdir, ignore)
    }

    fn activate(
        rootfs_dir: &Path,
        context_dir: &Path,
        completed_stages: &[(Option<String>, PathBuf)],
        bind_mounts: &[RunBindMount],
        workdir: &str,
        ignore: Option<&DockerIgnore>,
    ) -> Result<Self> {
        let mut mounts = Self {
            staging_dir: None,
            overlays: Vec::new(),
            restored: false,
        };

        if bind_mounts.is_empty() {
            return Ok(mounts);
        }

        let staging_dir = rootfs_dir
            .join(".a3s-box-run-bind-overlays")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&staging_dir).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to create RUN bind mount staging dir {}: {}",
                staging_dir.display(),
                e
            ))
        })?;
        mounts.staging_dir = Some(staging_dir.clone());

        for (idx, mount) in bind_mounts.iter().enumerate() {
            let (source_root, source_ignore, source_label) = match mount.from.as_deref() {
                Some(from_ref) => {
                    let rootfs = resolve_stage_rootfs(from_ref, completed_stages)?;
                    (
                        rootfs,
                        None,
                        format!("stage '{}' rootfs {}", from_ref, rootfs.display()),
                    )
                }
                None => (
                    context_dir,
                    ignore,
                    format!("build context {}", context_dir.display()),
                ),
            };
            let (source, source_rel) =
                run_bind_mount_source(source_root, &source_label, mount, source_ignore)?;
            let (target, _target_rel) = run_bind_mount_target(rootfs_dir, workdir, mount)?;
            let backup = if target.exists() {
                let backup = staging_dir.join(format!("target-{idx}"));
                std::fs::rename(&target, &backup).map_err(|e| {
                    BoxError::BuildError(format!(
                        "Failed to hide RUN bind mount target {}: {}",
                        target.display(),
                        e
                    ))
                })?;
                Some(backup)
            } else {
                None
            };

            mounts.overlays.push(RunBindMountOverlay {
                target: target.clone(),
                backup,
            });
            copy_run_bind_mount_source(&source, &source_rel, &target, source_ignore)?;
        }

        Ok(mounts)
    }

    fn restore(mut self) -> Result<()> {
        let result = self.restore_inner();
        if result.is_ok() {
            self.restored = true;
        }
        result
    }

    fn restore_inner(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }

        let mut first_error = None;
        for overlay in self.overlays.iter().rev() {
            if let Err(error) = remove_path_any(&overlay.target) {
                first_error.get_or_insert(error);
            }
            if let Some(backup) = &overlay.backup {
                if let Err(error) = std::fs::rename(backup, &overlay.target).map_err(|e| {
                    BoxError::BuildError(format!(
                        "Failed to restore RUN bind mount target {}: {}",
                        overlay.target.display(),
                        e
                    ))
                }) {
                    first_error.get_or_insert(error);
                }
            }
        }

        if let Some(staging_dir) = &self.staging_dir {
            if let Err(error) = std::fs::remove_dir_all(staging_dir).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to remove RUN bind mount staging dir {}: {}",
                    staging_dir.display(),
                    e
                ))
            }) {
                first_error.get_or_insert(error);
            }
            if let Some(parent) = staging_dir.parent() {
                match std::fs::remove_dir(parent) {
                    Ok(()) => {}
                    Err(err)
                        if matches!(
                            err.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                        ) => {}
                    Err(err) => {
                        first_error.get_or_insert(BoxError::BuildError(format!(
                            "Failed to remove RUN bind mount staging parent {}: {}",
                            parent.display(),
                            err
                        )));
                    }
                }
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => {
                self.restored = true;
                Ok(())
            }
        }
    }
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
impl Drop for RunBindMountOverlays {
    fn drop(&mut self) {
        let _ = self.restore_inner();
    }
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
struct RunTmpfsMountOverlays {
    staging_dir: Option<PathBuf>,
    overlays: Vec<RunTmpfsMountOverlay>,
    restored: bool,
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
struct RunTmpfsMountOverlay {
    target: PathBuf,
    backup: Option<PathBuf>,
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
impl RunTmpfsMountOverlays {
    fn activate(rootfs_dir: &Path, tmpfs_mounts: &[RunTmpfsMount], workdir: &str) -> Result<Self> {
        let mut mounts = Self {
            staging_dir: None,
            overlays: Vec::new(),
            restored: false,
        };

        if tmpfs_mounts.is_empty() {
            return Ok(mounts);
        }

        let staging_dir = rootfs_dir
            .join(".a3s-box-run-tmpfs-overlays")
            .join(uuid::Uuid::new_v4().to_string());
        std::fs::create_dir_all(&staging_dir).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to create RUN tmpfs mount staging dir {}: {}",
                staging_dir.display(),
                e
            ))
        })?;
        mounts.staging_dir = Some(staging_dir.clone());

        for (idx, mount) in tmpfs_mounts.iter().enumerate() {
            let (target, _target_rel) = run_tmpfs_mount_target(rootfs_dir, workdir, mount)?;
            let backup = if target.exists() {
                let backup = staging_dir.join(format!("target-{idx}"));
                std::fs::rename(&target, &backup).map_err(|e| {
                    BoxError::BuildError(format!(
                        "Failed to hide RUN tmpfs mount target {}: {}",
                        target.display(),
                        e
                    ))
                })?;
                Some(backup)
            } else {
                None
            };

            mounts.overlays.push(RunTmpfsMountOverlay {
                target: target.clone(),
                backup,
            });
            std::fs::create_dir_all(&target).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to activate RUN tmpfs mount target {}: {}",
                    target.display(),
                    e
                ))
            })?;
        }

        Ok(mounts)
    }

    fn restore(mut self) -> Result<()> {
        let result = self.restore_inner();
        if result.is_ok() {
            self.restored = true;
        }
        result
    }

    fn restore_inner(&mut self) -> Result<()> {
        if self.restored {
            return Ok(());
        }

        let mut first_error = None;
        for overlay in self.overlays.iter().rev() {
            if let Err(error) = remove_path_any(&overlay.target) {
                first_error.get_or_insert(error);
            }
            if let Some(backup) = &overlay.backup {
                if let Err(error) = std::fs::rename(backup, &overlay.target).map_err(|e| {
                    BoxError::BuildError(format!(
                        "Failed to restore RUN tmpfs mount target {}: {}",
                        overlay.target.display(),
                        e
                    ))
                }) {
                    first_error.get_or_insert(error);
                }
            }
        }

        if let Some(staging_dir) = &self.staging_dir {
            if let Err(error) = std::fs::remove_dir_all(staging_dir).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to remove RUN tmpfs mount staging dir {}: {}",
                    staging_dir.display(),
                    e
                ))
            }) {
                first_error.get_or_insert(error);
            }
            if let Some(parent) = staging_dir.parent() {
                match std::fs::remove_dir(parent) {
                    Ok(()) => {}
                    Err(err)
                        if matches!(
                            err.kind(),
                            std::io::ErrorKind::NotFound | std::io::ErrorKind::DirectoryNotEmpty
                        ) => {}
                    Err(err) => {
                        first_error.get_or_insert(BoxError::BuildError(format!(
                            "Failed to remove RUN tmpfs mount staging parent {}: {}",
                            parent.display(),
                            err
                        )));
                    }
                }
            }
        }

        match first_error {
            Some(error) => Err(error),
            None => {
                self.restored = true;
                Ok(())
            }
        }
    }
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
impl Drop for RunTmpfsMountOverlays {
    fn drop(&mut self) {
        let _ = self.restore_inner();
    }
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
fn remove_path_any(path: &Path) -> Result<()> {
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.is_dir() && !meta.file_type().is_symlink() => {
            std::fs::remove_dir_all(path)
        }
        Ok(_) => std::fs::remove_file(path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        Err(err) => {
            return Err(BoxError::BuildError(format!(
                "Failed to inspect RUN cache mount target {}: {}",
                path.display(),
                err
            )));
        }
    }
    .map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to remove RUN cache mount target {}: {}",
            path.display(),
            e
        ))
    })
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
fn apply_run_cache_mount_metadata(target: &Path, mount: &RunCacheMount) -> Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        use std::os::unix::fs::PermissionsExt;

        if let Some(mode) = mount.mode {
            let mut permissions = std::fs::metadata(target)
                .map_err(|e| {
                    BoxError::BuildError(format!(
                        "Failed to inspect RUN cache mount target {}: {}",
                        target.display(),
                        e
                    ))
                })?
                .permissions();
            permissions.set_mode(mode);
            std::fs::set_permissions(target, permissions).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to set RUN cache mount mode {:o} on {}: {}",
                    mode,
                    target.display(),
                    e
                ))
            })?;
        }

        if mount.uid.is_some() || mount.gid.is_some() {
            let uid = mount.uid.map(|uid| uid as libc::uid_t).unwrap_or(!0);
            let gid = mount.gid.map(|gid| gid as libc::gid_t).unwrap_or(!0);
            let c_path = std::ffi::CString::new(target.as_os_str().as_bytes()).map_err(|_| {
                BoxError::BuildError(format!(
                    "RUN cache mount target contains NUL: {}",
                    target.display()
                ))
            })?;
            let ret = unsafe { libc::chown(c_path.as_ptr(), uid, gid) };
            if ret != 0 {
                return Err(BoxError::BuildError(format!(
                    "Failed to set RUN cache mount ownership on {}: {}",
                    target.display(),
                    std::io::Error::last_os_error()
                )));
            }
        }
    }

    #[cfg(not(unix))]
    {
        let _ = (target, mount);
    }

    Ok(())
}

fn print_run_output(output: &std::process::Output, quiet: bool) {
    print_output_parts(&output.stdout, &output.stderr, quiet);
}

fn print_output_parts(stdout: &[u8], stderr: &[u8], quiet: bool) {
    if quiet {
        return;
    }

    if !stdout.is_empty() {
        print!("{}", String::from_utf8_lossy(stdout));
    }
    if !stderr.is_empty() {
        use std::io::Write as _;
        let _ = std::io::stderr().write_all(String::from_utf8_lossy(stderr).as_bytes());
    }
}

fn run_command_failed_error(command: &str, output: &std::process::Output) -> BoxError {
    let exit = output
        .status
        .code()
        .map(|code| code.to_string())
        .unwrap_or_else(|| "signal".to_string());
    run_command_failed_error_message(command, exit, &output.stdout, &output.stderr)
}

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
fn run_command_failed_error_parts(
    command: &str,
    exit_code: i32,
    stdout: &[u8],
    stderr: &[u8],
) -> BoxError {
    run_command_failed_error_message(command, exit_code.to_string(), stdout, stderr)
}

fn run_command_failed_error_message(
    command: &str,
    exit: String,
    stdout: &[u8],
    stderr: &[u8],
) -> BoxError {
    let mut message = format!("RUN command failed (exit {exit}): {command}");

    append_output_context(&mut message, "stdout", stdout);
    append_output_context(&mut message, "stderr", stderr);

    if stdout.is_empty() && stderr.is_empty() {
        message.push_str("\n(no stdout or stderr captured)");
    }

    BoxError::BuildError(message)
}

#[cfg_attr(all(not(feature = "pool"), not(test)), allow(dead_code))]
fn prepare_pool_run_filesystem(rootfs_dir: &Path) -> Result<()> {
    for dir in ["dev", "proc", "sys", "tmp", "var/tmp", "etc"] {
        std::fs::create_dir_all(rootfs_dir.join(dir)).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to prepare RUN directory {}: {}",
                rootfs_dir.join(dir).display(),
                e
            ))
        })?;
    }

    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        for dir in ["tmp", "var/tmp"] {
            let path = rootfs_dir.join(dir);
            let mut perms = std::fs::metadata(&path)
                .map_err(|e| {
                    BoxError::BuildError(format!("Failed to inspect {}: {}", path.display(), e))
                })?
                .permissions();
            perms.set_mode(0o1777);
            std::fs::set_permissions(&path, perms).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to set sticky tmp permissions on {}: {}",
                    path.display(),
                    e
                ))
            })?;
        }
        ensure_run_symlink(rootfs_dir.join("dev/fd"), "/proc/self/fd")?;
        ensure_run_symlink(rootfs_dir.join("dev/stdin"), "/proc/self/fd/0")?;
        ensure_run_symlink(rootfs_dir.join("dev/stdout"), "/proc/self/fd/1")?;
        ensure_run_symlink(rootfs_dir.join("dev/stderr"), "/proc/self/fd/2")?;
    }

    ensure_run_resolv_conf(rootfs_dir)
}

fn append_output_context(message: &mut String, label: &str, bytes: &[u8]) {
    if bytes.is_empty() {
        return;
    }

    message.push('\n');
    message.push_str(label);
    message.push_str(":\n");
    message.push_str(&lossy_tail(bytes, RUN_OUTPUT_CONTEXT_BYTES));
}

fn lossy_tail(bytes: &[u8], max_bytes: usize) -> String {
    let (slice, truncated) = if bytes.len() > max_bytes {
        (&bytes[bytes.len() - max_bytes..], true)
    } else {
        (bytes, false)
    };
    let mut output = String::new();
    if truncated {
        output.push_str(&format!(
            "[showing last {} bytes of {} captured bytes]\n",
            max_bytes,
            bytes.len()
        ));
    }
    output.push_str(String::from_utf8_lossy(slice).trim_end());
    output
}

#[cfg(target_os = "linux")]
fn prepare_linux_run_filesystem(rootfs_dir: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    for dir in ["dev", "proc", "tmp", "var/tmp", "etc"] {
        std::fs::create_dir_all(rootfs_dir.join(dir)).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to prepare RUN directory {}: {}",
                rootfs_dir.join(dir).display(),
                e
            ))
        })?;
    }

    for dir in ["tmp", "var/tmp"] {
        let path = rootfs_dir.join(dir);
        let mut perms = std::fs::metadata(&path)
            .map_err(|e| {
                BoxError::BuildError(format!("Failed to inspect {}: {}", path.display(), e))
            })?
            .permissions();
        perms.set_mode(0o1777);
        std::fs::set_permissions(&path, perms).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to set sticky tmp permissions on {}: {}",
                path.display(),
                e
            ))
        })?;
    }

    for dev in ["null", "zero", "random", "urandom"] {
        let target = rootfs_dir.join("dev").join(dev);
        if !target.exists() {
            std::fs::File::create(&target).map_err(|e| {
                BoxError::BuildError(format!("Failed to create {}: {}", target.display(), e))
            })?;
        }
    }

    ensure_run_symlink(rootfs_dir.join("dev/fd"), "/proc/self/fd")?;
    ensure_run_symlink(rootfs_dir.join("dev/stdin"), "/proc/self/fd/0")?;
    ensure_run_symlink(rootfs_dir.join("dev/stdout"), "/proc/self/fd/1")?;
    ensure_run_symlink(rootfs_dir.join("dev/stderr"), "/proc/self/fd/2")?;
    ensure_run_resolv_conf(rootfs_dir)?;

    Ok(())
}

#[cfg(unix)]
#[cfg_attr(
    all(not(feature = "pool"), not(target_os = "linux"), not(test)),
    allow(dead_code)
)]
fn ensure_run_symlink(path: PathBuf, target: &str) -> Result<()> {
    match std::fs::symlink_metadata(&path) {
        Ok(meta) if meta.file_type().is_symlink() => return Ok(()),
        Ok(_) => return Ok(()),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
        Err(err) => {
            return Err(BoxError::BuildError(format!(
                "Failed to inspect {}: {}",
                path.display(),
                err
            )));
        }
    }

    std::os::unix::fs::symlink(target, &path).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to create RUN symlink {} -> {}: {}",
            path.display(),
            target,
            e
        ))
    })
}

#[cfg_attr(
    all(not(feature = "pool"), not(target_os = "linux"), not(test)),
    allow(dead_code)
)]
fn ensure_run_resolv_conf(rootfs_dir: &Path) -> Result<()> {
    let path = rootfs_dir.join("etc/resolv.conf");
    if std::fs::metadata(&path)
        .map(|m| m.len() > 0)
        .unwrap_or(false)
    {
        return Ok(());
    }

    let content = std::fs::read_to_string("/etc/resolv.conf")
        .unwrap_or_else(|_| "nameserver 8.8.8.8\nnameserver 8.8.4.4\n".to_string());
    std::fs::write(&path, content)
        .map_err(|e| BoxError::BuildError(format!("Failed to write {}: {}", path.display(), e)))
}

#[cfg(target_os = "linux")]
struct LinuxRunMounts {
    mounted: Vec<PathBuf>,
    cache_dirs: Vec<tempfile::TempDir>,
}

#[cfg(target_os = "linux")]
impl LinuxRunMounts {
    fn mount(rootfs_dir: &Path) -> Result<Self> {
        let mut mounts = Self {
            mounted: Vec::new(),
            cache_dirs: Vec::new(),
        };

        mounts.mount_proc(&rootfs_dir.join("proc"))?;
        for dev in ["null", "zero", "random", "urandom"] {
            mounts.bind_mount(
                Path::new("/dev").join(dev),
                rootfs_dir.join("dev").join(dev),
            )?;
        }

        Ok(mounts)
    }

    fn with_cache_mounts(
        mut self,
        rootfs_dir: &Path,
        cache_mounts: &[RunCacheMount],
        completed_stages: &[(Option<String>, PathBuf)],
    ) -> Result<Self> {
        for mount in cache_mounts {
            let cache_dir = tempfile::Builder::new()
                .prefix("a3s-box-run-cache-")
                .tempdir()
                .map_err(|e| {
                    BoxError::BuildError(format!("Failed to create RUN cache mount: {}", e))
                })?;
            if let Some(seed_source) = run_cache_mount_seed_source(completed_stages, mount)? {
                copy_run_cache_seed_to(&seed_source, cache_dir.path())?;
            }
            let target = rootfs_dir.join(mount.target.trim_start_matches('/'));
            self.bind_mount(cache_dir.path().to_path_buf(), target)?;
            self.cache_dirs.push(cache_dir);
        }
        Ok(self)
    }

    fn mount_proc(&mut self, target: &Path) -> Result<()> {
        mount_linux(
            Some(Path::new("proc")),
            target,
            Some("proc"),
            libc::MS_NOSUID | libc::MS_NOEXEC | libc::MS_NODEV,
        )?;
        self.mounted.push(target.to_path_buf());
        Ok(())
    }

    fn bind_mount(&mut self, source: PathBuf, target: PathBuf) -> Result<()> {
        mount_linux(Some(&source), &target, None, libc::MS_BIND)?;
        self.mounted.push(target);
        Ok(())
    }

    fn unmount(mut self) -> Result<()> {
        let mut first_error = None;
        for target in self.mounted.iter().rev() {
            if let Err(error) = unmount_linux(target) {
                first_error.get_or_insert(error);
            }
        }
        self.mounted.clear();
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

#[cfg(target_os = "linux")]
impl Drop for LinuxRunMounts {
    fn drop(&mut self) {
        for target in self.mounted.iter().rev() {
            let _ = unmount_linux(target);
        }
    }
}

#[cfg(target_os = "linux")]
fn unmount_linux(target: &Path) -> Result<()> {
    let c_target = path_cstring(target, "unmount target")?;
    let ret = unsafe { libc::umount2(c_target.as_ptr(), libc::MNT_DETACH) };
    if ret == 0 {
        Ok(())
    } else {
        Err(BoxError::BuildError(format!(
            "Failed to unmount RUN support at {}: {}",
            target.display(),
            std::io::Error::last_os_error()
        )))
    }
}

#[cfg(target_os = "linux")]
fn mount_linux(
    source: Option<&Path>,
    target: &Path,
    fstype: Option<&str>,
    flags: libc::c_ulong,
) -> Result<()> {
    let c_source = source
        .map(|source| path_cstring(source, "mount source"))
        .transpose()?;
    let c_target = path_cstring(target, "mount target")?;
    let c_fstype = fstype
        .map(std::ffi::CString::new)
        .transpose()
        .map_err(|_| BoxError::BuildError("Cannot mount fstype containing NUL".to_string()))?;

    let ret = unsafe {
        libc::mount(
            c_source
                .as_ref()
                .map(|value| value.as_ptr())
                .unwrap_or(std::ptr::null()),
            c_target.as_ptr(),
            c_fstype
                .as_ref()
                .map(|value| value.as_ptr())
                .unwrap_or(std::ptr::null()),
            flags,
            std::ptr::null(),
        )
    };

    if ret == 0 {
        Ok(())
    } else {
        Err(BoxError::BuildError(format!(
            "Failed to mount RUN support at {}: {}",
            target.display(),
            std::io::Error::last_os_error()
        )))
    }
}

#[cfg(target_os = "linux")]
fn path_cstring(path: &Path, label: &str) -> Result<std::ffi::CString> {
    use std::os::unix::ffi::OsStrExt;

    std::ffi::CString::new(path.as_os_str().as_bytes())
        .map_err(|_| BoxError::BuildError(format!("{label} contains NUL: {}", path.display())))
}

#[cfg(target_os = "macos")]
fn unsafe_host_run_enabled() -> bool {
    std::env::var(UNSAFE_HOST_RUN_ENV)
        .map(|value| matches!(value.as_str(), "1" | "true" | "TRUE" | "yes" | "YES"))
        .unwrap_or(false)
}

/// Execute RUN command directly on host (unsafe macOS escape hatch).
///
/// This does not provide container/Linux build semantics. It exists only for
/// explicit local experiments while isolated macOS build execution is pending.
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
fn handle_run_on_host_unsafe(
    command: &RunCommand,
    cache_mounts: &[RunCacheMount],
    bind_mounts: &[RunBindMount],
    tmpfs_mounts: &[RunTmpfsMount],
    context_dir: &Path,
    completed_stages: &[(Option<String>, PathBuf)],
    rootfs_dir: &Path,
    layers_dir: &Path,
    workdir: &str,
    env: &[(String, String)],
    shell: &[String],
    layer_index: usize,
    quiet: bool,
    ignore: Option<&DockerIgnore>,
) -> Result<Option<LayerInfo>> {
    use super::super::layer::DirSnapshot;

    let RunCommand::Shell(command) = command else {
        return Err(BoxError::BuildError(
            "Dockerfile RUN exec form requires isolated Linux execution; use --run-pool or --builder=buildkit-vm on macOS".to_string(),
        ));
    };

    if !quiet {
        println!("→ Executing RUN command on host (unsafe)");
    }

    // Capture filesystem state before execution
    let before = DirSnapshot::capture(rootfs_dir)?;

    // Build the shell command
    let shell_cmd = if !shell.is_empty() {
        let mut parts = shell.to_vec();
        parts.push(command.to_string());
        parts
    } else {
        vec!["/bin/sh".to_string(), "-c".to_string(), command.to_string()]
    };

    // Execute command in rootfs directory
    if !quiet {
        println!("→ Executing: {}", command);
    }

    let workdir_path = if workdir.is_empty() || workdir == "/" {
        rootfs_dir.to_path_buf()
    } else {
        rootfs_dir.join(workdir.trim_start_matches('/'))
    };

    // Ensure workdir exists
    if !workdir_path.exists() {
        std::fs::create_dir_all(&workdir_path).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to create workdir {}: {}",
                workdir_path.display(),
                e
            ))
        })?;
    }
    ensure_run_cache_mount_targets(rootfs_dir, cache_mounts)?;

    let mut cmd = std::process::Command::new(&shell_cmd[0]);
    cmd.args(&shell_cmd[1..]).current_dir(&workdir_path);
    for (key, value) in env {
        cmd.env(key, value);
    }
    let bind_mount_guard = RunBindMountOverlays::activate(
        rootfs_dir,
        context_dir,
        completed_stages,
        bind_mounts,
        workdir,
        ignore,
    )?;
    let tmpfs_mount_guard = RunTmpfsMountOverlays::activate(rootfs_dir, tmpfs_mounts, workdir)?;
    let output = cmd
        .output()
        .map_err(|e| BoxError::BuildError(format!("Failed to execute command: {}", e)))?;

    if !output.status.success() {
        tmpfs_mount_guard.restore()?;
        bind_mount_guard.restore()?;
        return Err(run_command_failed_error(command, &output));
    }
    tmpfs_mount_guard.restore()?;
    bind_mount_guard.restore()?;
    print_run_output(&output, quiet);

    // Capture filesystem state after execution
    let after = DirSnapshot::capture(rootfs_dir)?;
    let changed = filter_run_mount_paths(
        before.diff(&after),
        cache_mounts,
        bind_mounts,
        tmpfs_mounts,
        workdir,
    );
    let deleted = filter_run_mount_paths(
        before.deletions(&after),
        cache_mounts,
        bind_mounts,
        tmpfs_mounts,
        workdir,
    );

    if changed.is_empty() && deleted.is_empty() {
        if !quiet {
            println!("→ No filesystem changes detected");
        }
        return Ok(None);
    }

    // Create layer from changes (and OCI whiteouts for deletions)
    let layer_path = layers_dir.join(format!("layer_{}.tar.gz", layer_index));
    let layer_info = create_layer_with_deletions(rootfs_dir, &changed, &deleted, &layer_path)?;

    if !quiet {
        println!(
            "→ Created layer with {} changes, {} deletions",
            changed.len(),
            deleted.len()
        );
    }

    Ok(Some(layer_info))
}

/// Handle ADD: like COPY but supports URL download and tar auto-extraction.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_add(
    src_patterns: &[String],
    dst: &str,
    chown: Option<&str>,
    context_dir: &Path,
    rootfs_dir: &Path,
    layers_dir: &Path,
    workdir: &str,
    layer_index: usize,
    ignore: Option<&DockerIgnore>,
) -> Result<LayerInfo> {
    let chown_ids = if let Some(spec) = chown {
        Some(resolve_chown(spec, rootfs_dir)?)
    } else {
        None
    };

    // Expand any glob source patterns against the context (Docker semantics);
    // remote URL sources pass through untouched.
    let src_patterns = &resolve_source_patterns(context_dir, src_patterns)?;

    let resolved_dst = resolve_path(workdir, dst);
    reject_path_traversal(&resolved_dst)?;
    let dst_in_rootfs = rootfs_dir.join(resolved_dst.trim_start_matches('/'));
    // The destination must not escape the rootfs via `..` or a base-image symlink.
    assert_within(rootfs_dir, &dst_in_rootfs)?;

    // Ensure destination directory exists
    if dst.ends_with('/') || src_patterns.len() > 1 {
        std::fs::create_dir_all(&dst_in_rootfs).map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to create ADD destination {}: {}",
                dst_in_rootfs.display(),
                e
            ))
        })?;
    } else if let Some(parent) = dst_in_rootfs.parent() {
        std::fs::create_dir_all(parent).map_err(|e| {
            BoxError::BuildError(format!("Failed to create parent directory: {}", e))
        })?;
    }

    for src in src_patterns {
        if src.starts_with("http://") || src.starts_with("https://") {
            // URL download — fetch and write to destination
            let bytes = download_url(src).map_err(|e| {
                BoxError::BuildError(format!("ADD URL download failed for {}: {}", src, e))
            })?;
            // Derive filename from URL path
            let filename = src
                .rsplit('/')
                .next()
                .filter(|s| !s.is_empty())
                .unwrap_or("downloaded");
            let dest_file = if dst_in_rootfs.is_dir() || src.ends_with('/') {
                dst_in_rootfs.join(filename)
            } else {
                dst_in_rootfs.clone()
            };
            if let Some(parent) = dest_file.parent() {
                std::fs::create_dir_all(parent).map_err(|e| {
                    BoxError::BuildError(format!("Failed to create parent for ADD URL: {}", e))
                })?;
            }
            std::fs::write(&dest_file, &bytes).map_err(|e| {
                BoxError::BuildError(format!("Failed to write downloaded file: {}", e))
            })?;
            tracing::info!(url = src.as_str(), dest = %dest_file.display(), "ADD URL downloaded");
            continue;
        }

        // See handle_copy: strip a leading slash so an absolute src resolves
        // within the context rather than discarding the base in `Path::join`.
        reject_path_traversal(src)?;
        let rel = PathBuf::from(if src == "." {
            ""
        } else {
            src.trim_start_matches('/')
        });
        let src_path = context_dir.join(src.trim_start_matches('/'));
        if !src_path.exists() {
            return Err(BoxError::BuildError(format!(
                "ADD source not found: {} (in context {})",
                src,
                context_dir.display()
            )));
        }
        // A source must resolve inside the build context (no `..`/symlink escape).
        assert_within(context_dir, &src_path)?;

        if let Some(ign) = ignore {
            if !rel.as_os_str().is_empty() && src_path.is_file() && ign.is_excluded(&rel) {
                return Err(BoxError::BuildError(format!(
                    "ADD source not found: {} (excluded by .dockerignore)",
                    src
                )));
            }
        }

        // Check if it's a tar archive that should be auto-extracted
        if is_tar_archive(src) && !src_path.is_dir() {
            extract_tar_to_dst(&src_path, &dst_in_rootfs)?;
        } else if src_path.is_dir() {
            copy_dir_filtered(&src_path, &dst_in_rootfs, &rel, ignore)?;
        } else {
            let target = if dst_in_rootfs.is_dir() {
                dst_in_rootfs.join(
                    src_path
                        .file_name()
                        .unwrap_or_else(|| std::ffi::OsStr::new(src)),
                )
            } else {
                dst_in_rootfs.clone()
            };
            std::fs::copy(&src_path, &target).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to copy {} to {}: {}",
                    src_path.display(),
                    target.display(),
                    e
                ))
            })?;
        }
    }

    // Create a layer from the destination, stamping --chown into tar headers.
    let layer_path = layers_dir.join(format!("layer_{}.tar.gz", layer_index));
    let target_prefix = Path::new(resolved_dst.trim_start_matches('/'));
    if dst_in_rootfs.is_dir() {
        create_layer_from_dir_with_chown(&dst_in_rootfs, target_prefix, &layer_path, chown_ids)
    } else if dst_in_rootfs.parent().is_some() {
        let changed = vec![PathBuf::from(
            dst_in_rootfs
                .strip_prefix(rootfs_dir)
                .unwrap_or(target_prefix),
        )];
        create_layer_with_chown(rootfs_dir, &changed, &[], &layer_path, chown_ids)
    } else {
        Err(BoxError::BuildError("Invalid ADD destination".to_string()))
    }
}

/// Execute an ONBUILD trigger instruction.
pub(super) fn execute_onbuild_trigger(
    trigger: &str,
    state: &mut BuildState,
    _config: &super::BuildConfig,
    _rootfs_dir: &Path,
    _layers_dir: &Path,
    _base_layers: &[LayerInfo],
    _completed_stages: &[(Option<String>, PathBuf)],
) -> Result<()> {
    // Parse the trigger as an instruction
    let instruction = super::super::dockerfile::parse_single_instruction(trigger)?;

    // Only handle metadata instructions in ONBUILD triggers for now
    // (RUN/COPY would need full execution context)
    match &instruction {
        Instruction::Env { vars } => {
            for (key, value) in vars {
                let expanded = expand_args(value, &state.build_args);
                if let Some(existing) = state.env.iter_mut().find(|(k, _)| k == key) {
                    existing.1 = expanded;
                } else {
                    state.env.push((key.clone(), expanded));
                }
            }
        }
        Instruction::Label { pairs } => {
            for (key, value) in pairs {
                state.labels.insert(key.clone(), value.clone());
            }
        }
        Instruction::Workdir { path } => {
            state.workdir = resolve_path(&state.workdir, path);
        }
        Instruction::Expose { ports } => {
            for port in ports {
                if !state.exposed_ports.contains(port) {
                    state.exposed_ports.push(port.clone());
                }
            }
        }
        Instruction::User { user } => {
            state.user = Some(user.clone());
        }
        _ => {
            return Err(BoxError::BuildError(format!(
                "ONBUILD trigger '{}' is not supported yet because it requires build execution context",
                trigger
            )));
        }
    }

    state.history.push(super::HistoryEntry {
        created_by: format!("ONBUILD {}", trigger),
        empty_layer: true,
    });

    Ok(())
}

/// Convert an Instruction back to a string representation for ONBUILD storage.
pub(super) fn instruction_to_string(instr: &Instruction) -> String {
    match instr {
        Instruction::Run {
            command,
            cache_mounts,
            bind_mounts,
            tmpfs_mounts,
        } => {
            let flags = cache_mounts
                .iter()
                .map(|mount| mount.raw.as_str())
                .chain(bind_mounts.iter().map(|mount| mount.raw.as_str()))
                .chain(tmpfs_mounts.iter().map(|mount| mount.raw.as_str()))
                .collect::<Vec<_>>()
                .join(" ");
            if flags.is_empty() {
                format!("RUN {}", run_command_to_string(command))
            } else {
                format!("RUN {} {}", flags, run_command_to_string(command))
            }
        }
        Instruction::Copy {
            src,
            dst,
            from,
            chown,
        } => {
            let mut prefix = String::from("COPY");
            if let Some(f) = from {
                prefix.push_str(&format!(" --from={}", f));
            }
            if let Some(c) = chown {
                prefix.push_str(&format!(" --chown={}", c));
            }
            format!("{} {} {}", prefix, src.join(" "), dst)
        }
        Instruction::Add { src, dst, chown } => {
            if let Some(c) = chown {
                format!("ADD --chown={} {} {}", c, src.join(" "), dst)
            } else {
                format!("ADD {} {}", src.join(" "), dst)
            }
        }
        Instruction::Workdir { path } => format!("WORKDIR {}", path),
        Instruction::Env { vars } => {
            let pairs: Vec<String> = vars.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
            format!("ENV {}", pairs.join(" "))
        }
        Instruction::Entrypoint { exec } => format!("ENTRYPOINT {:?}", exec),
        Instruction::Cmd { exec } => format!("CMD {:?}", exec),
        Instruction::Expose { ports } => format!("EXPOSE {}", ports.join(" ")),
        Instruction::Label { pairs } => format!(
            "LABEL {}",
            pairs
                .iter()
                .map(|(k, v)| format!("{}={}", k, v))
                .collect::<Vec<_>>()
                .join(" ")
        ),
        Instruction::User { user } => format!("USER {}", user),
        Instruction::Arg { name, default } => {
            if let Some(d) = default {
                format!("ARG {}={}", name, d)
            } else {
                format!("ARG {}", name)
            }
        }
        Instruction::Shell { exec } => format!("SHELL {:?}", exec),
        Instruction::StopSignal { signal } => format!("STOPSIGNAL {}", signal),
        Instruction::HealthCheck { cmd, .. } => {
            if let Some(c) = cmd {
                format!("HEALTHCHECK CMD {}", c.join(" "))
            } else {
                "HEALTHCHECK NONE".to_string()
            }
        }
        Instruction::OnBuild { instruction } => {
            format!("ONBUILD {}", instruction_to_string(instruction))
        }
        Instruction::Volume { paths } => format!("VOLUME {}", paths.join(" ")),
        Instruction::From { image, alias } => {
            if let Some(a) = alias {
                format!("FROM {} AS {}", image, a)
            } else {
                format!("FROM {}", image)
            }
        }
    }
}

/// Apply base image config to build state.
pub(super) fn apply_base_config(
    state: &mut BuildState,
    config: &crate::oci::image::OciImageConfig,
) {
    state.env = config.env.clone();
    state.entrypoint = config.entrypoint.clone();
    state.cmd = config.cmd.clone();
    state.user = config.user.clone();
    state.exposed_ports = config.exposed_ports.clone();
    state.labels = config.labels.clone();
    if let Some(ref wd) = config.working_dir {
        state.workdir = wd.clone();
    }
    if let Some(ref sig) = config.stop_signal {
        state.stop_signal = Some(sig.clone());
    }
    if let Some(ref hc) = config.health_check {
        state.health_check = Some(hc.clone());
    }
    // Inherit volumes from base image
    for v in &config.volumes {
        if !state.volumes.contains(v) {
            state.volumes.push(v.clone());
        }
    }
    // Note: onbuild triggers are NOT inherited — they are executed, not stored
}

/// Download a URL and return the response bytes.
///
/// Uses `tokio::task::block_in_place` to run async reqwest from a sync context
/// while inside a tokio runtime (the build engine runs inside `async fn build()`).
fn download_url(url: &str) -> std::result::Result<Vec<u8>, String> {
    tokio::task::block_in_place(|| {
        tokio::runtime::Handle::current().block_on(async {
            let client = reqwest::Client::builder()
                .timeout(std::time::Duration::from_secs(60))
                .no_proxy()
                .build()
                .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

            let mut response = client
                .get(url)
                .send()
                .await
                .map_err(|e| format!("HTTP request failed: {}", e))?;

            if !response.status().is_success() {
                return Err(format!("HTTP {} for {}", response.status(), url));
            }

            // Cap the download so a hostile/huge URL cannot OOM the build host:
            // `bytes()` buffers the WHOLE body with no limit. Reject an oversized
            // advertised length early, then stream with a hard cap (the length
            // header may be absent or lie).
            const MAX_ADD_URL_BYTES: u64 = 512 * 1024 * 1024; // 512 MiB
            if let Some(len) = response.content_length() {
                if len > MAX_ADD_URL_BYTES {
                    return Err(format!(
                        "ADD URL body too large: {len} bytes (max {MAX_ADD_URL_BYTES})"
                    ));
                }
            }
            let mut buf: Vec<u8> = Vec::new();
            while let Some(chunk) = response
                .chunk()
                .await
                .map_err(|e| format!("Failed to read response body: {}", e))?
            {
                if buf.len() as u64 + chunk.len() as u64 > MAX_ADD_URL_BYTES {
                    return Err(format!(
                        "ADD URL body exceeds max {MAX_ADD_URL_BYTES} bytes"
                    ));
                }
                buf.extend_from_slice(&chunk);
            }
            Ok(buf)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::super::super::dockerfile::{
        Instruction, RunBindMount, RunCacheMount, RunCacheSharing, RunCommand, RunTmpfsMount,
    };
    use super::{
        execute_onbuild_trigger, expand_glob_sources, glob_segment_match, handle_add,
        instruction_to_string, run_command_failed_error, shell_command_in_workdir,
    };
    use crate::oci::build::engine::{BuildConfig, BuildState};
    use a3s_box_core::error::BoxError;
    use std::collections::HashMap;
    use std::path::PathBuf;

    fn shell_run(command: &str) -> RunCommand {
        RunCommand::Shell(command.to_string())
    }

    #[test]
    fn test_glob_segment_match() {
        assert!(glob_segment_match("*.conf", "alpha.conf"));
        assert!(glob_segment_match("*.conf", ".conf"));
        assert!(!glob_segment_match("*.conf", "skip.txt"));
        assert!(glob_segment_match("a?c", "abc"));
        assert!(!glob_segment_match("a?c", "ac"));
        assert!(glob_segment_match("*", "anything"));
        assert!(glob_segment_match("pre*post", "pre_middle_post"));
        assert!(!glob_segment_match("pre*post", "pre_middle"));
    }

    #[cfg(unix)]
    #[test]
    fn test_run_command_failed_error_includes_stdout_and_stderr() {
        use std::os::unix::process::ExitStatusExt;

        let output = std::process::Output {
            status: std::process::ExitStatus::from_raw(2 << 8),
            stdout: b"resolved package metadata\n".to_vec(),
            stderr: b"corepack prepare failed\n".to_vec(),
        };

        let BoxError::BuildError(message) =
            run_command_failed_error("corepack prepare pnpm@10.30.3 --activate", &output)
        else {
            panic!("expected build error");
        };

        assert!(message.contains("RUN command failed (exit 2)"));
        assert!(message.contains("corepack prepare pnpm@10.30.3 --activate"));
        assert!(message.contains("stdout:\nresolved package metadata"));
        assert!(message.contains("stderr:\ncorepack prepare failed"));
    }

    #[test]
    fn test_expand_glob_sources() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join("alpha.conf"), "1").unwrap();
        std::fs::write(dir.path().join("beta.conf"), "2").unwrap();
        std::fs::write(dir.path().join("skip.txt"), "x").unwrap();
        let mut got = expand_glob_sources(dir.path(), "*.conf");
        got.sort();
        assert_eq!(got, vec!["alpha.conf".to_string(), "beta.conf".to_string()]);
        // Non-matching glob yields no entries.
        assert!(expand_glob_sources(dir.path(), "*.md").is_empty());
    }

    #[test]
    fn test_instruction_to_string_run() {
        let instr = Instruction::Run {
            command: RunCommand::Shell("echo hello".to_string()),
            cache_mounts: vec![],
            bind_mounts: vec![],
            tmpfs_mounts: vec![],
        };
        assert_eq!(instruction_to_string(&instr), "RUN echo hello");
    }

    #[test]
    fn test_instruction_to_string_run_exec_form() {
        let instr = Instruction::Run {
            command: RunCommand::Exec(vec![
                "/bin/sh".to_string(),
                "-c".to_string(),
                "echo hello".to_string(),
            ]),
            cache_mounts: vec![],
            bind_mounts: vec![],
            tmpfs_mounts: vec![],
        };
        assert_eq!(
            instruction_to_string(&instr),
            r#"RUN ["/bin/sh","-c","echo hello"]"#
        );
    }

    #[test]
    fn test_instruction_to_string_run_with_cache_mount() {
        let instr = Instruction::Run {
            command: RunCommand::Shell("pnpm install".to_string()),
            cache_mounts: vec![RunCacheMount {
                raw: "--mount=type=cache,sharing=locked,target=/root/.cache".to_string(),
                id: None,
                from: None,
                source: ".".to_string(),
                sharing: RunCacheSharing::Locked,
                mode: None,
                uid: None,
                gid: None,
                target: "/root/.cache".to_string(),
            }],
            bind_mounts: vec![],
            tmpfs_mounts: vec![],
        };
        assert_eq!(
            instruction_to_string(&instr),
            "RUN --mount=type=cache,sharing=locked,target=/root/.cache pnpm install"
        );
    }

    #[test]
    fn test_instruction_to_string_run_with_bind_mount() {
        let instr = Instruction::Run {
            command: RunCommand::Shell("go build ./...".to_string()),
            cache_mounts: vec![],
            bind_mounts: vec![RunBindMount {
                from: None,
                raw: "--mount=type=bind,source=.,target=.".to_string(),
                source: ".".to_string(),
                target: ".".to_string(),
                read_write: false,
            }],
            tmpfs_mounts: vec![],
        };
        assert_eq!(
            instruction_to_string(&instr),
            "RUN --mount=type=bind,source=.,target=. go build ./..."
        );
    }

    #[test]
    fn test_instruction_to_string_run_with_tmpfs_mount() {
        let instr = Instruction::Run {
            command: RunCommand::Shell("make test".to_string()),
            cache_mounts: vec![],
            bind_mounts: vec![],
            tmpfs_mounts: vec![RunTmpfsMount {
                raw: "--mount=type=tmpfs,target=/tmp".to_string(),
                target: "/tmp".to_string(),
            }],
        };
        assert_eq!(
            instruction_to_string(&instr),
            "RUN --mount=type=tmpfs,target=/tmp make test"
        );
    }

    #[test]
    fn test_shell_command_in_workdir_enters_workdir_inside_chroot() {
        assert_eq!(
            shell_command_in_workdir("/app", "pnpm install"),
            "cd '/app' && pnpm install"
        );
        assert_eq!(
            shell_command_in_workdir("/app's dir", "pwd"),
            "cd '/app'\\''s dir' && pwd"
        );
        assert_eq!(shell_command_in_workdir("/", "pwd"), "pwd");
    }

    #[test]
    fn test_build_run_shell_cmd_uses_configured_shell_and_workdir() {
        let cmd = super::build_run_shell_cmd(
            &["/bin/bash".to_string(), "-lc".to_string()],
            "/app",
            "echo hi",
        );

        assert_eq!(cmd, vec!["/bin/bash", "-lc", "cd '/app' && echo hi"]);
    }

    #[test]
    fn test_run_env_entries_includes_defaults_and_build_env() {
        let env = super::run_env_entries(&[("FOO".to_string(), "bar".to_string())]);

        assert!(env
            .iter()
            .any(|entry| entry
                == "PATH=/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin"));
        assert!(env.iter().any(|entry| entry == "HOME=/root"));
        assert!(env.iter().any(|entry| entry == "FOO=bar"));
    }

    #[test]
    fn test_prepare_pool_run_filesystem_creates_support_paths() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        super::prepare_pool_run_filesystem(&rootfs).unwrap();

        assert!(rootfs.join("dev").is_dir());
        assert!(rootfs.join("proc").is_dir());
        assert!(rootfs.join("sys").is_dir());
        assert!(rootfs.join("tmp").is_dir());
        assert!(rootfs.join("etc/resolv.conf").is_file());
    }

    #[test]
    fn test_run_bind_mount_overlays_context_and_restores_target() {
        let tmp = tempfile::TempDir::new().unwrap();
        let context = tmp.path().join("context");
        let rootfs = tmp.path().join("rootfs");
        let source = context.join("src");
        let target = rootfs.join("work");
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("input.txt"), "from-context").unwrap();
        std::fs::write(target.join("original.txt"), "from-rootfs").unwrap();

        let mounts = vec![RunBindMount {
            from: None,
            raw: "--mount=type=bind,source=src,target=.".to_string(),
            source: "src".to_string(),
            target: ".".to_string(),
            read_write: true,
        }];

        let guard = super::RunBindMountOverlays::activate_context(
            &rootfs, &context, &mounts, "/work", None,
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(target.join("input.txt")).unwrap(),
            "from-context"
        );
        assert!(!target.join("original.txt").exists());
        std::fs::write(target.join("generated.txt"), "discard me").unwrap();

        guard.restore().unwrap();

        assert_eq!(
            std::fs::read_to_string(target.join("original.txt")).unwrap(),
            "from-rootfs"
        );
        assert!(!target.join("input.txt").exists());
        assert!(!target.join("generated.txt").exists());
    }

    #[test]
    fn test_run_bind_mount_overlays_stage_source_and_restores_target() {
        let tmp = tempfile::TempDir::new().unwrap();
        let context = tmp.path().join("context");
        let stage_rootfs = tmp.path().join("stage-rootfs");
        let rootfs = tmp.path().join("rootfs");
        let source = stage_rootfs.join("out");
        let target = rootfs.join("work");
        std::fs::create_dir_all(&context).unwrap();
        std::fs::create_dir_all(&source).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(source.join("artifact.txt"), "from-stage").unwrap();
        std::fs::write(target.join("original.txt"), "from-rootfs").unwrap();

        let mounts = vec![RunBindMount {
            from: Some("builder".to_string()),
            raw: "--mount=type=bind,from=builder,source=/out,target=.".to_string(),
            source: "/out".to_string(),
            target: ".".to_string(),
            read_write: false,
        }];
        let completed_stages = vec![(Some("builder".to_string()), stage_rootfs)];

        let guard = super::RunBindMountOverlays::activate(
            &rootfs,
            &context,
            &completed_stages,
            &mounts,
            "/work",
            None,
        )
        .unwrap();

        assert_eq!(
            std::fs::read_to_string(target.join("artifact.txt")).unwrap(),
            "from-stage"
        );
        assert!(!target.join("original.txt").exists());
        std::fs::write(target.join("generated.txt"), "discard me").unwrap();

        guard.restore().unwrap();

        assert_eq!(
            std::fs::read_to_string(target.join("original.txt")).unwrap(),
            "from-rootfs"
        );
        assert!(!target.join("artifact.txt").exists());
        assert!(!target.join("generated.txt").exists());
    }

    #[test]
    fn test_filter_run_mount_paths_excludes_bind_target() {
        let mounts = vec![RunBindMount {
            from: None,
            raw: "--mount=type=bind,source=src,target=.".to_string(),
            source: "src".to_string(),
            target: ".".to_string(),
            read_write: false,
        }];
        let paths = vec![
            PathBuf::from("work/input.txt"),
            PathBuf::from("work/generated.txt"),
            PathBuf::from("out.txt"),
        ];

        let filtered = super::filter_run_mount_paths(paths, &[], &mounts, &[], "/work");

        assert_eq!(filtered, vec![PathBuf::from("out.txt")]);
    }

    #[test]
    fn test_filter_run_mount_paths_keeps_siblings_under_mount_ancestor() {
        let mounts = vec![RunCacheMount {
            raw: "--mount=type=cache,target=/root/.cache".to_string(),
            id: None,
            from: None,
            source: ".".to_string(),
            sharing: RunCacheSharing::Shared,
            mode: None,
            uid: None,
            gid: None,
            target: "/root/.cache".to_string(),
        }];
        let paths = vec![
            PathBuf::from("root"),
            PathBuf::from("root/.cache/pkg"),
            PathBuf::from("root/.profile"),
        ];

        let filtered = super::filter_run_mount_paths(paths, &mounts, &[], &[], "/");

        assert_eq!(filtered, vec![PathBuf::from("root/.profile")]);
    }

    #[test]
    fn test_run_tmpfs_mount_overlays_empty_dir_and_restores_target() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let target = rootfs.join("work/tmp");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("original.txt"), "from-rootfs").unwrap();

        let mounts = vec![RunTmpfsMount {
            raw: "--mount=type=tmpfs,target=tmp".to_string(),
            target: "tmp".to_string(),
        }];

        let guard = super::RunTmpfsMountOverlays::activate(&rootfs, &mounts, "/work").unwrap();

        assert!(target.is_dir());
        assert!(!target.join("original.txt").exists());
        std::fs::write(target.join("generated.txt"), "discard me").unwrap();

        guard.restore().unwrap();

        assert_eq!(
            std::fs::read_to_string(target.join("original.txt")).unwrap(),
            "from-rootfs"
        );
        assert!(!target.join("generated.txt").exists());
    }

    #[test]
    fn test_filter_run_mount_paths_excludes_tmpfs_target() {
        let mounts = vec![RunTmpfsMount {
            raw: "--mount=type=tmpfs,target=/tmp".to_string(),
            target: "/tmp".to_string(),
        }];
        let paths = vec![
            PathBuf::from("tmp/generated.txt"),
            PathBuf::from("var/output.txt"),
        ];

        let filtered = super::filter_run_mount_paths(paths, &[], &[], &mounts, "/");

        assert_eq!(filtered, vec![PathBuf::from("var/output.txt")]);
    }

    #[test]
    fn test_pool_run_cache_mounts_restore_original_target() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let target = rootfs.join("root/.cache");
        let cache_root = tmp.path().join("run-cache");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("original.txt"), "original").unwrap();
        let mounts = vec![RunCacheMount {
            raw: "--mount=type=cache,sharing=locked,target=/root/.cache".to_string(),
            id: None,
            from: None,
            source: ".".to_string(),
            sharing: RunCacheSharing::Locked,
            mode: None,
            uid: None,
            gid: None,
            target: "/root/.cache".to_string(),
        }];

        super::ensure_run_cache_mount_targets(&rootfs, &mounts).unwrap();
        let guard =
            super::PoolRunCacheMounts::activate_with_cache_root(&rootfs, &mounts, &cache_root, &[])
                .unwrap();

        assert!(!target.join("original.txt").exists());
        std::fs::write(target.join("cache-only.txt"), "cache").unwrap();

        guard.restore().unwrap();

        assert_eq!(
            std::fs::read_to_string(target.join("original.txt")).unwrap(),
            "original"
        );
        assert!(!target.join("cache-only.txt").exists());
    }

    #[test]
    fn test_pool_run_cache_mounts_persist_by_id() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let target = rootfs.join("root/.cache");
        let cache_root = tmp.path().join("run-cache");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = vec![RunCacheMount {
            raw: "--mount=type=cache,id=shared,sharing=locked,target=/root/.cache".to_string(),
            id: Some("shared".to_string()),
            from: None,
            source: ".".to_string(),
            sharing: RunCacheSharing::Locked,
            mode: None,
            uid: None,
            gid: None,
            target: "/root/.cache".to_string(),
        }];

        super::ensure_run_cache_mount_targets(&rootfs, &mounts).unwrap();
        let guard =
            super::PoolRunCacheMounts::activate_with_cache_root(&rootfs, &mounts, &cache_root, &[])
                .unwrap();
        std::fs::write(target.join("cache-only.txt"), "cache").unwrap();
        guard.restore().unwrap();
        let cache_dir = super::run_cache_mount_dir(&cache_root, &mounts[0]);
        assert_eq!(
            std::fs::read_to_string(cache_dir.join("cache-only.txt")).unwrap(),
            "cache"
        );

        let guard =
            super::PoolRunCacheMounts::activate_with_cache_root(&rootfs, &mounts, &cache_root, &[])
                .unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("cache-only.txt")).unwrap(),
            "cache"
        );
        guard.restore().unwrap();
    }

    #[test]
    fn test_pool_run_cache_mounts_seed_from_stage_once() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stage_rootfs = tmp.path().join("stage-rootfs");
        let seed_dir = stage_rootfs.join("seed-cache");
        let rootfs = tmp.path().join("rootfs");
        let target = rootfs.join("root/.cache");
        let cache_root = tmp.path().join("run-cache");
        std::fs::create_dir_all(&seed_dir).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(seed_dir.join("seed.txt"), "seed-v1").unwrap();
        std::fs::write(target.join("original.txt"), "original").unwrap();
        let mounts = vec![RunCacheMount {
            raw: "--mount=type=cache,id=seeded,sharing=locked,from=builder,source=/seed-cache,target=/root/.cache".to_string(),
            id: Some("seeded".to_string()),
            from: Some("builder".to_string()),
            source: "/seed-cache".to_string(),
            sharing: RunCacheSharing::Locked,
            mode: None,
            uid: None,
            gid: None,
            target: "/root/.cache".to_string(),
        }];
        let completed_stages = vec![(Some("builder".to_string()), stage_rootfs)];

        super::ensure_run_cache_mount_targets(&rootfs, &mounts).unwrap();
        let guard = super::PoolRunCacheMounts::activate_with_cache_root(
            &rootfs,
            &mounts,
            &cache_root,
            &completed_stages,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("seed.txt")).unwrap(),
            "seed-v1"
        );
        assert!(!target.join("original.txt").exists());
        std::fs::write(target.join("generated.txt"), "persisted").unwrap();
        guard.restore().unwrap();

        assert_eq!(
            std::fs::read_to_string(target.join("original.txt")).unwrap(),
            "original"
        );
        assert!(!target.join("seed.txt").exists());
        let cache_dir = super::run_cache_mount_dir(&cache_root, &mounts[0]);
        assert_eq!(
            std::fs::read_to_string(cache_dir.join("seed.txt")).unwrap(),
            "seed-v1"
        );
        assert_eq!(
            std::fs::read_to_string(cache_dir.join("generated.txt")).unwrap(),
            "persisted"
        );

        std::fs::write(seed_dir.join("seed.txt"), "seed-v2").unwrap();
        let guard = super::PoolRunCacheMounts::activate_with_cache_root(
            &rootfs,
            &mounts,
            &cache_root,
            &completed_stages,
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(target.join("seed.txt")).unwrap(),
            "seed-v1",
            "existing persistent cache should not be re-seeded"
        );
        guard.restore().unwrap();
    }

    #[test]
    fn test_pool_run_cache_mounts_reject_missing_seed_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stage_rootfs = tmp.path().join("stage-rootfs");
        let rootfs = tmp.path().join("rootfs");
        let target = rootfs.join("root/.cache");
        let cache_root = tmp.path().join("run-cache");
        std::fs::create_dir_all(&stage_rootfs).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("original.txt"), "original").unwrap();
        let mounts = vec![RunCacheMount {
            raw: "--mount=type=cache,id=seeded,sharing=locked,from=builder,source=/missing,target=/root/.cache".to_string(),
            id: Some("seeded".to_string()),
            from: Some("builder".to_string()),
            source: "/missing".to_string(),
            sharing: RunCacheSharing::Locked,
            mode: None,
            uid: None,
            gid: None,
            target: "/root/.cache".to_string(),
        }];
        let completed_stages = vec![(Some("builder".to_string()), stage_rootfs)];

        let err = match super::PoolRunCacheMounts::activate_with_cache_root(
            &rootfs,
            &mounts,
            &cache_root,
            &completed_stages,
        ) {
            Ok(_) => panic!("missing RUN cache seed source should fail"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains("RUN cache mount seed source not found"));
        assert_eq!(
            std::fs::read_to_string(target.join("original.txt")).unwrap(),
            "original"
        );
    }

    #[test]
    fn test_pool_run_cache_mounts_reject_file_seed_source() {
        let tmp = tempfile::TempDir::new().unwrap();
        let stage_rootfs = tmp.path().join("stage-rootfs");
        let rootfs = tmp.path().join("rootfs");
        let target = rootfs.join("root/.cache");
        let cache_root = tmp.path().join("run-cache");
        std::fs::create_dir_all(&stage_rootfs).unwrap();
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(stage_rootfs.join("seed-cache"), "not a directory").unwrap();
        std::fs::write(target.join("original.txt"), "original").unwrap();
        let mounts = vec![RunCacheMount {
            raw: "--mount=type=cache,id=seeded,sharing=locked,from=builder,source=/seed-cache,target=/root/.cache".to_string(),
            id: Some("seeded".to_string()),
            from: Some("builder".to_string()),
            source: "/seed-cache".to_string(),
            sharing: RunCacheSharing::Locked,
            mode: None,
            uid: None,
            gid: None,
            target: "/root/.cache".to_string(),
        }];
        let completed_stages = vec![(Some("builder".to_string()), stage_rootfs)];

        let err = match super::PoolRunCacheMounts::activate_with_cache_root(
            &rootfs,
            &mounts,
            &cache_root,
            &completed_stages,
        ) {
            Ok(_) => panic!("file RUN cache seed source should fail"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains("RUN cache mount seed source must be a directory"));
        assert_eq!(
            std::fs::read_to_string(target.join("original.txt")).unwrap(),
            "original"
        );
    }

    #[test]
    fn test_pool_run_cache_mounts_restore_without_sync_discards_failed_run_cache() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let target = rootfs.join("root/.cache");
        let cache_root = tmp.path().join("run-cache");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("original.txt"), "original").unwrap();
        let mounts = vec![RunCacheMount {
            raw: "--mount=type=cache,id=failed,sharing=locked,target=/root/.cache".to_string(),
            id: Some("failed".to_string()),
            from: None,
            source: ".".to_string(),
            sharing: RunCacheSharing::Locked,
            mode: None,
            uid: None,
            gid: None,
            target: "/root/.cache".to_string(),
        }];

        super::ensure_run_cache_mount_targets(&rootfs, &mounts).unwrap();
        let guard =
            super::PoolRunCacheMounts::activate_with_cache_root(&rootfs, &mounts, &cache_root, &[])
                .unwrap();
        std::fs::write(target.join("partial.txt"), "partial").unwrap();
        guard.restore_without_sync().unwrap();

        assert_eq!(
            std::fs::read_to_string(target.join("original.txt")).unwrap(),
            "original"
        );
        assert!(!target.join("partial.txt").exists());
        let cache_dir = super::run_cache_mount_dir(&cache_root, &mounts[0]);
        assert!(
            !cache_dir.join("partial.txt").exists(),
            "failed RUN cache contents must not be persisted"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_pool_run_cache_mounts_apply_root_metadata() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};

        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let target = rootfs.join("var/cache/apt");
        let cache_root = tmp.path().join("run-cache");
        std::fs::create_dir_all(&rootfs).unwrap();
        let uid = unsafe { libc::geteuid() };
        let gid = unsafe { libc::getegid() };
        let mounts = vec![RunCacheMount {
            raw: format!(
                "--mount=type=cache,id=apt,sharing=locked,mode=0750,uid={uid},gid={gid},target=/var/cache/apt"
            ),
            id: Some("apt".to_string()),
            from: None,
            source: ".".to_string(),
            sharing: RunCacheSharing::Locked,
            mode: Some(0o750),
            uid: Some(uid),
            gid: Some(gid),
            target: "/var/cache/apt".to_string(),
        }];

        super::ensure_run_cache_mount_targets(&rootfs, &mounts).unwrap();
        let guard =
            super::PoolRunCacheMounts::activate_with_cache_root(&rootfs, &mounts, &cache_root, &[])
                .unwrap();
        let metadata = std::fs::metadata(&target).unwrap();
        assert_eq!(metadata.permissions().mode() & 0o7777, 0o750);
        assert_eq!(metadata.uid(), uid);
        assert_eq!(metadata.gid(), gid);
        guard.restore().unwrap();
    }

    #[test]
    fn test_pool_run_cache_mounts_reject_duplicate_cache_key() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let cache_root = tmp.path().join("run-cache");
        std::fs::create_dir_all(rootfs.join("a")).unwrap();
        std::fs::create_dir_all(rootfs.join("b")).unwrap();
        let mounts = vec![
            RunCacheMount {
                raw: "--mount=type=cache,id=shared,sharing=locked,target=/a".to_string(),
                id: Some("shared".to_string()),
                from: None,
                source: ".".to_string(),
                sharing: RunCacheSharing::Locked,
                mode: None,
                uid: None,
                gid: None,
                target: "/a".to_string(),
            },
            RunCacheMount {
                raw: "--mount=type=cache,id=shared,sharing=locked,target=/b".to_string(),
                id: Some("shared".to_string()),
                from: None,
                source: ".".to_string(),
                sharing: RunCacheSharing::Locked,
                mode: None,
                uid: None,
                gid: None,
                target: "/b".to_string(),
            },
        ];

        let err = match super::PoolRunCacheMounts::activate_with_cache_root(
            &rootfs,
            &mounts,
            &cache_root,
            &[],
        ) {
            Ok(_) => panic!("duplicate RUN cache mount key should fail"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains("Duplicate RUN cache mount"));
        assert!(rootfs.join("a").is_dir());
        assert!(rootfs.join("b").is_dir());
    }

    #[test]
    fn test_pool_run_cache_mount_hydrate_failure_does_not_sync_partial_cache() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let target = rootfs.join("root/.cache");
        let cache_root = tmp.path().join("run-cache");
        std::fs::create_dir_all(&target).unwrap();
        std::fs::write(target.join("original.txt"), "original").unwrap();
        let mounts = vec![RunCacheMount {
            raw: "--mount=type=cache,id=broken,sharing=locked,target=/root/.cache".to_string(),
            id: Some("broken".to_string()),
            from: None,
            source: ".".to_string(),
            sharing: RunCacheSharing::Locked,
            mode: None,
            uid: None,
            gid: None,
            target: "/root/.cache".to_string(),
        }];
        let cache_dir = super::run_cache_mount_dir(&cache_root, &mounts[0]);
        std::fs::create_dir_all(cache_dir.parent().unwrap()).unwrap();
        std::fs::write(&cache_dir, "not a directory").unwrap();

        let err = match super::PoolRunCacheMounts::activate_with_cache_root(
            &rootfs,
            &mounts,
            &cache_root,
            &[],
        ) {
            Ok(_) => panic!("file cache entry should fail to hydrate"),
            Err(err) => err.to_string(),
        };

        assert!(err.contains("is not a directory"));
        assert_eq!(
            std::fs::read_to_string(target.join("original.txt")).unwrap(),
            "original"
        );
        assert!(cache_dir.is_file());
        assert_eq!(
            std::fs::read_to_string(cache_dir).unwrap(),
            "not a directory"
        );
    }

    #[cfg(unix)]
    #[test]
    fn test_pool_run_cache_mount_lock_blocks_same_cache_key() {
        use std::sync::mpsc;
        use std::time::Duration;

        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs-a");
        let cache_root = tmp.path().join("run-cache");
        std::fs::create_dir_all(&rootfs).unwrap();
        let mounts = vec![RunCacheMount {
            raw: "--mount=type=cache,id=shared,sharing=locked,target=/root/.cache".to_string(),
            id: Some("shared".to_string()),
            from: None,
            source: ".".to_string(),
            sharing: RunCacheSharing::Locked,
            mode: None,
            uid: None,
            gid: None,
            target: "/root/.cache".to_string(),
        }];
        super::ensure_run_cache_mount_targets(&rootfs, &mounts).unwrap();
        let guard =
            super::PoolRunCacheMounts::activate_with_cache_root(&rootfs, &mounts, &cache_root, &[])
                .unwrap();

        let rootfs_b = tmp.path().join("rootfs-b");
        std::fs::create_dir_all(&rootfs_b).unwrap();
        super::ensure_run_cache_mount_targets(&rootfs_b, &mounts).unwrap();
        let thread_mounts = mounts.clone();
        let thread_cache_root = cache_root.clone();
        let (started_tx, started_rx) = mpsc::channel();
        let (done_tx, done_rx) = mpsc::channel();
        let waiter = std::thread::spawn(move || {
            started_tx.send(()).unwrap();
            let guard = super::PoolRunCacheMounts::activate_with_cache_root(
                &rootfs_b,
                &thread_mounts,
                &thread_cache_root,
                &[],
            )
            .unwrap();
            done_tx.send(()).unwrap();
            guard.restore().unwrap();
        });

        started_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        assert!(
            done_rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "same RUN cache key should remain locked while the first mount is active"
        );

        guard.restore().unwrap();
        done_rx.recv_timeout(Duration::from_secs(1)).unwrap();
        waiter.join().unwrap();
    }

    #[test]
    fn test_instruction_to_string_copy() {
        let instr = Instruction::Copy {
            src: vec!["file1.txt".to_string(), "file2.txt".to_string()],
            dst: "/app/".to_string(),
            from: None,
            chown: None,
        };
        assert_eq!(
            instruction_to_string(&instr),
            "COPY file1.txt file2.txt /app/"
        );
    }

    #[test]
    fn test_instruction_to_string_copy_from_stage() {
        let instr = Instruction::Copy {
            src: vec!["app".to_string()],
            dst: "/usr/local/bin/".to_string(),
            from: Some("builder".to_string()),
            chown: None,
        };
        assert_eq!(
            instruction_to_string(&instr),
            "COPY --from=builder app /usr/local/bin/"
        );
    }

    #[test]
    fn test_instruction_to_string_add() {
        let instr = Instruction::Add {
            src: vec!["app.tar.gz".to_string()],
            dst: "/app/".to_string(),
            chown: Some("1000:1000".to_string()),
        };
        assert_eq!(
            instruction_to_string(&instr),
            "ADD --chown=1000:1000 app.tar.gz /app/"
        );
    }

    #[test]
    fn test_instruction_to_string_add_no_chown() {
        let instr = Instruction::Add {
            src: vec!["file.tar.gz".to_string()],
            dst: "/tmp/".to_string(),
            chown: None,
        };
        assert_eq!(instruction_to_string(&instr), "ADD file.tar.gz /tmp/");
    }

    #[test]
    fn test_instruction_to_string_env() {
        let instr = Instruction::Env {
            vars: vec![("PATH".to_string(), "/usr/local/bin:/usr/bin".to_string())],
        };
        assert_eq!(
            instruction_to_string(&instr),
            "ENV PATH=/usr/local/bin:/usr/bin"
        );
    }

    #[test]
    fn test_instruction_to_string_workdir() {
        let instr = Instruction::Workdir {
            path: "/app".to_string(),
        };
        assert_eq!(instruction_to_string(&instr), "WORKDIR /app");
    }

    #[test]
    fn test_instruction_to_string_entrypoint() {
        let instr = Instruction::Entrypoint {
            exec: vec!["/bin/agent".to_string(), "--listen".to_string()],
        };
        assert_eq!(
            instruction_to_string(&instr),
            "ENTRYPOINT [\"/bin/agent\", \"--listen\"]"
        );
    }

    #[test]
    fn test_instruction_to_string_cmd() {
        let instr = Instruction::Cmd {
            exec: vec!["python".to_string(), "app.py".to_string()],
        };
        assert_eq!(
            instruction_to_string(&instr),
            "CMD [\"python\", \"app.py\"]"
        );
    }

    #[test]
    fn test_instruction_to_string_expose() {
        let instr = Instruction::Expose {
            ports: vec!["8080/tcp".to_string()],
        };
        assert_eq!(instruction_to_string(&instr), "EXPOSE 8080/tcp");
    }

    #[test]
    fn test_instruction_to_string_label() {
        let instr = Instruction::Label {
            pairs: vec![("version".to_string(), "1.0.0".to_string())],
        };
        assert_eq!(instruction_to_string(&instr), "LABEL version=1.0.0");
    }

    #[test]
    fn test_instruction_to_string_user() {
        let instr = Instruction::User {
            user: "nobody".to_string(),
        };
        assert_eq!(instruction_to_string(&instr), "USER nobody");
    }

    #[test]
    fn test_instruction_to_string_arg_no_default() {
        let instr = Instruction::Arg {
            name: "VERSION".to_string(),
            default: None,
        };
        assert_eq!(instruction_to_string(&instr), "ARG VERSION");
    }

    #[test]
    fn test_instruction_to_string_arg_with_default() {
        let instr = Instruction::Arg {
            name: "VERSION".to_string(),
            default: Some("1.0.0".to_string()),
        };
        assert_eq!(instruction_to_string(&instr), "ARG VERSION=1.0.0");
    }

    #[test]
    fn test_instruction_to_string_shell() {
        let instr = Instruction::Shell {
            exec: vec!["/bin/bash".to_string(), "-c".to_string()],
        };
        assert_eq!(
            instruction_to_string(&instr),
            "SHELL [\"/bin/bash\", \"-c\"]"
        );
    }

    #[test]
    fn test_instruction_to_string_stopsignal() {
        let instr = Instruction::StopSignal {
            signal: "SIGTERM".to_string(),
        };
        assert_eq!(instruction_to_string(&instr), "STOPSIGNAL SIGTERM");
    }

    #[test]
    fn test_instruction_to_string_healthcheck_none() {
        let instr = Instruction::HealthCheck {
            cmd: None,
            interval: None,
            timeout: None,
            retries: None,
            start_period: None,
        };
        assert_eq!(instruction_to_string(&instr), "HEALTHCHECK NONE");
    }

    #[test]
    fn test_instruction_to_string_healthcheck_with_cmd() {
        let instr = Instruction::HealthCheck {
            cmd: Some(vec![
                "curl".to_string(),
                "-f".to_string(),
                "http://localhost/".to_string(),
            ]),
            interval: Some(10),
            timeout: Some(5),
            retries: Some(3),
            start_period: Some(30),
        };
        assert_eq!(
            instruction_to_string(&instr),
            "HEALTHCHECK CMD curl -f http://localhost/"
        );
    }

    #[test]
    fn test_instruction_to_string_volume() {
        let instr = Instruction::Volume {
            paths: vec!["/data".to_string(), "/var/log".to_string()],
        };
        assert_eq!(instruction_to_string(&instr), "VOLUME /data /var/log");
    }

    #[test]
    fn test_instruction_to_string_from() {
        let instr = Instruction::From {
            image: "alpine:3.19".to_string(),
            alias: None,
        };
        assert_eq!(instruction_to_string(&instr), "FROM alpine:3.19");
    }

    #[test]
    fn test_instruction_to_string_from_with_alias() {
        let instr = Instruction::From {
            image: "golang:1.21".to_string(),
            alias: Some("builder".to_string()),
        };
        assert_eq!(instruction_to_string(&instr), "FROM golang:1.21 AS builder");
    }

    #[test]
    fn test_instruction_to_string_onbuild() {
        let inner = Instruction::Run {
            command: RunCommand::Shell("echo triggered".to_string()),
            cache_mounts: vec![],
            bind_mounts: vec![],
            tmpfs_mounts: vec![],
        };
        let instr = Instruction::OnBuild {
            instruction: Box::new(inner),
        };
        assert_eq!(instruction_to_string(&instr), "ONBUILD RUN echo triggered");
    }

    #[test]
    fn test_handle_add_chown_numeric_uid_gid() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let layers = tmp.path().join("layers");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&layers).unwrap();
        // Write the file so ADD can find it
        std::fs::write(tmp.path().join("file.txt"), "data").unwrap();

        // Numeric uid:gid — resolves without /etc/passwd, should succeed.
        let result = handle_add(
            &["file.txt".to_string()],
            "/tmp/file.txt",
            Some("1000:1000"),
            tmp.path(),
            &rootfs,
            &layers,
            "/",
            0,
            None,
        );
        assert!(
            result.is_ok(),
            "ADD --chown with numeric uid:gid should succeed: {:?}",
            result.err()
        );
        // Checking that the layer was created is sufficient for unit coverage.
        assert!(result.unwrap().path.exists());
    }

    #[test]
    fn test_execute_onbuild_trigger_rejects_execution_instruction() {
        let mut state = BuildState::new(HashMap::new());
        let config = BuildConfig {
            context_dir: PathBuf::from("/tmp/context"),
            dockerfile_path: PathBuf::from("/tmp/context/Dockerfile"),
            tag: None,
            build_args: HashMap::new(),
            quiet: true,
            platforms: vec![],
            target: None,
            no_cache: false,
            metrics: None,
            run_pool: None,
        };
        let tmp = tempfile::TempDir::new().unwrap();

        let err = execute_onbuild_trigger(
            "RUN echo trigger",
            &mut state,
            &config,
            tmp.path(),
            tmp.path(),
            &[],
            &[],
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("ONBUILD trigger 'RUN echo trigger' is not supported yet"));
    }

    #[test]
    fn test_linux_run_shell_path_defaults_to_bin_sh() {
        assert_eq!(super::linux_run_shell_path(&[]), "/bin/sh");
        assert_eq!(
            super::linux_run_shell_path(&["/bin/bash".to_string(), "-c".to_string()]),
            "/bin/bash"
        );
    }

    #[test]
    fn test_linux_run_preconditions_reject_non_root() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(rootfs.join("bin")).unwrap();
        std::fs::write(rootfs.join("bin/sh"), "fake shell").unwrap();

        let err = super::validate_linux_run_preconditions(&rootfs, &shell_run("true"), &[], 1000)
            .unwrap_err()
            .to_string();

        assert!(err.contains("requires root privileges"));
    }

    #[test]
    fn test_linux_run_preconditions_reject_missing_shell() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        let err = super::validate_linux_run_preconditions(&rootfs, &shell_run("true"), &[], 0)
            .unwrap_err()
            .to_string();

        assert!(err.contains("was not found in rootfs"));
    }

    #[cfg(unix)]
    #[test]
    fn test_linux_run_preconditions_accept_absolute_shell_symlink() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(rootfs.join("bin")).unwrap();
        std::fs::write(rootfs.join("bin/busybox"), "fake busybox").unwrap();
        std::os::unix::fs::symlink("/bin/busybox", rootfs.join("bin/sh")).unwrap();

        super::validate_linux_run_preconditions(&rootfs, &shell_run("true"), &[], 0).unwrap();
    }

    #[test]
    fn test_linux_run_preconditions_reject_relative_shell() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        let err = super::validate_linux_run_preconditions(
            &rootfs,
            &shell_run("true"),
            &["sh".to_string()],
            0,
        )
        .unwrap_err()
        .to_string();

        assert!(err.contains("is not absolute"));
    }

    #[test]
    fn test_run_exec_preconditions_accept_absolute_executable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(rootfs.join("bin")).unwrap();
        std::fs::write(rootfs.join("bin/echo"), "fake echo").unwrap();

        super::validate_run_exec_preconditions(&rootfs, &["/bin/echo".to_string()]).unwrap();
    }

    #[test]
    fn test_run_exec_preconditions_reject_missing_absolute_executable() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        let err = super::validate_run_exec_preconditions(&rootfs, &["/bin/missing".to_string()])
            .unwrap_err()
            .to_string();

        assert!(err.contains("was not found in rootfs"));
    }

    #[test]
    fn test_ensure_linux_run_workdir_creates_absolute_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        let workdir = super::ensure_linux_run_workdir(&rootfs, "/app/build").unwrap();

        assert_eq!(workdir, rootfs.join("app/build"));
        assert!(workdir.is_dir());
    }

    #[test]
    fn test_ensure_linux_run_workdir_rejects_relative_path() {
        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        std::fs::create_dir_all(&rootfs).unwrap();

        let err = super::ensure_linux_run_workdir(&rootfs, "app")
            .unwrap_err()
            .to_string();

        assert!(err.contains("is not absolute"));
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn test_handle_run_rejects_macos_without_unsafe_opt_in() {
        std::env::remove_var(super::UNSAFE_HOST_RUN_ENV);

        let tmp = tempfile::TempDir::new().unwrap();
        let rootfs = tmp.path().join("rootfs");
        let layers = tmp.path().join("layers");
        std::fs::create_dir_all(&rootfs).unwrap();
        std::fs::create_dir_all(&layers).unwrap();

        let command = shell_run("echo unsafe");
        let result = super::handle_run(
            &command,
            &[],
            &[],
            &[],
            tmp.path(),
            &[],
            &rootfs,
            &layers,
            "/",
            &[],
            &[],
            0,
            true,
            None,
        );
        let err = result.unwrap_err().to_string();
        assert!(err.contains("Dockerfile RUN is not supported on macOS yet"));
        assert!(err.contains("--builder=buildkit-vm"));
        assert!(err.contains(super::UNSAFE_HOST_RUN_ENV));
    }
}
