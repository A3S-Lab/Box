//! Build engine for constructing OCI images from Dockerfiles.
//!
//! Orchestrates the build process: parses the Dockerfile, pulls the base image,
//! executes each instruction, creates layers, and assembles the final OCI image.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use a3s_box_core::error::{BoxError, Result};

use super::dockerfile::{Dockerfile, Instruction};
use super::layer::{
    create_layer, create_layer_from_dir, sha256_bytes, sha256_file, LayerInfo,
};
use crate::oci::image::OciImageConfig;
use crate::oci::layers::extract_layer;
use crate::oci::store::ImageStore;
use crate::oci::{ImagePuller, RegistryAuth};

/// Configuration for a build operation.
#[derive(Debug, Clone)]
pub struct BuildConfig {
    /// Path to the build context directory
    pub context_dir: PathBuf,
    /// Path to the Dockerfile (relative to context or absolute)
    pub dockerfile_path: PathBuf,
    /// Image tag (e.g., "myimage:latest")
    pub tag: Option<String>,
    /// Build arguments (ARG overrides)
    pub build_args: HashMap<String, String>,
    /// Suppress build output
    pub quiet: bool,
}

/// Result of a successful build.
#[derive(Debug)]
pub struct BuildResult {
    /// Image reference stored in the image store
    pub reference: String,
    /// Content digest
    pub digest: String,
    /// Total image size in bytes
    pub size: u64,
    /// Number of layers
    pub layer_count: usize,
}

/// Mutable state accumulated during the build.
struct BuildState {
    /// Working directory inside the image
    workdir: String,
    /// Environment variables
    env: Vec<(String, String)>,
    /// Entrypoint
    entrypoint: Option<Vec<String>>,
    /// Default command
    cmd: Option<Vec<String>>,
    /// User
    user: Option<String>,
    /// Exposed ports
    exposed_ports: Vec<String>,
    /// Labels
    labels: HashMap<String, String>,
    /// Layer info accumulated during build
    layers: Vec<LayerInfo>,
    /// Diff IDs (uncompressed layer digests) for the OCI config
    diff_ids: Vec<String>,
    /// History entries
    history: Vec<HistoryEntry>,
    /// Build arguments
    build_args: HashMap<String, String>,
}

/// A single history entry for the OCI config.
#[derive(Debug, Clone)]
struct HistoryEntry {
    created_by: String,
    empty_layer: bool,
}

impl BuildState {
    fn new(build_args: HashMap<String, String>) -> Self {
        Self {
            workdir: "/".to_string(),
            env: Vec::new(),
            entrypoint: None,
            cmd: None,
            user: None,
            exposed_ports: Vec::new(),
            labels: HashMap::new(),
            layers: Vec::new(),
            diff_ids: Vec::new(),
            history: Vec::new(),
            build_args,
        }
    }
}

