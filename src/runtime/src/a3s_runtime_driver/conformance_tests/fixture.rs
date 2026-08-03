use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a3s_runtime::contract::{
    ArtifactRef, RuntimeCapabilities, RuntimeInspection, RuntimeOutputArtifact, RuntimeOutputSpec,
    RuntimeUnitSpec, RuntimeUnitState, SecretReference,
};
use a3s_runtime::{
    runtime_profile_requirements, FileRuntimeStateStore, ManagedRuntimeClient, RuntimeClient,
    RuntimeConformanceFixture, RuntimeConformanceInventory, RuntimeConformanceProfile,
    RuntimeConformanceProfileEvidence, RuntimeError, RuntimeResult,
};
use async_trait::async_trait;
use sha2::{Digest, Sha256};

use a3s_box_core::ExecutionIsolation;

use super::super::metadata::{local_identity, UNIT_LABEL};
use super::super::{
    BoxArtifactPort, BoxArtifactPortError, BoxRegistryCredential, BoxRuntimeDriver,
    BoxRuntimeDriverConfig, BoxSecretMaterial, BoxSecretMaterializationError,
    BoxSecretMaterializer,
};
use super::cases::CaseFactory;
use super::{
    external, failure, require, Result, PRIVATE_REGISTRY_PASSWORD,
    PRIVATE_REGISTRY_SECRET_REFERENCE, PRIVATE_REGISTRY_USERNAME,
};

pub(super) const SECRET_ENV_REFERENCE: &str = "secret://r17/provider-token/v1";
pub(super) const SECRET_ENV_VALUE: &str = "r17-secret-alpha-long";
pub(super) const SECRET_FILE_REFERENCE: &str = "secret://r17/provider-file/v1";
pub(super) const SECRET_FILE_VALUE: &str = "secret-alpha";

struct ConformanceSecretMaterializer {
    calls: AtomicUsize,
    authorized: AtomicBool,
}

impl Default for ConformanceSecretMaterializer {
    fn default() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            authorized: AtomicBool::new(true),
        }
    }
}

#[async_trait]
impl BoxSecretMaterializer for ConformanceSecretMaterializer {
    async fn materialize(
        &self,
        reference: &SecretReference,
    ) -> std::result::Result<BoxSecretMaterial, BoxSecretMaterializationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !self.authorized.load(Ordering::SeqCst) {
            return Err(BoxSecretMaterializationError::Rejected(
                "R17 fixture authorization was revoked".into(),
            ));
        }
        let value = match reference.reference.as_str() {
            SECRET_ENV_REFERENCE => SECRET_ENV_VALUE,
            SECRET_FILE_REFERENCE => SECRET_FILE_VALUE,
            _ => {
                return Err(BoxSecretMaterializationError::Rejected(
                    "R17 fixture reference is not registered".into(),
                ))
            }
        };
        BoxSecretMaterial::new(value.as_bytes().to_vec())
    }

    async fn materialize_registry_credential(
        &self,
        reference: &SecretReference,
        registry: &str,
    ) -> std::result::Result<BoxRegistryCredential, BoxSecretMaterializationError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        if !self.authorized.load(Ordering::SeqCst) {
            return Err(BoxSecretMaterializationError::Rejected(
                "R17 fixture authorization was revoked".into(),
            ));
        }
        if reference.reference != PRIVATE_REGISTRY_SECRET_REFERENCE
            || !registry.starts_with("127.0.0.1:")
        {
            return Err(BoxSecretMaterializationError::Rejected(
                "R17 fixture registry reference is not registered for this registry".into(),
            ));
        }
        BoxRegistryCredential::new(PRIVATE_REGISTRY_USERNAME, PRIVATE_REGISTRY_PASSWORD)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ConformanceOutputCapture {
    pub(super) spec_digest: String,
    pub(super) name: String,
    pub(super) files: BTreeMap<String, Vec<u8>>,
    pub(super) artifact: RuntimeOutputArtifact,
}

struct ConformanceArtifactPort {
    source: PathBuf,
    captures: Mutex<Vec<ConformanceOutputCapture>>,
    cleanups: Mutex<Vec<String>>,
}

