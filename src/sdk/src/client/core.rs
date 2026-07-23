impl A3sBoxClient {
    /// List box records from the shared state file.
    pub fn list_boxes(&self, options: ListBoxesOptions) -> Result<Vec<BoxSummary>> {
        let state = self.load_state()?;
        Ok(state
            .list(options.all)
            .into_iter()
            .map(BoxSummary::from_record)
            .collect())
    }

    /// Get one box by exact id, short id, id prefix, or name.
    pub fn get_box(&self, query: &str) -> Result<Option<BoxSummary>> {
        let state = self.load_state()?;
        Ok(resolve_record(&state, query)
            .transpose()?
            .map(BoxSummary::from_record))
    }

    /// Remove a box record and its host-side runtime resources.
    ///
    /// By default active boxes are rejected. Pass [`RemoveBox::force`] to mirror
    /// CLI-style forced removal, which only signals a recorded PID after the
    /// PID identity check still matches the original box process.
    pub fn remove_box(&self, query: &str, request: RemoveBox) -> Result<RemoveBoxSummary> {
        let state = self.load_state()?;
        let record = resolve_required_record(&state, query)?.clone();

        if record.is_active() {
            if !request.force {
                return Err(ClientError::Validation(format!(
                    "box {} is {}. Stop it before removing it, or force removal explicitly.",
                    record.name, record.status
                )));
            }

            terminate_recorded_process(&record);
        }

        let id = record.id.clone();
        let name = record.name.clone();
        cleanup_removed_box(&self.paths, &record);
        let removed =
            StateFile::modify(&self.paths.boxes_file, |state| Ok(state.remove_by_id(&id)))?;
        if !removed {
            return Err(ClientError::BoxNotFound(id));
        }

        Ok(RemoveBoxSummary { id, name })
    }

    /// Remove all created, stopped, and dead boxes from SDK-managed state.
    ///
    /// Running and paused boxes are kept. Host-side runtime resources for each
    /// removed box are cleaned up after the records are removed under the state
    /// lock, so concurrent readers no longer observe pruned boxes.
    pub fn prune_boxes(&self) -> Result<Vec<RemoveBoxSummary>> {
        let records = StateFile::modify(&self.paths.boxes_file, |state| {
            let records = state
                .list(true)
                .into_iter()
                .filter(|record| is_prunable_box_record(record))
                .cloned()
                .collect::<Vec<_>>();
            for record in &records {
                state.remove_by_id(&record.id);
            }
            Ok(records)
        })?;

        for record in &records {
            cleanup_removed_box(&self.paths, record);
        }

        Ok(records
            .into_iter()
            .map(|record| RemoveBoxSummary {
                id: record.id,
                name: record.name,
            })
            .collect())
    }

    /// Pause a running box by stopping its host shim process and updating state.
    ///
    /// This mirrors the CLI's pause semantics for the direct SDK surface: only a
    /// currently running box can be paused, and stale or reused PIDs are rejected
    /// before any signal is sent.
    pub fn pause_box(&self, query: &str) -> Result<BoxSummary> {
        self.signal_box_status_transition(query, LifecycleTransition::Pause)
    }

    /// Resume a paused box by continuing its host shim process and updating state.
    pub fn unpause_box(&self, query: &str) -> Result<BoxSummary> {
        self.signal_box_status_transition(query, LifecycleTransition::Unpause)
    }

    /// Stop a running or paused box with guest-first graceful shutdown.
    #[cfg(unix)]
    pub async fn stop_box(&self, query: &str, request: StopBox) -> Result<StopBoxSummary> {
        let state = self.load_state()?;
        let record = resolve_required_record(&state, query)?.clone();
        require_active(&record, "stop")?;
        let pid = require_live_pid(&record, "stop")?;
        if record.status == "paused" {
            send_host_signal(pid, libc::SIGCONT)
                .map_err(|error| ClientError::Validation(error.to_string()))?;
        }

        let stop_signal = record
            .stop_signal
            .as_deref()
            .map(parse_signal_name)
            .unwrap_or(libc::SIGTERM);
        let timeout_secs = request.timeout_secs.or(record.stop_timeout).unwrap_or(10);
        let outcome =
            graceful_stop_via_guest(pid, &exec_socket(&record), stop_signal, timeout_secs).await;
        let exit_code = stopped_exit_code(record.exit_code, outcome, stop_signal);
        let record_id = record.id.clone();
        let auto_removed = record.auto_remove;
        let name = record.name.clone();

        if auto_removed {
            cleanup_removed_box(&self.paths, &record);
            StateFile::modify(&self.paths.boxes_file, |state| {
                state.remove_by_id(&record_id);
                Ok(())
            })?;
            return Ok(StopBoxSummary {
                id: record_id,
                name,
                outcome,
                exit_code,
                auto_removed: true,
                box_summary: None,
            });
        }

        cleanup_stopped_box(&self.paths, &record);
        let box_summary = StateFile::modify(&self.paths.boxes_file, |state| {
            let Some(record) = state.find_by_id_mut(&record_id) else {
                return Ok(None);
            };
            record.status = "stopped".to_string();
            record.pid = None;
            record.stopped_by_user = true;
            record.exit_code = exit_code;
            record.health_status = "none".to_string();
            record.health_retries = 0;
            Ok(Some(BoxSummary::from_record(record)))
        })?
        .ok_or_else(|| ClientError::BoxNotFound(record_id.clone()))?;

        Ok(StopBoxSummary {
            id: record_id,
            name,
            outcome,
            exit_code,
            auto_removed: false,
            box_summary: Some(box_summary),
        })
    }

    /// Read recent logs for one box from the runtime log files.
    ///
    /// The SDK follows the same source preference as the CLI: structured
    /// `logs/container.json` first, then the raw console log as a fallback. This
    /// method is a bounded snapshot reader; it does not follow live output.
    pub fn read_box_logs(
        &self,
        query: &str,
        options: ReadBoxLogsOptions,
    ) -> Result<Vec<BoxLogLine>> {
        let state = self.load_state()?;
        let record = resolve_required_record(&state, query)?;

        if record.log_config.driver == LogDriver::None {
            return Err(ClientError::Validation(format!(
                "logging is disabled for box {}",
                record.name
            )));
        }

        let Some(source) = resolve_log_source(record) else {
            return Ok(Vec::new());
        };

        read_log_source(source, options.tail)
    }

    /// Collect host-side resource usage snapshots for all active boxes.
    ///
    /// CPU and memory are read from the recorded shim process. Network counters
    /// are read from the runtime netproxy stats file when it exists. This is a
    /// bounded snapshot reader; it does not stream and does not exec into the
    /// guest.
    pub fn list_box_stats(&self) -> Result<Vec<BoxStatsSummary>> {
        let state = self.load_state()?;
        let records = state
            .list(true)
            .into_iter()
            .filter(|record| record.is_active())
            .collect::<Vec<_>>();
        Ok(collect_box_stats(&records))
    }

    /// Collect one host-side resource usage snapshot by exact id, short id,
    /// id prefix, or name. Returns `None` when the box is not active or its host
    /// process is no longer available.
    pub fn get_box_stats(&self, query: &str) -> Result<Option<BoxStatsSummary>> {
        let state = self.load_state()?;
        let record = resolve_required_record(&state, query)?;
        if !record.is_active() {
            return Ok(None);
        }
        Ok(collect_box_stats(&[record]).into_iter().next())
    }

    /// List cached images from the runtime image store.
    pub async fn list_images(&self) -> Result<Vec<ImageSummary>> {
        let store = self.open_image_store()?;
        let mut images = store
            .list()
            .await
            .into_iter()
            .map(ImageSummary::from)
            .collect::<Vec<_>>();
        images.sort_by(|a, b| a.reference.cmp(&b.reference));
        Ok(images)
    }

    /// Resolve one image by reference or digest.
    pub async fn get_image(&self, reference_or_digest: &str) -> Result<Option<ImageSummary>> {
        let store = self.open_image_store()?;
        let images = store.list().await;
        match resolve_stored_image(&images, reference_or_digest)? {
            Some(image) => Ok(store
                .get(&image.reference)
                .await
                .or(Some(image))
                .map(ImageSummary::from)),
            None => Ok(None),
        }
    }

    /// Inspect one cached image's local OCI configuration.
    pub async fn inspect_image(
        &self,
        reference_or_digest: &str,
    ) -> Result<Option<ImageInspectSummary>> {
        let store = self.open_image_store()?;
        let images = store.list().await;
        let Some(image) = resolve_stored_image(&images, reference_or_digest)? else {
            return Ok(None);
        };
        Ok(Some(ImageInspectSummary::from_stored_image(image)?))
    }

    /// Read one cached image's OCI build history.
    pub async fn image_history(
        &self,
        reference_or_digest: &str,
    ) -> Result<Option<Vec<ImageHistoryEntry>>> {
        let store = self.open_image_store()?;
        let images = store.list().await;
        let Some(image) = resolve_stored_image(&images, reference_or_digest)? else {
            return Ok(None);
        };
        Ok(Some(load_image_history(&image.path)?))
    }

    /// Add a new tag pointing at an existing cached image.
    pub async fn tag_image(&self, request: TagImage) -> Result<ImageSummary> {
        request.validate()?;
        let store = self.open_image_store()?;
        let images = store.list().await;
        let source = resolve_stored_image(&images, &request.source)?.ok_or_else(|| {
            ClientError::Validation(format!("image '{}' is not cached", request.source))
        })?;
        Ok(ImageSummary::from(
            store
                .put(&request.target, &source.digest, &source.path)
                .await?,
        ))
    }

    /// Remove one cached image by reference or digest.
    pub async fn remove_image(&self, reference_or_digest: &str) -> Result<()> {
        self.open_image_store()?
            .remove(reference_or_digest)
            .await
            .map_err(ClientError::Runtime)
    }

    /// Evict least-recently-used images until the image cache is under its limit.
    pub async fn evict_images(&self) -> Result<Vec<String>> {
        Ok(self.open_image_store()?.evict().await?)
    }

    /// Pull an OCI image through the runtime image puller and cache it locally.
    pub async fn pull_image(&self, request: PullImage) -> Result<ImageSummary> {
        request.validate()?;
        let store = Arc::new(self.open_image_store()?);
        let auth = request.registry_auth()?;
        let puller = ImagePuller::with_platform(store, auth, request.platform.clone())
            .with_signature_policy(request.signature_policy.clone());

        if request.force {
            puller.force_pull(&request.reference).await?;
        } else {
            puller.pull(&request.reference).await?;
        }

        self.get_image(&request.reference).await?.ok_or_else(|| {
            ClientError::Validation(format!("image '{}' was not cached", request.reference))
        })
    }

    /// Build an OCI image with the runtime Dockerfile build engine.
    pub async fn build_image(&self, request: BuildImage) -> Result<BuildImageSummary> {
        request.validate()?;
        let store = Arc::new(self.open_image_store()?);
        let result = a3s_box_runtime::oci::build::build(
            RuntimeBuildConfig {
                context_dir: request.context_dir,
                dockerfile_path: request.dockerfile_path,
                tag: request.tag,
                build_args: request.build_args,
                quiet: request.quiet,
                platforms: request.platforms,
                target: request.target,
                no_cache: request.no_cache,
                metrics: None,
                run_pool: None,
            },
            store,
        )
        .await?;

        Ok(BuildImageSummary::from(result))
    }

    /// Push a locally cached image through the runtime registry pusher.
    pub async fn push_image(&self, request: PushImage) -> Result<PushImageSummary> {
        request.validate()?;
        let image = self.get_image(&request.source).await?.ok_or_else(|| {
            ClientError::Validation(format!("image '{}' is not cached", request.source))
        })?;
        let target = ImageReference::parse(&request.target).map_err(ClientError::Runtime)?;
        let auth = request.registry_auth(&target);
        let result = RegistryPusher::with_auth_and_protocol(auth, request.registry_protocol)
            .push(&target, &image.path)
            .await?;

        Ok(PushImageSummary::from_push_result(request.target, result))
    }

    /// List named volumes from the runtime volume store.
    pub fn list_volumes(&self) -> Result<Vec<VolumeSummary>> {
        let store = self.volume_store();
        let mut volumes = store
            .list()?
            .into_iter()
            .map(VolumeSummary::from)
            .collect::<Vec<_>>();
        volumes.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(volumes)
    }

    /// Get one named volume.
    pub fn get_volume(&self, name: &str) -> Result<Option<VolumeSummary>> {
        Ok(self.volume_store().get(name)?.map(VolumeSummary::from))
    }

    /// Create a named volume.
    pub fn create_volume(&self, request: CreateVolume) -> Result<VolumeSummary> {
        request.validate()?;
        let mut config = VolumeConfig::new(&request.name, "");
        config.driver = request.driver;
        config.labels = request.labels;
        config.size_limit = request.size_limit;
        Ok(VolumeSummary::from(self.volume_store().create(config)?))
    }

    /// Remove a named volume.
    pub fn remove_volume(&self, name: &str, force: bool) -> Result<VolumeSummary> {
        Ok(VolumeSummary::from(
            self.volume_store().remove(name, force)?,
        ))
    }

    /// Remove all unused named volumes.
    pub fn prune_volumes(&self) -> Result<Vec<String>> {
        Ok(self.volume_store().prune()?)
    }

    /// List networks from the runtime network store.
    pub fn list_networks(&self) -> Result<Vec<NetworkSummary>> {
        let store = self.network_store();
        let mut networks = store
            .list()?
            .into_iter()
            .map(NetworkSummary::from)
            .collect::<Vec<_>>();
        networks.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(networks)
    }

    /// Get one network.
    pub fn get_network(&self, name: &str) -> Result<Option<NetworkSummary>> {
        Ok(self.network_store().get(name)?.map(NetworkSummary::from))
    }

    /// Create a bridge network.
    pub fn create_network(&self, request: CreateNetwork) -> Result<NetworkSummary> {
        request.validate()?;
        let mut config =
            NetworkConfig::new(&request.name, &request.subnet).map_err(ClientError::Validation)?;
        config.driver = request.driver;
        config.labels = request.labels;
        config.policy.isolation = request.isolation;
        config.policy.validate().map_err(ClientError::Validation)?;

        let store = self.network_store();
        store.create(config)?;
        store
            .get(&request.name)?
            .map(NetworkSummary::from)
            .ok_or_else(|| {
                ClientError::Validation(format!("network '{}' was not saved", request.name))
            })
    }

    /// Remove a network. The runtime rejects networks that still have endpoints.
    pub fn remove_network(&self, name: &str) -> Result<NetworkSummary> {
        Ok(NetworkSummary::from(self.network_store().remove(name)?))
    }

    /// Remove all unused non-predefined networks.
    ///
    /// A network is unused when it has no endpoints and no box record references
    /// it by `network_name` or bridge `network_mode`. Docker-style predefined
    /// networks (`bridge`, `host`, `none`) are never pruned.
    pub fn prune_networks(&self) -> Result<Vec<String>> {
        let state = self.load_state()?;
        let in_use = state
            .list(true)
            .into_iter()
            .filter_map(record_network_name)
            .map(str::to_string)
            .collect::<std::collections::HashSet<_>>();

        let mut networks = self.network_store().list()?;
        networks.sort_by(|a, b| a.name.cmp(&b.name));

        let mut removed = Vec::new();
        for network in networks {
            if is_predefined_network(&network.name)
                || !network.endpoints.is_empty()
                || in_use.contains(&network.name)
            {
                continue;
            }
            self.network_store().remove(&network.name)?;
            removed.push(network.name);
        }
        Ok(removed)
    }

    /// Attach an inactive box record to a network and allocate an endpoint.
    ///
    /// Hot-plug for active boxes is not supported by the runtime yet, so this
    /// mirrors the CLI rule: stop the box before changing its network.
    pub fn connect_network(
        &self,
        network: &str,
        box_query: &str,
    ) -> Result<NetworkEndpointSummary> {
        let mut state = self.load_state()?;
        let record = resolve_required_record(&state, box_query)?.clone();
        require_inactive_for_network_change(&record, "connect to a network")?;

        let endpoint = self.network_store().with_write_lock(
            |networks| -> std::result::Result<NetworkEndpoint, a3s_box_core::error::BoxError> {
                let config = networks.get_mut(network).ok_or_else(|| {
                    a3s_box_core::error::BoxError::NetworkError(format!(
                        "network '{}' not found",
                        network
                    ))
                })?;
                config
                    .policy
                    .validate()
                    .map_err(a3s_box_core::error::BoxError::NetworkError)?;
                config
                    .connect(&record.id, &record.name)
                    .map_err(a3s_box_core::error::BoxError::NetworkError)
            },
        )?;

        let state_record = state
            .find_by_id_mut(&record.id)
            .ok_or_else(|| ClientError::BoxNotFound(box_query.to_string()))?;
        state_record.network_mode = NetworkMode::Bridge {
            network: network.to_string(),
        };
        state_record.network_name = Some(network.to_string());
        state.save()?;

        Ok(NetworkEndpointSummary::from(endpoint))
    }

    /// Detach an inactive box record from a network.
    pub fn disconnect_network(
        &self,
        network: &str,
        box_query: &str,
    ) -> Result<NetworkEndpointSummary> {
        let mut state = self.load_state()?;
        let record = resolve_required_record(&state, box_query)?.clone();
        require_inactive_for_network_change(&record, "disconnect from a network")?;

        let endpoint = self.network_store().with_write_lock(
            |networks| -> std::result::Result<NetworkEndpoint, a3s_box_core::error::BoxError> {
                let config = networks.get_mut(network).ok_or_else(|| {
                    a3s_box_core::error::BoxError::NetworkError(format!(
                        "network '{}' not found",
                        network
                    ))
                })?;
                config
                    .disconnect(&record.id)
                    .map_err(a3s_box_core::error::BoxError::NetworkError)
            },
        )?;

        let state_record = state
            .find_by_id_mut(&record.id)
            .ok_or_else(|| ClientError::BoxNotFound(box_query.to_string()))?;
        state_record.network_mode = NetworkMode::Tsi;
        state_record.network_name = None;
        state.save()?;

        Ok(NetworkEndpointSummary::from(endpoint))
    }

    /// List VM snapshots from the runtime snapshot store.
    pub fn list_snapshots(&self) -> Result<Vec<SnapshotSummary>> {
        Ok(self
            .snapshot_store()?
            .list()?
            .into_iter()
            .map(SnapshotSummary::from)
            .collect())
    }

    /// Get one VM snapshot by id.
    pub fn get_snapshot(&self, id: &str) -> Result<Option<SnapshotSummary>> {
        Ok(self.snapshot_store()?.get(id)?.map(SnapshotSummary::from))
    }

    /// Remove one VM snapshot by id.
    pub fn remove_snapshot(&self, id: &str) -> Result<bool> {
        validate_name("snapshot", id)?;
        Ok(self.snapshot_store()?.delete(id)?)
    }

    /// Create a VM snapshot from a stopped box's on-disk root filesystem.
    pub fn create_snapshot(
        &self,
        box_query: &str,
        request: CreateSnapshot,
    ) -> Result<SnapshotSummary> {
        request.validate()?;
        let initial_state = self.load_state()?;
        let record_id = resolve_required_record(&initial_state, box_query)?.id.clone();
        let _lifecycle_lock = a3s_box_runtime::acquire_execution_lifecycle_lock(
            &self.paths.home,
            &record_id,
        )?;
        let state = self.load_state()?;
        let record = state
            .find_by_id(&record_id)
            .cloned()
            .ok_or_else(|| ClientError::BoxNotFound(record_id.clone()))?;
        if record.is_active() {
            return Err(ClientError::Validation(format!(
                "cannot snapshot active box {}: stop it first; live host-path snapshots are disabled because a running guest can race filesystem traversal",
                record.name
            )));
        }
        let rootfs_path = resolve_box_rootfs(&record.box_dir).ok_or_else(|| {
            ClientError::Validation(format!(
                "rootfs not found for box {} under {} (looked for merged/ and rootfs/)",
                record.name,
                record.box_dir.display()
            ))
        })?;
        let snapshot_id = format!(
            "snap-{}",
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        );
        let snapshot_name = request
            .name
            .unwrap_or_else(|| format!("{}-snapshot", record.name));

        let mut metadata = SnapshotMetadata::new(
            snapshot_id,
            snapshot_name,
            record.id.clone(),
            record.image.clone(),
        );
        metadata.vcpus = record.cpus;
        metadata.memory_mb = record.memory_mb;
        metadata.volumes = record.volumes.clone();
        metadata.env = record.env.clone();
        metadata.cmd = record.cmd.clone();
        metadata.entrypoint = record.entrypoint.clone();
        metadata.workdir = record.workdir.clone();
        metadata.port_map = record.port_map.clone();
        metadata.labels = record.labels.clone();
        metadata.network_mode = Some(record.network_mode.to_string());
        metadata.description = request.description.unwrap_or_default();
        metadata.health_check = record.health_check.clone();
        metadata.healthcheck_disabled = record.healthcheck_disabled;
        metadata.image_config = load_resolved_image_config(&record.box_dir)?;
        if metadata.image_config.is_none() {
            return Err(ClientError::Validation(format!(
                "resolved image configuration is missing for box {}; restart it before creating a filesystem snapshot",
                record.name
            )));
        }

        Ok(SnapshotSummary::from(
            self.snapshot_store()?.save(metadata, &rootfs_path)?,
        ))
    }

    /// Restore a snapshot into a new, created box record.
    pub fn restore_snapshot(
        &self,
        snapshot_query: &str,
        request: RestoreSnapshot,
    ) -> Result<BoxSummary> {
        request.validate()?;
        let store = self.snapshot_store()?;
        let metadata = resolve_snapshot_metadata(&store, snapshot_query)?;
        metadata
            .require_image_config()
            .map_err(|error| ClientError::Validation(error.to_string()))?;
        #[cfg(windows)]
        if metadata.has_effective_health_check() {
            return Err(ClientError::Validation(
                "container health checks are not supported on Windows; the snapshot defines an effective health check"
                    .to_string(),
            ));
        }
        let state = self.load_state()?;
        let box_name = request
            .name
            .unwrap_or_else(|| default_restored_box_name(&state, &metadata));
        validate_name("box", &box_name)?;
        if state.find_by_name(&box_name).is_some() {
            return Err(ClientError::Validation(format!(
                "box name '{}' already exists",
                box_name
            )));
        }

        let snap_rootfs = store.rootfs_path(&metadata.id);
        if !snap_rootfs.exists() {
            return Err(ClientError::Validation(format!(
                "snapshot rootfs is missing for snapshot {}",
                metadata.id
            )));
        }

        let box_id = uuid::Uuid::new_v4().to_string();
        let short_id = BoxRecord::make_short_id(&box_id);
        let box_dir = self.paths.home.join("boxes").join(&box_id);
        let socket_dir = box_dir.join("sockets");
        let logs_dir = box_dir.join("logs");
        let mut box_dir_guard = BoxDirGuard::new(box_dir.clone());
        std::fs::create_dir_all(&socket_dir)?;
        std::fs::create_dir_all(&logs_dir)?;
        std::fs::write(
            box_dir.join(".snapshot-lower"),
            snap_rootfs.to_string_lossy().as_bytes(),
        )?;

        let record = BoxRecord {
            id: box_id,
            short_id,
            name: box_name,
            image: metadata.image.clone(),
            isolation: Default::default(),
            managed_execution: None,
            status: "created".to_string(),
            pid: None,
            pid_start_time: None,
            cpus: metadata.vcpus,
            memory_mb: metadata.memory_mb,
            volumes: metadata.volumes.clone(),
            virtiofs_cache: None,
            env: metadata.env.clone(),
            cmd: metadata.cmd.clone(),
            entrypoint: metadata.entrypoint.clone(),
            box_dir: box_dir.clone(),
            exec_socket_path: socket_dir.join("exec.sock"),
            console_log: logs_dir.join("console.log"),
            created_at: chrono::Utc::now(),
            started_at: None,
            auto_remove: false,
            hostname: None,
            user: None,
            workdir: metadata.workdir.clone(),
            restart_policy: "no".to_string(),
            port_map: metadata.port_map.clone(),
            labels: metadata.labels.clone(),
            stopped_by_user: false,
            restart_count: 0,
            max_restart_count: 0,
            exit_code: None,
            health_check: metadata.health_check.clone(),
            healthcheck_disabled: metadata.healthcheck_disabled,
            health_status: "none".to_string(),
            health_retries: 0,
            health_last_check: None,
            network_mode: NetworkMode::default(),
            network_name: None,
            volume_names: vec![],
            tmpfs: vec![],
            anonymous_volumes: vec![],
            resource_limits: a3s_box_core::config::ResourceLimits::default(),
            log_config: a3s_box_core::log::LogConfig::default(),
            add_host: vec![],
            platform: None,
            init: false,
            read_only: false,
            cap_add: vec![],
            cap_drop: vec![],
            security_opt: vec![],
            privileged: false,
            devices: vec![],
            gpus: None,
            shm_size: None,
            stop_signal: None,
            stop_timeout: None,
            oom_kill_disable: false,
            oom_score_adj: None,
        };
        let summary = BoxSummary::from_record(&record);
        let registered = StateFile::modify(&self.paths.boxes_file, |state| {
            if state.find_by_name(&summary.name).is_some() {
                return Ok(false);
            }
            state.records_mut().push(record);
            Ok(true)
        })?;
        if !registered {
            return Err(ClientError::Validation(format!(
                "box name '{}' already exists",
                summary.name
            )));
        }
        box_dir_guard.disarm();
        Ok(summary)
    }

    /// Prune old snapshots according to count and byte limits.
    ///
    /// A value of 0 means unlimited for each limit, matching the runtime store.
    pub fn prune_snapshots(&self, max_count: usize, max_bytes: u64) -> Result<Vec<String>> {
        Ok(self.snapshot_store()?.prune(max_count, max_bytes)?)
    }

    /// Execute a command in a running box through the runtime exec client.
    #[cfg(unix)]
    pub async fn exec_box(&self, query: &str, request: &ExecRequest) -> Result<ExecOutput> {
        Ok(self.exec_client(query).await?.exec_command(request).await?)
    }

    /// Execute a command through the generation-fenced managed session facade.
    #[cfg(unix)]
    pub async fn execute_execution(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request: ExecRequest,
    ) -> Result<ExecOutput> {
        let manager = self.execution_session_manager.as_ref().ok_or_else(|| {
            ClientError::Validation(
                "this client was constructed without an execution session manager".to_string(),
            )
        })?;
        Ok(manager.execute(execution_id, generation, request).await?)
    }

    /// Transfer a file to or from a running box through the runtime exec client.
    #[cfg(unix)]
    pub async fn transfer_box_file(
        &self,
        query: &str,
        request: &FileRequest,
    ) -> Result<FileResponse> {
        Ok(self
            .exec_client(query)
            .await?
            .file_transfer(request)
            .await?)
    }

    /// Transfer a file through the generation-fenced managed session facade.
    #[cfg(unix)]
    pub async fn transfer_execution_file(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request: FileRequest,
    ) -> Result<FileResponse> {
        let manager = self.execution_session_manager.as_ref().ok_or_else(|| {
            ClientError::Validation(
                "this client was constructed without an execution session manager".to_string(),
            )
        })?;
        Ok(manager
            .transfer_file(execution_id, generation, request)
            .await?)
    }

    /// Access filesystem metadata through the generation-fenced session facade.
    #[cfg(unix)]
    pub async fn filesystem_execution(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        request: FilesystemRequest,
    ) -> Result<FilesystemResponse> {
        let manager = self.execution_session_manager.as_ref().ok_or_else(|| {
            ClientError::Validation(
                "this client was constructed without an execution session manager".to_string(),
            )
        })?;
        Ok(manager
            .filesystem(execution_id, generation, request)
            .await?)
    }

    /// Check whether a running box's exec server responds to heartbeat.
    #[cfg(unix)]
    pub async fn heartbeat_box(&self, query: &str) -> Result<bool> {
        Ok(self.exec_client(query).await?.heartbeat().await?)
    }

    /// Ask the guest to deliver a signal to the main process.
    #[cfg(unix)]
    pub async fn signal_box_main(&self, query: &str, signal: i32) -> Result<bool> {
        Ok(self.exec_client(query).await?.signal_main(signal).await?)
    }

    /// Ask a deferred-main guest to spawn its configured main process.
    #[cfg(unix)]
    pub async fn spawn_box_main(&self, query: &str, spec_json: Option<&[u8]>) -> Result<bool> {
        Ok(self.exec_client(query).await?.spawn_main(spec_json).await?)
    }

    /// Open the runtime exec client for a running box.
    #[cfg(unix)]
    pub async fn exec_client(&self, query: &str) -> Result<ExecClient> {
        let socket = self.require_runtime_socket(query, RuntimeSocket::Exec)?;
        Ok(ExecClient::connect(&socket).await?)
    }

    /// Open the runtime PTY client for a running box.
    #[cfg(unix)]
    pub async fn pty_client(&self, query: &str) -> Result<PtyClient> {
        let socket = self.require_runtime_socket(query, RuntimeSocket::Pty)?;
        Ok(PtyClient::connect(&socket).await?)
    }

    /// Request a raw attestation report through the runtime attestation client.
    #[cfg(unix)]
    pub async fn attestation_report(
        &self,
        query: &str,
        request: &AttestationRequest,
    ) -> Result<AttestationReport> {
        let socket = self.require_runtime_socket(query, RuntimeSocket::Attest)?;
        Ok(a3s_box_runtime::AttestationClient::connect(&socket)
            .await?
            .get_report(request)
            .await?)
    }

    fn load_state(&self) -> Result<StateFile> {
        Ok(StateFile::load(&self.paths.boxes_file)?)
    }

    fn signal_box_status_transition(
        &self,
        query: &str,
        transition: LifecycleTransition,
    ) -> Result<BoxSummary> {
        let state = self.load_state()?;
        let record = resolve_required_record(&state, query)?.clone();
        transition.validate_status(&record)?;
        let pid = require_live_pid(&record, transition.action())?;
        send_host_signal(pid, transition.signal())
            .map_err(|error| ClientError::Validation(error.to_string()))?;

        let record_id = record.id.clone();
        let updated = StateFile::modify(&self.paths.boxes_file, |state| {
            let Some(record) = state.find_by_id_mut(&record_id) else {
                return Ok(None);
            };
            record.status = transition.target_status().to_string();
            Ok(Some(BoxSummary::from_record(record)))
        })?;

        updated.ok_or(ClientError::BoxNotFound(record_id))
    }

    /// Open the runtime image store rooted at this client's state paths.
    pub fn open_image_store(&self) -> Result<ImageStore> {
        Ok(ImageStore::new(
            &self.paths.images_dir,
            self.image_cache_size,
        )?)
    }

    /// Open the runtime volume store rooted at this client's state paths.
    pub fn volume_store(&self) -> VolumeStore {
        VolumeStore::new(&self.paths.volumes_file, &self.paths.volumes_dir)
    }

    /// Open the runtime network store rooted at this client's state paths.
    pub fn network_store(&self) -> NetworkStore {
        NetworkStore::new(&self.paths.networks_file)
    }

    /// Open the runtime snapshot store rooted at this client's state paths.
    pub fn snapshot_store(&self) -> Result<SnapshotStore> {
        Ok(SnapshotStore::new(&self.paths.snapshots_dir)?)
    }

    #[cfg(unix)]
    fn require_runtime_socket(&self, query: &str, socket: RuntimeSocket) -> Result<PathBuf> {
        let state = self.load_state()?;
        let record = resolve_required_record(&state, query)?;
        require_running(record, socket.action())?;
        let path = runtime_socket(record, socket);
        if path.exists() {
            return Ok(path);
        }

        Err(ClientError::Validation(format!(
            "{} socket is missing for running box {} at {}. The box state may be stale or the guest control channel is not ready; reconcile state and restart the box if the socket is still missing.",
            socket.label(),
            record.name,
            path.display(),
        )))
    }
}
