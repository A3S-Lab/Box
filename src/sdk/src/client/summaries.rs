/// Typed box summary for management UIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxSummary {
    pub id: String,
    pub short_id: String,
    pub name: String,
    pub image: String,
    pub status: String,
    pub status_summary: String,
    pub active: bool,
    pub pid: Option<u32>,
    pub cpus: u32,
    pub memory_mb: u32,
    pub ports: Vec<String>,
    pub command: Vec<String>,
    pub health: String,
    pub labels: HashMap<String, String>,
    pub created_at: String,
    pub started_at: Option<String>,
    pub network_name: Option<String>,
    pub volume_names: Vec<String>,
}

impl BoxSummary {
    fn from_record(record: &BoxRecord) -> Self {
        Self {
            id: record.id.clone(),
            short_id: record.short_id.clone(),
            name: record.name.clone(),
            image: record.image.clone(),
            status: record.status.clone(),
            status_summary: record.status_summary(),
            active: record.is_active(),
            pid: record.pid,
            cpus: record.cpus,
            memory_mb: record.memory_mb,
            ports: record.port_map.clone(),
            command: record.cmd.clone(),
            health: record.health_status.clone(),
            labels: record.labels.clone(),
            created_at: record.created_at.to_rfc3339(),
            started_at: record.started_at.map(|ts| ts.to_rfc3339()),
            network_name: record.network_name.clone(),
            volume_names: record.volume_names.clone(),
        }
    }
}

/// One decoded box log line.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoxLogLine {
    pub stream: String,
    pub timestamp: Option<String>,
    pub message: String,
}

/// Host-side resource usage snapshot for one active box.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct BoxStatsSummary {
    pub id: String,
    pub short_id: String,
    pub name: String,
    pub status: String,
    pub pid: u32,
    pub cpus: u32,
    pub cpu_percent: f32,
    pub cpu_percent_scaled: f64,
    pub memory_bytes: u64,
    pub memory_limit_bytes: u64,
    pub memory_percent: f64,
    pub network_rx_bytes: u64,
    pub network_tx_bytes: u64,
    pub block_read_bytes: u64,
    pub block_write_bytes: u64,
}

/// Local runtime diagnostics suitable for status bars and diagnostics panes.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeDiagnostics {
    pub core_version: String,
    pub runtime_version: String,
    pub sdk_version: String,
    pub home: PathBuf,
    pub virtualization: RuntimeVirtualizationSummary,
}

impl RuntimeDiagnostics {
    fn collect(paths: &A3sBoxPaths) -> Self {
        Self {
            core_version: a3s_box_core::VERSION.to_string(),
            runtime_version: a3s_box_runtime::VERSION.to_string(),
            sdk_version: env!("CARGO_PKG_VERSION").to_string(),
            home: paths.home.clone(),
            virtualization: RuntimeVirtualizationSummary::collect(),
        }
    }
}

/// Local disk usage grouped by runtime-owned state areas.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeDiskUsage {
    pub home: PathBuf,
    pub total_bytes: u64,
    pub boxes_bytes: u64,
    pub images_bytes: u64,
    pub volumes_bytes: u64,
    pub snapshots_bytes: u64,
    pub state_bytes: u64,
    pub other_bytes: u64,
}

impl RuntimeDiskUsage {
    fn collect(paths: &A3sBoxPaths) -> Result<Self> {
        let boxes_dir = paths.home.join("boxes");
        let boxes_bytes = disk_usage_path(&boxes_dir)?;
        let images_bytes = disk_usage_path(&paths.images_dir)?;
        let volumes_bytes = disk_usage_path(&paths.volumes_dir)?;
        let snapshots_bytes = disk_usage_path(&paths.snapshots_dir)?;
        let state_bytes = disk_usage_paths(&[
            paths.boxes_file.as_path(),
            paths.volumes_file.as_path(),
            paths.networks_file.as_path(),
        ])?;

        let known_bytes = boxes_bytes
            .saturating_add(images_bytes)
            .saturating_add(volumes_bytes)
            .saturating_add(snapshots_bytes)
            .saturating_add(state_bytes);
        let known_home_bytes = [
            (boxes_dir.as_path(), boxes_bytes),
            (paths.images_dir.as_path(), images_bytes),
            (paths.volumes_dir.as_path(), volumes_bytes),
            (paths.snapshots_dir.as_path(), snapshots_bytes),
            (
                paths.boxes_file.as_path(),
                file_size_or_zero(&paths.boxes_file)?,
            ),
            (
                paths.volumes_file.as_path(),
                file_size_or_zero(&paths.volumes_file)?,
            ),
            (
                paths.networks_file.as_path(),
                file_size_or_zero(&paths.networks_file)?,
            ),
        ]
        .into_iter()
        .filter(|(path, _)| path.starts_with(&paths.home))
        .map(|(_, bytes)| bytes)
        .fold(0u64, u64::saturating_add);
        let home_bytes = disk_usage_path(&paths.home)?;
        let other_bytes = home_bytes.saturating_sub(known_home_bytes);

        Ok(Self {
            home: paths.home.clone(),
            total_bytes: known_bytes.saturating_add(other_bytes),
            boxes_bytes,
            images_bytes,
            volumes_bytes,
            snapshots_bytes,
            state_bytes,
            other_bytes,
        })
    }
}

