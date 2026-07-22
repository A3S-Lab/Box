fn resolve_stored_image(images: &[StoredImage], query: &str) -> Result<Option<StoredImage>> {
    let query = query.trim();
    if query.is_empty() {
        return Ok(None);
    }

    for mode in [
        ImageMatchMode::Exact,
        ImageMatchMode::Alias,
        ImageMatchMode::Digest,
    ] {
        let matches = matching_images(images, query, mode);
        match matches.as_slice() {
            [] => {}
            [image] => return Ok(Some(image.clone())),
            _ => {
                return Err(ClientError::Validation(ambiguous_image_error(
                    query, &matches,
                )))
            }
        }
    }

    Ok(None)
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ImageMatchMode {
    Exact,
    Alias,
    Digest,
}

fn matching_images(images: &[StoredImage], query: &str, mode: ImageMatchMode) -> Vec<StoredImage> {
    let query_aliases = image_reference_aliases(query);
    let mut matches = Vec::new();

    for image in images {
        let matched = match mode {
            ImageMatchMode::Exact => image.reference == query,
            ImageMatchMode::Alias => {
                let image_aliases = image_reference_aliases(&image.reference);
                !image_aliases.is_disjoint(&query_aliases)
            }
            ImageMatchMode::Digest => {
                is_digest_reference(query) && digest_matches(&image.digest, query)
            }
        };

        if matched
            && !matches
                .iter()
                .any(|candidate: &StoredImage| candidate.reference == image.reference)
        {
            matches.push(image.clone());
        }
    }

    matches
}

fn ambiguous_image_error(query: &str, matches: &[StoredImage]) -> String {
    let mut references = matches
        .iter()
        .map(|image| image.reference.as_str())
        .collect::<Vec<_>>();
    references.sort_unstable();
    format!(
        "image reference '{query}' is ambiguous; it matches: {}",
        references.join(", ")
    )
}

fn image_reference_aliases(reference: &str) -> HashSet<String> {
    let mut aliases = HashSet::new();
    let reference = reference.trim();
    if reference.is_empty() {
        return aliases;
    }

    aliases.insert(reference.to_string());
    if !is_digest_reference(reference) {
        if let Ok(parsed) = ImageReference::parse(reference) {
            aliases.insert(parsed.full_reference());
        }
    }
    aliases
}

fn is_digest_reference(reference: &str) -> bool {
    if reference.starts_with("sha256:") {
        return true;
    }
    let len = reference.len();
    (12..=64).contains(&len)
        && reference
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))
}

fn digest_matches(stored_digest: &str, query: &str) -> bool {
    if stored_digest == query {
        return true;
    }
    let query_hex = query.strip_prefix("sha256:").unwrap_or(query);
    if query_hex.is_empty() {
        return false;
    }
    let stored_hex = stored_digest
        .strip_prefix("sha256:")
        .unwrap_or(stored_digest);
    stored_hex.starts_with(query_hex)
}

fn validate_tag_target(target: &str) -> Result<()> {
    let target = target.trim();
    if target.is_empty() {
        return Err(ClientError::Validation(
            "target image reference cannot be empty".to_string(),
        ));
    }

    let without_digest = target.split('@').next().unwrap_or(target);
    let last_slash = without_digest.rfind('/');
    let repo = match without_digest.rfind(':') {
        Some(colon) if last_slash.is_none_or(|slash| colon > slash) => &without_digest[..colon],
        _ => without_digest,
    };
    if repo.bytes().any(|byte| byte.is_ascii_uppercase()) {
        return Err(ClientError::Validation(format!(
            "invalid reference format: repository name must be lowercase: '{target}'"
        )));
    }

    ImageReference::parse(target).map_err(ClientError::Runtime)?;
    Ok(())
}

fn load_image_history(image_dir: &Path) -> Result<Vec<ImageHistoryEntry>> {
    let config = load_image_config_json(image_dir)?;
    let layer_sizes = load_image_layer_sizes(image_dir)?;
    let history = config
        .get("history")
        .and_then(|value| value.as_array())
        .cloned()
        .unwrap_or_default();
    let mut layer_index = 0usize;

    Ok(history
        .into_iter()
        .map(|entry| {
            let empty_layer = entry
                .get("empty_layer")
                .and_then(|value| value.as_bool())
                .unwrap_or(false);
            let size_bytes = if empty_layer {
                0
            } else {
                let size = layer_sizes.get(layer_index).copied().unwrap_or(0);
                layer_index += 1;
                size
            };
            ImageHistoryEntry {
                created: entry
                    .get("created")
                    .and_then(|value| value.as_str())
                    .map(str::to_string),
                created_by: entry
                    .get("created_by")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                size_bytes,
                comment: entry
                    .get("comment")
                    .and_then(|value| value.as_str())
                    .unwrap_or_default()
                    .to_string(),
                empty_layer,
            }
        })
        .collect())
}

