use std::sync::Arc;
use std::time::Duration;

use a3s_runtime::contract::{
    RestartPolicy, RuntimeFeature, RuntimeInspection, RuntimeUnitClass, RuntimeUnitState,
    SecretReference, SecretTarget,
};
use a3s_runtime::{RuntimeDriver, RuntimeError};

use super::metadata::GENERATION_LABEL;
use super::test_support::{
    accepted, action, fake_driver, fake_driver_with_backend,
    fake_driver_with_backend_and_secret_materializer, fake_driver_with_secret_materializer,
    runtime_spec, unit, unknown, DriverFakeBackend, DriverFakeSecretMaterializer,
};

fn tmpfs_secret_root() -> (tempfile::TempDir, std::path::PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let mount = tempfile::tempdir_in("/dev/shm")
        .expect("Linux Runtime Secret tests require the standard /dev/shm tmpfs");
    let root = mount.path().join("runtime-secrets");
    std::fs::create_dir(&root).unwrap();
    std::fs::set_permissions(&root, std::fs::Permissions::from_mode(0o700)).unwrap();
    (mount, root)
}

fn environment_secret(reference: &str) -> SecretReference {
    SecretReference {
        name: "provider-token".into(),
        reference: reference.into(),
        target: SecretTarget::Environment {
            variable: "A3S_PROVIDER_TOKEN".into(),
        },
    }
}

fn registry_secret(reference: &str) -> SecretReference {
    SecretReference {
        name: "registry-credential".into(),
        reference: reference.into(),
        target: SecretTarget::RegistryCredential,
    }
}

#[tokio::test]
async fn unconfigured_secret_port_rejects_before_provider_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let (driver, backend) = fake_driver(&directory);
    let mut spec = runtime_spec("secret-unconfigured", 1, RuntimeUnitClass::Service);
    spec.secrets
        .push(environment_secret("secret://provider/token/v1"));

    assert!(matches!(
        driver.apply(&spec, &accepted(&spec)).await,
        Err(RuntimeError::UnsupportedCapabilities(missing))
            if missing == vec!["feature:SecretReferences"]
    ));
    assert_eq!(backend.starts(), 0);
    assert!(driver.manager.managed_records().await.unwrap().is_empty());
}

#[tokio::test]
async fn secret_materialization_recovers_without_persisting_plaintext_and_cleans_on_remove() {
    let directory = tempfile::tempdir().unwrap();
    let (_secret_mount, secret_root) = tmpfs_secret_root();
    let materializer = Arc::new(DriverFakeSecretMaterializer::default());
    let reference = "secret://provider/token/v7";
    let plaintext = "box-fixture-secret-plaintext";
    materializer.insert(reference, plaintext.as_bytes().to_vec());
    let (driver, backend) =
        fake_driver_with_secret_materializer(&directory, secret_root.clone(), materializer.clone());
    let mut spec = runtime_spec("secret-retry", 1, RuntimeUnitClass::Service);
    spec.secrets.push(environment_secret(reference));

    let capabilities = driver.capabilities().await.unwrap();
    assert!(capabilities
        .features
        .contains(&RuntimeFeature::SecretReferences));

    materializer.fail_next();
    assert!(matches!(
        driver.apply(&spec, &accepted(&spec)).await,
        Err(RuntimeError::ProviderUnavailable(message))
            if message.contains("temporarily unavailable")
    ));
    assert_eq!(backend.starts(), 0);
    assert_eq!(driver.manager.managed_records().await.unwrap().len(), 1);
    let materialized_directory = super::secret::secret_directory(&secret_root, &spec).unwrap();
    assert!(!materialized_directory.exists());

    let running = driver.apply(&spec, &accepted(&spec)).await.unwrap();
    assert_eq!(running.state, RuntimeUnitState::Running);
    assert_eq!(backend.starts(), 1);
    let materialized = super::secret::secret_file(&secret_root, &spec, 0).unwrap();
    assert_eq!(std::fs::read(&materialized).unwrap(), plaintext.as_bytes());

    let state = std::fs::read_to_string(driver.manager.state_path()).unwrap();
    assert!(!state.contains(plaintext));
    let record = driver.manager.managed_records().await.unwrap().remove(0);
    let creation_intent =
        serde_json::to_string(&record.managed_execution.unwrap().request).unwrap();
    assert!(!creation_intent.contains(plaintext));

    let calls_before_reconstruction = materializer.calls();
    let reopened = fake_driver_with_backend_and_secret_materializer(
        &directory,
        backend.clone(),
        secret_root.clone(),
        materializer.clone(),
    );
    let replayed = reopened.apply(&spec, &running).await.unwrap();
    assert_eq!(replayed.provider_resource_id, running.provider_resource_id);
    assert_eq!(backend.starts(), 1);
    assert_eq!(materializer.calls(), calls_before_reconstruction);
    assert!(materialized.exists());

    let running_unit = unit(spec.clone(), replayed);
    let stopped = reopened
        .stop(&running_unit, &action("secret-stop", &spec))
        .await
        .unwrap();
    assert_eq!(stopped.state, RuntimeUnitState::Stopped);
    assert!(materialized.exists(), "stop must retain restart material");

    reopened
        .remove(
            &unit(spec.clone(), stopped),
            &action("secret-remove", &spec),
        )
        .await
        .unwrap();
    assert!(!materialized_directory.exists());
    assert!(reopened.manager.managed_records().await.unwrap().is_empty());
}

