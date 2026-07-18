use a3s_box_core::exec::{ExecOutput, ExecRequest};
use a3s_runtime::contract::{RuntimeExecRequest, RuntimeUnitClass, RuntimeUnitState};
use a3s_runtime::{RuntimeDriver, RuntimeError};

use super::test_support::{accepted, action, fake_driver, runtime_spec, unit};

#[cfg(unix)]
#[tokio::test]
async fn exec_binds_identity_timeout_replay_and_truncation_to_the_running_generation() {
    let directory = tempfile::tempdir().unwrap();
    let (driver, _backend) = fake_driver(&directory);
    let spec = runtime_spec("exec-wire", 1, RuntimeUnitClass::Service);
    let running = driver.apply(&spec, &accepted(&spec)).await.unwrap();
    let provider_id = running.provider_resource_id.clone().unwrap();
    let record = driver.find_generation(&spec).await.unwrap().unwrap();
    std::fs::create_dir_all(record.exec_socket_path.parent().unwrap()).unwrap();
    let listener = tokio::net::UnixListener::bind(&record.exec_socket_path).unwrap();
    let (request_tx, mut request_rx) = tokio::sync::mpsc::unbounded_channel();
    let server = tokio::spawn(async move {
        for _ in 0..2 {
            let (stream, _) = listener.accept().await.unwrap();
            let (read, write) = tokio::io::split(stream);
            let mut reader = a3s_transport::FrameReader::new(read);
            let mut writer = a3s_transport::FrameWriter::new(write);
            let frame = reader.read_frame().await.unwrap().unwrap();
            assert_eq!(frame.frame_type, a3s_transport::FrameType::Data);
            let request: ExecRequest = serde_json::from_slice(&frame.payload).unwrap();
            request_tx.send(request).unwrap();
            writer
                .write_data(
                    &serde_json::to_vec(&ExecOutput {
                        stdout: b"replayed stdout\n".to_vec(),
                        stderr: b"bounded stderr\n".to_vec(),
                        exit_code: 23,
                        truncated: true,
                    })
                    .unwrap(),
                )
                .await
                .unwrap();
        }
    });

    let request = RuntimeExecRequest {
        schema: RuntimeExecRequest::SCHEMA.into(),
        request_id: "exec-request-1".into(),
        unit_id: spec.unit_id.clone(),
        generation: spec.generation,
        command: vec!["/bin/sh".into(), "-c".into(), "printf ready".into()],
        timeout_ms: 500,
        deadline_at_ms: None,
    };
    let running_unit = unit(spec.clone(), running.clone());
    let first = driver.exec(&running_unit, &request).await.unwrap();
    let replayed = driver.exec(&running_unit, &request).await.unwrap();
    assert_eq!(first.request_id, request.request_id);
    assert_eq!(
        first.observation.provider_resource_id.as_deref(),
        Some(provider_id.as_str())
    );
    assert_eq!(first.exit_code, 23);
    assert_eq!(first.stdout, "replayed stdout\n");
    assert_eq!(first.stderr, "bounded stderr\n");
    assert!(first.truncated);
    assert_eq!(replayed.exit_code, first.exit_code);
    assert_eq!(replayed.stdout, first.stdout);
    assert_eq!(replayed.stderr, first.stderr);
    assert!(replayed.truncated);

    for _ in 0..2 {
        let guest_request = request_rx.recv().await.unwrap();
        assert_eq!(guest_request.request_id.as_deref(), Some("exec-request-1"));
        assert_eq!(guest_request.cmd, request.command);
        assert_eq!(guest_request.timeout_ns, 500_000_000);
        assert!(!guest_request.streaming);
    }
    server.await.unwrap();

    let mut wrong_generation = request.clone();
    wrong_generation.generation += 1;
    assert!(matches!(
        driver.exec(&running_unit, &wrong_generation).await,
        Err(RuntimeError::InvalidRequest(message)) if message.contains("identity")
    ));

    let stopped = driver
        .stop(&running_unit, &action("exec-service-stop", &spec))
        .await
        .unwrap();
    assert_eq!(stopped.state, RuntimeUnitState::Stopped);
    assert!(matches!(
        driver.exec(&unit(spec, stopped), &request).await,
        Err(RuntimeError::InvalidRequest(message)) if message.contains("running unit")
    ));
}
