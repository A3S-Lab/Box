use a3s_box_core::ExecutionManager;
use a3s_runtime::contract::{RuntimeInspection, RuntimeUnitClass, RuntimeUnitState};
use a3s_runtime::RuntimeDriver;

use super::metadata::GENERATION_LABEL;
use super::test_support::{
    accepted, action, fake_driver, fake_driver_with_backend, runtime_spec, unit, unknown,
};

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