#[async_trait]
impl BoxArtifactPort for ConformanceArtifactPort {
    async fn mount_path(
        &self,
        _spec: &RuntimeUnitSpec,
        _mount: &a3s_runtime::contract::RuntimeMount,
    ) -> std::result::Result<PathBuf, BoxArtifactPortError> {
        Ok(self.source.clone())
    }

    async fn capture_output(
        &self,
        spec: &RuntimeUnitSpec,
        output: &RuntimeOutputSpec,
        source: &Path,
    ) -> std::result::Result<RuntimeOutputArtifact, BoxArtifactPortError> {
        let mut files = BTreeMap::new();
        let entries = std::fs::read_dir(source).map_err(|error| {
            BoxArtifactPortError::Unavailable(format!(
                "R17 output directory could not be read: {error}"
            ))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                BoxArtifactPortError::Unavailable(format!(
                    "R17 output entry could not be read: {error}"
                ))
            })?;
            let metadata = std::fs::symlink_metadata(entry.path()).map_err(|error| {
                BoxArtifactPortError::Unavailable(format!(
                    "R17 output metadata could not be read: {error}"
                ))
            })?;
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(BoxArtifactPortError::Rejected(
                    "R17 output fixture accepts only regular root-level files".into(),
                ));
            }
            let name = entry.file_name().into_string().map_err(|_| {
                BoxArtifactPortError::Rejected("R17 output name is not UTF-8".into())
            })?;
            let value = std::fs::read(entry.path()).map_err(|error| {
                BoxArtifactPortError::Unavailable(format!(
                    "R17 output content could not be read: {error}"
                ))
            })?;
            files.insert(name, value);
        }
        let size_bytes = files.values().try_fold(0_u64, |total, value| {
            total
                .checked_add(value.len() as u64)
                .ok_or_else(|| BoxArtifactPortError::Rejected("R17 output size overflowed".into()))
        })?;
        if size_bytes == 0 || size_bytes > output.max_bytes {
            return Err(BoxArtifactPortError::Rejected(
                "R17 output violates its declared bound".into(),
            ));
        }
        let digest = output_digest(&files);
        let artifact = RuntimeOutputArtifact {
            name: output.name.clone(),
            artifact: ArtifactRef {
                uri: format!("https://artifacts.example/a3s/r17/{digest}"),
                digest,
                media_type: output.media_type.clone(),
            },
            size_bytes,
        };
        self.captures
            .lock()
            .unwrap()
            .push(ConformanceOutputCapture {
                spec_digest: spec.digest().map_err(BoxArtifactPortError::Rejected)?,
                name: output.name.clone(),
                files,
                artifact: artifact.clone(),
            });
        Ok(artifact)
    }

    async fn cleanup_spec(
        &self,
        spec_digest: &str,
    ) -> std::result::Result<(), BoxArtifactPortError> {
        self.cleanups.lock().unwrap().push(spec_digest.into());
        Ok(())
    }
}

pub(super) fn output_digest(files: &BTreeMap<String, Vec<u8>>) -> String {
    let mut digest = Sha256::new();
    for (name, value) in files {
        digest.update((name.len() as u64).to_be_bytes());
        digest.update(name.as_bytes());
        digest.update((value.len() as u64).to_be_bytes());
        digest.update(value);
    }
    format!("sha256:{:x}", digest.finalize())
}

#[derive(Debug, Clone, Default)]
struct SeenResource {
    unit_id: Option<String>,
    pid: Option<u32>,
    pid_start_time: Option<u64>,
    cgroup_path: Option<PathBuf>,
    owner_pid: Option<u32>,
    owner_pid_start_time: Option<u64>,
    log_worker_pid: Option<u32>,
    log_worker_pid_start_time: Option<u64>,
}