/// Execute a full image build from a Dockerfile.
///
/// # Process
///
/// 1. Parse the Dockerfile
/// 2. Pull the base image (FROM)
/// 3. Extract base image layers into a temporary rootfs
/// 4. Execute each instruction, creating layers as needed
/// 5. Assemble the final OCI image layout
/// 6. Store in the image store with the given tag
pub async fn build(config: BuildConfig, store: Arc<ImageStore>) -> Result<BuildResult> {
    // Parse Dockerfile
    let dockerfile = Dockerfile::from_file(&config.dockerfile_path)?;

    if !config.quiet {
        println!(
            "Building from {}",
            config.dockerfile_path.display()
        );
    }

    let mut state = BuildState::new(config.build_args.clone());

    // Create temp directory for build workspace
    let build_dir = tempfile::TempDir::new().map_err(|e| {
        BoxError::BuildError(format!("Failed to create build directory: {}", e))
    })?;
    let rootfs_dir = build_dir.path().join("rootfs");
    let layers_dir = build_dir.path().join("layers");
    std::fs::create_dir_all(&rootfs_dir).map_err(|e| {
        BoxError::BuildError(format!("Failed to create rootfs directory: {}", e))
    })?;
    std::fs::create_dir_all(&layers_dir).map_err(|e| {
        BoxError::BuildError(format!("Failed to create layers directory: {}", e))
    })?;

    // Track base image layer info for the final image
    let mut base_layers: Vec<LayerInfo> = Vec::new();
    let mut base_diff_ids: Vec<String> = Vec::new();

    // Process instructions
    let total = dockerfile.instructions.len();
    for (idx, instruction) in dockerfile.instructions.iter().enumerate() {
        let step = idx + 1;

        match instruction {
            Instruction::From { image, .. } => {
                if !config.quiet {
                    println!("Step {}/{}: FROM {}", step, total, image);
                }
                let (layers, diff_ids, base_config) = handle_from(
                    image,
                    &rootfs_dir,
                    &layers_dir,
                    &store,
                    &state.build_args,
                )
                .await?;
                base_layers = layers;
                base_diff_ids = diff_ids;

                // Inherit config from base image
                apply_base_config(&mut state, &base_config);

                state.history.push(HistoryEntry {
                    created_by: format!("FROM {}", image),
                    empty_layer: true,
                });
            }

            Instruction::Copy { src, dst, from } => {
                if from.is_some() {
                    if !config.quiet {
                        println!(
                            "Step {}/{}: COPY --from (skipped, multi-stage not supported)",
                            step, total
                        );
                    }
                    state.history.push(HistoryEntry {
                        created_by: format!("COPY --from={} {} {}", from.as_deref().unwrap_or("?"), src.join(" "), dst),
                        empty_layer: true,
                    });
                    continue;
                }

                if !config.quiet {
                    println!("Step {}/{}: COPY {} {}", step, total, src.join(" "), dst);
                }
                let layer_info = handle_copy(
                    src,
                    dst,
                    &config.context_dir,
                    &rootfs_dir,
                    &layers_dir,
                    &state.workdir,
                    state.layers.len() + base_layers.len(),
                )?;
                state.diff_ids.push(compute_diff_id(&layer_info.path)?);
                state.layers.push(layer_info);
                state.history.push(HistoryEntry {
                    created_by: format!("COPY {} {}", src.join(" "), dst),
                    empty_layer: false,
                });
            }

            Instruction::Run { command } => {
                if !config.quiet {
                    println!("Step {}/{}: RUN {}", step, total, command);
                }
                let layer_opt = handle_run(
                    command,
                    &rootfs_dir,
                    &layers_dir,
                    &state.workdir,
                    &state.env,
                    state.layers.len() + base_layers.len(),
                    config.quiet,
                )?;
                if let Some(layer_info) = layer_opt {
                    state.diff_ids.push(compute_diff_id(&layer_info.path)?);
                    state.layers.push(layer_info);
                    state.history.push(HistoryEntry {
                        created_by: format!("RUN {}", command),
                        empty_layer: false,
                    });
                } else {
                    state.history.push(HistoryEntry {
                        created_by: format!("RUN {}", command),
                        empty_layer: true,
                    });
                }
            }

            Instruction::Workdir { path } => {
                if !config.quiet {
                    println!("Step {}/{}: WORKDIR {}", step, total, path);
                }
                state.workdir = resolve_path(&state.workdir, path);
                // Create the directory in rootfs
                let full = rootfs_dir.join(state.workdir.trim_start_matches('/'));
                let _ = std::fs::create_dir_all(&full);
                state.history.push(HistoryEntry {
                    created_by: format!("WORKDIR {}", path),
                    empty_layer: true,
                });
            }

            Instruction::Env { key, value } => {
                if !config.quiet {
                    println!("Step {}/{}: ENV {}={}", step, total, key, value);
                }
                // Replace existing or add new
                let expanded_value = expand_args(value, &state.build_args);
                if let Some(existing) = state.env.iter_mut().find(|(k, _)| k == key) {
                    existing.1 = expanded_value;
                } else {
                    state.env.push((key.clone(), expanded_value));
                }
                state.history.push(HistoryEntry {
                    created_by: format!("ENV {}={}", key, value),
                    empty_layer: true,
                });
            }

            Instruction::Entrypoint { exec } => {
                if !config.quiet {
                    println!("Step {}/{}: ENTRYPOINT {:?}", step, total, exec);
                }
                state.entrypoint = Some(exec.clone());
                state.history.push(HistoryEntry {
                    created_by: format!("ENTRYPOINT {:?}", exec),
                    empty_layer: true,
                });
            }

            Instruction::Cmd { exec } => {
                if !config.quiet {
                    println!("Step {}/{}: CMD {:?}", step, total, exec);
                }
                state.cmd = Some(exec.clone());
                state.history.push(HistoryEntry {
                    created_by: format!("CMD {:?}", exec),
                    empty_layer: true,
                });
            }

            Instruction::Expose { port } => {
                if !config.quiet {
                    println!("Step {}/{}: EXPOSE {}", step, total, port);
                }
                state.exposed_ports.push(port.clone());
                state.history.push(HistoryEntry {
                    created_by: format!("EXPOSE {}", port),
                    empty_layer: true,
                });
            }

            Instruction::Label { key, value } => {
                if !config.quiet {
                    println!("Step {}/{}: LABEL {}={}", step, total, key, value);
                }
                state.labels.insert(key.clone(), value.clone());
                state.history.push(HistoryEntry {
                    created_by: format!("LABEL {}={}", key, value),
                    empty_layer: true,
                });
            }

            Instruction::User { user } => {
                if !config.quiet {
                    println!("Step {}/{}: USER {}", step, total, user);
                }
                state.user = Some(user.clone());
                state.history.push(HistoryEntry {
                    created_by: format!("USER {}", user),
                    empty_layer: true,
                });
            }

            Instruction::Arg { name, default } => {
                if !config.quiet {
                    println!("Step {}/{}: ARG {}", step, total, name);
                }
                // Only set if not already provided via --build-arg
                if !state.build_args.contains_key(name) {
                    if let Some(val) = default {
                        state.build_args.insert(name.clone(), val.clone());
                    }
                }
                state.history.push(HistoryEntry {
                    created_by: format!("ARG {}", name),
                    empty_layer: true,
                });
            }
        }
    }

    // Assemble the final OCI image
    let reference = config
        .tag
        .clone()
        .unwrap_or_else(|| "a3s-build:latest".to_string());

    let result = assemble_image(
        &reference,
        &state,
        &base_layers,
        &base_diff_ids,
        &layers_dir,
        &store,
    )
    .await?;

    if !config.quiet {
        println!(
            "Successfully built {} ({} layers, {})",
            reference,
            result.layer_count,
            format_size(result.size)
        );
    }

    Ok(result)
}

