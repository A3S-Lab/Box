//! Build engine for constructing OCI images from Dockerfiles.
//!
//! Orchestrates the build process: parses the Dockerfile, pulls the base image,
//! executes each instruction, creates layers, and assembles the final OCI image.

use std::collections::HashMap;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use a3s_box_core::error::{BoxError, Result};
use a3s_box_core::platform::Platform;

use super::cache::{hash_context_sources, BuildCache};
use super::dockerfile::{Dockerfile, Instruction, RunBindMount, RunCacheMount};
use super::dockerignore::DockerIgnore;
use super::layer::{sha256_bytes, sha256_file, LayerInfo};
use crate::oci::image::OciImageConfig;
use crate::oci::layers::extract_layer;
use crate::oci::store::ImageStore;
use crate::oci::{ImagePuller, RegistryAuth};

mod handlers;
mod stages;
mod utils;

#[cfg(test)]
mod tests;

use handlers::{
    apply_base_config, execute_onbuild_trigger, handle_add, handle_copy, handle_run,
    handle_run_with_pool, instruction_to_string,
};
use stages::{global_arg_decls, resolve_stage_rootfs, split_into_stages};
use utils::{compute_diff_id, expand_args, format_size, resolve_path};

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
    /// Target platforms for multi-platform builds.
    /// Empty means build for the host platform only.
    pub platforms: Vec<Platform>,
    /// Build only up to this stage (`--target`), by alias or numeric index.
    /// `None` builds the final stage.
    pub target: Option<String>,
    /// Disable the layer build cache (`--no-cache`): every layer is rebuilt.
    pub no_cache: bool,
    /// Prometheus metrics (optional).
    pub metrics: Option<crate::prom::RuntimeMetrics>,
    /// Execute Dockerfile RUN instructions through a warm-pool daemon lease.
    pub run_pool: Option<BuildRunPoolConfig>,
}

/// Configuration for executing Dockerfile RUN instructions in a warm-pool VM.
#[derive(Debug, Clone)]
pub struct BuildRunPoolConfig {
    /// Pool daemon Unix socket.
    pub socket: String,
    /// Helper VM image. `None` uses the daemon's default image.
    pub image: Option<String>,
    /// Helper VM vCPU count for lazily-created pools.
    pub vcpus: u32,
    /// Helper VM memory in MiB for lazily-created pools.
    pub memory_mb: u32,
    /// Guest path where the stage rootfs is mounted.
    pub guest_rootfs: String,
    /// RUN exec timeout in nanoseconds.
    pub timeout_ns: u64,
    /// Persistent cache directory for `RUN --mount=type=cache`.
    pub run_cache_dir: PathBuf,
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

#[cfg_attr(not(feature = "pool"), allow(dead_code))]
struct BuildRunPoolSession {
    guest_rootfs: String,
    timeout_ns: u64,
    run_cache_dir: PathBuf,
    #[cfg(feature = "pool")]
    lease: crate::pool::PoolLeaseClient,
}

impl BuildRunPoolSession {
    async fn acquire(config: &BuildRunPoolConfig, rootfs_dir: &Path) -> Result<Self> {
        #[cfg(feature = "pool")]
        {
            let rootfs_dir = rootfs_dir.canonicalize().map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to canonicalize build RUN rootfs {}: {}",
                    rootfs_dir.display(),
                    e
                ))
            })?;
            let volume = format!("{}:{}:rw", rootfs_dir.display(), config.guest_rootfs);
            let lease = crate::pool::PoolLeaseClient::acquire(crate::pool::PoolClientLease {
                socket: config.socket.clone(),
                image: config.image.clone(),
                volumes: vec![volume],
                vcpus: config.vcpus,
                memory_mb: config.memory_mb,
            })
            .await
            .map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to lease warm-pool VM for Dockerfile RUN: {}",
                    e
                ))
            })?;
            Ok(Self {
                guest_rootfs: config.guest_rootfs.clone(),
                timeout_ns: config.timeout_ns,
                run_cache_dir: config.run_cache_dir.clone(),
                lease,
            })
        }

        #[cfg(not(feature = "pool"))]
        {
            let _ = (config, rootfs_dir);
            Err(BoxError::BuildError(
                "Dockerfile RUN warm-pool execution requires the runtime 'pool' feature"
                    .to_string(),
            ))
        }
    }

    async fn release(self) -> Result<()> {
        #[cfg(feature = "pool")]
        {
            self.lease.release().await.map_err(|e| {
                BoxError::BuildError(format!(
                    "Failed to release warm-pool Dockerfile RUN lease: {}",
                    e
                ))
            })
        }

        #[cfg(not(feature = "pool"))]
        {
            Ok(())
        }
    }
}

fn run_bind_mount_input_hash(
    context_dir: &Path,
    completed_stages: &[(Option<String>, PathBuf)],
    bind_mounts: &[RunBindMount],
) -> Option<String> {
    let mut input = String::new();

    for mount in bind_mounts {
        if has_parent_component(&mount.source) {
            return None;
        }

        let source = if mount.source.is_empty() {
            "."
        } else {
            mount.source.as_str()
        };
        let (origin, source_root) = match mount.from.as_deref() {
            Some(from_ref) => (
                format!("stage:{from_ref}"),
                resolve_stage_rootfs(from_ref, completed_stages).ok()?,
            ),
            None => ("context".to_string(), context_dir),
        };

        let source_hash = hash_context_sources(source_root, &[source.to_string()])?;
        input.push_str(&origin);
        input.push('\0');
        input.push_str(source);
        input.push('\0');
        input.push_str(&source_hash);
        input.push('\0');

        if mount.from.is_none() {
            let dockerignore = context_dir.join(".dockerignore");
            if let Ok(bytes) = std::fs::read(&dockerignore) {
                input.push_str(".dockerignore");
                input.push('\0');
                input.push_str(&sha256_bytes(&bytes));
                input.push('\0');
            }
        }
    }

    Some(sha256_bytes(input.as_bytes()))
}