#[tokio::test]
async fn stale_generation_retirement_removes_only_its_secret_material() {
    let directory = tempfile::tempdir().unwrap();
    let (_secret_mount, secret_root) = tmpfs_secret_root();
    let materializer = Arc::new(DriverFakeSecretMaterializer::default());
    materializer.insert("secret://provider/token/v1", b"generation-one".to_vec());
    materializer.insert("secret://provider/token/v2", b"generation-two".to_vec());
    let (driver, _backend) =
        fake_driver_with_secret_materializer(&directory, secret_root.clone(), materializer);
    let mut first = runtime_spec("secret-handoff", 1, RuntimeUnitClass::Service);
    first
        .secrets
        .push(environment_secret("secret://provider/token/v1"));
    driver.apply(&first, &accepted(&first)).await.unwrap();
    let first_directory = super::secret::secret_directory(&secret_root, &first).unwrap();
    assert!(first_directory.exists());

    let mut second = runtime_spec("secret-handoff", 2, RuntimeUnitClass::Service);
    second.process.args = vec!["-c".into(), "echo generation-two".into()];
    second
        .secrets
        .push(environment_secret("secret://provider/token/v2"));
    let second_running = driver.apply(&second, &accepted(&second)).await.unwrap();
    let second_directory = super::secret::secret_directory(&secret_root, &second).unwrap();

    assert!(!first_directory.exists());
    assert!(second_directory.exists());
    let records = driver.manager.managed_records().await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        second_running.provider_resource_id.as_deref(),
        Some(records[0].id.as_str())
    );

    driver
        .remove(
            &unit(second.clone(), second_running),
            &action("secret-handoff-remove", &second),
        )
        .await
        .unwrap();
    assert!(!second_directory.exists());
}