pub(super) struct BoxRuntimeConformanceFixture {
    pub(super) home_dir: PathBuf,
    pub(super) driver: Arc<BoxRuntimeDriver>,
    pub(super) state: Arc<FileRuntimeStateStore>,
    pub(super) cases: CaseFactory,
    execution_isolation: ExecutionIsolation,
    base_case: a3s_runtime::RuntimeBaseConformanceCase,
    secret_materializer: Arc<ConformanceSecretMaterializer>,
    artifact_port: Arc<ConformanceArtifactPort>,
    private_artifact_root: PathBuf,
    drivers: Mutex<Vec<Arc<BoxRuntimeDriver>>>,
    state_roots: Mutex<BTreeSet<PathBuf>>,
    provider_homes: Mutex<BTreeSet<PathBuf>>,
    fixture_roots: Mutex<BTreeSet<PathBuf>>,
    seen: Mutex<BTreeMap<(PathBuf, String), SeenResource>>,
}

impl BoxRuntimeConformanceFixture {
    pub(super) fn from_environment(execution_isolation: ExecutionIsolation) -> Result<Self> {
        require(
            std::env::var("A3S_BOX_RUNTIME_CONFORMANCE").as_deref() == Ok("1"),
            "set A3S_BOX_RUNTIME_CONFORMANCE=1 to acknowledge the destructive R17 suite",
        )?;
        let home_dir = std::env::var_os("A3S_HOME")
            .map(PathBuf::from)
            .ok_or_else(|| failure("A3S_HOME must select a dedicated R17 home"))?;
        require(home_dir.is_absolute(), "A3S_HOME must be absolute")?;
        require(
            home_dir
                .file_name()
                .and_then(|value| value.to_str())
                .is_some_and(|value| value.contains("runtime-conformance")),
            "A3S_HOME final component must contain runtime-conformance",
        )?;
        let canonical_home = home_dir
            .canonicalize()
            .map_err(|error| external("canonicalize A3S_HOME", error))?;
        require(
            canonical_home == home_dir,
            "A3S_HOME must already be canonical and must not be a symlink",
        )?;
        validate_runtime_assets(&home_dir, execution_isolation)?;

        let state_root = home_dir.join("runtime-state");
        require(
            !state_root.exists(),
            "dedicated R17 Runtime state root already exists",
        )?;
        let prefix = format!("r17-{}", uuid::Uuid::new_v4().simple());
        let cases = CaseFactory::from_environment(prefix)?;
        let base_case = cases.base_case();
        base_case.validate().map_err(super::invalid)?;

        let secret_root = home_dir.join("runtime-secrets");
        require(
            secret_root.is_dir(),
            "A3S_HOME/runtime-secrets must be a pre-mounted private tmpfs directory",
        )?;
        let config = driver_config(home_dir.clone());
        let secret_materializer = Arc::new(ConformanceSecretMaterializer::default());
        let private_artifact_root = home_dir
            .parent()
            .ok_or_else(|| failure("R17 home has no parent for private Artifact storage"))?
            .join(format!(
                ".a3s-r17-artifacts-{}",
                uuid::Uuid::new_v4().simple()
            ));
        let private_artifact_source = private_artifact_root.join("mount");
        std::fs::create_dir(&private_artifact_root)
            .map_err(|error| external("create private Artifact root", error))?;
        std::fs::create_dir(&private_artifact_source)
            .map_err(|error| external("create private Artifact mount", error))?;
        set_private_artifact_modes(&private_artifact_root, &private_artifact_source)?;
        std::fs::write(
            private_artifact_source.join("payload.txt"),
            b"r17-private-artifact",
        )
        .map_err(|error| external("write private Artifact payload", error))?;
        let artifact_port = Arc::new(ConformanceArtifactPort {
            source: private_artifact_source,
            captures: Mutex::new(Vec::new()),
            cleanups: Mutex::new(Vec::new()),
        });
        let driver = Arc::new(
            BoxRuntimeDriver::new_with_isolation(config, execution_isolation)?
                .with_secret_materializer(secret_materializer.clone())
                .with_artifact_port(artifact_port.clone()),
        );
        let state = Arc::new(FileRuntimeStateStore::new(&state_root));
        Ok(Self {
            home_dir,
            driver: driver.clone(),
            state,
            cases,
            execution_isolation,
            base_case,
            secret_materializer,
            artifact_port,
            private_artifact_root: private_artifact_root.clone(),
            drivers: Mutex::new(vec![driver]),
            state_roots: Mutex::new(BTreeSet::from([state_root])),
            provider_homes: Mutex::new(BTreeSet::new()),
            fixture_roots: Mutex::new(BTreeSet::from([private_artifact_root])),
            seen: Mutex::new(BTreeMap::new()),
        })
    }