fn load_image_layer_sizes(image_dir: &Path) -> Result<Vec<u64>> {
    let manifest = load_image_manifest_json(image_dir)?;
    Ok(manifest
        .get("layers")
        .and_then(|value| value.as_array())
        .map(|layers| {
            layers
                .iter()
                .map(|layer| {
                    layer
                        .get("size")
                        .and_then(|value| value.as_u64())
                        .unwrap_or(0)
                })
                .collect()
        })
        .unwrap_or_default())
}

fn load_image_config_json(image_dir: &Path) -> Result<serde_json::Value> {
    let manifest = load_image_manifest_json(image_dir)?;
    let config_digest = manifest
        .get("config")
        .and_then(|config| config.get("digest"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| image_layout_error("No config digest in image manifest"))?;
    read_json_file(&blob_path(image_dir, config_digest))
}

fn load_image_manifest_json(image_dir: &Path) -> Result<serde_json::Value> {
    let index = read_json_file(&image_dir.join("index.json"))?;
    let manifest_digest = index
        .get("manifests")
        .and_then(|value| value.as_array())
        .and_then(|manifests| manifests.first())
        .and_then(|manifest| manifest.get("digest"))
        .and_then(|value| value.as_str())
        .ok_or_else(|| image_layout_error("No manifest digest in image index"))?;
    read_json_file(&blob_path(image_dir, manifest_digest))
}

fn blob_path(root_dir: &Path, digest: &str) -> PathBuf {
    let (algorithm, hash) = digest.split_once(':').unwrap_or(("sha256", digest));
    root_dir.join("blobs").join(algorithm).join(hash)
}

fn read_json_file(path: &Path) -> Result<serde_json::Value> {
    let data = std::fs::read_to_string(path).map_err(|error| {
        image_layout_error(format!(
            "failed to read image JSON {}: {error}",
            path.display()
        ))
    })?;
    serde_json::from_str(&data).map_err(|error| {
        image_layout_error(format!(
            "failed to parse image JSON {}: {error}",
            path.display()
        ))
    })
}

fn image_layout_error(message: impl Into<String>) -> ClientError {
    ClientError::Runtime(a3s_box_core::error::BoxError::OciImageError(message.into()))
}

fn resolve_record<'a>(state: &'a StateFile, query: &str) -> Option<Result<&'a BoxRecord>> {
    if let Some(record) = state
        .find_by_id(query)
        .or_else(|| state.find_by_name(query))
    {
        return Some(Ok(record));
    }

    let matches = state.find_by_id_prefix(query);
    match matches.as_slice() {
        [] => None,
        [record] => Some(Ok(*record)),
        records => Some(Err(ClientError::AmbiguousBoxQuery {
            query: query.to_string(),
            matches: records.iter().map(|record| record.name.clone()).collect(),
        })),
    }
}

fn resolve_required_record<'a>(state: &'a StateFile, query: &str) -> Result<&'a BoxRecord> {
    resolve_record(state, query)
        .transpose()?
        .ok_or_else(|| ClientError::BoxNotFound(query.to_string()))
}

fn resolve_snapshot_metadata(store: &SnapshotStore, query: &str) -> Result<SnapshotMetadata> {
    if let Some(metadata) = store.get(query)? {
        return Ok(metadata);
    }

    let matches = store
        .list()?
        .into_iter()
        .filter(|snapshot| snapshot.name == query)
        .collect::<Vec<_>>();

    match matches.as_slice() {
        [] => Err(ClientError::Validation(format!(
            "snapshot {query:?} was not found"
        ))),
        [metadata] => Ok(metadata.clone()),
        snapshots => Err(ClientError::Validation(format!(
            "snapshot query {query:?} matched multiple snapshots: {:?}",
            snapshots
                .iter()
                .map(|snapshot| snapshot.id.clone())
                .collect::<Vec<_>>()
        ))),
    }
}