// =============================================================================
// Instruction handlers
// =============================================================================

/// Handle FROM: pull base image and extract layers into rootfs.
///
/// Returns (base_layers, base_diff_ids, base_config).
async fn handle_from(
    image: &str,
    rootfs_dir: &Path,
    _layers_dir: &Path,
    store: &Arc<ImageStore>,
    build_args: &HashMap<String, String>,
) -> Result<(Vec<LayerInfo>, Vec<String>, OciImageConfig)> {
    let image_ref = expand_args(image, build_args);

    // Pull the base image
    let puller = ImagePuller::new(store.clone(), RegistryAuth::from_env());
    let oci_image = puller.pull(&image_ref).await?;

    // Extract all layers into rootfs
    for layer_path in oci_image.layer_paths() {
        extract_layer(layer_path, rootfs_dir)?;
    }

    // Collect base layer info
    let mut base_layers = Vec::new();
    let mut base_diff_ids = Vec::new();

    for layer_path in oci_image.layer_paths() {
        let digest = sha256_file(layer_path)?;
        let size = std::fs::metadata(layer_path).map(|m| m.len()).unwrap_or(0);

        // Compute diff_id (SHA256 of uncompressed content)
        let diff_id = compute_diff_id(layer_path)?;
        base_diff_ids.push(diff_id);

        base_layers.push(LayerInfo {
            path: layer_path.to_path_buf(),
            digest,
            size,
        });
    }

    let config = oci_image.config().clone();
    Ok((base_layers, base_diff_ids, config))
}

/// Handle COPY: copy files from build context into rootfs, create a layer.
fn handle_copy(
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
            BoxError::BuildError(format!(
                "Failed to create parent directory: {}",
                e
            ))
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
        Err(BoxError::BuildError(
            "Invalid COPY destination".to_string(),
        ))
    }
}

