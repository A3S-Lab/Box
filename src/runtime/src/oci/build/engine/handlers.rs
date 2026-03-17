//! Instruction handlers for the build engine.

use std::path::{Path, PathBuf};

use a3s_box_core::error::{BoxError, Result};

use super::super::dockerfile::Instruction;
use super::super::layer::{create_layer, create_layer_from_dir, LayerInfo};
use super::utils::{
    copy_dir_recursive, expand_args, extract_tar_to_dst, is_tar_archive, resolve_path,
};
use super::BuildState;

/// Handle COPY: copy files from build context into rootfs, create a layer.
pub(super) fn handle_copy(
    src_patterns: &[String],
    dst: &str,
    context_dir: &Path,
    rootfs_dir: &Path,
    layers_dir: &Path,
    workdir: &str,
    layer_index: usize,
) -> Result<LayerInfo> {
    // Resolve destination path
    let resolved_dst = resolve_path(workdir, dst);
    let dst_in_rootfs = rootfs_dir.join(resolved_dst.trim_start_matches('/'));

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
        let src_path = context_dir.join(src);
        if !src_path.exists() {
            return Err(BoxError::BuildError(format!(
                "COPY source not found: {} (in context {})",
                src,
                context_dir.display()
            )));
        }

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_in_rootfs)?;
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

    // Create a layer from the copied files
    // We use create_layer_from_dir approach: snapshot the destination
    let layer_path = layers_dir.join(format!("layer_{}.tar.gz", layer_index));

    // For COPY, create a layer containing just the destination files
    let target_prefix = Path::new(resolved_dst.trim_start_matches('/'));
    if dst_in_rootfs.is_dir() {
        create_layer_from_dir(&dst_in_rootfs, target_prefix, &layer_path)
    } else if dst_in_rootfs.parent().is_some() {
        // Single file copy: create layer with just that file
        let changed = vec![PathBuf::from(
            dst_in_rootfs
                .strip_prefix(rootfs_dir)
                .unwrap_or(target_prefix),
        )];
        create_layer(rootfs_dir, &changed, &layer_path)
    } else {
        Err(BoxError::BuildError("Invalid COPY destination".to_string()))
    }
}

/// Handle RUN: execute a command in the rootfs.
///
/// On Linux, uses chroot. On macOS, tries Docker/Podman, or skips with a warning.
/// Returns Some(LayerInfo) if a layer was created, None if skipped.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_run(
    command: &str,
    rootfs_dir: &Path,
    layers_dir: &Path,
    workdir: &str,
    env: &[(String, String)],
    shell: &[String],
    layer_index: usize,
    quiet: bool,
) -> Result<Option<LayerInfo>> {
    #[cfg(target_os = "macos")]
    {
        // On macOS, use a3s-box MicroVM to execute RUN commands
        return handle_run_via_microvm(
            command,
            rootfs_dir,
            layers_dir,
            workdir,
            env,
            shell,
            layer_index,
            quiet,
        );
    }

    // Linux: execute via chroot
    #[cfg(target_os = "linux")]
    {
        use super::super::layer::DirSnapshot;

        let before = DirSnapshot::capture(rootfs_dir)?;

        // Build the command using the configured shell
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
        cmd.arg(command);

        // Set environment
        cmd.env_clear();
        cmd.env(
            "PATH",
            "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin",
        );
        cmd.env("HOME", "/root");
        for (key, value) in env {
            cmd.env(key, value);
        }

        let output = cmd
            .output()
            .map_err(|e| BoxError::BuildError(format!("Failed to execute RUN command: {}", e)))?;

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(BoxError::BuildError(format!(
                "RUN command failed (exit {}): {}",
                output.status.code().unwrap_or(-1),
                stderr.trim()
            )));
        }

        if !quiet {
            let stdout = String::from_utf8_lossy(&output.stdout);
            if !stdout.is_empty() {
                print!("{}", stdout);
            }
        }

        // Capture diff
        let after = DirSnapshot::capture(rootfs_dir)?;
        let changed = before.diff(&after);

        if changed.is_empty() {
            return Ok(None);
        }

        let layer_path = layers_dir.join(format!("layer_{}.tar.gz", layer_index));
        let layer_info = create_layer(rootfs_dir, &changed, &layer_path)?;
        Ok(Some(layer_info))
    }

    // Other platforms: not supported
    #[cfg(not(any(target_os = "linux", target_os = "macos")))]
    {
        let _ = (command, rootfs_dir, layers_dir, workdir, env, shell, layer_index, quiet);
        Ok(None)
    }
}