fn default_restored_box_name(state: &StateFile, metadata: &SnapshotMetadata) -> String {
    let base = format!("{}-restore", metadata.name);
    if state.find_by_name(&base).is_none() {
        return base;
    }

    for index in 2.. {
        let candidate = format!("{base}-{index}");
        if state.find_by_name(&candidate).is_none() {
            return candidate;
        }
    }

    unreachable!("unbounded restored box name search should always find a free suffix")
}

fn require_inactive_for_network_change(record: &BoxRecord, action: &str) -> Result<()> {
    if !record.is_active() {
        return Ok(());
    }

    Err(ClientError::Validation(format!(
        "cannot {action} box {} while it is {}; stop it first",
        record.name, record.status
    )))
}

fn is_prunable_box_record(record: &BoxRecord) -> bool {
    matches!(record.status.as_str(), "created" | "stopped" | "dead")
}

fn require_active(record: &BoxRecord, action: &str) -> Result<()> {
    if record.is_active() {
        return Ok(());
    }

    Err(ClientError::Validation(format!(
        "cannot {action} box {} because it is {}",
        record.name, record.status
    )))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum LifecycleTransition {
    Pause,
    Unpause,
}

impl LifecycleTransition {
    fn action(self) -> &'static str {
        match self {
            Self::Pause => "pause",
            Self::Unpause => "unpause",
        }
    }

    fn source_status(self) -> &'static str {
        match self {
            Self::Pause => "running",
            Self::Unpause => "paused",
        }
    }

    fn target_status(self) -> &'static str {
        match self {
            Self::Pause => "paused",
            Self::Unpause => "running",
        }
    }

    #[cfg(unix)]
    fn signal(self) -> i32 {
        match self {
            Self::Pause => libc::SIGSTOP,
            Self::Unpause => libc::SIGCONT,
        }
    }

    #[cfg(not(unix))]
    fn signal(self) -> i32 {
        let _ = self;
        0
    }

    fn validate_status(self, record: &BoxRecord) -> Result<()> {
        if record.status == self.source_status() {
            return Ok(());
        }

        Err(ClientError::Validation(format!(
            "cannot {} box {} because it is {}",
            self.action(),
            record.name,
            record.status
        )))
    }
}

fn require_live_pid(record: &BoxRecord, action: &str) -> Result<u32> {
    match record.pid {
        Some(pid) if is_process_alive_with_identity(pid, record.pid_start_time) => Ok(pid),
        Some(pid) => Err(ClientError::Validation(format!(
            "cannot {action} box {} because its recorded PID {pid} is not running",
            record.name
        ))),
        None => Err(ClientError::Validation(format!(
            "cannot {action} box {} because it has no recorded PID",
            record.name
        ))),
    }
}