/// Handle RUN: execute a command in the rootfs.
///
/// On Linux, uses chroot. On macOS, skips with a warning.
/// Returns Some(LayerInfo) if a layer was created, None if skipped.
fn handle_run(
    command: &str,
    _rootfs_dir: &Path,
    _layers_dir: &Path,
    _workdir: &str,
    _env: &[(String, String)],
    _layer_index: usize,
    quiet: bool,
) -> Result<Option<LayerInfo>> {
    if cfg!(target_os = "macos") {
        if !quiet {
            println!(
                "  ⚠ RUN skipped on macOS (Linux rootfs cannot be executed on macOS host)"
            );
            println!("    Command: {}", command);
        }
        return Ok(None);
    }

    // Linux: execute via chroot
    #[cfg(target_os = "linux")]
    {
        use super::layer::DirSnapshot;

        let rootfs_dir = _rootfs_dir;
        let layers_dir = _layers_dir;
        let env = _env;
        let layer_index = _layer_index;

        let before = DirSnapshot::capture(rootfs_dir)?;

        // Build the command
        let mut cmd = std::process::Command::new("chroot");
        cmd.arg(rootfs_dir);
        cmd.arg("/bin/sh");
        cmd.arg("-c");
        cmd.arg(command);

        // Set environment
        cmd.env_clear();
        cmd.env("PATH", "/usr/local/sbin:/usr/local/bin:/usr/sbin:/usr/bin:/sbin:/bin");
        cmd.env("HOME", "/root");
        for (key, value) in env {
            cmd.env(key, value);
        }

        let output = cmd.output().map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to execute RUN command: {}",
                e
            ))
        })?;

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
        return Ok(Some(layer_info));
    }

    #[cfg(not(target_os = "linux"))]
    Ok(None)
}

// =============================================================================
// Image assembly
// =============================================================================

