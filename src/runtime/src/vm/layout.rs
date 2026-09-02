//! Layout preparation — rootfs building, caching, TEE config, binary discovery.

use std::path::{Path, PathBuf};

use crate::cache::RootfsCache;
use crate::oci::OciRootfsBuilder;
use crate::vmm::TeeInstanceConfig;
use a3s_box_core::config::TeeConfig;
use a3s_box_core::error::{BoxError, Result};

use super::{BoxLayout, VmManager};

mod paths;
pub(crate) use paths::{legacy_sandbox_runtime_root, runtime_socket_dir, sandbox_runtime_root};

mod image;

pub(crate) use image::persistent_rootfs_generation_exists;
use image::{registry_auth_for_image, validate_image_health_support};
impl VmManager {
    /// Resolve only immutable image metadata needed to reserve Box-owned
    /// resources. This deliberately does not prepare a rootfs, workspace,
    /// socket, mount, or volume directory.
    pub(crate) async fn plan_image_anonymous_volumes(&mut self) -> Result<Vec<String>> {
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        let image_config = match crate::resolved_image::load_resolved_image_config(&box_dir)? {
            Some(persisted) => crate::oci::OciImageConfig::from(persisted),
            None => {
                let reference = self.config.image.clone();
                let images_dir = self.home_dir.join("images");
                let store =
                    crate::oci::ImageStore::new(&images_dir, crate::DEFAULT_IMAGE_CACHE_SIZE)?;
                let auth = registry_auth_for_image(
                    &self.home_dir,
                    &reference,
                    self.transient_registry_auth.take(),
                )?;
                let mut puller = crate::oci::ImagePuller::new(std::sync::Arc::new(store), auth);
                if let Some(ref metrics) = self.prom {
                    puller = puller.set_metrics(metrics.clone());
                }
                if let Some(ref progress) = self.pull_progress_fn {
                    puller = puller.with_progress_fn(progress.clone());
                }
                let image = puller.pull(&reference).await?;
                let config = image.config().clone();
                drop(puller);
                config
            }
        };

        self.plan_anonymous_volumes(&image_config)
            .map(|plans| plans.into_iter().map(|plan| plan.name).collect())
    }