#[cfg(unix)]
fn send_host_signal(pid: u32, signal: i32) -> std::io::Result<()> {
    let result = unsafe { libc::kill(pid as i32, signal) };
    if result == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

#[cfg(not(unix))]
fn send_host_signal(_pid: u32, _signal: i32) -> std::io::Result<()> {
    Err(std::io::Error::new(
        std::io::ErrorKind::Unsupported,
        "host process signals are not supported on this platform",
    ))
}

#[cfg(unix)]
fn terminate_recorded_process(record: &BoxRecord) {
    if let Some(pid) = record.pid {
        if is_process_alive_with_identity(pid, record.pid_start_time) {
            let _ = send_host_signal(pid, libc::SIGKILL);
        }
    }
}

#[cfg(not(unix))]
fn terminate_recorded_process(_record: &BoxRecord) {}

fn stopped_exit_code(
    previous_exit_code: Option<i32>,
    outcome: StopOutcome,
    stop_signal: i32,
) -> Option<i32> {
    outcome
        .inferred_exit_code(stop_signal)
        .or(previous_exit_code)
}

#[cfg(unix)]
async fn graceful_stop(pid: u32, signal: i32, timeout_secs: u64) -> StopOutcome {
    if !is_process_alive(pid) {
        return StopOutcome::AlreadyExited;
    }

    if send_host_signal(pid, signal).is_err() && !is_process_alive(pid) {
        return StopOutcome::AlreadyExited;
    }

    wait_for_exit_or_kill(pid, timeout_secs).await
}

#[cfg(unix)]
async fn graceful_stop_via_guest(
    pid: u32,
    exec_socket: &Path,
    signal: i32,
    timeout_secs: u64,
) -> StopOutcome {
    if !is_process_alive(pid) {
        return StopOutcome::AlreadyExited;
    }

    let delivered = match ExecClient::connect(exec_socket).await {
        Ok(client) => client.signal_main(signal).await.unwrap_or(false),
        Err(_) => false,
    };
    if !delivered {
        return graceful_stop(pid, signal, timeout_secs).await;
    }

    wait_for_exit_or_kill(pid, timeout_secs).await
}

#[cfg(unix)]
async fn wait_for_exit_or_kill(pid: u32, timeout_secs: u64) -> StopOutcome {
    let start = Instant::now();
    let timeout_ms = timeout_secs.saturating_mul(1000);
    loop {
        if !is_process_alive(pid) {
            return StopOutcome::GracefulExit;
        }
        if start.elapsed().as_millis() >= timeout_ms as u128 {
            let _ = send_host_signal(pid, libc::SIGKILL);
            tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
            return StopOutcome::ForceKilled;
        }
        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
    }
}

fn cleanup_stopped_box(paths: &A3sBoxPaths, record: &BoxRecord) {
    detach_volumes(paths, &record.volume_names, &record.id);
    a3s_box_runtime::rootfs::unmount_box_overlay(&record.box_dir.join("merged"));
    cleanup_external_socket_dir(&record.box_dir, &record.exec_socket_path);
    remove_host_cgroup(&record.id);
}

fn cleanup_removed_box(paths: &A3sBoxPaths, record: &BoxRecord) {
    detach_volumes(paths, &record.volume_names, &record.id);
    cleanup_network_endpoint(paths, record);
    cleanup_anonymous_volumes(paths, &record.anonymous_volumes);
    remove_host_cgroup(&record.id);
    if record.box_dir.exists() {
        a3s_box_runtime::rootfs::unmount_box_overlay(&record.box_dir.join("merged"));
        let _ = std::fs::remove_dir_all(&record.box_dir);
    }
    cleanup_external_socket_dir(&record.box_dir, &record.exec_socket_path);

    let fs_mount_dir = std::env::temp_dir().join(format!("a3s-fs-mount-{}", record.id));
    if fs_mount_dir.exists() {
        let _ = std::fs::remove_dir_all(fs_mount_dir);
    }
}

fn detach_volumes(paths: &A3sBoxPaths, volume_names: &[String], box_id: &str) {
    let store = VolumeStore::new(&paths.volumes_file, &paths.volumes_dir);
    for volume_name in volume_names {
        let _ = store.modify(volume_name, |config| {
            config.in_use_by.retain(|id| id != box_id);
        });
    }
}

fn cleanup_anonymous_volumes(paths: &A3sBoxPaths, volume_names: &[String]) {
    let store = VolumeStore::new(&paths.volumes_file, &paths.volumes_dir);
    for volume_name in volume_names {
        let _ = store.remove(volume_name, true);
    }
}

fn cleanup_network_endpoint(paths: &A3sBoxPaths, record: &BoxRecord) {
    let Some(network_name) = record_network_name(record).map(str::to_string) else {
        return;
    };
    let store = NetworkStore::new(&paths.networks_file);
    let _ = store.with_write_lock(
        |networks| -> std::result::Result<(), a3s_box_core::error::BoxError> {
            if let Some(network) = networks.get_mut(&network_name) {
                let _ = network.disconnect(&record.id);
            }
            Ok(())
        },
    );
}

fn cleanup_external_socket_dir(box_dir: &Path, exec_socket_path: &Path) {
    let Some(socket_dir) = exec_socket_path.parent() else {
        return;
    };
    #[cfg(target_os = "linux")]
    a3s_box_runtime::network::terminate_passt(socket_dir);
    if socket_dir.starts_with(box_dir) {
        return;
    }
    let _ = std::fs::remove_dir_all(socket_dir);
}

fn remove_host_cgroup(box_id: &str) {
    #[cfg(target_os = "linux")]
    {
        let _ = std::fs::remove_dir(format!("/sys/fs/cgroup/a3s-box/{box_id}"));
    }
    #[cfg(not(target_os = "linux"))]
    let _ = box_id;
}

#[cfg(unix)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RuntimeSocket {
    Exec,
    Pty,
    Attest,
}

#[cfg(unix)]
impl RuntimeSocket {
    fn file_name(self) -> &'static str {
        match self {
            Self::Exec => "exec.sock",
            Self::Pty => "pty.sock",
            Self::Attest => "attest.sock",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Exec => "exec",
            Self::Pty => "PTY",
            Self::Attest => "attestation",
        }
    }

    fn action(self) -> &'static str {
        match self {
            Self::Exec => "exec in",
            Self::Pty => "open a PTY in",
            Self::Attest => "request attestation from",
        }
    }
}