    pub(super) fn primary_client(&self) -> ManagedRuntimeClient {
        self.client_with(self.driver.clone(), self.state.clone())
    }

    pub(super) fn client_with(
        &self,
        driver: Arc<BoxRuntimeDriver>,
        state: Arc<FileRuntimeStateStore>,
    ) -> ManagedRuntimeClient {
        ManagedRuntimeClient::new(state, driver)
    }

    pub(super) fn restarted_driver(&self) -> Result<Arc<BoxRuntimeDriver>> {
        let driver = Arc::new(
            BoxRuntimeDriver::new_with_isolation(
                driver_config(self.home_dir.clone()),
                self.execution_isolation,
            )?
            .with_secret_materializer(self.secret_materializer.clone())
            .with_artifact_port(self.artifact_port.clone()),
        );
        self.register_driver(driver.clone());
        Ok(driver)
    }

    pub(super) fn secret_materialization_calls(&self) -> usize {
        self.secret_materializer.calls.load(Ordering::SeqCst)
    }

    pub(super) fn set_secret_authorized(&self, authorized: bool) {
        self.secret_materializer
            .authorized
            .store(authorized, Ordering::SeqCst);
    }

    pub(super) fn private_artifact_source(&self) -> &Path {
        &self.artifact_port.source
    }

    pub(super) fn private_artifact_root(&self) -> &Path {
        &self.private_artifact_root
    }

    pub(super) fn output_captures(&self) -> Vec<ConformanceOutputCapture> {
        self.artifact_port.captures.lock().unwrap().clone()
    }

    pub(super) fn artifact_cleanup_calls(&self) -> Vec<String> {
        self.artifact_port.cleanups.lock().unwrap().clone()
    }

    pub(super) fn register_driver(&self, driver: Arc<BoxRuntimeDriver>) {
        self.drivers.lock().unwrap().push(driver);
    }

    pub(super) fn register_state_root(&self, root: PathBuf) {
        self.state_roots.lock().unwrap().insert(root);
    }

    pub(super) fn register_provider_home(&self, home: PathBuf) {
        self.provider_homes.lock().unwrap().insert(home);
    }

    pub(super) fn private_registry_driver(
        &self,
        home_dir: PathBuf,
    ) -> Result<Arc<BoxRuntimeDriver>> {
        let driver = Arc::new(
            BoxRuntimeDriver::new_with_isolation(
                BoxRuntimeDriverConfig {
                    secret_root: self.home_dir.join("runtime-secrets"),
                    home_dir,
                    control_timeout: Duration::from_secs(120),
                    task_poll_interval: Duration::from_millis(25),
                },
                self.execution_isolation,
            )?
            .with_secret_materializer(self.secret_materializer.clone()),
        );
        self.register_driver(driver.clone());
        Ok(driver)
    }

    pub(super) async fn cleanup_registered(&self) -> Result<()> {
        self.cleanup_all().await
    }

    pub(super) async fn record_for(&self, spec: &RuntimeUnitSpec) -> Result<crate::BoxRecord> {
        let record =
            self.driver
                .find_generation(spec)
                .await?
                .ok_or_else(|| RuntimeError::NotFound {
                    unit_id: spec.unit_id.clone(),
                })?;
        self.remember(&self.home_dir, &record);
        Ok(record)
    }

    pub(super) async fn records_for(
        &self,
        driver: &BoxRuntimeDriver,
        spec: &RuntimeUnitSpec,
    ) -> Result<Vec<crate::BoxRecord>> {
        let records = driver.unit_records(&spec.unit_id).await?;
        for record in &records {
            self.remember(driver.config.home_dir.as_path(), record);
        }
        Ok(records)
    }