fn run_cache_mount_input_hash(
    completed_stages: &[(Option<String>, PathBuf)],
    cache_mounts: &[RunCacheMount],
) -> Option<String> {
    let mut input = String::new();
    let mut saw_seeded_cache = false;

    for mount in cache_mounts {
        let Some(from_ref) = mount.from.as_deref() else {
            continue;
        };
        if has_parent_component(&mount.source) {
            return None;
        }

        saw_seeded_cache = true;
        let source = if mount.source.is_empty() {
            "."
        } else {
            mount.source.as_str()
        };
        let source_root = resolve_stage_rootfs(from_ref, completed_stages).ok()?;
        let source_hash = hash_context_sources(source_root, &[source.to_string()])?;
        input.push_str("cache-seed:");
        input.push_str(from_ref);
        input.push('\0');
        input.push_str(source);
        input.push('\0');
        input.push_str(&source_hash);
        input.push('\0');
    }

    saw_seeded_cache.then(|| sha256_bytes(input.as_bytes()))
}

fn run_mount_input_hash(
    context_dir: &Path,
    completed_stages: &[(Option<String>, PathBuf)],
    cache_mounts: &[RunCacheMount],
    bind_mounts: &[RunBindMount],
) -> Option<String> {
    let bind_hash = if bind_mounts.is_empty() {
        None
    } else {
        run_bind_mount_input_hash(context_dir, completed_stages, bind_mounts)
    };
    let cache_hash = run_cache_mount_input_hash(completed_stages, cache_mounts);

    match (bind_hash, cache_hash) {
        (None, None) => None,
        (Some(hash), None) | (None, Some(hash)) => Some(hash),
        (Some(bind_hash), Some(cache_hash)) => Some(sha256_bytes(
            format!("bind\0{bind_hash}\0cache\0{cache_hash}").as_bytes(),
        )),
    }
}

fn has_parent_component(path: &str) -> bool {
    Path::new(path)
        .components()
        .any(|component| matches!(component, std::path::Component::ParentDir))
}

async fn resolve_run_mount_source_roots(
    completed_stages: &[(Option<String>, PathBuf)],
    bind_mounts: &[RunBindMount],
    cache_mounts: &[RunCacheMount],
    store: &Arc<ImageStore>,
    build_dir: &Path,
    external_from_rootfs: &mut HashMap<String, PathBuf>,
) -> Result<Option<Vec<(Option<String>, PathBuf)>>> {
    let mut roots: Option<Vec<(Option<String>, PathBuf)>> = None;
    let mut external_refs = HashSet::new();

    let mut from_refs: Vec<&str> = Vec::new();
    from_refs.extend(bind_mounts.iter().filter_map(|mount| mount.from.as_deref()));
    from_refs.extend(
        cache_mounts
            .iter()
            .filter_map(|mount| mount.from.as_deref()),
    );

    for from_ref in from_refs {
        if resolve_stage_rootfs(from_ref, completed_stages).is_ok()
            || roots
                .as_deref()
                .is_some_and(|resolved| resolve_stage_rootfs(from_ref, resolved).is_ok())
        {
            continue;
        }

        if !external_refs.insert(from_ref.to_string()) {
            continue;
        }

        let rootfs = resolve_external_from_rootfs(
            from_ref,
            "RUN bind mount",
            store,
            build_dir,
            external_from_rootfs,
        )
        .await?;
        roots
            .get_or_insert_with(|| completed_stages.to_vec())
            .push((Some(from_ref.to_string()), rootfs));
    }

    Ok(roots)
}

/// Mutable state accumulated during the build.
pub(super) struct BuildState {
    /// Working directory inside the image
    pub(super) workdir: String,
    /// Environment variables
    pub(super) env: Vec<(String, String)>,
    /// Entrypoint
    pub(super) entrypoint: Option<Vec<String>>,
    /// Default command
    pub(super) cmd: Option<Vec<String>>,
    /// User
    pub(super) user: Option<String>,
    /// Exposed ports
    pub(super) exposed_ports: Vec<String>,
    /// Labels
    pub(super) labels: HashMap<String, String>,
    /// Layer info accumulated during build
    pub(super) layers: Vec<LayerInfo>,
    /// Diff IDs (uncompressed layer digests) for the OCI config
    pub(super) diff_ids: Vec<String>,
    /// History entries
    pub(super) history: Vec<HistoryEntry>,
    /// Build arguments (all `--build-arg` values plus ARG defaults). A value is
    /// only usable in variable expansion if its name is also in `declared_args`.
    pub(super) build_args: HashMap<String, String>,
    /// Names declared via an `ARG` instruction in scope for this stage (plus any
    /// global pre-FROM ARGs). Docker only substitutes `$NAME` for declared names;
    /// an undeclared `--build-arg` is ignored for expansion.
    pub(super) declared_args: HashSet<String>,
    /// Shell override (default: ["/bin/sh", "-c"])
    pub(super) shell: Vec<String>,
    /// Stop signal
    pub(super) stop_signal: Option<String>,
    /// Health check configuration
    pub(super) health_check: Option<OciHealthCheck>,
    /// ONBUILD triggers to store in the image config
    pub(super) onbuild: Vec<String>,
    /// Volumes declared via VOLUME instruction
    pub(super) volumes: Vec<String>,
}

/// A single history entry for the OCI config.
#[derive(Debug, Clone)]
pub(super) struct HistoryEntry {
    pub(super) created_by: String,
    pub(super) empty_layer: bool,
}