#[cfg(unix)]
fn require_running(record: &BoxRecord, action: &str) -> Result<()> {
    if record.status == "running" {
        return Ok(());
    }

    Err(ClientError::Validation(format!(
        "cannot {action} box {} because it is {}",
        record.name, record.status
    )))
}

#[cfg(unix)]
fn sibling_socket(record: &BoxRecord, socket_name: &str) -> PathBuf {
    if let Some(parent) = record.exec_socket_path.parent() {
        return parent.join(socket_name);
    }
    record.box_dir.join("sockets").join(socket_name)
}

#[cfg(unix)]
fn exec_socket(record: &BoxRecord) -> PathBuf {
    if !record.exec_socket_path.as_os_str().is_empty() {
        return record.exec_socket_path.clone();
    }
    record.box_dir.join("sockets").join("exec.sock")
}

#[cfg(unix)]
fn runtime_socket(record: &BoxRecord, socket: RuntimeSocket) -> PathBuf {
    match socket {
        RuntimeSocket::Exec => exec_socket(record),
        RuntimeSocket::Pty | RuntimeSocket::Attest => sibling_socket(record, socket.file_name()),
    }
}

fn record_network_name(record: &BoxRecord) -> Option<&str> {
    record
        .network_name
        .as_deref()
        .or(match &record.network_mode {
            NetworkMode::Bridge { network } => Some(network.as_str()),
            _ => None,
        })
}

fn is_predefined_network(name: &str) -> bool {
    matches!(name, "bridge" | "host" | "none")
}

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
struct NetworkStats {
    rx_bytes: u64,
    tx_bytes: u64,
}

fn collect_box_stats(records: &[&BoxRecord]) -> Vec<BoxStatsSummary> {
    let pids = records
        .iter()
        .filter_map(|record| record.pid)
        .map(Pid::from_u32)
        .collect::<Vec<_>>();

    if pids.is_empty() {
        return Vec::new();
    }

    let mut system = System::new();
    for pid in &pids {
        system.refresh_process(*pid);
    }
    std::thread::sleep(std::time::Duration::from_millis(200));
    for pid in &pids {
        system.refresh_process(*pid);
    }

    records
        .iter()
        .filter_map(|record| build_box_stats(&system, record))
        .collect()
}

fn build_box_stats(system: &System, record: &BoxRecord) -> Option<BoxStatsSummary> {
    let pid = record.pid?;
    let process = system.process(Pid::from_u32(pid))?;
    let disk = process.disk_usage();
    let memory_limit_bytes = record.memory_mb as u64 * 1024 * 1024;
    let memory_bytes = process.memory();
    let cpu_percent = process.cpu_usage();
    let cpu_percent_scaled = cpu_percent as f64 / record.cpus.max(1) as f64;
    let memory_percent = if memory_limit_bytes > 0 {
        memory_bytes as f64 / memory_limit_bytes as f64 * 100.0
    } else {
        0.0
    };
    let network = collect_network_stats(record);

    Some(BoxStatsSummary {
        id: record.id.clone(),
        short_id: record.short_id.clone(),
        name: record.name.clone(),
        status: record.status.clone(),
        pid,
        cpus: record.cpus,
        cpu_percent,
        cpu_percent_scaled,
        memory_bytes,
        memory_limit_bytes,
        memory_percent,
        network_rx_bytes: network.rx_bytes,
        network_tx_bytes: network.tx_bytes,
        block_read_bytes: disk.total_read_bytes,
        block_write_bytes: disk.total_written_bytes,
    })
}

fn collect_network_stats(record: &BoxRecord) -> NetworkStats {
    read_network_stats_file(&record.box_dir.join("sockets").join("net.stats.json"))
        .unwrap_or_default()
}