    pub(super) async fn remove_unit(
        &self,
        client: &dyn RuntimeClient,
        spec: &RuntimeUnitSpec,
        label: &str,
    ) -> Result<()> {
        if spec.class == a3s_runtime::contract::RuntimeUnitClass::Service {
            let stop = self.cases.action(&format!("{label}-stop"), spec);
            let inspection = client.stop(&stop).await?;
            if let RuntimeInspection::Found { observation, .. } = inspection {
                require(
                    matches!(
                        observation.state,
                        RuntimeUnitState::Stopped
                            | RuntimeUnitState::Failed
                            | RuntimeUnitState::Unknown
                    ),
                    format!("{label} stop returned an active state"),
                )?;
            }
        }
        let remove = self.cases.action(&format!("{label}-remove"), spec);
        let removal = client.remove(&remove).await?;
        require(
            removal.unit_id == spec.unit_id && removal.generation == spec.generation,
            format!("{label} removal changed immutable identity"),
        )
    }

    pub(super) fn evidence(
        &self,
        capabilities: &RuntimeCapabilities,
        profile: RuntimeConformanceProfile,
    ) -> Result<RuntimeConformanceProfileEvidence> {
        let required = runtime_profile_requirements(capabilities, profile)?;
        Ok(RuntimeConformanceProfileEvidence {
            profile,
            case_ids: required.case_ids,
            capability_claims: required.capability_claims,
        })
    }

    fn remember(&self, home: &Path, record: &crate::BoxRecord) {
        let mut seen = self.seen.lock().unwrap();
        let entry = seen
            .entry((home.to_path_buf(), record.id.clone()))
            .or_default();
        entry.unit_id = record.labels.get(UNIT_LABEL).cloned();
        entry.pid = record.pid;
        entry.pid_start_time = record.pid_start_time;
        if record.isolation.is_sandbox() {
            if let Some(pid) = record.pid {
                if let Some(path) = crate::sandbox::capability::process_cgroup_v2_path(pid) {
                    entry.cgroup_path = Some(path);
                }
            }
        } else {
            entry.cgroup_path = Some(PathBuf::from("/sys/fs/cgroup/a3s-box").join(&record.id));
        }
        #[cfg(target_os = "linux")]
        if let Ok(Some(runtime)) =
            crate::vm::reap::load_recorded_sandbox_runtime(home, &record.box_dir, &record.id)
        {
            entry.owner_pid = runtime.owner_pid;
            entry.owner_pid_start_time = runtime.owner_pid_start_time;
            entry.log_worker_pid = runtime.log_worker_pid;
            entry.log_worker_pid_start_time = runtime.log_worker_pid_start_time;
        }
    }

    async fn provider_inventory(&self) -> Result<RuntimeConformanceInventory> {
        let drivers = self.drivers.lock().unwrap().clone();
        let mut entries = BTreeMap::new();
        for driver in &drivers {
            let records = driver
                .manager
                .managed_records()
                .await
                .map_err(|error| external("load Box managed inventory", error))?;
            for record in records {
                self.remember(&driver.config.home_dir, &record);
                let (_, generation, state) = local_identity(&record)?;
                entries.insert(
                    format!("record:{}:{}", driver.config.home_dir.display(), record.id),
                    format!("generation={} state={state}", generation.get()),
                );
            }
        }

        let seen = self.seen.lock().unwrap().clone();
        let mountinfo = std::fs::read_to_string("/proc/self/mountinfo").unwrap_or_default();
        for ((home, id), resource) in seen {
            for (kind, path) in [
                ("box-dir", home.join("boxes").join(&id)),
                ("a3s-oci-root", crate::vm::sandbox_runtime_root(&home, &id)),
                (
                    "socket-dir",
                    PathBuf::from("/tmp/a3s-box-sockets").join(&id),
                ),
            ] {
                if path.exists() {
                    entries.insert(
                        format!("{kind}:{}:{id}", home.display()),
                        path.display().to_string(),
                    );
                }
            }
            if let Some(path) = resource.cgroup_path.as_ref() {
                if path.exists() {
                    entries.insert(
                        format!("cgroup:{}:{id}", home.display()),
                        path.display().to_string(),
                    );
                }
            }
            if mountinfo.lines().any(|line| line.contains(&id)) {
                entries.insert(format!("mount:{}:{id}", home.display()), "present".into());
            }
            for (kind, pid, start_time) in [
                ("init", resource.pid, resource.pid_start_time),
                (
                    "runtime-owner",
                    resource.owner_pid,
                    resource.owner_pid_start_time,
                ),
                (
                    "log-worker",
                    resource.log_worker_pid,
                    resource.log_worker_pid_start_time,
                ),
            ] {
                if let Some(pid) = pid {
                    if crate::process::is_process_running_with_identity(pid, start_time) {
                        entries.insert(
                            format!("process:{kind}:{}:{id}", home.display()),
                            format!("pid={pid} unit_id={:?}", resource.unit_id),
                        );
                    }
                }
            }
        }

        for root in self.state_roots.lock().unwrap().iter() {
            if root.exists() {
                entries.insert(
                    format!("runtime-state:{}", root.display()),
                    directory_shape(root)?,
                );
            }
        }
        for home in self.provider_homes.lock().unwrap().iter() {
            if home.exists() {
                entries.insert(
                    format!("provider-home:{}", home.display()),
                    directory_shape(home)?,
                );
            }
        }
        Ok(RuntimeConformanceInventory { entries })
    }