/// Execute RUN command via a3s-box MicroVM on macOS.
///
/// Workflow:
/// 1. Create a temporary OCI image from current rootfs
/// 2. Run command in a MicroVM container
/// 3. Commit the container to capture changes
/// 4. Export and extract back to rootfs
#[cfg(target_os = "macos")]
#[allow(clippy::too_many_arguments)]
fn handle_run_via_microvm(
    command: &str,
    rootfs_dir: &Path,
    layers_dir: &Path,
    workdir: &str,
    env: &[(String, String)],
    shell: &[String],
    layer_index: usize,
    quiet: bool,
) -> Result<Option<LayerInfo>> {
    use super::super::layer::DirSnapshot;

    if !quiet {
        println!("→ Using a3s-box MicroVM to execute RUN command");
    }

    // Capture filesystem state before execution
    let before = DirSnapshot::capture(rootfs_dir)?;

    // Create temporary image and container names
    let temp_image = format!("a3s-box-build-temp-{}", uuid::Uuid::new_v4());
    let temp_container = format!("a3s-box-build-run-{}", uuid::Uuid::new_v4());
    let committed_image = format!("a3s-box-build-committed-{}", uuid::Uuid::new_v4());

    // Step 1: Create tar from rootfs and build temporary image
    if !quiet {
        println!("→ Creating temporary image from rootfs...");
    }

    let temp_tar = std::env::temp_dir().join(format!("{}.tar", temp_image));
    let tar_output = std::process::Command::new("tar")
        .arg("-cf")
        .arg(&temp_tar)
        .arg("-C")
        .arg(rootfs_dir)
        .arg(".")
        .output()
        .map_err(|e| BoxError::BuildError(format!("Failed to create tar: {}", e)))?;

    if !tar_output.status.success() {
        let stderr = String::from_utf8_lossy(&tar_output.stderr);
        return Err(BoxError::BuildError(format!(
            "Failed to create rootfs tar: {}",
            stderr.trim()
        )));
    }

    // Build image from tar using a3s-box build with a minimal Dockerfile
    let temp_dockerfile = std::env::temp_dir().join(format!("{}.Dockerfile", temp_image));
    let dockerfile_content = if workdir == "/" {
        format!(
            "FROM scratch\nADD {} /\n",
            temp_tar.file_name().unwrap().to_string_lossy()
        )
    } else {
        format!(
            "FROM scratch\nADD {} /\nWORKDIR {}\n",
            temp_tar.file_name().unwrap().to_string_lossy(),
            workdir
        )
    };

    std::fs::write(&temp_dockerfile, dockerfile_content)
        .map_err(|e| BoxError::BuildError(format!("Failed to write Dockerfile: {}", e)))?;

    let build_output = std::process::Command::new("a3s-box")
        .arg("build")
        .arg("-t")
        .arg(&temp_image)
        .arg("-f")
        .arg(&temp_dockerfile)
        .arg(temp_tar.parent().unwrap())
        .output()
        .map_err(|e| {
            std::fs::remove_file(&temp_tar).ok();
            std::fs::remove_file(&temp_dockerfile).ok();
            BoxError::BuildError(format!("Failed to build temp image: {}", e))
        })?;

    std::fs::remove_file(&temp_tar).ok();
    std::fs::remove_file(&temp_dockerfile).ok();

    if !build_output.status.success() {
        let stderr = String::from_utf8_lossy(&build_output.stderr);
        return Err(BoxError::BuildError(format!(
            "Failed to build temporary image: {}",
            stderr.trim()
        )));
    }

    // Step 2: Create container and execute command
    if !quiet {
        println!("→ Executing: {}", command);
    }

    // Build the shell command
    let shell_cmd = if !shell.is_empty() {
        let mut parts = shell.to_vec();
        parts.push(command.to_string());
        parts
    } else {
        vec!["/bin/sh".to_string(), "-c".to_string(), command.to_string()]
    };

    let mut create_cmd = std::process::Command::new("a3s-box");
    create_cmd
        .arg("create")
        .arg("--name")
        .arg(&temp_container)
        .arg("-w")
        .arg(workdir);

    // Add environment variables
    for (key, value) in env {
        create_cmd.arg("-e").arg(format!("{}={}", key, value));
    }

    // Add image and command
    create_cmd.arg(&temp_image);
    for part in &shell_cmd {
        create_cmd.arg(part);
    }

    let create_output = create_cmd.output().map_err(|e| {
        std::process::Command::new("a3s-box")
            .arg("rmi")
            .arg(&temp_image)
            .output()
            .ok();
        BoxError::BuildError(format!("Failed to create container: {}", e))
    })?;

    if !create_output.status.success() {
        let stderr = String::from_utf8_lossy(&create_output.stderr);
        std::process::Command::new("a3s-box")
            .arg("rmi")
            .arg(&temp_image)
            .output()
            .ok();
        return Err(BoxError::BuildError(format!(
            "Failed to create container: {}",
            stderr.trim()
        )));
    }

    // Start and wait for container
    let start_output = std::process::Command::new("a3s-box")
        .arg("start")
        .arg("-a")  // Attach to see output
        .arg(&temp_container)
        .output()
        .map_err(|e| {
            cleanup_temp_resources(&temp_container, &temp_image, None);
            BoxError::BuildError(format!("Failed to start container: {}", e))
        })?;

    if !start_output.status.success() {
        let stderr = String::from_utf8_lossy(&start_output.stderr);
        cleanup_temp_resources(&temp_container, &temp_image, None);
        return Err(BoxError::BuildError(format!(
            "RUN command failed (exit {}): {}",
            start_output.status.code().unwrap_or(-1),
            stderr.trim()
        )));
    }

    if !quiet {
        let stdout = String::from_utf8_lossy(&start_output.stdout);
        if !stdout.is_empty() {
            print!("{}", stdout);
        }
    }

    // Step 3: Commit container to new image
    if !quiet {
        println!("→ Committing changes...");
    }

    let commit_output = std::process::Command::new("a3s-box")
        .arg("commit")
        .arg(&temp_container)
        .arg(&committed_image)
        .output()
        .map_err(|e| {
            cleanup_temp_resources(&temp_container, &temp_image, None);
            BoxError::BuildError(format!("Failed to commit container: {}", e))
        })?;

    if !commit_output.status.success() {
        let stderr = String::from_utf8_lossy(&commit_output.stderr);
        cleanup_temp_resources(&temp_container, &temp_image, None);
        return Err(BoxError::BuildError(format!(
            "Failed to commit changes: {}",
            stderr.trim()
        )));
    }

    // Step 4: Export committed image
    if !quiet {
        println!("→ Exporting modified filesystem...");
    }

    let export_tar = std::env::temp_dir().join(format!("{}-export.tar", committed_image));
    let export_output = std::process::Command::new("a3s-box")
        .arg("export")
        .arg(&temp_container)
        .arg("-o")
        .arg(&export_tar)
        .output()
        .map_err(|e| {
            cleanup_temp_resources(&temp_container, &temp_image, Some(&committed_image));
            BoxError::BuildError(format!("Failed to export container: {}", e))
        })?;

    if !export_output.status.success() {
        let stderr = String::from_utf8_lossy(&export_output.stderr);
        cleanup_temp_resources(&temp_container, &temp_image, Some(&committed_image));
        std::fs::remove_file(&export_tar).ok();
        return Err(BoxError::BuildError(format!(
            "Failed to export container: {}",
            stderr.trim()
        )));
    }

    // Step 5: Extract to rootfs
    let extract_output = std::process::Command::new("tar")
        .arg("-xf")
        .arg(&export_tar)
        .arg("-C")
        .arg(rootfs_dir)
        .output()
        .map_err(|e| {
            cleanup_temp_resources(&temp_container, &temp_image, Some(&committed_image));
            std::fs::remove_file(&export_tar).ok();
            BoxError::BuildError(format!("Failed to extract tar: {}", e))
        })?;

    // Cleanup
    cleanup_temp_resources(&temp_container, &temp_image, Some(&committed_image));
    std::fs::remove_file(&export_tar).ok();

    if !extract_output.status.success() {
        let stderr = String::from_utf8_lossy(&extract_output.stderr);
        return Err(BoxError::BuildError(format!(
            "Failed to extract filesystem: {}",
            stderr.trim()
        )));
    }

    // Capture filesystem changes
    let after = DirSnapshot::capture(rootfs_dir)?;
    let changed = before.diff(&after);

    if changed.is_empty() {
        if !quiet {
            println!("→ No filesystem changes detected");
        }
        return Ok(None);
    }

    // Create layer from changes
    let layer_path = layers_dir.join(format!("layer_{}.tar.gz", layer_index));
    let layer_info = create_layer(rootfs_dir, &changed, &layer_path)?;

    if !quiet {
        println!("→ Created layer with {} changes", changed.len());
    }

    Ok(Some(layer_info))
}