/// Host virtualization status reported by the runtime support checker.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RuntimeVirtualizationSummary {
    pub available: bool,
    pub backend: Option<String>,
    pub details: String,
}

impl RuntimeVirtualizationSummary {
    fn collect() -> Self {
        match a3s_box_runtime::check_virtualization_support() {
            Ok(support) => Self {
                available: true,
                backend: Some(support.backend),
                details: support.details,
            },
            Err(error) => Self {
                available: false,
                backend: None,
                details: error.to_string(),
            },
        }
    }
}

/// Typed image summary for management UIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageSummary {
    pub reference: String,
    pub digest: String,
    pub size_bytes: u64,
    pub pulled_at: String,
    pub last_used: String,
    pub path: PathBuf,
}

impl From<StoredImage> for ImageSummary {
    fn from(image: StoredImage) -> Self {
        Self {
            reference: image.reference,
            digest: image.digest,
            size_bytes: image.size_bytes,
            pulled_at: image.pulled_at.to_rfc3339(),
            last_used: image.last_used.to_rfc3339(),
            path: image.path,
        }
    }
}

/// Detailed local OCI image metadata for management UIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageInspectSummary {
    pub reference: String,
    pub digest: String,
    pub size_bytes: u64,
    pub pulled_at: String,
    pub last_used: String,
    pub path: PathBuf,
    pub manifest_digest: String,
    pub layer_count: usize,
    pub entrypoint: Option<Vec<String>>,
    pub command: Option<Vec<String>>,
    pub env: HashMap<String, String>,
    pub working_dir: Option<String>,
    pub user: Option<String>,
    pub exposed_ports: Vec<String>,
    pub volumes: Vec<String>,
    pub stop_signal: Option<String>,
    pub health_check: Option<ImageHealthCheckSummary>,
    pub onbuild: Vec<String>,
    pub labels: HashMap<String, String>,
}

impl ImageInspectSummary {
    fn from_stored_image(image: StoredImage) -> Result<Self> {
        let oci = OciImage::from_path(&image.path)?;
        let config = oci.config();
        Ok(Self {
            reference: image.reference,
            digest: image.digest,
            size_bytes: image.size_bytes,
            pulled_at: image.pulled_at.to_rfc3339(),
            last_used: image.last_used.to_rfc3339(),
            path: image.path,
            manifest_digest: oci.manifest_digest().to_string(),
            layer_count: oci.layer_paths().len(),
            entrypoint: config.entrypoint.clone(),
            command: config.cmd.clone(),
            env: config.env.iter().cloned().collect(),
            working_dir: config.working_dir.clone(),
            user: config.user.clone(),
            exposed_ports: config.exposed_ports.clone(),
            volumes: config.volumes.clone(),
            stop_signal: config.stop_signal.clone(),
            health_check: config
                .health_check
                .clone()
                .map(ImageHealthCheckSummary::from),
            onbuild: config.onbuild.clone(),
            labels: config.labels.clone(),
        })
    }
}

/// Docker-compatible image health check metadata.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageHealthCheckSummary {
    pub test: Vec<String>,
    pub interval: Option<u64>,
    pub timeout: Option<u64>,
    pub retries: Option<u32>,
    pub start_period: Option<u64>,
}

impl From<a3s_box_runtime::oci::OciHealthCheck> for ImageHealthCheckSummary {
    fn from(health_check: a3s_box_runtime::oci::OciHealthCheck) -> Self {
        Self {
            test: health_check.test,
            interval: health_check.interval,
            timeout: health_check.timeout,
            retries: health_check.retries,
            start_period: health_check.start_period,
        }
    }
}

/// One OCI image history entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageHistoryEntry {
    pub created: Option<String>,
    pub created_by: String,
    pub size_bytes: u64,
    pub comment: String,
    pub empty_layer: bool,
}

/// Request to create a named volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateVolume {
    pub name: String,
    pub driver: String,
    pub labels: HashMap<String, String>,
    pub size_limit: u64,
}

impl CreateVolume {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            driver: "local".to_string(),
            labels: HashMap::new(),
            size_limit: 0,
        }
    }

    pub fn driver(mut self, driver: impl Into<String>) -> Self {
        self.driver = driver.into();
        self
    }

    pub fn label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    pub fn size_limit(mut self, bytes: u64) -> Self {
        self.size_limit = bytes;
        self
    }

    fn validate(&self) -> Result<()> {
        validate_name("volume", &self.name)?;
        if self.driver != "local" {
            return Err(ClientError::Validation(format!(
                "unsupported volume driver '{}'; only 'local' is supported",
                self.driver
            )));
        }
        Ok(())
    }
}