    async fn cleanup_all(&self) -> Result<()> {
        let drivers = self.drivers.lock().unwrap().clone();
        let mut failures = Vec::new();
        for driver in drivers.iter().rev() {
            let records = match driver.manager.managed_records().await {
                Ok(records) => records,
                Err(error) => {
                    failures.push(format!(
                        "load cleanup inventory for {}: {error}",
                        driver.config.home_dir.display()
                    ));
                    continue;
                }
            };
            for record in records {
                self.remember(&driver.config.home_dir, &record);
                if record.exit_code != Some(0) {
                    emit_exit_diagnostics(&driver.config.home_dir, &record);
                }
                let unit_id = record
                    .labels
                    .get(UNIT_LABEL)
                    .cloned()
                    .unwrap_or_else(|| "r17-cleanup".into());
                if let Err(error) = driver.retire_record(record, &unit_id).await {
                    failures.push(format!("retire {unit_id}: {error}"));
                }
            }
        }

        for root in self.state_roots.lock().unwrap().iter() {
            if let Err(error) = remove_tree(root) {
                failures.push(format!("remove Runtime state {}: {error}", root.display()));
            }
        }
        for home in self.provider_homes.lock().unwrap().iter() {
            if let Err(error) = remove_tree(home) {
                failures.push(format!("remove provider home {}: {error}", home.display()));
            }
        }
        for root in self.fixture_roots.lock().unwrap().iter() {
            if let Err(error) = remove_tree(root) {
                failures.push(format!(
                    "remove external fixture root {}: {error}",
                    root.display()
                ));
            }
        }
        for path in [
            self.home_dir.join("boxes.json"),
            self.home_dir.join("boxes.json.lock"),
            self.home_dir.join("boxes.json.tmp"),
        ] {
            if let Err(error) = remove_file(&path) {
                failures.push(format!("remove provider state {}: {error}", path.display()));
            }
        }
        remove_empty_directory(&self.home_dir.join("boxes"));
        let volume_store = crate::VolumeStore::new(
            self.home_dir.join("volumes.json"),
            self.home_dir.join("volumes"),
        );
        match volume_store.list() {
            Ok(volumes) => {
                for volume in volumes {
                    if let Err(error) = volume_store.remove(&volume.name, false) {
                        failures.push(format!(
                            "remove conformance Volume {:?}: {error}",
                            volume.name
                        ));
                    }
                }
            }
            Err(error) => failures.push(format!("load conformance Volumes: {error}")),
        }
        for path in [
            self.home_dir.join("volumes.json"),
            self.home_dir.join("volumes.json.lock"),
            self.home_dir.join("volumes.json.tmp"),
        ] {
            if let Err(error) = remove_file(&path) {
                failures.push(format!("remove Volume state {}: {error}", path.display()));
            }
        }
        remove_empty_directory(&self.home_dir.join("volumes"));
        remove_empty_directory(&self.home_dir.join("run/a3s-oci"));
        remove_empty_directory(&self.home_dir.join("run"));

        if failures.is_empty() {
            Ok(())
        } else {
            Err(failure(format!(
                "R17 cleanup was incomplete: {}",
                failures.join("; ")
            )))
        }
    }
}