#[tokio::test]
async fn registry_credentials_resolve_only_for_an_uncached_start_and_never_persist() {
    let directory = tempfile::tempdir().unwrap();
    let secret_root = directory.path().join("runtime-secrets");
    let materializer = Arc::new(DriverFakeSecretMaterializer::default());
    let reference = "secret://registry/credential/v7";
    materializer.insert_registry_credential(
        reference,
        "box-registry-user",
        "box-registry-password",
    );
    let (driver, backend) =
        fake_driver_with_secret_materializer(&directory, secret_root.clone(), materializer.clone());
    let mut spec = runtime_spec("registry-secret", 1, RuntimeUnitClass::Service);
    spec.secrets.push(registry_secret(reference));

    materializer.fail_next();
    assert!(matches!(
        driver.apply(&spec, &accepted(&spec)).await,
        Err(RuntimeError::ProviderUnavailable(message))
            if message.contains("temporarily unavailable")
    ));
    assert_eq!(backend.starts(), 0);
    assert_eq!(
        driver.transient_registry_auth.as_ref().unwrap().pending(),
        0
    );

    backend.fail_next_start_response();
    let running = driver.apply(&spec, &accepted(&spec)).await.unwrap();
    assert_eq!(running.state, RuntimeUnitState::Running);
    assert_eq!(backend.starts(), 1);
    assert_eq!(materializer.calls(), 2);
    assert_eq!(
        driver.transient_registry_auth.as_ref().unwrap().pending(),
        0
    );
    assert!(!secret_root.exists());

    let state = std::fs::read_to_string(driver.manager.state_path()).unwrap();
    assert!(!state.contains("box-registry-user"));
    assert!(!state.contains("box-registry-password"));
    assert!(!state.contains(reference));

    let reopened = fake_driver_with_backend_and_secret_materializer(
        &directory,
        backend.clone(),
        secret_root,
        materializer.clone(),
    );
    let replayed = reopened.apply(&spec, &running).await.unwrap();
    assert_eq!(replayed.provider_resource_id, running.provider_resource_id);
    assert_eq!(materializer.calls(), 2);

    let provider_id = replayed.provider_resource_id.clone().unwrap();
    backend.finish(&provider_id, 9);
    let inspection = reopened
        .inspect(&unit(spec.clone(), replayed))
        .await
        .unwrap();
    assert!(matches!(
        inspection,
        RuntimeInspection::Found { ref observation, .. }
            if observation.state == RuntimeUnitState::Running
    ));
    assert_eq!(backend.starts(), 2);
    assert_eq!(materializer.calls(), 3);
    assert_eq!(
        reopened.transient_registry_auth.as_ref().unwrap().pending(),
        0
    );

    let state = std::fs::read_to_string(reopened.manager.state_path()).unwrap();
    assert!(!state.contains("box-registry-user"));
    assert!(!state.contains("box-registry-password"));
}

#[tokio::test]
async fn cached_registry_artifact_does_not_resolve_its_credential() {
    let directory = tempfile::tempdir().unwrap();
    let home = directory.path().join("home");
    let images = home.join("images");
    let source = directory.path().join("cached-image");
    std::fs::create_dir_all(&source).unwrap();
    std::fs::write(source.join("fixture"), b"cached").unwrap();
    let digest = format!("sha256:{}", "a".repeat(64));
    let reference = format!("registry.example/a3s/runtime@{digest}");
    let store = crate::ImageStore::new(&images, crate::DEFAULT_IMAGE_CACHE_SIZE).unwrap();
    store.put(&reference, &digest, &source).await.unwrap();

    let materializer = Arc::new(DriverFakeSecretMaterializer::default());
    let (driver, backend) = fake_driver_with_secret_materializer(
        &directory,
        directory.path().join("runtime-secrets"),
        materializer.clone(),
    );
    let mut spec = runtime_spec("cached-registry-secret", 1, RuntimeUnitClass::Service);
    spec.secrets
        .push(registry_secret("secret://registry/credential/unregistered"));

    let running = driver.apply(&spec, &accepted(&spec)).await.unwrap();
    assert_eq!(running.state, RuntimeUnitState::Running);
    assert_eq!(backend.starts(), 1);
    assert_eq!(materializer.calls(), 0);
}

#[tokio::test]
async fn service_replay_reopens_the_same_identity_and_stop_remove_are_idempotent() {
    let directory = tempfile::tempdir().unwrap();
    let (driver, backend) = fake_driver(&directory);
    let spec = runtime_spec("service-replay", 1, RuntimeUnitClass::Service);

    let running = driver.apply(&spec, &accepted(&spec)).await.unwrap();
    assert_eq!(running.state, RuntimeUnitState::Running);
    assert_eq!(backend.starts(), 1);
    let provider_id = running.provider_resource_id.clone().unwrap();

    let reopened = fake_driver_with_backend(&directory, backend.clone());
    let replayed = reopened.apply(&spec, &running).await.unwrap();
    assert_eq!(
        replayed.provider_resource_id.as_deref(),
        Some(provider_id.as_str())
    );
    assert_eq!(backend.starts(), 1);
    assert_eq!(reopened.manager.managed_records().await.unwrap().len(), 1);

    let running_unit = unit(spec.clone(), replayed);
    let stopped = reopened
        .stop(&running_unit, &action("service-stop", &spec))
        .await
        .unwrap();
    assert_eq!(stopped.state, RuntimeUnitState::Stopped);
    assert_eq!(backend.kills(), 1);

    let stopped_unit = unit(spec.clone(), stopped.clone());
    let stop_replay = reopened
        .stop(&stopped_unit, &action("service-stop-replay", &spec))
        .await
        .unwrap();
    assert_eq!(stop_replay, stopped);
    assert_eq!(backend.kills(), 1);

    let removal = reopened
        .remove(&stopped_unit, &action("service-remove", &spec))
        .await
        .unwrap();
    assert!(!removal.already_absent);
    let replayed_removal = reopened
        .remove(&stopped_unit, &action("service-remove-replay", &spec))
        .await
        .unwrap();
    assert!(replayed_removal.already_absent);
    assert!(reopened.manager.managed_records().await.unwrap().is_empty());
}