/// Typed volume summary for management UIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct VolumeSummary {
    pub name: String,
    pub driver: String,
    pub mount_point: String,
    pub labels: HashMap<String, String>,
    pub in_use_by: Vec<String>,
    pub in_use: bool,
    pub size_limit: u64,
    pub created_at: String,
}

impl From<VolumeConfig> for VolumeSummary {
    fn from(volume: VolumeConfig) -> Self {
        Self {
            in_use: volume.is_in_use(),
            name: volume.name,
            driver: volume.driver,
            mount_point: volume.mount_point,
            labels: volume.labels,
            in_use_by: volume.in_use_by,
            size_limit: volume.size_limit,
            created_at: volume.created_at,
        }
    }
}

/// Request to create a network.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CreateNetwork {
    pub name: String,
    pub subnet: String,
    pub driver: String,
    pub labels: HashMap<String, String>,
    pub isolation: IsolationMode,
}

impl CreateNetwork {
    pub fn new(name: impl Into<String>) -> Self {
        Self {
            name: name.into(),
            subnet: "10.89.0.0/24".to_string(),
            driver: "bridge".to_string(),
            labels: HashMap::new(),
            isolation: IsolationMode::None,
        }
    }

    pub fn subnet(mut self, subnet: impl Into<String>) -> Self {
        self.subnet = subnet.into();
        self
    }

    pub fn driver(mut self, driver: impl Into<String>) -> Self {
        self.driver = driver.into();
        self
    }

    pub fn label(mut self, key: impl Into<String>, value: impl Into<String>) -> Self {
        self.labels.insert(key.into(), value.into());
        self
    }

    pub fn isolation(mut self, isolation: IsolationMode) -> Self {
        self.isolation = isolation;
        self
    }

    fn validate(&self) -> Result<()> {
        validate_name("network", &self.name)?;
        if self.driver != "bridge" {
            return Err(ClientError::Validation(format!(
                "unsupported network driver '{}'; only 'bridge' is supported",
                self.driver
            )));
        }
        Ok(())
    }
}

/// Typed network summary for management UIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkSummary {
    pub name: String,
    pub driver: String,
    pub subnet: String,
    pub gateway: String,
    pub labels: HashMap<String, String>,
    pub endpoints: Vec<NetworkEndpointSummary>,
    pub endpoint_count: usize,
    pub isolation: String,
    pub created_at: String,
}

impl From<NetworkConfig> for NetworkSummary {
    fn from(network: NetworkConfig) -> Self {
        let mut endpoints = network
            .endpoints
            .into_values()
            .map(NetworkEndpointSummary::from)
            .collect::<Vec<_>>();
        endpoints.sort_by(|a, b| a.box_name.cmp(&b.box_name));
        Self {
            name: network.name,
            driver: network.driver,
            subnet: network.subnet,
            gateway: network.gateway.to_string(),
            labels: network.labels,
            endpoint_count: endpoints.len(),
            endpoints,
            isolation: format!("{:?}", network.policy.isolation).to_lowercase(),
            created_at: network.created_at,
        }
    }
}

/// Typed network endpoint summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NetworkEndpointSummary {
    pub box_id: String,
    pub box_name: String,
    pub aliases: Vec<String>,
    pub ip_address: String,
    pub mac_address: String,
}

impl From<NetworkEndpoint> for NetworkEndpointSummary {
    fn from(endpoint: NetworkEndpoint) -> Self {
        Self {
            box_id: endpoint.box_id,
            box_name: endpoint.box_name,
            aliases: endpoint.aliases,
            ip_address: endpoint.ip_address.to_string(),
            mac_address: endpoint.mac_address,
        }
    }
}

/// Typed snapshot summary for management UIs.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SnapshotSummary {
    pub id: String,
    pub name: String,
    pub source_box_id: String,
    pub image: String,
    pub vcpus: u32,
    pub memory_mb: u32,
    pub volumes: Vec<String>,
    pub command: Vec<String>,
    pub port_map: Vec<String>,
    pub labels: HashMap<String, String>,
    pub network_mode: Option<String>,
    pub size_bytes: u64,
    pub created_at: String,
    pub description: String,
}

impl From<SnapshotMetadata> for SnapshotSummary {
    fn from(snapshot: SnapshotMetadata) -> Self {
        Self {
            id: snapshot.id,
            name: snapshot.name,
            source_box_id: snapshot.source_box_id,
            image: snapshot.image,
            vcpus: snapshot.vcpus,
            memory_mb: snapshot.memory_mb,
            volumes: snapshot.volumes,
            command: snapshot.cmd,
            port_map: snapshot.port_map,
            labels: snapshot.labels,
            network_mode: snapshot.network_mode,
            size_bytes: snapshot.size_bytes,
            created_at: snapshot.created_at.to_rfc3339(),
            description: snapshot.description,
        }
    }
}
