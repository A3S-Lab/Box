use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use a3s_box_core::{ExecOutput, ExecRequest, ExecutionManager};
use a3s_runtime::contract::{
    HealthProbe, RestartPolicy, RuntimeHealthCheck, RuntimeHealthState, RuntimeServiceLifecycle,
    RuntimeUnitClass, RuntimeUnitState,
};
use a3s_runtime::RuntimeDriver;

use super::mapping::{creation_request, operation};
use super::test_support::{accepted, fake_driver, runtime_spec};

#[tokio::test]
async fn unhealthy_liveness_restarts_once_and_returns_new_generation_health() {
    let directory = tempfile::tempdir().unwrap();
    let (driver, backend) = fake_driver(&directory);
    let mut spec = runtime_spec("service-liveness-restart", 1, RuntimeUnitClass::Service);
    let readiness = RuntimeHealthCheck {
        probe: HealthProbe::Command {
            command: vec!["readiness".into()],
        },
        interval_ms: 500,
        timeout_ms: 500,
        start_period_ms: 0,
        success_threshold: 1,
        failure_threshold: 1,
    };
    spec.health = Some(readiness);
    spec.service_lifecycle = Some(RuntimeServiceLifecycle {
        liveness: RuntimeHealthCheck {
            probe: HealthProbe::Command {
                command: vec!["liveness".into()],
            },
            interval_ms: 500,
            timeout_ms: 500,
            start_period_ms: 0,
            success_threshold: 1,
            failure_threshold: 1,
        },
        shutdown_grace_seconds: 1,
    });
    spec.restart = RestartPolicy::Always;

    let reservation = driver
        .manager
        .create(
            creation_request(&spec, driver.execution_isolation()).unwrap(),
            &operation(&spec).unwrap(),
        )
        .await
        .unwrap();
    let record = driver
        .manager
        .managed_record(&reservation.execution_id)
        .await
        .unwrap()
        .unwrap();
    std::fs::create_dir_all(record.exec_socket_path.parent().unwrap()).unwrap();
    let listener = tokio::net::UnixListener::bind(&record.exec_socket_path).unwrap();
    let liveness_calls = Arc::new(AtomicUsize::new(0));
    let server_liveness_calls = Arc::clone(&liveness_calls);
    let (shutdown_tx, mut shutdown_rx) = tokio::sync::oneshot::channel();
    let server = tokio::spawn(async move {
        loop {
            let stream = tokio::select! {
                _ = &mut shutdown_rx => break,
                accepted = listener.accept() => accepted.unwrap().0,
            };
            let liveness_calls = Arc::clone(&server_liveness_calls);
            tokio::spawn(async move {
                let (read, write) = tokio::io::split(stream);
                let mut reader = a3s_transport::FrameReader::new(read);
                let mut writer = a3s_transport::FrameWriter::new(write);
                let frame = reader.read_frame().await.unwrap().unwrap();
                let request: ExecRequest = serde_json::from_slice(&frame.payload).unwrap();
                let exit_code = if request.cmd == ["liveness"] {
                    (liveness_calls.fetch_add(1, Ordering::SeqCst) == 0) as i32
                } else {
                    assert_eq!(request.cmd, ["readiness"]);
                    0
                };
                writer
                    .write_data(
                        &serde_json::to_vec(&ExecOutput {
                            stdout: Vec::new(),
                            stderr: Vec::new(),
                            exit_code,
                            truncated: false,
                        })
                        .unwrap(),
                    )
                    .await
                    .unwrap();
            });
        }
    });

    let observation = driver.apply(&spec, &accepted(&spec)).await.unwrap();

    assert_eq!(observation.state, RuntimeUnitState::Running);
    assert_eq!(
        observation.liveness.as_ref().map(|health| health.state),
        Some(RuntimeHealthState::Healthy)
    );
    assert_eq!(backend.starts(), 2);
    assert_eq!(liveness_calls.load(Ordering::SeqCst), 2);
    let records = driver.manager.managed_records().await.unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0]
            .managed_execution
            .as_ref()
            .unwrap()
            .generation
            .get(),
        2
    );

    shutdown_tx.send(()).unwrap();
    server.await.unwrap();
}