pub use crate::oci::image::OciHealthCheck;

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
            declared_args: HashSet::new(),
            shell: vec!["/bin/sh".to_string(), "-c".to_string()],
            stop_signal: None,
            health_check: None,
            onbuild: Vec::new(),
            volumes: Vec::new(),
        }
    }

    /// Build args whose names were declared via `ARG` (gates `$NAME` expansion,
    /// so an undeclared `--build-arg` is not substituted — matching Docker).
    fn declared_build_args(&self) -> HashMap<String, String> {
        self.build_args
            .iter()
            .filter(|(name, _)| self.declared_args.contains(*name))
            .map(|(name, value)| (name.clone(), value.clone()))
            .collect()
    }

    /// Variables in scope for `$NAME`/`${NAME}` expansion in ENV/WORKDIR/FROM:
    /// declared ARG values overlaid with already-set ENV (ENV wins), matching
    /// Docker. An undeclared/unset name is left untouched by `expand_args`.
    fn expansion_vars(&self) -> HashMap<String, String> {
        let mut vars = self.declared_build_args();
        for (key, value) in &self.env {
            vars.insert(key.clone(), value.clone());
        }
        vars
    }

    /// Environment for RUN: declared ARG values are available while executing
    /// the command, and ENV values override ARGs with the same name. ARGs are
    /// not persisted into the final image config unless an ENV stores them.
    fn run_env(&self) -> Vec<(String, String)> {
        let mut vars = self.declared_build_args();
        for (key, value) in &self.env {
            vars.insert(key.clone(), value.clone());
        }
        let mut pairs = vars.into_iter().collect::<Vec<_>>();
        pairs.sort_by(|a, b| a.0.cmp(&b.0));
        pairs
    }

    /// Seed a global (pre-FROM) ARG into this stage: declare its name and apply
    /// its default unless a `--build-arg` already overrides it.
    fn seed_global_arg(&mut self, name: &str, default: Option<&str>) {
        self.declared_args.insert(name.to_string());
        if !self.build_args.contains_key(name) {
            if let Some(val) = default {
                self.build_args.insert(name.to_string(), val.to_string());
            }
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
///
/// Supports multi-stage builds: each FROM starts a new stage. Only the final
/// stage produces the output image. `COPY --from=<stage>` copies from a
/// previous stage's rootfs.
pub async fn build(config: BuildConfig, store: Arc<ImageStore>) -> Result<BuildResult> {
    validate_build_config(&config)?;

    // Parse Dockerfile
    let dockerfile = Dockerfile::from_file(&config.dockerfile_path)?;

    // Load the context's .dockerignore once; applied to every context COPY/ADD.
    let dockerignore = DockerIgnore::load(&config.context_dir);

    if !config.quiet {
        println!("Building from {}", config.dockerfile_path.display());
        if !dockerignore.is_empty() {
            println!("Using .dockerignore");
        }
    }

    // Split instructions into stages by FROM
    let stages = split_into_stages(&dockerfile.instructions);
    // Global (pre-FROM) ARG declarations: in scope for every stage. Stage 0 also
    // processes them inline (they are prepended to it), so they are only seeded
    // into later stages to avoid double-counting.
    let global_args = global_arg_decls(&dockerfile.instructions);
    let total_stages = stages.len();

    // Resolve --target to the stage that produces the output image (by alias or
    // numeric index). Without --target the final stage is the output. Stages
    // after the target are never executed.
    let output_stage_idx = match config.target.as_deref() {
        Some(target) => stages
            .iter()
            .position(|s| s.alias.as_deref() == Some(target))
            .or_else(|| target.parse::<usize>().ok().filter(|i| *i < total_stages))
            .ok_or_else(|| {
                BoxError::BuildError(format!("target build stage '{}' not found", target))
            })?,
        None => total_stages - 1,
    };

    // Track completed stages: (alias, rootfs_path)
    let mut completed_stages: Vec<(Option<String>, PathBuf)> = Vec::new();
    // Cache external images already pulled+extracted for `COPY --from=<image>`
    // and RUN mount `from=<image>` sources so repeated references pull once.
    let mut external_from_rootfs: HashMap<String, PathBuf> = HashMap::new();

    // Create temp directory for build workspace
    let build_dir = tempfile::TempDir::new()
        .map_err(|e| BoxError::BuildError(format!("Failed to create build directory: {}", e)))?;

    let mut final_state = BuildState::new(config.build_args.clone());
    let mut final_base_layers: Vec<LayerInfo> = Vec::new();
    let mut final_base_diff_ids: Vec<String> = Vec::new();

    let total_instructions = dockerfile.instructions.len();
    let mut global_step = 0;

    for (stage_idx, stage) in stages.iter().enumerate() {
        let is_final_stage = stage_idx == output_stage_idx;

        let rootfs_dir = build_dir.path().join(format!("rootfs_{}", stage_idx));
        let layers_dir = build_dir.path().join(format!("layers_{}", stage_idx));
        std::fs::create_dir_all(&rootfs_dir).map_err(|e| {
            BoxError::BuildError(format!("Failed to create rootfs directory: {}", e))
        })?;
        std::fs::create_dir_all(&layers_dir).map_err(|e| {
            BoxError::BuildError(format!("Failed to create layers directory: {}", e))
        })?;

        let mut state = BuildState::new(config.build_args.clone());
        // Seed later stages with the global pre-FROM ARGs (stage 0 gets them
        // inline). Without this, a later `FROM image:$GLOBAL_ARG` would not
        // resolve and the global ARG would be unavailable to the stage body.
        if stage_idx > 0 {
            for (name, default) in &global_args {
                state.seed_global_arg(name, default.as_deref());
            }
        }
        let mut base_layers: Vec<LayerInfo> = Vec::new();
        let mut base_diff_ids: Vec<String> = Vec::new();
        let mut run_pool_session: Option<BuildRunPoolSession> = None;

        // Layer-level build cache (best-effort; None disables caching).
        let cache = if config.no_cache {
            None
        } else {
            BuildCache::open()
        };
        // Running chain key over all instructions in this stage. Reset at FROM.
        let mut chain_key = String::new();
        // Once a cache miss forces re-execution, all later layers must be rebuilt.
        let mut cache_valid = true;

        for instruction in &stage.instructions {
            global_step += 1;
            let step = global_step;
            let run_mount_source_roots = if let Instruction::Run {
                bind_mounts,
                cache_mounts,
                ..
            } = instruction
            {
                resolve_run_mount_source_roots(
                    &completed_stages,
                    bind_mounts,
                    cache_mounts,
                    &store,
                    build_dir.path(),
                    &mut external_from_rootfs,
                )
                .await?
            } else {
                None
            };

            // Advance the chain key BEFORE the match so a cache-hit `continue`
            // does not skip it. FROM resets the key (keyed on base content below);
            // every other instruction extends it, including config-only ones
            // (ENV/WORKDIR/...) since they affect later RUNs.
            if !matches!(instruction, Instruction::From { .. }) {
                // Use build-arg-expanded text in the cache key for instructions
                // whose effect depends on ARG/--build-arg values, so a different
                // build arg correctly invalidates downstream layers. (RUN/COPY
                // paths are not arg-expanded by this engine, so their raw repr is
                // faithful; build-arg-driven behavior reaches RUN only via ENV.)
                let repr = match instruction {
                    Instruction::Env { vars } => {
                        let pairs: Vec<String> = vars
                            .iter()
                            .map(|(k, v)| {
                                format!("{}={}", k, expand_args(v, &state.expansion_vars()))
                            })
                            .collect();
                        format!("ENV {}", pairs.join(" "))
                    }
                    Instruction::Arg { name, default } => {
                        let effective = state
                            .build_args
                            .get(name)
                            .cloned()
                            .or_else(|| default.clone())
                            .unwrap_or_default();
                        format!("ARG {}={}", name, effective)
                    }
                    other => instruction_to_string(other),
                };
                let input_hash = match instruction {
                    Instruction::Copy {
                        src, from: None, ..
                    } => hash_context_sources(&config.context_dir, src),
                    Instruction::Copy {
                        src,
                        from: Some(from_ref),
                        ..
                    } => {
                        // COPY --from=<stage>: key on the ACTUAL source files'
                        // content in the (already-built) source stage's rootfs.
                        // Without this the output stage's chain key never depends
                        // on what the source stage produced, so a changed builder
                        // binary is served STALE from the on-disk build cache.
                        // External-image sources resolve to Err here and fall to
                        // None (the image ref is already in `repr`).
                        resolve_stage_rootfs(from_ref, &completed_stages)
                            .ok()
                            .and_then(|rootfs| hash_context_sources(rootfs, src))
                    }
                    Instruction::Add { src, .. } => hash_context_sources(&config.context_dir, src),
                    Instruction::Run {
                        cache_mounts,
                        bind_mounts,
                        ..
                    } => run_mount_input_hash(
                        &config.context_dir,
                        run_mount_source_roots
                            .as_deref()
                            .unwrap_or(&completed_stages),
                        cache_mounts,
                        bind_mounts,
                    ),
                    _ => None,
                };
                chain_key = BuildCache::chain(&chain_key, &repr, input_hash.as_deref());
            }

            match instruction {
                Instruction::From { image, alias } => {
                    if !config.quiet {
                        if total_stages > 1 {
                            println!(
                                "Step {}/{}: FROM {} (stage {}/{}{})",
                                step,
                                total_instructions,
                                image,
                                stage_idx + 1,
                                total_stages,
                                alias
                                    .as_ref()
                                    .map(|a| format!(" as {}", a))
                                    .unwrap_or_default()
                            );
                        } else {
                            println!("Step {}/{}: FROM {}", step, total_instructions, image);
                        }
                    }
                    let (layers, diff_ids, base_config) = handle_from(
                        image,
                        &rootfs_dir,
                        &layers_dir,
                        &store,
                        &state.declared_build_args(),
                    )
                    .await?;
                    base_layers = layers;
                    base_diff_ids = diff_ids;

                    // Key the cache chain on the actual base image content so a
                    // different base invalidates everything that follows. FROM
                    // itself is never cached.
                    chain_key = sha256_bytes(base_diff_ids.join(",").as_bytes());
                    cache_valid = true;

                    // Inherit config from base image
                    apply_base_config(&mut state, &base_config);

                    // Execute ONBUILD triggers from base image
                    if !base_config.onbuild.is_empty() && !config.quiet {
                        println!(
                            "  Executing {} ONBUILD trigger(s) from base image",
                            base_config.onbuild.len()
                        );
                    }
                    for trigger in &base_config.onbuild {
                        execute_onbuild_trigger(
                            trigger,
                            &mut state,
                            &config,
                            &rootfs_dir,
                            &layers_dir,
                            &base_layers,
                            &completed_stages,
                        )?;
                    }

                    state.history.push(HistoryEntry {
                        created_by: format!("FROM {}", image),
                        empty_layer: true,
                    });
                }

                Instruction::Copy {
                    src,
                    dst,
                    from,
                    chown,
                } => {
                    let created_by = if let Some(from_ref) = from {
                        format!("COPY --from={} {} {}", from_ref, src.join(" "), dst)
                    } else if let Some(owner) = chown {
                        format!("COPY --chown={} {} {}", owner, src.join(" "), dst)
                    } else {
                        format!("COPY {} {}", src.join(" "), dst)
                    };
                    if try_reuse_cached_layer(
                        CachedLayerReuse {
                            cache_valid,
                            cache: cache.as_ref(),
                            chain_key: &chain_key,
                            rootfs_dir: &rootfs_dir,
                            layers_dir: &layers_dir,
                            layer_index: state.layers.len() + base_layers.len(),
                            created_by: &created_by,
                        },
                        &mut state,
                    )?
                    .is_some()
                    {
                        if !config.quiet {
                            println!(
                                "Step {}/{}: {} (CACHED)",
                                step, total_instructions, created_by
                            );
                        }
                        continue;
                    }
                    cache_valid = false;

                    if let Some(from_ref) = from {
                        if !config.quiet {
                            println!(
                                "Step {}/{}: COPY --from={} {} {}",
                                step,
                                total_instructions,
                                from_ref,
                                src.join(" "),
                                dst
                            );
                        }
                        // `--from` is a prior stage (by alias or index) or, like
                        // Docker, an external image reference to pull and copy
                        // from.
                        let from_rootfs: PathBuf =
                            match resolve_stage_rootfs(from_ref, &completed_stages) {
                                Ok(stage_rootfs) => stage_rootfs.to_path_buf(),
                                Err(_) => {
                                    resolve_external_from_rootfs(
                                        from_ref,
                                        "COPY --from",
                                        &store,
                                        build_dir.path(),
                                        &mut external_from_rootfs,
                                    )
                                    .await?
                                }
                            };
                        // .dockerignore applies to the build context, not to a
                        // source stage's rootfs.
                        let layer_info = handle_copy(
                            src,
                            dst,
                            chown.as_deref(),
                            &from_rootfs,
                            &rootfs_dir,
                            &layers_dir,
                            &state.workdir,
                            state.layers.len() + base_layers.len(),
                            None,
                        )?;
                        let diff_id = compute_diff_id(&layer_info.path)?;
                        if let Some(c) = &cache {
                            c.store(&chain_key, &layer_info, &diff_id);
                        }
                        state.diff_ids.push(diff_id);
                        state.layers.push(layer_info);
                        state.history.push(HistoryEntry {
                            created_by: format!(
                                "COPY --from={} {} {}",
                                from_ref,
                                src.join(" "),
                                dst
                            ),
                            empty_layer: false,
                        });
                    } else {
                        if !config.quiet {
                            println!(
                                "Step {}/{}: COPY {} {}",
                                step,
                                total_instructions,
                                src.join(" "),
                                dst
                            );
                        }
                        let layer_info = handle_copy(
                            src,
                            dst,
                            chown.as_deref(),
                            &config.context_dir,
                            &rootfs_dir,
                            &layers_dir,
                            &state.workdir,
                            state.layers.len() + base_layers.len(),
                            Some(&dockerignore),
                        )?;
                        let diff_id = compute_diff_id(&layer_info.path)?;
                        if let Some(c) = &cache {
                            c.store(&chain_key, &layer_info, &diff_id);
                        }
                        state.diff_ids.push(diff_id);
                        state.layers.push(layer_info);
                        state.history.push(HistoryEntry {
                            created_by: format!("COPY {} {}", src.join(" "), dst),
                            empty_layer: false,
                        });
                    }
                }

                Instruction::Add { src, dst, chown } => {
                    let created_by = format!("ADD {} {}", src.join(" "), dst);
                    if try_reuse_cached_layer(
                        CachedLayerReuse {
                            cache_valid,
                            cache: cache.as_ref(),
                            chain_key: &chain_key,
                            rootfs_dir: &rootfs_dir,
                            layers_dir: &layers_dir,
                            layer_index: state.layers.len() + base_layers.len(),
                            created_by: &created_by,
                        },
                        &mut state,
                    )?
                    .is_some()
                    {
                        if !config.quiet {
                            println!(
                                "Step {}/{}: {} (CACHED)",
                                step, total_instructions, created_by
                            );
                        }
                        continue;
                    }
                    cache_valid = false;

                    if !config.quiet {
                        println!(
                            "Step {}/{}: ADD {} {}",
                            step,
                            total_instructions,
                            src.join(" "),
                            dst
                        );
                    }
                    let layer_info = handle_add(
                        src,
                        dst,
                        chown.as_deref(),
                        &config.context_dir,
                        &rootfs_dir,
                        &layers_dir,
                        &state.workdir,
                        state.layers.len() + base_layers.len(),
                        Some(&dockerignore),
                    )?;
                    let diff_id = compute_diff_id(&layer_info.path)?;
                    if let Some(c) = &cache {
                        c.store(&chain_key, &layer_info, &diff_id);
                    }
                    state.diff_ids.push(diff_id);
                    state.layers.push(layer_info);
                    state.history.push(HistoryEntry {
                        created_by: format!("ADD {} {}", src.join(" "), dst),
                        empty_layer: false,
                    });
                }

                Instruction::Run {
                    command,
                    cache_mounts,
                    bind_mounts,
                    tmpfs_mounts,
                } => {
                    let created_by = instruction_to_string(instruction);
                    if try_reuse_cached_layer(
                        CachedLayerReuse {
                            cache_valid,
                            cache: cache.as_ref(),
                            chain_key: &chain_key,
                            rootfs_dir: &rootfs_dir,
                            layers_dir: &layers_dir,
                            layer_index: state.layers.len() + base_layers.len(),
                            created_by: &created_by,
                        },
                        &mut state,
                    )?
                    .is_some()
                    {
                        if !config.quiet {
                            println!(
                                "Step {}/{}: {} (CACHED)",
                                step, total_instructions, created_by
                            );
                        }
                        continue;
                    }
                    cache_valid = false;

                    if !config.quiet {
                        println!("Step {}/{}: {}", step, total_instructions, created_by);
                    }
                    let layer_opt = if let Some(pool_config) = &config.run_pool {
                        if run_pool_session.is_none() {
                            run_pool_session =
                                Some(BuildRunPoolSession::acquire(pool_config, &rootfs_dir).await?);
                        }
                        let session = run_pool_session
                            .as_ref()
                            .expect("run pool session was just initialized");
                        handle_run_with_pool(
                            command,
                            cache_mounts,
                            bind_mounts,
                            tmpfs_mounts,
                            &config.context_dir,
                            run_mount_source_roots
                                .as_deref()
                                .unwrap_or(&completed_stages),
                            &rootfs_dir,
                            &layers_dir,
                            &state.workdir,
                            &state.run_env(),
                            &state.shell,
                            state.user.as_deref(),
                            state.layers.len() + base_layers.len(),
                            config.quiet,
                            session,
                            Some(&dockerignore),
                        )
                        .await?
                    } else {
                        handle_run(
                            command,
                            cache_mounts,
                            bind_mounts,
                            tmpfs_mounts,
                            &config.context_dir,
                            run_mount_source_roots
                                .as_deref()
                                .unwrap_or(&completed_stages),
                            &rootfs_dir,
                            &layers_dir,
                            &state.workdir,
                            &state.run_env(),
                            &state.shell,
                            state.layers.len() + base_layers.len(),
                            config.quiet,
                            Some(&dockerignore),
                        )?
                    };
                    if let Some(layer_info) = layer_opt {
                        let diff_id = compute_diff_id(&layer_info.path)?;
                        if let Some(c) = &cache {
                            c.store(&chain_key, &layer_info, &diff_id);
                        }
                        state.diff_ids.push(diff_id);
                        state.layers.push(layer_info);
                        state.history.push(HistoryEntry {
                            created_by: created_by.clone(),
                            empty_layer: false,
                        });
                    } else {
                        state.history.push(HistoryEntry {
                            created_by: created_by.clone(),
                            empty_layer: true,
                        });
                    }
                }

                Instruction::Workdir { path } => {
                    if !config.quiet {
                        println!("Step {}/{}: WORKDIR {}", step, total_instructions, path);
                    }
                    // Expand prior ENV/ARG in the WORKDIR path (Docker does too).
                    let expanded_path = expand_args(path, &state.expansion_vars());
                    state.workdir = resolve_path(&state.workdir, &expanded_path);
                    let full = rootfs_dir.join(state.workdir.trim_start_matches('/'));
                    let _ = std::fs::create_dir_all(&full);
                    state.history.push(HistoryEntry {
                        created_by: format!("WORKDIR {}", path),
                        empty_layer: true,
                    });
                }

                Instruction::Env { vars } => {
                    let display: Vec<String> =
                        vars.iter().map(|(k, v)| format!("{}={}", k, v)).collect();
                    let display = display.join(" ");
                    if !config.quiet {
                        println!("Step {}/{}: ENV {}", step, total_instructions, display);
                    }
                    for (key, value) in vars {
                        // Expand prior ENV (and declared ARGs) in the value, left
                        // to right, so `ENV A=/x B=$A/y` resolves B against A.
                        let expanded_value = expand_args(value, &state.expansion_vars());
                        if let Some(existing) = state.env.iter_mut().find(|(k, _)| k == key) {
                            existing.1 = expanded_value;
                        } else {
                            state.env.push((key.clone(), expanded_value));
                        }
                    }
                    state.history.push(HistoryEntry {
                        created_by: format!("ENV {}", display),
                        empty_layer: true,
                    });
                }

                Instruction::Entrypoint { exec } => {
                    if !config.quiet {
                        println!(
                            "Step {}/{}: ENTRYPOINT {:?}",
                            step, total_instructions, exec
                        );
                    }
                    state.entrypoint = Some(exec.clone());
                    state.history.push(HistoryEntry {
                        created_by: format!("ENTRYPOINT {:?}", exec),
                        empty_layer: true,
                    });
                }

                Instruction::Cmd { exec } => {
                    if !config.quiet {
                        println!("Step {}/{}: CMD {:?}", step, total_instructions, exec);
                    }
                    state.cmd = Some(exec.clone());
                    state.history.push(HistoryEntry {
                        created_by: format!("CMD {:?}", exec),
                        empty_layer: true,
                    });
                }

                Instruction::Expose { ports } => {
                    let joined = ports.join(" ");
                    if !config.quiet {
                        println!("Step {}/{}: EXPOSE {}", step, total_instructions, joined);
                    }
                    for port in ports {
                        if !state.exposed_ports.contains(port) {
                            state.exposed_ports.push(port.clone());
                        }
                    }
                    state.history.push(HistoryEntry {
                        created_by: format!("EXPOSE {}", joined),
                        empty_layer: true,
                    });
                }

                Instruction::Label { pairs } => {
                    let joined = pairs
                        .iter()
                        .map(|(k, v)| format!("{}={}", k, v))
                        .collect::<Vec<_>>()
                        .join(" ");
                    if !config.quiet {
                        println!("Step {}/{}: LABEL {}", step, total_instructions, joined);
                    }
                    for (key, value) in pairs {
                        state.labels.insert(key.clone(), value.clone());
                    }
                    state.history.push(HistoryEntry {
                        created_by: format!("LABEL {}", joined),
                        empty_layer: true,
                    });
                }

                Instruction::User { user } => {
                    if !config.quiet {
                        println!("Step {}/{}: USER {}", step, total_instructions, user);
                    }
                    state.user = Some(user.clone());
                    state.history.push(HistoryEntry {
                        created_by: format!("USER {}", user),
                        empty_layer: true,
                    });
                }

                Instruction::Arg { name, default } => {
                    if !config.quiet {
                        println!("Step {}/{}: ARG {}", step, total_instructions, name);
                    }
                    state.declared_args.insert(name.clone());
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

                Instruction::Shell { exec } => {
                    if !config.quiet {
                        println!("Step {}/{}: SHELL {:?}", step, total_instructions, exec);
                    }
                    state.shell = exec.clone();
                    state.history.push(HistoryEntry {
                        created_by: format!("SHELL {:?}", exec),
                        empty_layer: true,
                    });
                }

                Instruction::StopSignal { signal } => {
                    if !config.quiet {
                        println!(
                            "Step {}/{}: STOPSIGNAL {}",
                            step, total_instructions, signal
                        );
                    }
                    state.stop_signal = Some(signal.clone());
                    state.history.push(HistoryEntry {
                        created_by: format!("STOPSIGNAL {}", signal),
                        empty_layer: true,
                    });
                }

                Instruction::HealthCheck {
                    cmd,
                    interval,
                    timeout,
                    retries,
                    start_period,
                } => {
                    if !config.quiet {
                        if cmd.is_some() {
                            println!("Step {}/{}: HEALTHCHECK CMD ...", step, total_instructions);
                        } else {
                            println!("Step {}/{}: HEALTHCHECK NONE", step, total_instructions);
                        }
                    }
                    state.health_check = cmd.as_ref().map(|c| OciHealthCheck {
                        test: c.clone(),
                        interval: *interval,
                        timeout: *timeout,
                        retries: *retries,
                        start_period: *start_period,
                    });
                    state.history.push(HistoryEntry {
                        created_by: if cmd.is_some() {
                            "HEALTHCHECK CMD ...".to_string()
                        } else {
                            "HEALTHCHECK NONE".to_string()
                        },
                        empty_layer: true,
                    });
                }

                Instruction::OnBuild { instruction } => {
                    let trigger = format!("{:?}", instruction);
                    if !config.quiet {
                        println!("Step {}/{}: ONBUILD {}", step, total_instructions, trigger);
                    }
                    // Store the raw instruction text for the image config
                    state.onbuild.push(instruction_to_string(instruction));
                    state.history.push(HistoryEntry {
                        created_by: format!("ONBUILD {}", instruction_to_string(instruction)),
                        empty_layer: true,
                    });
                }

                Instruction::Volume { paths } => {
                    if !config.quiet {
                        println!(
                            "Step {}/{}: VOLUME {}",
                            step,
                            total_instructions,
                            paths.join(" ")
                        );
                    }
                    for p in paths {
                        if !state.volumes.contains(p) {
                            state.volumes.push(p.clone());
                        }
                    }
                    // Create volume directories in rootfs
                    for p in paths {
                        let full = rootfs_dir.join(p.trim_start_matches('/'));
                        let _ = std::fs::create_dir_all(&full);
                    }
                    state.history.push(HistoryEntry {
                        created_by: format!("VOLUME {}", paths.join(" ")),
                        empty_layer: true,
                    });
                }
            }
        }

        if let Some(session) = run_pool_session.take() {
            session.release().await?;
        }

        // Store completed stage rootfs for COPY --from
        completed_stages.push((stage.alias.clone(), rootfs_dir.clone()));

        if is_final_stage {
            final_state = state;
            final_base_layers = base_layers;
            final_base_diff_ids = base_diff_ids;
            // Stages after the --target stage are not part of the output; stop.
            break;
        }
    }

    // Assemble the final OCI image from the output (final or --target) stage
    let reference = config
        .tag
        .clone()
        .unwrap_or_else(|| "a3s-build:latest".to_string());

    let final_layers_dir = build_dir
        .path()
        .join(format!("layers_{}", output_stage_idx));

    // Determine target platform (use first platform or host default)
    let target_platform = config
        .platforms
        .first()
        .cloned()
        .unwrap_or_else(default_target_platform);

    let result = assemble_image(
        &reference,
        &final_state,
        &final_base_layers,
        &final_base_diff_ids,
        &final_layers_dir,
        &store,
        &target_platform,
    )
    .await?;

    if !config.quiet {
        println!(
            "Successfully built {} ({} layers, {}, {})",
            reference,
            result.layer_count,
            format_size(result.size),
            target_platform,
        );
    }

    if let Some(ref m) = config.metrics {
        m.image_build_total.inc();
    }

    Ok(result)
}

// =============================================================================
// Helper functions
// =============================================================================

/// Attempt to reuse a cached layer for a layer-producing instruction.
///
/// On a cache hit (and only when `cache_valid` is still true and a cache is
/// open), this applies the cached layer's diff to `rootfs_dir` so later
/// instructions build on the correct rootfs, then records the layer, diff_id,
/// and a non-empty history entry in `state`. Returns `Some(())` on a hit (the
/// caller should `continue`), or `None` to fall through to normal execution.
struct CachedLayerReuse<'a> {
    cache_valid: bool,
    cache: Option<&'a BuildCache>,
    chain_key: &'a str,
    rootfs_dir: &'a Path,
    layers_dir: &'a Path,
    layer_index: usize,
    created_by: &'a str,
}

fn try_reuse_cached_layer(
    request: CachedLayerReuse<'_>,
    state: &mut BuildState,
) -> Result<Option<()>> {
    if !request.cache_valid {
        return Ok(None);
    }
    let Some(cached) = request.cache.and_then(|c| c.lookup(request.chain_key)) else {
        return Ok(None);
    };

    let local_layer = request.layers_dir.join(format!(
        "cached_{}_{}.tar.gz",
        request.layer_index, cached.digest
    ));
    if let Err(error) = std::fs::copy(&cached.blob_path, &local_layer) {
        tracing::warn!(
            key = %request.chain_key,
            source = %cached.blob_path.display(),
            error = %error,
            "Build cache blob disappeared before it could be materialized; rebuilding instruction"
        );
        return Ok(None);
    }

    // Apply the cached diff so subsequent instructions see the right rootfs.
    extract_layer(&local_layer, request.rootfs_dir)?;
    let local_size = std::fs::metadata(&local_layer)
        .map(|metadata| metadata.len())
        .unwrap_or(cached.size);

    state.layers.push(LayerInfo {
        path: local_layer,
        digest: cached.digest,
        size: local_size,
    });
    state.diff_ids.push(cached.diff_id);
    state.history.push(HistoryEntry {
        created_by: request.created_by.to_string(),
        empty_layer: false,
    });
    Ok(Some(()))
}

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
    if image_ref == "scratch" {
        return Ok((Vec::new(), Vec::new(), scratch_config()));
    }

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

/// Resolve an external image source when `from=<image>` is not a build stage:
/// pull the image and extract it to a temp rootfs (Docker behavior). Memoized
/// per build so several copies or RUN bind mounts from one image pull only once.
async fn resolve_external_from_rootfs(
    image_ref: &str,
    operation: &str,
    store: &Arc<ImageStore>,
    build_dir: &Path,
    cache: &mut HashMap<String, PathBuf>,
) -> Result<PathBuf> {
    if let Some(dir) = cache.get(image_ref) {
        return Ok(dir.clone());
    }

    let dir = build_dir.join(format!("copyfrom_{}", cache.len()));
    std::fs::create_dir_all(&dir).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to create {operation} image rootfs {}: {}",
            dir.display(),
            e
        ))
    })?;

    let puller = ImagePuller::new(store.clone(), RegistryAuth::from_env());
    let oci_image = puller.pull(image_ref).await.map_err(|e| {
        BoxError::BuildError(format!(
            "{operation} from={}: not a build stage and could not be pulled as an image: {}",
            image_ref, e
        ))
    })?;
    for layer_path in oci_image.layer_paths() {
        extract_layer(layer_path, &dir)?;
    }

    cache.insert(image_ref.to_string(), dir.clone());
    Ok(dir)
}