/// Assemble the final OCI image layout and store it.
async fn assemble_image(
    reference: &str,
    state: &BuildState,
    base_layers: &[LayerInfo],
    base_diff_ids: &[String],
    layers_dir: &Path,
    store: &Arc<ImageStore>,
) -> Result<BuildResult> {
    // Create output directory
    let output_dir = layers_dir.join("_output");
    let blobs_dir = output_dir.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs_dir).map_err(|e| {
        BoxError::BuildError(format!("Failed to create output blobs dir: {}", e))
    })?;

    // Collect all layers: base + new
    let mut all_layer_descriptors = Vec::new();
    let mut all_diff_ids: Vec<String> = base_diff_ids.to_vec();

    // Copy base layers to output
    for layer in base_layers {
        let blob_path = blobs_dir.join(&layer.digest);
        if !blob_path.exists() {
            std::fs::copy(&layer.path, &blob_path).map_err(|e| {
                BoxError::BuildError(format!("Failed to copy base layer: {}", e))
            })?;
        }
        all_layer_descriptors.push(serde_json::json!({
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": layer.prefixed_digest(),
            "size": layer.size
        }));
    }

    // Copy new layers to output
    for (i, layer) in state.layers.iter().enumerate() {
        let blob_path = blobs_dir.join(&layer.digest);
        if !blob_path.exists() {
            std::fs::copy(&layer.path, &blob_path).map_err(|e| {
                BoxError::BuildError(format!("Failed to copy layer {}: {}", i, e))
            })?;
        }
        all_layer_descriptors.push(serde_json::json!({
            "mediaType": "application/vnd.oci.image.layer.v1.tar+gzip",
            "digest": layer.prefixed_digest(),
            "size": layer.size
        }));
    }

    // Merge diff_ids
    all_diff_ids.extend(state.diff_ids.iter().cloned());

    // Build OCI config
    let now = chrono::Utc::now().to_rfc3339();
    let arch = match std::env::consts::ARCH {
        "x86_64" => "amd64",
        "aarch64" => "arm64",
        other => other,
    };

    let env_list: Vec<String> = state
        .env
        .iter()
        .map(|(k, v)| format!("{}={}", k, v))
        .collect();

    let mut config_obj = serde_json::json!({
        "architecture": arch,
        "os": "linux",
        "created": now,
        "config": {},
        "rootfs": {
            "type": "layers",
            "diff_ids": all_diff_ids.iter()
                .map(|d| format!("sha256:{}", d))
                .collect::<Vec<_>>()
        },
        "history": state.history.iter().map(|h| {
            let mut entry = serde_json::json!({
                "created": now,
                "created_by": h.created_by
            });
            if h.empty_layer {
                entry["empty_layer"] = serde_json::json!(true);
            }
            entry
        }).collect::<Vec<_>>()
    });

    // Populate config section
    let config_section = config_obj["config"].as_object_mut().unwrap();
    if !env_list.is_empty() {
        config_section.insert(
            "Env".to_string(),
            serde_json::json!(env_list),
        );
    }
    if let Some(ref ep) = state.entrypoint {
        config_section.insert("Entrypoint".to_string(), serde_json::json!(ep));
    }
    if let Some(ref cmd) = state.cmd {
        config_section.insert("Cmd".to_string(), serde_json::json!(cmd));
    }
    if state.workdir != "/" {
        config_section.insert(
            "WorkingDir".to_string(),
            serde_json::json!(state.workdir),
        );
    }
    if let Some(ref user) = state.user {
        config_section.insert("User".to_string(), serde_json::json!(user));
    }
    if !state.exposed_ports.is_empty() {
        let ports: HashMap<String, serde_json::Value> = state
            .exposed_ports
            .iter()
            .map(|p| (p.clone(), serde_json::json!({})))
            .collect();
        config_section.insert("ExposedPorts".to_string(), serde_json::json!(ports));
    }
    if !state.labels.is_empty() {
        config_section.insert("Labels".to_string(), serde_json::json!(state.labels));
    }

    // Write config blob
    let config_bytes = serde_json::to_vec_pretty(&config_obj)?;
    let config_digest = sha256_bytes(&config_bytes);
    std::fs::write(blobs_dir.join(&config_digest), &config_bytes).map_err(|e| {
        BoxError::BuildError(format!("Failed to write config blob: {}", e))
    })?;

    // Build manifest
    let manifest = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.manifest.v1+json",
        "config": {
            "mediaType": "application/vnd.oci.image.config.v1+json",
            "digest": format!("sha256:{}", config_digest),
            "size": config_bytes.len()
        },
        "layers": all_layer_descriptors
    });

    let manifest_bytes = serde_json::to_vec_pretty(&manifest)?;
    let manifest_digest = sha256_bytes(&manifest_bytes);
    std::fs::write(blobs_dir.join(&manifest_digest), &manifest_bytes).map_err(|e| {
        BoxError::BuildError(format!("Failed to write manifest blob: {}", e))
    })?;

    // Write index.json
    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{}", manifest_digest),
            "size": manifest_bytes.len()
        }]
    });
    std::fs::write(
        output_dir.join("index.json"),
        serde_json::to_string_pretty(&index)?,
    )
    .map_err(|e| {
        BoxError::BuildError(format!("Failed to write index.json: {}", e))
    })?;

    // Write oci-layout
    std::fs::write(
        output_dir.join("oci-layout"),
        r#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .map_err(|e| {
        BoxError::BuildError(format!("Failed to write oci-layout: {}", e))
    })?;

    // Store in image store
    let digest_str = format!("sha256:{}", manifest_digest);
    let stored = store.put(reference, &digest_str, &output_dir).await?;

    let total_layers = base_layers.len() + state.layers.len();

    Ok(BuildResult {
        reference: reference.to_string(),
        digest: digest_str,
        size: stored.size_bytes,
        layer_count: total_layers,
    })
}

// =============================================================================
// Helpers
// =============================================================================

/// Apply base image config to build state.
fn apply_base_config(state: &mut BuildState, config: &OciImageConfig) {
    state.env = config.env.clone();
    state.entrypoint = config.entrypoint.clone();
    state.cmd = config.cmd.clone();
    state.user = config.user.clone();
    state.exposed_ports = config.exposed_ports.clone();
    state.labels = config.labels.clone();
    if let Some(ref wd) = config.working_dir {
        state.workdir = wd.clone();
    }
}