/// Clean up temporary resources (container and images).
#[cfg(target_os = "macos")]
fn cleanup_temp_resources(container: &str, temp_image: &str, committed_image: Option<&str>) {
    // Remove container
    std::process::Command::new("a3s-box")
        .arg("rm")
        .arg("-f")
        .arg(container)
        .output()
        .ok();

    // Remove temp image
    std::process::Command::new("a3s-box")
        .arg("rmi")
        .arg(temp_image)
        .output()
        .ok();

    // Remove committed image if provided
    if let Some(img) = committed_image {
        std::process::Command::new("a3s-box")
            .arg("rmi")
            .arg(img)
            .output()
            .ok();
    }
}

/// Handle ADD: like COPY but supports URL download and tar auto-extraction.
#[allow(clippy::too_many_arguments)]
pub(super) fn handle_add(
    src_patterns: &[String],
    dst: &str,
    _chown: Option<&str>,
    context_dir: &Path,
    rootfs_dir: &Path,
    layers_dir: &Path,
    workdir: &str,
    layer_index: usize,
) -> Result<LayerInfo> {
    let resolved_dst = resolve_path(workdir, dst);
    let dst_in_rootfs = rootfs_dir.join(resolved_dst.trim_start_matches('/'));

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

        let src_path = context_dir.join(src);
        if !src_path.exists() {
            return Err(BoxError::BuildError(format!(
                "ADD source not found: {} (in context {})",
                src,
                context_dir.display()
            )));
        }

        // Check if it's a tar archive that should be auto-extracted
        if is_tar_archive(src) && !src_path.is_dir() {
            extract_tar_to_dst(&src_path, &dst_in_rootfs)?;
        } else if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_in_rootfs)?;
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

    // Create a layer from the destination
    let layer_path = layers_dir.join(format!("layer_{}.tar.gz", layer_index));
    let target_prefix = Path::new(resolved_dst.trim_start_matches('/'));
    if dst_in_rootfs.is_dir() {
        create_layer_from_dir(&dst_in_rootfs, target_prefix, &layer_path)
    } else if dst_in_rootfs.parent().is_some() {
        let changed = vec![PathBuf::from(
            dst_in_rootfs
                .strip_prefix(rootfs_dir)
                .unwrap_or(target_prefix),
        )];
        create_layer(rootfs_dir, &changed, &layer_path)
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
        Instruction::Env { key, value } => {
            let expanded = expand_args(value, &state.build_args);
            if let Some(existing) = state.env.iter_mut().find(|(k, _)| k == key) {
                existing.1 = expanded;
            } else {
                state.env.push((key.clone(), expanded));
            }
        }
        Instruction::Label { key, value } => {
            state.labels.insert(key.clone(), value.clone());
        }
        Instruction::Workdir { path } => {
            state.workdir = resolve_path(&state.workdir, path);
        }
        Instruction::Expose { port } => {
            state.exposed_ports.push(port.clone());
        }
        Instruction::User { user } => {
            state.user = Some(user.clone());
        }
        _ => {
            tracing::warn!(
                trigger = trigger,
                "ONBUILD trigger requires execution context, skipping"
            );
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
        Instruction::Run { command } => format!("RUN {}", command),
        Instruction::Copy { src, dst, from } => {
            if let Some(f) = from {
                format!("COPY --from={} {} {}", f, src.join(" "), dst)
            } else {
                format!("COPY {} {}", src.join(" "), dst)
            }
        }
        Instruction::Add { src, dst, chown } => {
            if let Some(c) = chown {
                format!("ADD --chown={} {} {}", c, src.join(" "), dst)
            } else {
                format!("ADD {} {}", src.join(" "), dst)
            }
        }
        Instruction::Workdir { path } => format!("WORKDIR {}", path),
        Instruction::Env { key, value } => format!("ENV {}={}", key, value),
        Instruction::Entrypoint { exec } => format!("ENTRYPOINT {:?}", exec),
        Instruction::Cmd { exec } => format!("CMD {:?}", exec),
        Instruction::Expose { port } => format!("EXPOSE {}", port),
        Instruction::Label { key, value } => format!("LABEL {}={}", key, value),
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
                .build()
                .map_err(|e| format!("Failed to build HTTP client: {}", e))?;

            let response = client
                .get(url)
                .send()
                .await
                .map_err(|e| format!("HTTP request failed: {}", e))?;

            if !response.status().is_success() {
                return Err(format!("HTTP {} for {}", response.status(), url));
            }

            response
                .bytes()
                .await
                .map(|b| b.to_vec())
                .map_err(|e| format!("Failed to read response body: {}", e))
        })
    })
}
