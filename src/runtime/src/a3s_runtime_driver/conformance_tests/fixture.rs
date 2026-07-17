use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a3s_runtime::contract::{
    RuntimeCapabilities, RuntimeInspection, RuntimeUnitSpec, RuntimeUnitState,
};
use a3s_runtime::{
    runtime_profile_requirements, FileRuntimeStateStore, ManagedRuntimeClient, RuntimeClient,
    RuntimeConformanceFixture, RuntimeConformanceInventory, RuntimeConformanceProfile,
    RuntimeConformanceProfileEvidence, RuntimeError, RuntimeResult,
};
use async_trait::async_trait;

use super::super::metadata::{local_identity, UNIT_LABEL};
use super::super::{BoxRuntimeDriver, BoxRuntimeDriverConfig};
use super::cases::CaseFactory;
use super::{external, failure, require, Result};

#[derive(Debug, Clone, Default)]
struct SeenResource {
    pid: Option<u32>,
    pid_start_time: Option<u64>,
    log_worker_pid: Option<u32>,
    log_worker_pid_start_time: Option<u64>,
}

pub(super) struct BoxRuntimeConformanceFixture {
    pub(super) home_dir: PathBuf,
    pub(super) driver: Arc<BoxRuntimeDriver>,
    pub(super) state: Arc<FileRuntimeStateStore>,
    pub(super) cases: CaseFactory,
    base_case: a3s_runtime::RuntimeBaseConformanceCase,
    drivers: Mutex<Vec<Arc<BoxRuntimeDriver>>>,
    state_roots: Mutex<BTreeSet<PathBuf>>,
    removable_homes: Mutex<BTreeSet<PathBuf>>,
    seen: Mutex<BTreeMap<(PathBuf, String), SeenResource>>,
}

impl BoxRuntimeConformanceFixture {
    pub(super) fn from_environment() -> Result<Self> {
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
        validate_runtime_assets(&home_dir)?;

        let state_root = home_dir.join("runtime-state");
        require(
            !state_root.exists(),
            "dedicated R17 Runtime state root already exists",
        )?;
        let prefix = format!("r17-{}", uuid::Uuid::new_v4().simple());
        let cases = CaseFactory::from_environment(prefix)?;
        let base_case = cases.base_case();
        base_case.validate().map_err(super::invalid)?;

        let config = driver_config(home_dir.clone());
        let driver = Arc::new(BoxRuntimeDriver::new(config)?);
        let state = Arc::new(FileRuntimeStateStore::new(&state_root));
        Ok(Self {
            home_dir,
            driver: driver.clone(),
            state,
            cases,
            base_case,
            drivers: Mutex::new(vec![driver]),
            state_roots: Mutex::new(BTreeSet::from([state_root])),
            removable_homes: Mutex::new(BTreeSet::new()),
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
        let driver = Arc::new(BoxRuntimeDriver::new(driver_config(self.home_dir.clone()))?);
        self.register_driver(driver.clone());
        Ok(driver)
    }

    pub(super) fn register_driver(&self, driver: Arc<BoxRuntimeDriver>) {
        self.drivers.lock().unwrap().push(driver);
    }

    pub(super) fn register_state_root(&self, root: PathBuf) {
        self.state_roots.lock().unwrap().insert(root);
    }

    pub(super) fn register_removable_home(&self, home: PathBuf) {
        self.removable_homes.lock().unwrap().insert(home);
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
        entry.pid = record.pid;
        entry.pid_start_time = record.pid_start_time;
        #[cfg(target_os = "linux")]
        if let Ok(Some(runtime)) =
            crate::vm::reap::load_recorded_sandbox_runtime(home, &record.box_dir, &record.id)
        {
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
                ("crun-root", home.join("run/crun").join(&id)),
                (
                    "socket-dir",
                    PathBuf::from("/tmp/a3s-box-sockets").join(&id),
                ),
                ("cgroup", PathBuf::from("/sys/fs/cgroup/a3s-box").join(&id)),
            ] {
                if path.exists() {
                    entries.insert(
                        format!("{kind}:{}:{id}", home.display()),
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
                    "log-worker",
                    resource.log_worker_pid,
                    resource.log_worker_pid_start_time,
                ),
            ] {
                if let Some(pid) = pid {
                    if crate::process::is_process_alive_with_identity(pid, start_time) {
                        entries.insert(
                            format!("process:{kind}:{}:{id}", home.display()),
                            pid.to_string(),
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
        for home in self.removable_homes.lock().unwrap().iter() {
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
        for home in self.removable_homes.lock().unwrap().iter() {
            if let Err(error) = remove_tree(home) {
                failures.push(format!("remove provider home {}: {error}", home.display()));
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
        remove_empty_directory(&self.home_dir.join("run/crun"));
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
            RuntimeConformanceProfile::Resources,
            RuntimeConformanceProfile::Logs,
            RuntimeConformanceProfile::Exec,
            RuntimeConformanceProfile::Security,
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
        match profile {
            RuntimeConformanceProfile::Recovery => {
                super::recovery_profile::run(self, client).await?
            }
            RuntimeConformanceProfile::Networking => {
                super::networking_profile::run(self, client).await?
            }
            RuntimeConformanceProfile::Resources => {
                super::resources_profile::run(self, client).await?
            }
            RuntimeConformanceProfile::Logs => super::logs_profile::run(self, client).await?,
            RuntimeConformanceProfile::Exec => super::exec_profile::run(self, client).await?,
            RuntimeConformanceProfile::Security => {
                super::security_profile::run(self, client).await?
            }
            unsupported => {
                return Err(RuntimeError::Protocol(format!(
                    "Box R17 fixture cannot execute unexpected {} profile",
                    unsupported.as_str()
                )))
            }
        }
        self.evidence(capabilities, profile)
    }

    async fn cleanup(&self) -> RuntimeResult<()> {
        self.cleanup_all().await
    }
}

fn driver_config(home_dir: PathBuf) -> BoxRuntimeDriverConfig {
    BoxRuntimeDriverConfig {
        home_dir,
        control_timeout: Duration::from_secs(120),
        task_poll_interval: Duration::from_millis(25),
    }
}

fn validate_runtime_assets(home_dir: &Path) -> Result<()> {
    let crun = std::env::var_os("A3S_BOX_CRUN_PATH")
        .map(PathBuf::from)
        .ok_or_else(|| failure("A3S_BOX_CRUN_PATH must select the certified crun artifact"))?;
    let expected = home_dir.join("bin/crun");
    let canonical_crun = crun
        .canonicalize()
        .map_err(|error| external("canonicalize A3S_BOX_CRUN_PATH", error))?;
    let canonical_expected = expected
        .canonicalize()
        .map_err(|error| external("canonicalize A3S_HOME/bin/crun", error))?;
    require(
        canonical_crun == canonical_expected,
        "A3S_BOX_CRUN_PATH must equal A3S_HOME/bin/crun",
    )?;
    for binary in ["a3s-box-guest-init", "a3s-box-shim"] {
        let path = home_dir.join("bin").join(binary);
        require(
            path.is_file(),
            format!("required R17 binary is missing: {}", path.display()),
        )?;
    }
    let snapshot = crate::sandbox::probe_sandbox_capabilities(Some(&canonical_crun));
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