#[tokio::test]
async fn generation_handoff_leaves_exactly_one_current_execution() {
    let directory = tempfile::tempdir().unwrap();
    let (driver, backend) = fake_driver(&directory);
    let first = runtime_spec("generation-handoff", 1, RuntimeUnitClass::Service);
    let first_running = driver.apply(&first, &accepted(&first)).await.unwrap();
    let first_provider_id = first_running.provider_resource_id.unwrap();

    let mut second = runtime_spec("generation-handoff", 2, RuntimeUnitClass::Service);
    second.process.args = vec!["-c".into(), "echo generation-2".into()];
    let second_running = driver.apply(&second, &accepted(&second)).await.unwrap();
    let second_provider_id = second_running.provider_resource_id.clone().unwrap();
    assert_ne!(second_provider_id, first_provider_id);
    assert_eq!(backend.starts(), 2);

    let records = driver.manager.managed_records().await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, second_provider_id);
    assert_eq!(
        records[0].labels.get(GENERATION_LABEL).map(String::as_str),
        Some("2")
    );

    let replayed = driver.apply(&second, &second_running).await.unwrap();
    assert_eq!(
        replayed.provider_resource_id,
        second_running.provider_resource_id
    );
    assert_eq!(backend.starts(), 2);
    assert_eq!(driver.manager.managed_records().await.unwrap().len(), 1);
}

#[tokio::test]
async fn tasks_report_success_failure_and_timeout_with_terminal_evidence() {
    let success_directory = tempfile::tempdir().unwrap();
    let (success_driver, success_backend) = fake_driver(&success_directory);
    let success_spec = runtime_spec("task-success", 1, RuntimeUnitClass::Task);
    success_backend.finish_next_start(0);
    let succeeded = success_driver
        .apply(&success_spec, &accepted(&success_spec))
        .await
        .unwrap();
    assert_eq!(succeeded.state, RuntimeUnitState::Succeeded);
    assert!(succeeded.finished_at_ms.is_some());
    assert!(succeeded.failure.is_none());

    let failure_directory = tempfile::tempdir().unwrap();
    let (failure_driver, failure_backend) = fake_driver(&failure_directory);
    let failure_spec = runtime_spec("task-failure", 1, RuntimeUnitClass::Task);
    failure_backend.finish_next_start(17);
    let failed = failure_driver
        .apply(&failure_spec, &accepted(&failure_spec))
        .await
        .unwrap();
    assert_eq!(failed.state, RuntimeUnitState::Failed);
    assert_eq!(failed.failure.as_ref().unwrap().code, "sandbox_exit");
    assert!(failed.failure.as_ref().unwrap().message.contains("17"));

    let timeout_directory = tempfile::tempdir().unwrap();
    let (timeout_driver, timeout_backend) = fake_driver(&timeout_directory);
    let mut timeout_spec = runtime_spec("task-timeout", 1, RuntimeUnitClass::Task);
    timeout_spec.resources.execution_timeout_ms = Some(25);
    let timed_out = timeout_driver
        .apply(&timeout_spec, &accepted(&timeout_spec))
        .await
        .unwrap();
    assert_eq!(timed_out.state, RuntimeUnitState::Failed);
    assert_eq!(
        timed_out.failure.as_ref().unwrap().code,
        "execution_timeout"
    );
    assert!(!timed_out.failure.as_ref().unwrap().retryable);
    assert_eq!(timeout_backend.kills(), 1);
}

