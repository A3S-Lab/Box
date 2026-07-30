use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use a3s_runtime::contract::{
    ArtifactRef, MountKind, RestartPolicy, RuntimeMount, RuntimeMountSource, RuntimeOutputArtifact,
    RuntimeOutputSpec, RuntimeUnitClass, RuntimeUnitSpec, SecretReference, SecretTarget,
};
use a3s_runtime::{RuntimeDriver, RuntimeError};
use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::VolumeStore;

use super::mapping::creation_request_for;
use super::test_support::{
    accepted, action, fake_driver, fake_driver_with_backend, runtime_spec, unit,
};
use super::{BoxArtifactPort, BoxArtifactPortError};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct CapturedOutput {
    spec_digest: String,
    name: String,
    files: BTreeMap<String, Vec<u8>>,
}

#[derive(Default)]
struct FakeArtifactPort {
    mounts: Mutex<BTreeMap<String, PathBuf>>,
    captures: Mutex<Vec<CapturedOutput>>,
    cleanups: Mutex<Vec<String>>,
}

impl FakeArtifactPort {
    fn register_mount(&self, name: impl Into<String>, path: PathBuf) {
        self.mounts.lock().unwrap().insert(name.into(), path);
    }

    fn captures(&self) -> Vec<CapturedOutput> {
        self.captures.lock().unwrap().clone()
    }

    fn cleanups(&self) -> Vec<String> {
        self.cleanups.lock().unwrap().clone()
    }
}

#[async_trait]
impl BoxArtifactPort for FakeArtifactPort {
    async fn mount_path(
        &self,
        _spec: &RuntimeUnitSpec,
        mount: &RuntimeMount,
    ) -> Result<PathBuf, BoxArtifactPortError> {
        self.mounts
            .lock()
            .unwrap()
            .get(&mount.name)
            .cloned()
            .ok_or_else(|| BoxArtifactPortError::Rejected("fixture mount is not registered".into()))
    }

    async fn capture_output(
        &self,
        spec: &RuntimeUnitSpec,
        output: &RuntimeOutputSpec,
        source: &Path,
    ) -> Result<RuntimeOutputArtifact, BoxArtifactPortError> {
        let mut files = BTreeMap::new();
        let entries = std::fs::read_dir(source).map_err(|error| {
            BoxArtifactPortError::Unavailable(format!("fixture output read failed: {error}"))
        })?;
        for entry in entries {
            let entry = entry.map_err(|error| {
                BoxArtifactPortError::Unavailable(format!(
                    "fixture output entry read failed: {error}"
                ))
            })?;
            let metadata = std::fs::symlink_metadata(entry.path()).map_err(|error| {
                BoxArtifactPortError::Unavailable(format!(
                    "fixture output metadata read failed: {error}"
                ))
            })?;
            if !metadata.file_type().is_file() || metadata.file_type().is_symlink() {
                return Err(BoxArtifactPortError::Rejected(
                    "fixture accepts only regular root-level files".into(),
                ));
            }
            let name = entry.file_name().into_string().map_err(|_| {
                BoxArtifactPortError::Rejected("fixture output name is not UTF-8".into())
            })?;
            let value = std::fs::read(entry.path()).map_err(|error| {
                BoxArtifactPortError::Unavailable(format!(
                    "fixture output content read failed: {error}"
                ))
            })?;
            files.insert(name, value);
        }
        let size_bytes = files.values().try_fold(0_u64, |total, value| {
            total.checked_add(value.len() as u64).ok_or_else(|| {
                BoxArtifactPortError::Rejected("fixture output size overflowed".into())
            })
        })?;
        if size_bytes == 0 || size_bytes > output.max_bytes {
            return Err(BoxArtifactPortError::Rejected(
                "fixture output violates its declared bound".into(),
            ));
        }
        let mut content_digest = Sha256::new();
        for (name, value) in &files {
            content_digest.update((name.len() as u64).to_be_bytes());
            content_digest.update(name.as_bytes());
            content_digest.update((value.len() as u64).to_be_bytes());
            content_digest.update(value);
        }
        let digest = format!("sha256:{:x}", content_digest.finalize());
        self.captures.lock().unwrap().push(CapturedOutput {
            spec_digest: spec.digest().map_err(BoxArtifactPortError::Rejected)?,
            name: output.name.clone(),
            files,
        });
        Ok(RuntimeOutputArtifact {
            name: output.name.clone(),
            artifact: ArtifactRef {
                uri: format!("https://artifacts.example/a3s/task-output/{digest}"),
                digest,
                media_type: output.media_type.clone(),
            },
            size_bytes,
        })
    }