#[async_trait]
impl RuntimeConformanceFixture for BoxRuntimeConformanceFixture {
    fn base_case(&self) -> &a3s_runtime::RuntimeBaseConformanceCase {
        &self.base_case
    }

    fn available_profiles(&self) -> BTreeSet<RuntimeConformanceProfile> {
        BTreeSet::from([
            RuntimeConformanceProfile::Recovery,
            RuntimeConformanceProfile::Networking,
            RuntimeConformanceProfile::Mounts,
            RuntimeConformanceProfile::Health,
            RuntimeConformanceProfile::Resources,
            RuntimeConformanceProfile::Logs,
            RuntimeConformanceProfile::Exec,
            RuntimeConformanceProfile::Security,
            RuntimeConformanceProfile::Outputs,
        ])
    }

    async fn inventory(&self) -> RuntimeResult<RuntimeConformanceInventory> {
        self.provider_inventory().await
    }

    async fn run_profile(
        &self,
        client: &dyn RuntimeClient,
        capabilities: &RuntimeCapabilities,
        profile: RuntimeConformanceProfile,
    ) -> RuntimeResult<RuntimeConformanceProfileEvidence> {
        let result = match profile {
            RuntimeConformanceProfile::Recovery => super::recovery_profile::run(self, client).await,
            RuntimeConformanceProfile::Networking => {
                super::networking_profile::run(self, client).await
            }
            RuntimeConformanceProfile::Mounts => super::mounts_profile::run(self, client).await,
            RuntimeConformanceProfile::Health => super::health_profile::run(self, client).await,
            RuntimeConformanceProfile::Resources => {
                super::resources_profile::run(self, client).await
            }
            RuntimeConformanceProfile::Logs => super::logs_profile::run(self, client).await,
            RuntimeConformanceProfile::Exec => super::exec_profile::run(self, client).await,
            RuntimeConformanceProfile::Security => super::security_profile::run(self, client).await,
            RuntimeConformanceProfile::Outputs => super::outputs_profile::run(self, client).await,
            unsupported => {
                return Err(RuntimeError::Protocol(format!(
                    "Box R17 fixture cannot execute unexpected {} profile",
                    unsupported.as_str()
                )))
            }
        };
        result.map_err(|error| {
            RuntimeError::Protocol(format!(
                "Box R17 {} profile failed: {error}",
                profile.as_str()
            ))
        })?;
        self.evidence(capabilities, profile)
    }

    async fn cleanup(&self) -> RuntimeResult<()> {
        self.cleanup_all().await
    }
}

fn driver_config(home_dir: PathBuf) -> BoxRuntimeDriverConfig {
    BoxRuntimeDriverConfig {
        secret_root: home_dir.join("runtime-secrets"),
        home_dir,
        control_timeout: Duration::from_secs(120),
        task_poll_interval: Duration::from_millis(25),
    }
}

#[cfg(unix)]
fn set_private_artifact_modes(root: &Path, source: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;

    std::fs::set_permissions(root, std::fs::Permissions::from_mode(0o700))
        .map_err(|error| external("protect private Artifact root", error))?;
    std::fs::set_permissions(source, std::fs::Permissions::from_mode(0o755))
        .map_err(|error| external("make private Artifact mount readable", error))
}

#[cfg(not(unix))]
fn set_private_artifact_modes(_root: &Path, _source: &Path) -> Result<()> {
    Err(failure(
        "R17 private Artifact attachment certification requires Unix permissions",
    ))
}

fn validate_runtime_assets(home_dir: &Path, execution_isolation: ExecutionIsolation) -> Result<()> {
    for binary in ["a3s-box-guest-init", "a3s-box-shim"] {
        let path = home_dir.join("bin").join(binary);
        require(
            path.is_file(),
            format!("required R17 binary is missing: {}", path.display()),
        )?;
    }

    match execution_isolation {
        ExecutionIsolation::Microvm => {
            crate::host_check::check_virtualization_support()
                .map_err(|error| failure(format!("R17 MicroVM preflight failed: {error}")))?;
            Ok(())
        }
        ExecutionIsolation::Sandbox => validate_sandbox_runtime_assets(home_dir),
    }
}