fn read_network_stats_file(path: &std::path::Path) -> Option<NetworkStats> {
    let data = std::fs::read_to_string(path).ok()?;
    let json: serde_json::Value = serde_json::from_str(&data).ok()?;
    Some(NetworkStats {
        rx_bytes: json.get("rx_bytes")?.as_u64()?,
        tx_bytes: json.get("tx_bytes")?.as_u64()?,
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct LogSource {
    path: PathBuf,
    structured: bool,
}

fn resolve_log_source(record: &BoxRecord) -> Option<LogSource> {
    let log_dir = record.box_dir.join("logs");
    let structured_log = json_log_path(&log_dir);
    if structured_log.exists() {
        return Some(LogSource {
            path: structured_log,
            structured: true,
        });
    }

    if record.console_log.exists() {
        return Some(LogSource {
            path: record.console_log.clone(),
            structured: false,
        });
    }

    None
}

fn read_log_source(source: LogSource, tail: usize) -> Result<Vec<BoxLogLine>> {
    if tail == 0 {
        return Ok(Vec::new());
    }

    let file = std::fs::File::open(source.path)?;
    let mut reader = std::io::BufReader::new(file);
    let runtime_filter = (!source.structured).then(RuntimeConsoleFilter::new);
    let mut lines = Vec::new();
    let mut buffer = Vec::new();
    loop {
        buffer.clear();
        if reader.read_until(b'\n', &mut buffer)? == 0 {
            break;
        }
        let complete = buffer.ends_with(b"\n");
        let line = std::str::from_utf8(&buffer)
            .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?
            .trim_end_matches(['\n', '\r']);
        let Some(decoded) = decode_log_line(
            line,
            source.structured,
            runtime_filter.as_ref(),
            complete,
        ) else {
            continue;
        };
        lines.push(decoded);
    }

    let start = lines.len().saturating_sub(tail);
    Ok(lines[start..].to_vec())
}

fn decode_log_line(
    line: &str,
    structured: bool,
    runtime_filter: Option<&RuntimeConsoleFilter>,
    complete: bool,
) -> Option<BoxLogLine> {
    if structured {
        return match serde_json::from_str::<LogEntry>(line) {
            Ok(entry) => Some(BoxLogLine {
                stream: entry.stream,
                timestamp: Some(entry.time),
                message: entry.log.trim_end_matches(['\n', '\r']).to_string(),
            }),
            Err(_) => Some(BoxLogLine {
                stream: "stdout".to_string(),
                timestamp: None,
                message: line.to_string(),
            }),
        };
    }

    if complete && runtime_filter.is_some_and(|filter| !filter.keep_line(line)) {
        return None;
    }

    Some(BoxLogLine {
        stream: "stdout".to_string(),
        timestamp: None,
        message: line.trim_end_matches(['\n', '\r']).to_string(),
    })
}

fn validate_name(kind: &str, name: &str) -> Result<()> {
    if name.trim().is_empty() {
        return Err(ClientError::Validation(format!(
            "{kind} name cannot be empty"
        )));
    }
    if name.contains('/') || name.contains('\\') {
        return Err(ClientError::Validation(format!(
            "{kind} name cannot contain path separators"
        )));
    }
    Ok(())
}

struct BoxDirGuard {
    path: PathBuf,
    armed: bool,
}

impl BoxDirGuard {
    fn new(path: PathBuf) -> Self {
        Self { path, armed: true }
    }

    fn disarm(&mut self) {
        self.armed = false;
    }
}

impl Drop for BoxDirGuard {
    fn drop(&mut self) {
        if self.armed {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }
}

fn resolve_box_rootfs(box_dir: &Path) -> Option<PathBuf> {
    let merged = box_dir.join("merged");
    if is_populated_dir(&merged) {
        return Some(merged);
    }
    let rootfs = box_dir.join("rootfs");
    if rootfs.is_dir() {
        return Some(rootfs);
    }
    None
}

fn is_populated_dir(path: &Path) -> bool {
    path.is_dir()
        && std::fs::read_dir(path)
            .map(|mut entries| entries.next().is_some())
            .unwrap_or(false)
}

fn disk_usage_paths(paths: &[&Path]) -> Result<u64> {
    paths.iter().try_fold(0u64, |total, path| {
        Ok(total.saturating_add(disk_usage_path(path)?))
    })
}

fn disk_usage_path(path: &Path) -> Result<u64> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(ClientError::State(error)),
    };

    if !metadata.is_dir() {
        return Ok(metadata.len());
    }

    let mut entries = std::fs::read_dir(path).map_err(ClientError::State)?;
    entries.try_fold(0u64, |total, entry| {
        let entry = entry.map_err(ClientError::State)?;
        Ok(total.saturating_add(disk_usage_path(&entry.path())?))
    })
}

fn file_size_or_zero(path: &Path) -> Result<u64> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
        Err(error) => return Err(ClientError::State(error)),
    };
    if metadata.is_dir() {
        Ok(0)
    } else {
        Ok(metadata.len())
    }
}