fn validate_build_config(config: &BuildConfig) -> Result<()> {
    if config.platforms.len() > 1 {
        return Err(BoxError::BuildError(
            "Multi-platform builds are not implemented yet; pass a single target platform"
                .to_string(),
        ));
    }

    for platform in &config.platforms {
        if platform.os != "linux" {
            return Err(BoxError::BuildError(format!(
                "Only linux target platforms are supported for image builds, got {}",
                platform
            )));
        }
    }

    Ok(())
}

fn default_target_platform() -> Platform {
    let host = Platform::host();
    Platform::new("linux", host.architecture)
}

fn scratch_config() -> OciImageConfig {
    OciImageConfig {
        entrypoint: None,
        cmd: None,
        env: Vec::new(),
        working_dir: None,
        user: None,
        exposed_ports: Vec::new(),
        labels: HashMap::new(),
        volumes: Vec::new(),
        stop_signal: None,
        health_check: None,
        onbuild: Vec::new(),
    }
}

/// Assemble the final OCI image layout and store it.
async fn assemble_image(
    reference: &str,
    state: &BuildState,
    base_layers: &[LayerInfo],
    base_diff_ids: &[String],
    layers_dir: &Path,
    store: &Arc<ImageStore>,
    target_platform: &Platform,
) -> Result<BuildResult> {
    // Create output directory
    let output_dir = layers_dir.join("_output");
    let blobs_dir = output_dir.join("blobs").join("sha256");
    std::fs::create_dir_all(&blobs_dir)
        .map_err(|e| BoxError::BuildError(format!("Failed to create output blobs dir: {}", e)))?;

    // Collect all layers: base + new
    let mut all_layer_descriptors = Vec::new();
    let mut all_diff_ids: Vec<String> = base_diff_ids.to_vec();

    // Copy base layers to output
    for layer in base_layers {
        let blob_path = blobs_dir.join(&layer.digest);
        if !blob_path.exists() {
            copy_layer_blob(layer, &blob_path, "base layer")?;
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
            copy_layer_blob(layer, &blob_path, &format!("layer {i}"))?;
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
    let arch = target_platform.oci_arch();

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
        config_section.insert("Env".to_string(), serde_json::json!(env_list));
    }
    if let Some(ref ep) = state.entrypoint {
        config_section.insert("Entrypoint".to_string(), serde_json::json!(ep));
    }
    if let Some(ref cmd) = state.cmd {
        config_section.insert("Cmd".to_string(), serde_json::json!(cmd));
    }
    if state.workdir != "/" {
        config_section.insert("WorkingDir".to_string(), serde_json::json!(state.workdir));
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
    if let Some(ref sig) = state.stop_signal {
        config_section.insert("StopSignal".to_string(), serde_json::json!(sig));
    }
    if let Some(ref hc) = state.health_check {
        let mut hc_obj = serde_json::json!({
            "Test": hc.test,
        });
        if let Some(interval) = hc.interval {
            // OCI stores intervals in nanoseconds
            hc_obj["Interval"] = serde_json::json!(interval * 1_000_000_000);
        }
        if let Some(timeout) = hc.timeout {
            hc_obj["Timeout"] = serde_json::json!(timeout * 1_000_000_000);
        }
        if let Some(retries) = hc.retries {
            hc_obj["Retries"] = serde_json::json!(retries);
        }
        if let Some(start_period) = hc.start_period {
            hc_obj["StartPeriod"] = serde_json::json!(start_period * 1_000_000_000);
        }
        config_section.insert("Healthcheck".to_string(), hc_obj);
    }
    if !state.onbuild.is_empty() {
        config_section.insert("OnBuild".to_string(), serde_json::json!(state.onbuild));
    }
    if !state.volumes.is_empty() {
        let vols: HashMap<String, serde_json::Value> = state
            .volumes
            .iter()
            .map(|v| (v.clone(), serde_json::json!({})))
            .collect();
        config_section.insert("Volumes".to_string(), serde_json::json!(vols));
    }

    // Write config blob
    let config_bytes = serde_json::to_vec_pretty(&config_obj)?;
    let config_digest = sha256_bytes(&config_bytes);
    std::fs::write(blobs_dir.join(&config_digest), &config_bytes)
        .map_err(|e| BoxError::BuildError(format!("Failed to write config blob: {}", e)))?;

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
    std::fs::write(blobs_dir.join(&manifest_digest), &manifest_bytes)
        .map_err(|e| BoxError::BuildError(format!("Failed to write manifest blob: {}", e)))?;

    // Write index.json
    let mut platform_obj = serde_json::json!({
        "os": target_platform.os,
        "architecture": target_platform.architecture
    });
    if let Some(ref variant) = target_platform.variant {
        platform_obj["variant"] = serde_json::json!(variant);
    }

    let index = serde_json::json!({
        "schemaVersion": 2,
        "mediaType": "application/vnd.oci.image.index.v1+json",
        "manifests": [{
            "mediaType": "application/vnd.oci.image.manifest.v1+json",
            "digest": format!("sha256:{}", manifest_digest),
            "size": manifest_bytes.len(),
            "platform": platform_obj
        }]
    });
    std::fs::write(
        output_dir.join("index.json"),
        serde_json::to_string_pretty(&index)?,
    )
    .map_err(|e| BoxError::BuildError(format!("Failed to write index.json: {}", e)))?;

    // Write oci-layout
    std::fs::write(
        output_dir.join("oci-layout"),
        r#"{"imageLayoutVersion":"1.0.0"}"#,
    )
    .map_err(|e| BoxError::BuildError(format!("Failed to write oci-layout: {}", e)))?;

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

fn copy_layer_blob(layer: &LayerInfo, blob_path: &Path, label: &str) -> Result<()> {
    if !layer.path.exists() {
        return Err(BoxError::BuildError(format!(
            "Failed to copy {label}: source layer {} for digest {} does not exist",
            layer.path.display(),
            layer.prefixed_digest()
        )));
    }

    std::fs::copy(&layer.path, blob_path).map_err(|e| {
        BoxError::BuildError(format!(
            "Failed to copy {label} from {} to {} (digest {}): {}",
            layer.path.display(),
            blob_path.display(),
            layer.prefixed_digest(),
            e
        ))
    })?;
    Ok(())
}