    async fn cleanup_spec(&self, spec_digest: &str) -> Result<(), BoxArtifactPortError> {
        self.cleanups.lock().unwrap().push(spec_digest.into());
        Ok(())
    }
}

fn input_artifact() -> ArtifactRef {
    let digest = format!("sha256:{}", "b".repeat(64));
    ArtifactRef {
        uri: format!("https://artifacts.example/a3s/model/{digest}"),
        digest,
        media_type: "application/vnd.a3s.directory.v1+tar".into(),
    }
}

fn output_spec() -> RuntimeOutputSpec {
    RuntimeOutputSpec {
        name: "result".into(),
        path: "/outputs/result".into(),
        media_type: "application/vnd.a3s.directory.v1+tar".into(),
        max_bytes: 1024,
    }
}

fn output_volume_path(
    home_dir: &Path,
    spec: &RuntimeUnitSpec,
    output: &RuntimeOutputSpec,
) -> PathBuf {
    let digest = spec.digest().unwrap();
    let hex = digest.strip_prefix("sha256:").unwrap();
    let name = format!(
        "a3s-runtime-output-{hex}-{:x}",
        Sha256::digest(output.name.as_bytes())
    );
    home_dir.join("volumes").join(name)
}

fn volume_store(home_dir: &Path) -> VolumeStore {
    VolumeStore::new(home_dir.join("volumes.json"), home_dir.join("volumes"))
}

#[tokio::test]
async fn artifact_port_advertises_only_the_storage_surface_it_enables() {
    let directory = tempfile::tempdir().unwrap();
    let (driver, _) = fake_driver(&directory);
    let driver = driver.with_artifact_port(Arc::new(FakeArtifactPort::default()));

    let capabilities = driver.capabilities().await.unwrap();
    assert_eq!(
        capabilities.mount_kinds,
        vec![MountKind::Artifact, MountKind::Volume, MountKind::Tmpfs]
    );
    assert!(capabilities
        .features
        .contains(&a3s_runtime::contract::RuntimeFeature::OutputArtifacts));
}