    pub(crate) async fn prepare_layout(&mut self) -> Result<BoxLayout> {
        let transient_registry_auth = self.transient_registry_auth.take();
        // Create box-specific directories
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        let socket_dir = self.socket_dir();
        let logs_dir = box_dir.join("logs");

        std::fs::create_dir_all(&socket_dir).map_err(|e| BoxError::BoxBootError {
            message: format!("Failed to create socket directory: {}", e),
            hint: None,
        })?;

        #[cfg(windows)]
        super::windows_stop::clear(&socket_dir).map_err(|error| BoxError::BoxBootError {
            message: format!(
                "Failed to clear stale Windows stop request in {}: {error}",
                socket_dir.display()
            ),
            hint: None,
        })?;

        std::fs::create_dir_all(&logs_dir).map_err(|e| BoxError::BoxBootError {
            message: format!("Failed to create logs directory: {}", e),
            hint: None,
        })?;

        // Resolve workspace path: empty config means use a per-box directory so the
        // host CWD is never accidentally exposed to the guest.
        let workspace_path = if self.config.workspace.as_os_str().is_empty() {
            box_dir.join("workspace")
        } else {
            PathBuf::from(&self.config.workspace)
        };
        if !workspace_path.exists() {
            std::fs::create_dir_all(&workspace_path).map_err(|e| BoxError::BoxBootError {
                message: format!("Failed to create workspace directory: {}", e),
                hint: None,
            })?;
        }
        // Canonicalize to absolute path (libkrun requires absolute paths for virtiofs)
        let workspace_path = workspace_path
            .canonicalize()
            .map_err(|e| BoxError::BoxBootError {
                message: format!(
                    "Failed to resolve workspace path {}: {}",
                    workspace_path.display(),
                    e
                ),
                hint: None,
            })?;

        let snapshot_requested = super::rootfs_snapshot_requested(&self.config);
        let rootfs_prepare_options = crate::rootfs::RootfsPrepareOptions {
            writable_layer_bytes: self.config.resources.ephemeral_storage_bytes,
        };
        if rootfs_prepare_options.writable_layer_bytes.is_some()
            && !self.config.isolation.is_sandbox()
        {
            return Err(BoxError::ConfigError(
                "Ephemeral storage quotas are supported only for Sandbox isolation".into(),
            ));
        }
        if !self.config.isolation.is_sandbox() {
            let resume = self.rootfs_provider.resume_for_boot(
                &box_dir,
                crate::rootfs::RootfsResumeOptions {
                    disk_mib: self.config.resources.disk_mb,
                    persistent: self.config.persistent,
                    snapshot: snapshot_requested,
                },
            )?;
            if let Some(resumed_rootfs) = resume {
                let persisted = crate::resolved_image::load_resolved_image_config(&box_dir)?
                    .ok_or_else(|| {
                        BoxError::StateError(format!(
                            "Persistent guest-native rootfs for {} has no resolved image configuration",
                            self.box_id
                        ))
                    })?;
                let oci_config = Some(crate::oci::OciImageConfig::from(persisted));
                validate_image_health_support(
                    oci_config
                        .as_ref()
                        .and_then(|config| config.health_check.as_ref()),
                    self.healthcheck_disabled,
                )?;
                tracing::info!(
                    rootfs = %resumed_rootfs.source,
                    "Reusing authoritative guest-native persistent rootfs"
                );
                let tee_instance_config = self.generate_tee_config(&box_dir)?;
                return Ok(BoxLayout {
                    // No code may inspect this compatibility mountpoint while
                    // `resumed_rootfs` is present. It remains only to avoid
                    // conflating the raw disk path with a directory path.
                    rootfs_path: box_dir.join("rootfs"),
                    resumed_rootfs: Some(resumed_rootfs),
                    exec_socket_path: socket_dir.join("exec.sock"),
                    pty_socket_path: socket_dir.join("pty.sock"),
                    attest_socket_path: socket_dir.join("attest.sock"),
                    port_forward_socket_path: socket_dir.join("portfwd.sock"),
                    workspace_path,
                    console_output: Some(logs_dir.join("console.log")),
                    oci_config,
                    #[cfg(target_os = "macos")]
                    oci_manifest_digest: None,
                    prefer_image_rootfs_metadata: false,
                    tee_instance_config,
                });
            }
        }

        // Snapshot restore (copy-on-write): `snapshot restore` writes a
        // `.snapshot-lower` marker pointing at the snapshot's pristine stored
        // rootfs. Mount it as a read-only overlay lower with a fresh per-box
        // upper, so the box's writes are copy-on-write, the snapshot stays
        // shared and untouched across all forks, and nothing is copied. On a
        // non-overlay host the CopyProvider falls back to a full copy (same
        // result, slower). This mirrors the rootfs cache-hit path below.
        if let Some(lower) = snapshot_lower_dir(&box_dir) {
            if lower.is_dir() {
                let oci_config = Some(crate::resolved_image::load_snapshot_oci_config(
                    &lower,
                    &self.config.image,
                )?);
                validate_image_health_support(
                    oci_config
                        .as_ref()
                        .and_then(|config| config.health_check.as_ref()),
                    self.healthcheck_disabled,
                )?;
                tracing::info!(
                    lower = %lower.display(),
                    "Restoring snapshot via copy-on-write overlay lower"
                );
                let rootfs_path = self.rootfs_provider.prepare_with_options(
                    &box_dir,
                    &lower,
                    rootfs_prepare_options,
                )?;
                // Refresh the guest init on the merged view (the write lands in
                // the per-box upper, never mutating the shared lower) in case the
                // snapshot carries an older binary than the current runtime.
                if let Ok(guest_init_path) = Self::find_guest_init() {
                    if let Err(e) = OciRootfsBuilder::new(&rootfs_path)
                        .with_guest_init(guest_init_path)
                        .install_guest_init_only()
                    {
                        tracing::warn!(error = %e, "Failed to refresh guest init on restored overlay");
                    }
                }
                if let Some(config) = oci_config.as_ref() {
                    crate::resolved_image::persist_resolved_image_config(&box_dir, config)?;
                }
                let tee_instance_config = self.generate_tee_config(&box_dir)?;
                return Ok(BoxLayout {
                    rootfs_path,
                    resumed_rootfs: None,
                    exec_socket_path: socket_dir.join("exec.sock"),
                    pty_socket_path: socket_dir.join("pty.sock"),
                    attest_socket_path: socket_dir.join("attest.sock"),
                    port_forward_socket_path: socket_dir.join("portfwd.sock"),
                    workspace_path,
                    console_output: Some(logs_dir.join("console.log")),
                    oci_config,
                    #[cfg(target_os = "macos")]
                    oci_manifest_digest: None,
                    prefer_image_rootfs_metadata: false,
                    tee_instance_config,
                });
            }
            tracing::warn!(
                lower = %lower.display(),
                "`.snapshot-lower` points at a missing dir; falling through to image pull"
            );
        }

        // Snapshot restore pre-populates `box_dir/rootfs` with a captured full
        // root filesystem. Boot directly from it instead of rebuilding from the
        // image, so the snapshot's filesystem state (including runtime changes)
        // is preserved. Normal boxes never have `box_dir/rootfs` — the overlay
        // provider materializes the rootfs at `merged` — so this path only
        // affects restored boxes and cannot regress the normal boot path.
        let prebuilt_rootfs = box_dir.join("rootfs");
        // A restore marker is written by `snapshot restore` next to the copied
        // rootfs; gating on it (not merely on `rootfs` existing) ensures this
        // path can never be taken for a normal box that happens to have a
        // leftover `rootfs` directory from a cache-miss build.
        let restore_marker = box_dir.join(".snapshot-rootfs");
        let prebuilt_is_populated = restore_marker.exists()
            && std::fs::read_dir(&prebuilt_rootfs)
                .map(|mut it| it.next().is_some())
                .unwrap_or(false);
        if prebuilt_is_populated {
            let oci_config = crate::resolved_image::load_resolved_image_config(&box_dir)?
                .map(crate::oci::OciImageConfig::from);
            validate_image_health_support(
                oci_config
                    .as_ref()
                    .and_then(|config| config.health_check.as_ref()),
                self.healthcheck_disabled,
            )?;
            tracing::info!(
                rootfs = %prebuilt_rootfs.display(),
                "Booting from pre-populated rootfs (snapshot restore)"
            );
            // A restored snapshot normally boots its populated tree directly.
            // When a writable-layer quota is requested, put that tree behind a
            // bounded overlay so snapshot writes receive the same enforcement
            // as a cold Sandbox boot.
            let rootfs_path = if rootfs_prepare_options.writable_layer_bytes.is_some() {
                self.rootfs_provider.prepare_with_options(
                    &box_dir,
                    &prebuilt_rootfs,
                    rootfs_prepare_options,
                )?
            } else {
                prebuilt_rootfs.clone()
            };
            // Refresh the guest init in case the snapshot carries an older binary
            // than the current runtime.
            if let Ok(guest_init_path) = Self::find_guest_init() {
                if let Err(e) = OciRootfsBuilder::new(&rootfs_path)
                    .with_guest_init(guest_init_path)
                    .install_guest_init_only()
                {
                    tracing::warn!(error = %e, "Failed to refresh guest init on restored rootfs");
                }
            }
            let tee_instance_config = self.generate_tee_config(&box_dir)?;
            return Ok(BoxLayout {
                rootfs_path,
                resumed_rootfs: None,
                exec_socket_path: socket_dir.join("exec.sock"),
                pty_socket_path: socket_dir.join("pty.sock"),
                attest_socket_path: socket_dir.join("attest.sock"),
                port_forward_socket_path: socket_dir.join("portfwd.sock"),
                workspace_path,
                console_output: Some(logs_dir.join("console.log")),
                oci_config,
                #[cfg(target_os = "macos")]
                oci_manifest_digest: None,
                prefer_image_rootfs_metadata: false,
                tee_instance_config,
            });
        }

        // Pull OCI image from registry and extract at rootfs root.
        // Extracting at root preserves absolute symlinks and dynamic linker paths.
        let reference = &self.config.image;
        let has_persistent_rootfs_generation =
            self.config.persistent && persistent_rootfs_generation_exists(&box_dir)?;

        // Snapshot-fork fast path: a restored guest reuses the exact rootfs that
        // was paired with its memory template. Skip registry resolution and the
        // guest-init refresh (the snapshot already has it). A missing identity or
        // cache entry fails closed so a moved tag can never pair new filesystem
        // content with old guest memory; the warm-pool caller may cold-boot instead.
        #[cfg(unix)]
        if super::is_restore_mode(&self.config) {
            let cache_key = self.restore_rootfs_cache_key.as_deref();
            let cached_path = match cache_key {
                Some(cache_key) => self.try_rootfs_cache_path(cache_key)?,
                None => None,
            };
            let (cache_key, cached_path) = require_snapshot_restore_rootfs(cache_key, cached_path)?;
            let rootfs_path = self.rootfs_provider.prepare_with_options(
                &box_dir,
                &cached_path,
                rootfs_prepare_options,
            )?;
            // Record that this box holds `cache_key` as its overlay lower, so a
            // concurrent box's cache prune won't evict it mid-mount (ENOENT).
            self.mark_rootfs_cache_key(&box_dir, cache_key);
            let tee_instance_config = self.generate_tee_config(&box_dir)?;
            return Ok(BoxLayout {
                rootfs_path,
                resumed_rootfs: None,
                exec_socket_path: socket_dir.join("exec.sock"),
                pty_socket_path: socket_dir.join("pty.sock"),
                attest_socket_path: socket_dir.join("attest.sock"),
                port_forward_socket_path: socket_dir.join("portfwd.sock"),
                workspace_path,
                console_output: Some(logs_dir.join("console.log")),
                oci_config: None,
                #[cfg(target_os = "macos")]
                oci_manifest_digest: None,
                prefer_image_rootfs_metadata: !has_persistent_rootfs_generation,
                tee_instance_config,
            });
        }

        let images_dir = self.home_dir.join("images");
        let store = crate::oci::ImageStore::new(&images_dir, crate::DEFAULT_IMAGE_CACHE_SIZE)?;
        let auth = registry_auth_for_image(&self.home_dir, reference, transient_registry_auth)?;
        let mut puller = crate::oci::ImagePuller::new(std::sync::Arc::new(store), auth);
        if let Some(ref m) = self.prom {
            puller = puller.set_metrics(m.clone());
        }
        if let Some(ref f) = self.pull_progress_fn {
            puller = puller.with_progress_fn(f.clone());
        }

        tracing::info!(reference = %reference, "Pulling OCI image from registry");

        let oci_image = puller.pull(reference).await?;
        // The image object owns only the cached OCI layout. Drop the puller as
        // soon as the registry boundary closes so transient authorization is
        // zeroized before rootfs extraction or guest preparation begins.
        drop(puller);
        validate_image_health_support(
            oci_image.config().health_check.as_ref(),
            self.healthcheck_disabled,
        )?;

        let image_path = oci_image.root_dir().to_path_buf();
        let manifest_digest = oci_image.manifest_digest().to_string();

        if self.rootfs_provider.supports_direct_oci_assembly() {
            let guest_init = Self::find_guest_init()?;
            let guest_init_sha256 = Self::guest_init_sha256(&guest_init)?;
            let image_platform = oci_image.platform();
            let platform = match image_platform.variant.as_deref() {
                Some(variant) => format!(
                    "{}/{}/{}",
                    image_platform.os, image_platform.architecture, variant
                ),
                None => format!("{}/{}", image_platform.os, image_platform.architecture),
            };
            let artifact_cache =
                self.config
                    .cache
                    .enabled
                    .then(|| crate::rootfs::RootfsArtifactCacheOptions {
                        directory: self.resolve_cache_dir().join("rootfs-ext4-v1"),
                        oci_manifest_digest: manifest_digest.clone(),
                        platform: platform.clone(),
                        guest_init_sha256: guest_init_sha256.clone(),
                        max_entries: self.config.cache.max_rootfs_entries,
                        max_allocated_bytes: self.config.cache.max_cache_bytes,
                    });
            if let Some(resumed_rootfs) = self.rootfs_provider.prepare_oci_for_boot(
                &box_dir,
                crate::rootfs::RootfsOciPrepareOptions {
                    image: &oci_image,
                    guest_init: &guest_init,
                    guest_init_sha256: &guest_init_sha256,
                    platform: &platform,
                    disk_mib: self.config.resources.disk_mb,
                    persistent: self.config.persistent,
                    snapshot: snapshot_requested,
                    artifact_cache,
                },
            )? {
                let oci_config = Some(oci_image.config().clone());
                if let Some(config) = oci_config.as_ref() {
                    crate::resolved_image::persist_resolved_image_config(&box_dir, config)?;
                }
                let tee_instance_config = self.generate_tee_config(&box_dir)?;
                return Ok(BoxLayout {
                    // The direct provider deliberately leaves this compatibility
                    // path absent. All rootfs control travels through the private
                    // boot bundle once `resumed_rootfs` is present.
                    rootfs_path: box_dir.join("rootfs"),
                    resumed_rootfs: Some(resumed_rootfs),
                    exec_socket_path: socket_dir.join("exec.sock"),
                    pty_socket_path: socket_dir.join("pty.sock"),
                    attest_socket_path: socket_dir.join("attest.sock"),
                    port_forward_socket_path: socket_dir.join("portfwd.sock"),
                    workspace_path,
                    console_output: Some(logs_dir.join("console.log")),
                    oci_config,
                    #[cfg(target_os = "macos")]
                    oci_manifest_digest: Some(manifest_digest),
                    prefer_image_rootfs_metadata: false,
                    tee_instance_config,
                });
            }
        }

        // Try rootfs cache first — on hit, use the rootfs provider (overlay or copy)
        let cache_key = RootfsCache::compute_image_key(reference, &manifest_digest);
        let (rootfs_path, oci_config, prefer_image_rootfs_metadata) = if let Some(cached_path) =
            self.try_rootfs_cache_path(&cache_key)?
        {
            tracing::info!(
                cache_key = %&cache_key[..12],
                reference = %reference,
                provider = self.rootfs_provider.name(),
                "Rootfs cache hit"
            );
            if let Some(ref prom) = self.prom {
                prom.rootfs_cache_hits.inc();
            }
            let rootfs_path = self.rootfs_provider.prepare_with_options(
                &box_dir,
                &cached_path,
                rootfs_prepare_options,
            )?;
            // Record that this box holds `cache_key` as its overlay lower, so a
            // concurrent box's cache prune won't evict it mid-mount (ENOENT).
            self.mark_rootfs_cache_key(&box_dir, &cache_key);

            if let Ok(guest_init_path) = Self::find_guest_init() {
                tracing::info!(
                    guest_init = %guest_init_path.display(),
                    "Refreshing guest init on cached rootfs"
                );
                OciRootfsBuilder::new(&rootfs_path)
                    .with_guest_init(guest_init_path)
                    .install_guest_init_only()?;
            }

            let builder = OciRootfsBuilder::new(&rootfs_path).with_image(&image_path);
            (
                rootfs_path,
                Some(builder.image_config()?),
                !has_persistent_rootfs_generation,
            )
        } else {
            tracing::info!(
                image = %image_path.display(),
                "Building rootfs from pulled OCI image (cache miss)"
            );
            if let Some(ref prom) = self.prom {
                prom.rootfs_cache_misses.inc();
            }

            let staged_rootfs_path = self.rootfs_provider.prepare_empty(&box_dir)?;
            let rootfs_populated = std::fs::read_dir(&staged_rootfs_path)
                .map(|mut entries| entries.next().is_some())
                .map_err(|error| {
                    BoxError::BuildError(format!(
                        "Failed to inspect rootfs {}: {error}",
                        staged_rootfs_path.display()
                    ))
                })?;
            let mut builder = OciRootfsBuilder::new(&staged_rootfs_path).with_image(&image_path);

            // A persistent copy/APFS provider already contains the prior
            // terminal rootfs generation. Re-extracting the image would
            // overwrite guest changes and fails on existing layer
            // hardlinks. The image config remains immutable OCI metadata,
            // so read it without rebuilding the filesystem.
            if rootfs_populated {
                tracing::info!(
                    rootfs = %staged_rootfs_path.display(),
                    "Reusing populated persistent rootfs"
                );
                let config = builder.image_config()?;
                let rootfs_path = if rootfs_prepare_options.writable_layer_bytes.is_some() {
                    self.rootfs_provider.prepare_with_options(
                        &box_dir,
                        &staged_rootfs_path,
                        rootfs_prepare_options,
                    )?
                } else {
                    staged_rootfs_path
                };
                (rootfs_path, Some(config), false)
            } else {
                // Install guest init if available (runs as PID 1, mounts virtiofs shares,
                // then execs the container entrypoint)
                if let Ok(guest_init_path) = Self::find_guest_init() {
                    tracing::info!(
                        guest_init = %guest_init_path.display(),
                        "Installing guest init"
                    );
                    builder = builder.with_guest_init(guest_init_path);
                } else {
                    tracing::warn!(
                        "Guest init binary not found; container entrypoint will run as PID 1"
                    );
                }

                builder.build()?;
                let config = builder.image_config()?;

                // Store in cache for next time
                self.store_rootfs_cache(&cache_key, &staged_rootfs_path, reference);
                self.mark_rootfs_cache_key(&box_dir, &cache_key);

                let rootfs_path = if rootfs_prepare_options.writable_layer_bytes.is_some() {
                    self.rootfs_provider.prepare_with_options(
                        &box_dir,
                        &staged_rootfs_path,
                        rootfs_prepare_options,
                    )?
                } else {
                    staged_rootfs_path
                };

                (rootfs_path, Some(config), true)
            }
        };

        if let Some(config) = oci_config.as_ref() {
            crate::resolved_image::persist_resolved_image_config(&box_dir, config)?;
        }

        // Generate TEE configuration if enabled
        let tee_instance_config = self.generate_tee_config(&box_dir)?;

        Ok(BoxLayout {
            rootfs_path,
            resumed_rootfs: None,
            exec_socket_path: socket_dir.join("exec.sock"),
            pty_socket_path: socket_dir.join("pty.sock"),
            attest_socket_path: socket_dir.join("attest.sock"),
            port_forward_socket_path: socket_dir.join("portfwd.sock"),
            workspace_path,
            console_output: Some(logs_dir.join("console.log")),
            oci_config,
            #[cfg(target_os = "macos")]
            oci_manifest_digest: Some(manifest_digest),
            prefer_image_rootfs_metadata,
            tee_instance_config,
        })
    }