/// Resolve a path relative to a working directory.
///
/// If `path` is absolute, return it as-is. Otherwise, join with `workdir`.
fn resolve_path(workdir: &str, path: &str) -> String {
    if path.starts_with('/') {
        path.to_string()
    } else {
        format!(
            "{}/{}",
            workdir.trim_end_matches('/'),
            path
        )
    }
}

/// Expand `${VAR}` and `$VAR` references in a string using build args.
fn expand_args(s: &str, args: &HashMap<String, String>) -> String {
    let mut result = s.to_string();
    for (key, value) in args {
        result = result.replace(&format!("${{{}}}", key), value);
        result = result.replace(&format!("${}", key), value);
    }
    result
}

/// Compute the diff_id (SHA256 of uncompressed layer content).
fn compute_diff_id(layer_path: &Path) -> Result<String> {
    let data = std::fs::read(layer_path).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to read layer for diff_id: {}",
            e
        ))
    })?;

    // Decompress gzip to get raw tar
    use flate2::read::GzDecoder;
    use std::io::Read;

    let decoder = GzDecoder::new(&data[..]);
    let mut uncompressed = Vec::new();
    std::io::BufReader::new(decoder)
        .read_to_end(&mut uncompressed)
        .map_err(|e| {
            BoxError::BuildError(format!(
                "Failed to decompress layer for diff_id: {}",
                e
            ))
        })?;

    Ok(sha256_bytes(&uncompressed))
}

/// Recursively copy a directory.
fn copy_dir_recursive(src: &Path, dst: &Path) -> Result<()> {
    std::fs::create_dir_all(dst).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to create directory {}: {}",
            dst.display(),
            e
        ))
    })?;

    for entry in std::fs::read_dir(src).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to read directory {}: {}",
            src.display(),
            e
        ))
    })? {
        let entry = entry.map_err(|e| {
            BoxError::BuildError(format!("Failed to read entry: {}", e))
        })?;
        let src_path = entry.path();
        let dst_path = dst.join(entry.file_name());

        if src_path.is_dir() {
            copy_dir_recursive(&src_path, &dst_path)?;
        } else {
            std::fs::copy(&src_path, &dst_path).map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to copy {} to {}: {}",
                    src_path.display(),
                    dst_path.display(),
                    e
                ))
            })?;
        }
    }
    Ok(())
}

/// Format a byte size as a human-readable string.
fn format_size(bytes: u64) -> String {
    if bytes >= 1024 * 1024 * 1024 {
        format!("{:.1} GB", bytes as f64 / (1024.0 * 1024.0 * 1024.0))
    } else if bytes >= 1024 * 1024 {
        format!("{:.1} MB", bytes as f64 / (1024.0 * 1024.0))
    } else if bytes >= 1024 {
        format!("{:.1} KB", bytes as f64 / 1024.0)
    } else {
        format!("{} B", bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_resolve_path_absolute() {
        assert_eq!(resolve_path("/app", "/usr/bin"), "/usr/bin");
    }

    #[test]
    fn test_resolve_path_relative() {
        assert_eq!(resolve_path("/app", "src"), "/app/src");
    }

    #[test]
    fn test_resolve_path_root_workdir() {
        assert_eq!(resolve_path("/", "app"), "/app");
    }

    #[test]
    fn test_expand_args_braces() {
        let mut args = HashMap::new();
        args.insert("VERSION".to_string(), "3.19".to_string());
        assert_eq!(expand_args("alpine:${VERSION}", &args), "alpine:3.19");
    }

    #[test]
    fn test_expand_args_dollar() {
        let mut args = HashMap::new();
        args.insert("TAG".to_string(), "latest".to_string());
        assert_eq!(expand_args("image:$TAG", &args), "image:latest");
    }

    #[test]
    fn test_expand_args_no_match() {
        let args = HashMap::new();
        assert_eq!(expand_args("alpine:3.19", &args), "alpine:3.19");
    }

    #[test]
    fn test_format_size() {
        assert_eq!(format_size(500), "500 B");
        assert_eq!(format_size(1536), "1.5 KB");
        assert_eq!(format_size(1_500_000), "1.4 MB");
        assert_eq!(format_size(1_500_000_000), "1.4 GB");
    }
}