#[tokio::test]
async fn artifact_and_volume_bindings_precede_secrets_and_artifacts_are_read_only() {
    let directory = tempfile::tempdir().unwrap();
    let (driver, _) = fake_driver(&directory);
    let port = Arc::new(FakeArtifactPort::default());
    let input = directory.path().join("materialized-model");
    std::fs::create_dir(&input).unwrap();
    let input = input.canonicalize().unwrap();
    port.register_mount("model", input.clone());
    let driver = driver.with_artifact_port(port);
    let mut spec = runtime_spec("storage-order", 1, RuntimeUnitClass::Service);
    spec.mounts = vec![
        RuntimeMount {
            name: "model".into(),
            source: RuntimeMountSource::Artifact {
                artifact: input_artifact(),
            },
            target: "/models/current".into(),
            read_only: true,
        },
        RuntimeMount {
            name: "workspace".into(),
            source: RuntimeMountSource::Volume {
                volume_id: "workspace-main".into(),
            },
            target: "/workspace".into(),
            read_only: false,
        },
    ];
    spec.secrets.push(SecretReference {
        name: "token".into(),
        reference: "secret://storage/token/v1".into(),
        target: SecretTarget::File {
            path: "/run/workload/token".into(),
            mode: 0o400,
        },
    });

    let plan = driver.artifact_storage.prepare_plan(&spec).await.unwrap();
    assert_eq!(plan.volumes().len(), 2);
    assert_eq!(
        plan.volumes()[0],
        format!("{}:/models/current:ro", input.display())
    );
    assert!(plan.volumes()[1].ends_with(":/workspace:rw"));
    assert_eq!(plan.volume_names().len(), 1);

    let request = creation_request_for(
        &spec,
        driver.execution_isolation,
        &driver.config.secret_root,
        &plan,
    )
    .unwrap();
    assert_eq!(&request.config.volumes[..2], plan.volumes());
    assert!(request.config.volumes[2].ends_with(":/run/workload/token:ro"));
    assert_eq!(request.policy.volume_names, plan.volume_names());

    let mut writable = spec;
    writable.mounts[0].read_only = false;
    assert!(matches!(
        driver.artifact_storage.prepare_plan(&writable).await,
        Err(RuntimeError::InvalidRequest(message)) if message.contains("read-only")
    ));
}

#[tokio::test]
async fn missing_artifact_port_fails_before_box_reservation() {
    let directory = tempfile::tempdir().unwrap();
    let (driver, backend) = fake_driver(&directory);
    let mut spec = runtime_spec("missing-artifact-port", 1, RuntimeUnitClass::Service);
    spec.mounts.push(RuntimeMount {
        name: "model".into(),
        source: RuntimeMountSource::Artifact {
            artifact: input_artifact(),
        },
        target: "/models/current".into(),
        read_only: true,
    });

    assert!(matches!(
        driver.apply(&spec, &accepted(&spec)).await,
        Err(RuntimeError::UnsupportedCapabilities(missing))
            if missing == vec!["mount_kind:Artifact"]
    ));
    assert_eq!(backend.starts(), 0);
    assert!(driver.manager.managed_records().await.unwrap().is_empty());
}

#[tokio::test]
async fn persistent_volume_reuses_data_and_detaches_without_becoming_an_artifact_store() {
    let directory = tempfile::tempdir().unwrap();
    let (driver, _) = fake_driver(&directory);
    let mut first = runtime_spec("persistent-volume", 1, RuntimeUnitClass::Service);
    first.mounts.push(RuntimeMount {
        name: "workspace".into(),
        source: RuntimeMountSource::Volume {
            volume_id: "workspace-main".into(),
        },
        target: "/workspace".into(),
        read_only: false,
    });

    let running = driver.apply(&first, &accepted(&first)).await.unwrap();
    let store = volume_store(&driver.config.home_dir);
    let volumes = store.list().unwrap();
    assert_eq!(volumes.len(), 1);
    assert_eq!(
        volumes[0].in_use_by,
        vec![running.provider_resource_id.clone().unwrap()]
    );
    let marker = PathBuf::from(&volumes[0].mount_point).join("generation-one");
    std::fs::write(&marker, b"durable").unwrap();

    let first_unit = unit(first.clone(), running);
    driver
        .remove(&first_unit, &action("remove-first-volume", &first))
        .await
        .unwrap();
    let detached = store.list().unwrap();
    assert_eq!(detached.len(), 1);
    assert!(detached[0].in_use_by.is_empty());
    assert_eq!(std::fs::read(&marker).unwrap(), b"durable");

    let mut second = first;
    second.generation = 2;
    let running = driver.apply(&second, &accepted(&second)).await.unwrap();
    let reused = store.list().unwrap();
    assert_eq!(reused.len(), 1);
    assert_eq!(std::fs::read(&marker).unwrap(), b"durable");
    assert_eq!(
        reused[0].in_use_by,
        vec![running.provider_resource_id.clone().unwrap()]
    );

    driver
        .remove(
            &unit(second.clone(), running),
            &action("remove-second-volume", &second),
        )
        .await
        .unwrap();
    assert!(store.list().unwrap()[0].in_use_by.is_empty());
}