#[tokio::test]
async fn service_failure_restarts_the_same_durable_execution() {
    let directory = tempfile::tempdir().unwrap();
    let (driver, backend) = fake_driver(&directory);
    let spec = runtime_spec("service-restart", 1, RuntimeUnitClass::Service);
    let running = driver.apply(&spec, &accepted(&spec)).await.unwrap();
    let provider_id = running.provider_resource_id.clone().unwrap();
    backend.finish(&provider_id, 9);

    let inspection = driver.inspect(&unit(spec.clone(), running)).await.unwrap();
    let RuntimeInspection::Found { observation, .. } = inspection else {
        panic!("restartable Service disappeared")
    };
    assert_eq!(observation.state, RuntimeUnitState::Running);
    assert_eq!(
        observation.provider_resource_id.as_deref(),
        Some(provider_id.as_str())
    );
    assert_eq!(backend.starts(), 2);
    assert_eq!(driver.manager.managed_records().await.unwrap().len(), 1);
}

#[tokio::test]
async fn restart_completion_during_startup_uses_persisted_terminal_evidence() {
    let directory = tempfile::tempdir().unwrap();
    let (driver, backend) = fake_driver(&directory);
    let spec = runtime_spec("restart-startup-completion", 1, RuntimeUnitClass::Service);
    let running = driver.apply(&spec, &accepted(&spec)).await.unwrap();
    let provider_id = running.provider_resource_id.clone().unwrap();
    backend.finish(&provider_id, 17);
    backend.finish_next_start(0);
    backend.fail_next_start_response();

    let inspection = driver.inspect(&unit(spec.clone(), running)).await.unwrap();

    let RuntimeInspection::Found { observation, .. } = inspection else {
        panic!("restart completion disappeared")
    };
    assert_eq!(observation.state, RuntimeUnitState::Stopped);
    assert_eq!(
        observation.provider_resource_id.as_deref(),
        Some(provider_id.as_str())
    );
    assert_eq!(backend.starts(), 2);
    let records = driver.manager.managed_records().await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].exit_code, Some(0));
    assert_eq!(
        records[0]
            .managed_execution
            .as_ref()
            .unwrap()
            .generation
            .get(),
        2
    );
}

#[tokio::test]
async fn task_restart_completion_during_startup_reports_the_second_generation() {
    let directory = tempfile::tempdir().unwrap();
    let (driver, backend) = fake_driver(&directory);
    let mut spec = runtime_spec("task-restart-startup-completion", 1, RuntimeUnitClass::Task);
    spec.restart = RestartPolicy::OnFailure { max_retries: 1 };
    backend.finish_next_start(17);
    backend.finish_next_start(0);
    backend.fail_start_response_at(2);

    let observation = driver.apply(&spec, &accepted(&spec)).await.unwrap();

    assert_eq!(observation.state, RuntimeUnitState::Succeeded);
    assert_eq!(backend.starts(), 2);
    let records = driver.manager.managed_records().await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].exit_code, Some(0));
    assert_eq!(
        records[0]
            .managed_execution
            .as_ref()
            .unwrap()
            .generation
            .get(),
        2
    );
}