fn validate_sandbox_runtime_assets(home_dir: &Path) -> Result<()> {
    let mut artifacts = Vec::new();
    for (variable, binary) in [
        ("A3S_BOX_OCI_RUNTIME_PATH", "a3s-oci"),
        ("A3S_BOX_OCI_AGENT_PATH", "a3s-oci-agent"),
    ] {
        let configured = std::env::var_os(variable)
            .map(PathBuf::from)
            .ok_or_else(|| failure(format!("{variable} must select {binary}")))?;
        let expected = home_dir.join("bin").join(binary);
        let canonical_configured = configured
            .canonicalize()
            .map_err(|error| external(variable, error))?;
        let canonical_expected = expected
            .canonicalize()
            .map_err(|error| external("canonicalize packaged A3S OCI artifact", error))?;
        require(
            canonical_configured == canonical_expected,
            format!("{variable} must equal A3S_HOME/bin/{binary}"),
        )?;
        artifacts.push(canonical_configured);
    }
    let snapshot = crate::sandbox::probe_sandbox_capabilities_for(
        a3s_box_core::ExecutionBackend::A3sOci,
        artifacts.first().map(PathBuf::as_path),
        artifacts.get(1).map(PathBuf::as_path),
    );
    snapshot
        .require_ready()
        .map_err(|error| failure(error.to_string()))
}

fn directory_shape(path: &Path) -> Result<String> {
    let mut entries = std::fs::read_dir(path)
        .map_err(|error| external("read inventory directory", error))?
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().into_owned())
        .collect::<Vec<_>>();
    entries.sort();
    Ok(entries.join(","))
}

fn remove_tree(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_dir_all(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_file(path: &Path) -> std::io::Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error),
    }
}

fn remove_empty_directory(path: &Path) {
    let _ = std::fs::remove_dir(path);
}

fn emit_exit_diagnostics(home_dir: &Path, record: &crate::BoxRecord) {
    let unit_id = record.labels.get(UNIT_LABEL).map(String::as_str);
    let volumes = record
        .managed_execution
        .as_ref()
        .map(|metadata| metadata.request.config.volumes.as_slice())
        .unwrap_or_default();
    eprintln!(
        "R17 unsuccessful-exit diagnostics: unit_id={unit_id:?} id={} status={} pid={:?} pid_start_time={:?} box_dir={} persisted_exit_code={:?} volumes={volumes:?}",
        record.id,
        record.status,
        record.pid,
        record.pid_start_time,
        record.box_dir.display(),
        crate::rootfs::read_persisted_exit_code(&record.box_dir),
    );

    let stderr_console = a3s_box_core::log::stderr_console_path(&record.console_log);
    for (label, path) in [
        ("console stdout", record.console_log.clone()),
        ("console stderr", stderr_console),
        (
            "MicroVM shim stdout",
            record.box_dir.join("logs/shim.stdout.log"),
        ),
        (
            "MicroVM shim stderr",
            record.box_dir.join("logs/shim.stderr.log"),
        ),
        ("MicroVM init", record.box_dir.join("logs/init-rust.log")),
        (
            "MicroVM root init",
            record.box_dir.join("rootfs/init-rust.log"),
        ),
        (
            "MicroVM root var-log init",
            record.box_dir.join("rootfs/var/log/init-rust.log"),
        ),
        ("Sandbox init", record.box_dir.join("logs/sandbox-init.log")),
        (
            "Sandbox log worker",
            record.box_dir.join("logs/sandbox-log-worker.log"),
        ),
        (
            "Sandbox runtime",
            record.box_dir.join("sandbox/runtime.json"),
        ),
        (
            "A3S OCI generation",
            crate::vm::sandbox_runtime_root(home_dir, &record.id).join("record.json"),
        ),
    ] {
        let Ok(bytes) = std::fs::read(&path) else {
            continue;
        };
        const MAX_DIAGNOSTIC_BYTES: usize = 16 * 1024;
        let start = bytes.len().saturating_sub(MAX_DIAGNOSTIC_BYTES);
        eprintln!(
            "R17 {label} diagnostics ({}):\n{}",
            path.display(),
            String::from_utf8_lossy(&bytes[start..]),
        );
    }
}