#[tokio::test]
async fn task_output_restart_resets_staging_and_replay_recaptures_one_exact_result() {
    let directory = tempfile::tempdir().unwrap();
    let (driver, backend) = fake_driver(&directory);
    let port = Arc::new(FakeArtifactPort::default());
    let driver = driver.with_artifact_port(port.clone());
    let mut spec = runtime_spec("task-output-restart", 1, RuntimeUnitClass::Task);
    spec.restart = RestartPolicy::OnFailure { max_retries: 1 };
    spec.outputs.push(output_spec());
    let output_dir = output_volume_path(&driver.config.home_dir, &spec, &spec.outputs[0]);

    backend.write_on_next_start(output_dir.join("failed-attempt"), b"stale".to_vec());
    backend.finish_next_start(17);
    backend.write_on_next_start(output_dir.join("result.json"), b"fresh".to_vec());
    backend.finish_next_start(0);

    let succeeded = driver.apply(&spec, &accepted(&spec)).await.unwrap();
    assert_eq!(backend.starts(), 2);
    assert_eq!(succeeded.outputs.len(), 1);
    let captures = port.captures();
    assert_eq!(captures.len(), 1);
    assert_eq!(captures[0].name, "result");
    assert_eq!(
        captures[0].files,
        BTreeMap::from([("result.json".into(), b"fresh".to_vec())])
    );
    assert!(!output_dir.join("failed-attempt").exists());
    assert!(volume_store(&driver.config.home_dir).list().unwrap()[0]
        .in_use_by
        .is_empty());

    let reopened =
        fake_driver_with_backend(&directory, backend.clone()).with_artifact_port(port.clone());
    let replayed = reopened.apply(&spec, &succeeded).await.unwrap();
    assert_eq!(
        replayed.provider_resource_id,
        succeeded.provider_resource_id
    );
    assert_eq!(backend.starts(), 2);
    let captures = port.captures();
    assert_eq!(captures.len(), 2);
    assert_eq!(
        captures.iter().cloned().collect::<BTreeSet<_>>().len(),
        1,
        "terminal replay must publish byte-identical output identity"
    );

    reopened
        .remove(
            &unit(spec.clone(), replayed),
            &action("remove-task-output", &spec),
        )
        .await
        .unwrap();
    assert_eq!(port.cleanups(), vec![spec.digest().unwrap()]);
    assert!(volume_store(&reopened.config.home_dir)
        .list()
        .unwrap()
        .is_empty());
    assert!(!output_dir.exists());
}

#[tokio::test]
async fn failed_task_never_publishes_output_but_removal_cleans_staging() {
    let directory = tempfile::tempdir().unwrap();
    let (driver, backend) = fake_driver(&directory);
    let port = Arc::new(FakeArtifactPort::default());
    let driver = driver.with_artifact_port(port.clone());
    let mut spec = runtime_spec("failed-task-output", 1, RuntimeUnitClass::Task);
    spec.outputs.push(output_spec());
    let output_dir = output_volume_path(&driver.config.home_dir, &spec, &spec.outputs[0]);
    backend.write_on_next_start(output_dir.join("partial"), b"not-published".to_vec());
    backend.finish_next_start(23);

    let failed = driver.apply(&spec, &accepted(&spec)).await.unwrap();
    assert_eq!(
        failed.state,
        a3s_runtime::contract::RuntimeUnitState::Failed
    );
    assert!(failed.outputs.is_empty());
    assert!(port.captures().is_empty());

    driver
        .remove(
            &unit(spec.clone(), failed),
            &action("remove-failed-output", &spec),
        )
        .await
        .unwrap();
    assert!(!output_dir.exists());
    assert_eq!(port.cleanups(), vec![spec.digest().unwrap()]);
}