#[tokio::test]
async fn response_loss_reattaches_and_confirmed_provider_loss_replaces_once() {
    let response_loss_directory = tempfile::tempdir().unwrap();
    let (response_loss_driver, response_loss_backend) = fake_driver(&response_loss_directory);
    let response_loss_spec = runtime_spec("start-response-loss", 1, RuntimeUnitClass::Service);
    response_loss_backend.fail_next_start_response();
    let recovered = response_loss_driver
        .apply(&response_loss_spec, &accepted(&response_loss_spec))
        .await
        .unwrap();
    assert_eq!(recovered.state, RuntimeUnitState::Running);
    assert_eq!(response_loss_backend.starts(), 1);
    assert_eq!(
        response_loss_driver
            .manager
            .managed_records()
            .await
            .unwrap()
            .len(),
        1
    );

    let loss_directory = tempfile::tempdir().unwrap();
    let (loss_driver, loss_backend) = fake_driver(&loss_directory);
    let loss_spec = runtime_spec("confirmed-provider-loss", 1, RuntimeUnitClass::Service);
    let running = loss_driver
        .apply(&loss_spec, &accepted(&loss_spec))
        .await
        .unwrap();
    let lost_provider_id = running.provider_resource_id.clone().unwrap();
    loss_backend.lose(&lost_provider_id);
    let inspection = loss_driver
        .inspect(&unit(loss_spec.clone(), running.clone()))
        .await
        .unwrap();
    assert!(matches!(inspection, RuntimeInspection::NotFound { .. }));

    let replacement = loss_driver
        .apply(&loss_spec, &unknown(&running))
        .await
        .unwrap();
    assert_eq!(replacement.state, RuntimeUnitState::Running);
    assert_ne!(
        replacement.provider_resource_id.as_deref(),
        Some(lost_provider_id.as_str())
    );
    assert_eq!(loss_backend.starts(), 2);
    assert_eq!(
        loss_driver.manager.managed_records().await.unwrap().len(),
        1
    );

    let replayed = loss_driver.apply(&loss_spec, &replacement).await.unwrap();
    assert_eq!(
        replayed.provider_resource_id,
        replacement.provider_resource_id
    );
    assert_eq!(loss_backend.starts(), 2);
    assert_eq!(
        loss_driver.manager.managed_records().await.unwrap().len(),
        1
    );
}

#[tokio::test]
async fn startup_failure_without_exit_code_preserves_the_provider_error() {
    let directory = tempfile::tempdir().unwrap();
    let (driver, backend) = fake_driver(&directory);
    let spec = runtime_spec("startup-provider-failure", 1, RuntimeUnitClass::Task);
    backend.fail_next_start_without_exit_code();

    let result = driver.apply(&spec, &accepted(&spec)).await;

    assert!(matches!(
        result,
        Err(RuntimeError::ProviderUnavailable(message))
            if message == "fake start response was lost"
    ));
}

#[tokio::test]
async fn cancelled_task_inspection_completes_before_replay() {
    let directory = tempfile::tempdir().unwrap();
    let backend = Arc::new(DriverFakeBackend::default());
    backend.arm_cancelled_inspection();
    let driver = Arc::new(fake_driver_with_backend(&directory, Arc::clone(&backend)));
    let mut spec = runtime_spec("cancelled-task-inspection", 1, RuntimeUnitClass::Task);
    spec.resources.execution_timeout_ms = Some(2_000);
    let current = accepted(&spec);
    let apply = {
        let driver = Arc::clone(&driver);
        let spec = spec.clone();
        let current = current.clone();
        tokio::spawn(async move { driver.apply(&spec, &current).await })
    };

    backend.wait_for_cancelled_inspection().await;
    let records = driver.manager.managed_records().await.unwrap();
    assert_eq!(records.len(), 1);
    let provider_id = records[0].id.clone();

    apply.abort();
    assert!(apply.await.unwrap_err().is_cancelled());

    let restarted = Arc::new(fake_driver_with_backend(&directory, Arc::clone(&backend)));
    let mut replay = {
        let restarted = Arc::clone(&restarted);
        let spec = spec.clone();
        let current = current.clone();
        tokio::spawn(async move { restarted.apply(&spec, &current).await })
    };
    let early = tokio::time::timeout(Duration::from_millis(50), &mut replay).await;
    backend.release_cancelled_inspection();
    let (waited_for_original_inspection, recovered) = match early {
        Ok(result) => (false, result.unwrap().unwrap()),
        Err(_) => (
            true,
            tokio::time::timeout(Duration::from_secs(2), replay)
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        ),
    };

    assert!(
        waited_for_original_inspection,
        "replay bypassed the cancelled inspection's lifecycle lock"
    );
    assert_eq!(recovered.state, RuntimeUnitState::Succeeded);
    assert_eq!(
        recovered.provider_resource_id.as_deref(),
        Some(provider_id.as_str())
    );
    let records = restarted.manager.managed_records().await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].id, provider_id);
}