    pub(crate) fn socket_dir(&self) -> PathBuf {
        runtime_socket_dir(&self.home_dir, &self.box_id)
    }

    /// Try to get a cached rootfs and copy it to the target path.
    ///
    /// Returns `Some(target_path)` if cache hit, `None` if cache miss.
    /// If caching is disabled in config, always returns `None`.
    #[cfg(test)]
    pub(crate) fn try_rootfs_cache(
        &self,
        cache_key: &str,
        target_path: &Path,
    ) -> Result<Option<PathBuf>> {
        if !self.config.cache.enabled {
            return Ok(None);
        }

        let cache_dir = self.resolve_cache_dir().join("rootfs");
        let cache = match RootfsCache::new(&cache_dir) {
            Ok(c) => c,
            Err(e) => {
                tracing::warn!(error = %e, "Failed to open rootfs cache, skipping");
                return Ok(None);
            }
        };

        match cache.get(cache_key)? {
            Some(cached_path) => {
                // Copy cached rootfs to target
                crate::cache::layer_cache::copy_dir_recursive(&cached_path, target_path)?;
                Ok(Some(target_path.to_path_buf()))
            }
            None => Ok(None),
        }
    }

    /// Try to get the cached rootfs path without copying.
    ///
    /// Returns `Some(cached_path)` if cache hit, `None` if cache miss.
    /// The caller is responsible for preparing the rootfs via `RootfsProvider`.
    pub(crate) fn try_rootfs_cache_path(&self, cache_key: &str) -> Result<Option<PathBuf>> {
        #[cfg(target_os = "macos")]
        {
            if !self.config.cache.enabled {
                return Ok(None);
            }
            let image = self
                .resolve_cache_dir()
                .join("rootfs-apfs-v2")
                .join(format!("{cache_key}.sparseimage"));
            if image.is_file() {
                // Cache pruning is LRU. Refresh both timestamps without changing
                // sparse-image contents so frequently used images remain hot.
                if let Ok(file) = std::fs::OpenOptions::new().write(true).open(&image) {
                    let now = std::time::SystemTime::now();
                    let times = std::fs::FileTimes::new()
                        .set_accessed(now)
                        .set_modified(now);
                    let _ = file.set_times(times);
                }
                Ok(Some(image))
            } else {
                Ok(None)
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            if !self.config.cache.enabled {
                return Ok(None);
            }

            let cache_dir = self.resolve_cache_dir().join("rootfs");
            let cache = match RootfsCache::new(&cache_dir) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to open rootfs cache, skipping");
                    return Ok(None);
                }
            };

            cache.get(cache_key)
        }
    }

    /// Store a built rootfs in the cache for future reuse.
    ///
    /// Errors are logged but not propagated — caching is best-effort.
    pub(crate) fn store_rootfs_cache(
        &self,
        cache_key: &str,
        rootfs_path: &Path,
        description: &str,
    ) {
        #[cfg(target_os = "macos")]
        {
            use std::process::Command;

            if !self.config.cache.enabled {
                return;
            }
            if self.rootfs_provider.supports_artifact_cache() {
                // Guest-native providers publish their immutable ext4 cache at
                // the final ownership handoff. Cloning and remounting an APFS
                // cache here is redundant and can make a newly created staging
                // image look like a legacy migration source.
                return;
            }
            let cache_dir = self.resolve_cache_dir().join("rootfs-apfs-v2");
            if let Err(error) = std::fs::create_dir_all(&cache_dir) {
                tracing::warn!(%error, "Failed to create APFS rootfs cache");
                return;
            }
            let mountpoint = rootfs_path.parent().unwrap_or(rootfs_path);
            let box_dir = mountpoint.parent().unwrap_or(mountpoint);
            let source = box_dir.join("rootfs-apfs-v2.sparseimage");
            let destination = cache_dir.join(format!("{cache_key}.sparseimage"));
            let temporary = cache_dir.join(format!(".{cache_key}.tmp-{}", std::process::id()));

            crate::rootfs::unmount_box_rootfs(rootfs_path);
            let cloned = Command::new("cp")
                .arg("-c")
                .arg(&source)
                .arg(&temporary)
                .status()
                .is_ok_and(|status| status.success());
            if cloned {
                if let Err(error) = std::fs::rename(&temporary, &destination) {
                    tracing::warn!(%error, "Failed to publish APFS rootfs cache image");
                } else {
                    tracing::debug!(
                        cache_key = %&cache_key[..cache_key.len().min(12)],
                        %description,
                        "Stored case-sensitive APFS rootfs cache"
                    );
                    if let Err(error) = prune_apfs_rootfs_cache(
                        &cache_dir,
                        self.config.cache.max_rootfs_entries,
                        self.config.cache.max_cache_bytes,
                        cache_key,
                    ) {
                        tracing::warn!(%error, "Failed to prune APFS rootfs cache");
                    }
                }
            } else {
                tracing::warn!(source = %source.display(), "Failed to clone APFS rootfs cache image");
                let _ = std::fs::remove_file(&temporary);
            }
            if let Err(error) = self.rootfs_provider.prepare_empty(box_dir) {
                tracing::warn!(%error, "Failed to remount rootfs after caching");
            }
        }

        #[cfg(not(target_os = "macos"))]
        {
            if !self.config.cache.enabled {
                return;
            }

            let cache_dir = self.resolve_cache_dir().join("rootfs");
            let cache = match RootfsCache::new(&cache_dir) {
                Ok(c) => c,
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to open rootfs cache for storing");
                    return;
                }
            };

            match cache.put(cache_key, rootfs_path, description) {
                Ok(_) => {
                    tracing::debug!(
                        cache_key = %&cache_key[..cache_key.len().min(12)],
                        description = %description,
                        "Stored rootfs in cache"
                    );
                    // Prune if needed — but never evict a cache entry that is in use as
                    // a live overlay lower for a concurrent box (deleting the lowerdir
                    // under its mount(2) is the same-image concurrency bug this guards).
                    let protected = self.referenced_rootfs_cache_keys();
                    if let Err(e) = cache.prune_protecting(
                        self.config.cache.max_rootfs_entries,
                        self.config.cache.max_cache_bytes,
                        &protected,
                    ) {
                        tracing::warn!(error = %e, "Failed to prune rootfs cache");
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "Failed to store rootfs in cache");
                }
            }
        }
    }

    /// Record which rootfs-cache key this box holds as its overlay lower, in a
    /// `<box_dir>/.rootfs-cache-key` marker (mirror of the snapshot store's
    /// `.snapshot-lower`). Read back by [`Self::referenced_rootfs_cache_keys`] so
    /// the cache prune never evicts a live lower. Best-effort; removed with box_dir.
    fn mark_rootfs_cache_key(&self, box_dir: &Path, cache_key: &str) {
        let _ = std::fs::write(box_dir.join(".rootfs-cache-key"), cache_key);
    }

    pub(crate) fn current_rootfs_cache_key(&self) -> Result<Option<String>> {
        retained_rootfs_cache_key(&self.home_dir.join("boxes").join(&self.box_id))
    }

    /// Rootfs-cache keys currently in use as an overlay lower by some live box.
    /// Boxes live under `<home>/boxes/<id>/`; a removed box's marker is gone with
    /// its dir, so an evictable key is simply one no live box references.
    #[cfg(not(target_os = "macos"))]
    fn referenced_rootfs_cache_keys(&self) -> std::collections::HashSet<String> {
        let mut set = std::collections::HashSet::new();
        if let Ok(entries) = std::fs::read_dir(self.home_dir.join("boxes")) {
            for entry in entries.flatten() {
                if let Ok(k) = std::fs::read_to_string(entry.path().join(".rootfs-cache-key")) {
                    set.insert(k.trim().to_string());
                }
            }
        }
        set
    }

    /// Resolve the cache directory from config or default.
    pub(crate) fn resolve_cache_dir(&self) -> PathBuf {
        self.config
            .cache
            .cache_dir
            .clone()
            .unwrap_or_else(|| self.home_dir.join("cache"))
    }

    /// Mount or expose a rootfs generation retained by a managed restart or
    /// filesystem-only pause without pulling an image or starting a runtime.
    pub(crate) fn prepare_preserved_rootfs(&self) -> Result<PathBuf> {
        let box_dir = self.home_dir.join("boxes").join(&self.box_id);
        let rootfs_prepare_options = crate::rootfs::RootfsPrepareOptions {
            writable_layer_bytes: self.config.resources.ephemeral_storage_bytes,
        };
        let rootfs = box_dir.join("rootfs");
        let populated_rootfs = std::fs::read_dir(&rootfs)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false);
        let lower = if populated_rootfs {
            rootfs.clone()
        } else if let Some(snapshot_lower) = snapshot_lower_dir(&box_dir) {
            if !snapshot_lower.is_dir() {
                return Err(BoxError::StateError(format!(
                    "Retained snapshot lower is missing for {}: {}",
                    self.box_id,
                    snapshot_lower.display()
                )));
            }
            snapshot_lower
        } else if let Some(cache_key) = retained_rootfs_cache_key(&box_dir)? {
            self.try_rootfs_cache_path(&cache_key)?.ok_or_else(|| {
                BoxError::StateError(format!(
                    "Retained rootfs cache entry {cache_key} is missing for {}",
                    self.box_id
                ))
            })?
        } else if let Some(cached) = self.try_rootfs_cache_path(&RootfsCache::compute_key(
            &self.config.image,
            &[],
            &[],
            &[],
        ))? {
            cached
        } else {
            #[cfg(target_os = "macos")]
            if box_dir.join("rootfs-apfs-v2.sparseimage").is_file() {
                return self.rootfs_provider.prepare_with_options(
                    &box_dir,
                    &rootfs,
                    rootfs_prepare_options,
                );
            }
            return Err(BoxError::StateError(format!(
                "Retained rootfs lower is missing for {}",
                self.box_id
            )));
        };
        self.rootfs_provider
            .prepare_with_options(&box_dir, &lower, rootfs_prepare_options)
    }

    /// Unmount a rootfs exposed for a quiescent operation while keeping its
    /// writable generation available for a later resume.
    pub(crate) fn cleanup_preserved_rootfs(&self) -> Result<()> {
        self.rootfs_provider
            .cleanup(&self.home_dir.join("boxes").join(&self.box_id), true)
    }

    /// Generate TEE configuration file if TEE is enabled.
    #[cfg(unix)]
    pub(crate) fn generate_tee_config(&self, box_dir: &Path) -> Result<Option<TeeInstanceConfig>> {
        match &self.config.tee {
            TeeConfig::None => Ok(None),
            TeeConfig::SevSnp {
                workload_id,
                generation,
                simulate,
            } => {
                // In simulation mode, skip hardware check and TEE config
                // (the guest will generate simulated reports via A3S_TEE_SIMULATE env)
                if *simulate {
                    tracing::warn!("TEE simulation mode: skipping hardware check and TEE config");
                    return Ok(None);
                }

                // Verify hardware support
                crate::tee::require_sev_snp_support()?;

                // Generate TEE config JSON
                let config = serde_json::json!({
                    "workload_id": workload_id,
                    "cpus": self.config.resources.vcpus,
                    "ram_mib": self.config.resources.memory_mb,
                    "tee": "snp",
                    "tee_data": format!(r#"{{"gen":"{}"}}"#, generation.as_str()),
                    "attestation_url": ""
                });

                let config_path = box_dir.join("tee-config.json");
                std::fs::write(&config_path, serde_json::to_string_pretty(&config)?).map_err(
                    |e| {
                        BoxError::TeeConfig(format!(
                            "Failed to write TEE config to {}: {}",
                            config_path.display(),
                            e
                        ))
                    },
                )?;

                tracing::info!(
                    workload_id = %workload_id,
                    generation = %generation.as_str(),
                    config_path = %config_path.display(),
                    "Generated TEE configuration"
                );

                Ok(Some(TeeInstanceConfig {
                    config_path,
                    tee_type: "snp".to_string(),
                }))
            }
            TeeConfig::Tdx {
                workload_id,
                simulate,
            } => {
                if *simulate {
                    tracing::warn!("TDX simulation mode: skipping hardware check and TEE config");
                    return Ok(None);
                }

                // Intel TDX runtime support is not yet implemented.
                // The config variant exists for forward compatibility, but we
                // cannot boot a TDX VM today.
                Err(BoxError::TeeConfig(format!(
                    "Intel TDX is not yet supported at runtime (workload_id='{}'). \
                     Use tee=sev-snp or tee=none.",
                    workload_id
                )))
            }
        }
    }

    /// Generate TEE configuration file if TEE is enabled.
    #[cfg(windows)]
    pub(crate) fn generate_tee_config(&self, _box_dir: &Path) -> Result<Option<TeeInstanceConfig>> {
        match &self.config.tee {
            TeeConfig::None => Ok(None),
            _ => Err(BoxError::TeeConfig(
                "TEE configuration is not supported on Windows".to_string(),
            )),
        }
    }

    /// Find the guest init binary in common locations.
    ///
    /// Searches in order:
    /// 1. The installed `A3S_HOME/bin` asset
    /// 2. Same directory as current executable
    /// 3. target/debug or target/release (for development)
    /// 4. PATH
    ///
    /// The binary must be a Linux ELF executable since it runs inside the VM.
    pub(crate) fn find_guest_init() -> Result<PathBuf> {
        let name = "a3s-box-guest-init";
        let installed = a3s_box_core::dirs_home().join("bin").join(name);
        let candidates = Self::find_binary_candidates(name);

        if let Some(path) = Self::select_guest_init(&installed, candidates) {
            return Ok(path);
        }

        Err(BoxError::BoxBootError {
            message: "Linux guest init binary not found".to_string(),
            hint: Some(
                "Cross-compile the static guest init for your guest arch, e.g.: \
                 cargo build -p a3s-box-guest-init --release --target x86_64-unknown-linux-musl \
                 (or aarch64-unknown-linux-musl). A glibc-dynamic host build is rejected because \
                 it cannot run as PID 1 inside a minimal guest rootfs."
                    .to_string(),
            ),
        })
    }

    fn select_guest_init(installed: &Path, mut candidates: Vec<PathBuf>) -> Option<PathBuf> {
        // An explicitly installed asset belongs to the active A3S_HOME and must
        // not be shadowed by a stale development artifact in target/. Release
        // installation already puts the static guest binary at this fixed path.
        if Self::is_linux_elf(installed) {
            return Some(installed.to_path_buf());
        }
        if installed.exists() {
            tracing::debug!(
                path = %installed.display(),
                "Skipping installed guest init (not a static Linux ELF binary)"
            );
        }

        // Prefer release-owned musl-static artifacts over debug and host
        // builds. A stale cross-target debug binary must not shadow the exact
        // release guest-init that packaging and the documented build command
        // produce. On Linux, a normal workspace build may also leave a glibc
        // binary beside the host executable; that binary cannot run as PID 1
        // in a minimal rootfs.
        candidates.sort_by_key(|path| {
            let path_str = path.to_string_lossy();
            match (
                path_str.contains("-unknown-linux-musl"),
                path_str.contains("/release/"),
            ) {
                (true, true) => 0,
                (true, false) => 1,
                (false, true) => 2,
                (false, false) => 3,
            }
        });

        for path in candidates {
            if Self::is_linux_elf(&path) {
                return Some(path);
            }
            tracing::debug!(
                path = %path.display(),
                "Skipping guest init (not a Linux ELF binary)"
            );
        }

        None
    }

    /// Search common locations for a binary by name.
    fn find_binary_candidates(name: &str) -> Vec<PathBuf> {
        let mut candidates = Vec::new();

        // Try same directory as current executable
        if let Ok(exe_path) = std::env::current_exe() {
            if let Some(exe_dir) = exe_path.parent() {
                let path = exe_dir.join(name);
                if path.exists() {
                    candidates.push(path);
                }

                // Also search cross-compilation directories relative to the
                // exe's target root. When the exe is at target/debug/a3s-box,
                // cross-compiled guest binaries live at
                // target/aarch64-unknown-linux-musl/{debug,release}/.
                if let Some(target_root) = exe_dir.parent() {
                    let cross_dirs = [
                        "aarch64-unknown-linux-musl/release",
                        "aarch64-unknown-linux-musl/debug",
                        "x86_64-unknown-linux-musl/release",
                        "x86_64-unknown-linux-musl/debug",
                    ];
                    for dir in &cross_dirs {
                        let path = target_root.join(dir).join(name);
                        if path.exists() {
                            candidates.push(path);
                        }
                    }
                }
            }
        }

        // Try cross-compilation target directories relative to CWD (for development)
        let target_dirs = [
            "target/aarch64-unknown-linux-musl/release",
            "target/aarch64-unknown-linux-musl/debug",
            "target/x86_64-unknown-linux-musl/release",
            "target/x86_64-unknown-linux-musl/debug",
            "target/release",
            "target/debug",
        ];
        for dir in &target_dirs {
            let path = PathBuf::from(dir).join(name);
            if path.exists() {
                candidates.push(path);
            }
        }

        // Try PATH
        if let Ok(path_var) = std::env::var("PATH") {
            for dir in std::env::split_paths(&path_var) {
                let path = dir.join(name);
                if path.exists() {
                    candidates.push(path);
                }
            }
        }

        candidates
    }

    /// Check if a file is a Linux ELF binary suitable to run as guest PID 1.
    ///
    /// Beyond the ELF magic and OS/ABI check, this rejects *dynamically linked*
    /// ELFs (those carrying a `PT_INTERP` program header). The guest init must
    /// be a static binary: a glibc-dynamic build cannot resolve its loader/libc
    /// inside a minimal (musl/Alpine/distroless) guest rootfs and would fail to
    /// exec as PID 1. A musl static-PIE binary has no `PT_INTERP`, so it passes.
    fn is_linux_elf(path: &std::path::Path) -> bool {
        let Ok(data) = std::fs::read(path) else {
            return false;
        };
        if data.len() < 64 || data[0..4] != [0x7f, b'E', b'L', b'F'] {
            return false;
        }
        // EI_OSABI: 0x00 = System V / Linux, 0x03 = Linux.
        if !matches!(data[7], 0x00 | 0x03) {
            return false;
        }

        // Only parse program headers for the common ELF64 little-endian case
        // (x86_64/aarch64). For other classes/endianness, accept on magic+ABI
        // rather than risk a false negative on an exotic-but-valid target.
        let is_elf64 = data[4] == 2;
        let is_le = data[5] == 1;
        if !is_elf64 || !is_le {
            return true;
        }

        let u16_at = |off: usize| u16::from_le_bytes([data[off], data[off + 1]]);
        let expected_machine = match std::env::consts::ARCH {
            "x86_64" => Some(62),
            "aarch64" => Some(183),
            _ => None,
        };
        if expected_machine.is_some_and(|expected| u16_at(0x12) != expected) {
            return false;
        }
        let u64_at =
            |off: usize| u64::from_le_bytes(data[off..off + 8].try_into().unwrap_or([0; 8]));
        let e_phoff = u64_at(0x20) as usize; // program header table offset
        let e_phentsize = u16_at(0x36) as usize;
        let e_phnum = u16_at(0x38) as usize;
        if e_phoff == 0 || e_phentsize < 4 {
            return true; // no usable program headers → accept on magic+ABI
        }

        const PT_INTERP: u32 = 3;
        for i in 0..e_phnum {
            let ph = e_phoff + i * e_phentsize;
            if ph + 4 > data.len() {
                break;
            }
            let p_type = u32::from_le_bytes(data[ph..ph + 4].try_into().unwrap_or([0; 4]));
            if p_type == PT_INTERP {
                // Dynamically linked: unsafe as guest PID 1.
                return false;
            }
        }
        true
    }
}

mod cache;
use cache::*;

#[cfg(test)]
#[path = "layout/tests.rs"]
mod tests;
