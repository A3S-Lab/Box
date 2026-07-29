use std::collections::VecDeque;
use std::net::SocketAddr;
use std::num::NonZeroU16;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionManagerError, ExecutionManagerResult,
    ExecutionPortConnector, ExecutionPortStream,
};
use a3s_runtime::contract::{
    NetworkMode, RuntimeFeature, RuntimeInspection, RuntimePort, RuntimeServiceEndpoint,
    RuntimeUnitClass, TransportProtocol,
};
use a3s_runtime::{RuntimeDriver, RuntimeError};
use async_trait::async_trait;
use tokio::io::{AsyncReadExt, AsyncWriteExt, DuplexStream};
use tokio::net::TcpStream;

use super::test_support::{
    accepted, action, fake_driver_with_backend_and_connector, runtime_spec, unit, DriverFakeBackend,
};

#[derive(Debug, Clone, PartialEq, Eq)]
struct ConnectCall {
    execution_id: ExecutionId,
    generation: ExecutionGeneration,
    port: NonZeroU16,
    timeout: Duration,
}

#[derive(Default)]
struct TestConnector {
    streams: Mutex<VecDeque<ExecutionPortStream>>,
    calls: Mutex<Vec<ConnectCall>>,
}

impl TestConnector {
    fn queue_stream(&self) -> DuplexStream {
        let (connector_stream, workload_stream) = tokio::io::duplex(1_024);
        let connector_stream: ExecutionPortStream = Box::pin(connector_stream);
        self.streams.lock().unwrap().push_back(connector_stream);
        workload_stream
    }

    fn calls(&self) -> Vec<ConnectCall> {
        self.calls.lock().unwrap().clone()
    }
}

#[async_trait]
impl ExecutionPortConnector for TestConnector {
    async fn connect_port(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        port: NonZeroU16,
        timeout: Duration,
    ) -> ExecutionManagerResult<ExecutionPortStream> {
        self.calls.lock().unwrap().push(ConnectCall {
            execution_id: execution_id.clone(),
            generation,
            port,
            timeout,
        });
        self.streams.lock().unwrap().pop_front().ok_or_else(|| {
            ExecutionManagerError::Unavailable(
                "test connector has no queued workload stream".into(),
            )
        })
    }
}

fn service_spec(
    unit_id: &str,
    generation: u64,
    ports: &[(&str, u16)],
) -> a3s_runtime::contract::RuntimeUnitSpec {
    let mut spec = runtime_spec(unit_id, generation, RuntimeUnitClass::Service);
    spec.network.mode = NetworkMode::Service;
    spec.network.ports = ports
        .iter()
        .map(|(name, port)| RuntimePort {
            name: (*name).into(),
            container_port: *port,
            protocol: TransportProtocol::Tcp,
        })
        .collect();
    spec
}

fn endpoint(
    observation: &a3s_runtime::contract::RuntimeObservation,
    name: &str,
) -> RuntimeServiceEndpoint {
    RuntimeServiceEndpoint::from_observation(observation, name).unwrap()
}

async fn assert_socket_closes(address: SocketAddr) {
    for _ in 0..50 {
        match tokio::time::timeout(Duration::from_millis(100), TcpStream::connect(address)).await {
            Ok(Err(_)) => return,
            Ok(Ok(stream)) => drop(stream),
            Err(_) => {}
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("Runtime Service endpoint {address} remained open");
}

async fn connect_endpoint(address: SocketAddr) -> TcpStream {
    for _ in 0..50 {
        if let Ok(Ok(stream)) =
            tokio::time::timeout(Duration::from_millis(100), TcpStream::connect(address)).await
        {
            return stream;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    panic!("Runtime Service endpoint {address} never accepted a connection");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn endpoints_are_exact_stable_unique_and_relay_bidirectional_tcp() {
    let directory = tempfile::tempdir().unwrap();
    let backend = Arc::new(DriverFakeBackend::default());
    let connector = Arc::new(TestConnector::default());
    let mut workload_stream = connector.queue_stream();
    let driver =
        fake_driver_with_backend_and_connector(&directory, backend.clone(), connector.clone());
    let spec = service_spec(
        "service-endpoint-relay",
        1,
        &[("api", 8_080), ("metrics", 9_090)],
    );

    let running = driver.apply(&spec, &accepted(&spec)).await.unwrap();
    let endpoints = running.service_endpoints().unwrap();
    assert_eq!(endpoints.len(), 2);
    assert_ne!(
        endpoint(&running, "api").socket_addr(),
        endpoint(&running, "metrics").socket_addr()
    );
    assert!(endpoints.iter().all(|endpoint| {
        endpoint.protocol == TransportProtocol::Tcp && endpoint.address.is_loopback()
    }));

    let replayed = driver.apply(&spec, &running).await.unwrap();
    assert_eq!(replayed.service_endpoints().unwrap(), endpoints);
    let RuntimeInspection::Found { observation, .. } =
        driver.inspect(&unit(spec.clone(), replayed)).await.unwrap()
    else {
        panic!("running Runtime Service disappeared")
    };
    assert_eq!(observation.service_endpoints().unwrap(), endpoints);

    let api_address = endpoint(&observation, "api").socket_addr();
    let mut host_stream = connect_endpoint(api_address).await;
    host_stream.write_all(b"host-to-workload").await.unwrap();
    let mut received = [0_u8; 16];
    tokio::time::timeout(
        Duration::from_secs(2),
        workload_stream.read_exact(&mut received),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(&received, b"host-to-workload");
    workload_stream
        .write_all(b"workload-to-host")
        .await
        .unwrap();
    let mut response = [0_u8; 16];
    tokio::time::timeout(
        Duration::from_secs(2),
        host_stream.read_exact(&mut response),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(&response, b"workload-to-host");

    let record = driver
        .manager
        .managed_records()
        .await
        .unwrap()
        .pop()
        .unwrap();
    let metadata = record.managed_execution.unwrap();
    assert_eq!(
        connector.calls(),
        vec![ConnectCall {
            execution_id: ExecutionId::new(record.id).unwrap(),
            generation: metadata.generation,
            port: NonZeroU16::new(8_080).unwrap(),
            timeout: Duration::from_secs(5),
        }]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn local_generation_replacement_rotates_and_closes_the_endpoint() {
    let directory = tempfile::tempdir().unwrap();
    let backend = Arc::new(DriverFakeBackend::default());
    let connector = Arc::new(TestConnector::default());
    let driver = fake_driver_with_backend_and_connector(&directory, backend.clone(), connector);
    let spec = service_spec("service-endpoint-restart", 1, &[("api", 8_080)]);
    let running = driver.apply(&spec, &accepted(&spec)).await.unwrap();
    let first = endpoint(&running, "api").socket_addr();
    backend.finish(running.provider_resource_id.as_deref().unwrap(), 17);

    let RuntimeInspection::Found { observation, .. } =
        driver.inspect(&unit(spec.clone(), running)).await.unwrap()
    else {
        panic!("restartable Runtime Service disappeared")
    };
    let second = endpoint(&observation, "api").socket_addr();
    assert_ne!(second, first);
    assert_socket_closes(first).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn driver_restart_reconstructs_only_its_in_memory_endpoint_owner() {
    let directory = tempfile::tempdir().unwrap();
    let backend = Arc::new(DriverFakeBackend::default());
    let first_connector = Arc::new(TestConnector::default());
    let first_driver =
        fake_driver_with_backend_and_connector(&directory, backend.clone(), first_connector);
    let spec = service_spec("service-endpoint-driver-restart", 1, &[("api", 8_080)]);
    let first_running = first_driver.apply(&spec, &accepted(&spec)).await.unwrap();
    let first_address = endpoint(&first_running, "api").socket_addr();

    let second_connector = Arc::new(TestConnector::default());
    let mut workload_stream = second_connector.queue_stream();
    let second_driver =
        fake_driver_with_backend_and_connector(&directory, backend, second_connector);
    let second_running = second_driver.apply(&spec, &first_running).await.unwrap();
    let second_address = endpoint(&second_running, "api").socket_addr();
    assert_ne!(second_address, first_address);
    assert_eq!(
        second_running.provider_resource_id,
        first_running.provider_resource_id
    );

    drop(first_driver);
    assert_socket_closes(first_address).await;
    let mut host_stream = connect_endpoint(second_address).await;
    host_stream.write_all(b"reconstructed").await.unwrap();
    let mut received = [0_u8; 13];
    tokio::time::timeout(
        Duration::from_secs(2),
        workload_stream.read_exact(&mut received),
    )
    .await
    .unwrap()
    .unwrap();
    assert_eq!(&received, b"reconstructed");

    drop(second_driver);
    assert_socket_closes(second_address).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn stop_remove_and_provider_loss_close_every_endpoint() {
    let stop_directory = tempfile::tempdir().unwrap();
    let stop_backend = Arc::new(DriverFakeBackend::default());
    let stop_driver = fake_driver_with_backend_and_connector(
        &stop_directory,
        stop_backend,
        Arc::new(TestConnector::default()),
    );
    let stop_spec = service_spec("service-endpoint-stop", 1, &[("api", 8_080)]);
    let stop_running = stop_driver
        .apply(&stop_spec, &accepted(&stop_spec))
        .await
        .unwrap();
    let stop_address = endpoint(&stop_running, "api").socket_addr();
    let stopped = stop_driver
        .stop(
            &unit(stop_spec.clone(), stop_running),
            &action("service-endpoint-stop", &stop_spec),
        )
        .await
        .unwrap();
    assert!(stopped.service_endpoints().unwrap().is_empty());
    assert_socket_closes(stop_address).await;

    let remove_directory = tempfile::tempdir().unwrap();
    let remove_backend = Arc::new(DriverFakeBackend::default());
    let remove_driver = fake_driver_with_backend_and_connector(
        &remove_directory,
        remove_backend,
        Arc::new(TestConnector::default()),
    );
    let remove_spec = service_spec("service-endpoint-remove", 1, &[("api", 8_080)]);
    let remove_running = remove_driver
        .apply(&remove_spec, &accepted(&remove_spec))
        .await
        .unwrap();
    let remove_address = endpoint(&remove_running, "api").socket_addr();
    remove_driver
        .remove(
            &unit(remove_spec.clone(), remove_running),
            &action("service-endpoint-remove", &remove_spec),
        )
        .await
        .unwrap();
    assert_socket_closes(remove_address).await;

    let loss_directory = tempfile::tempdir().unwrap();
    let loss_backend = Arc::new(DriverFakeBackend::default());
    let loss_driver = fake_driver_with_backend_and_connector(
        &loss_directory,
        loss_backend.clone(),
        Arc::new(TestConnector::default()),
    );
    let loss_spec = service_spec("service-endpoint-loss", 1, &[("api", 8_080)]);
    let loss_running = loss_driver
        .apply(&loss_spec, &accepted(&loss_spec))
        .await
        .unwrap();
    let loss_address = endpoint(&loss_running, "api").socket_addr();
    loss_backend.lose(loss_running.provider_resource_id.as_deref().unwrap());
    assert!(matches!(
        loss_driver
            .inspect(&unit(loss_spec, loss_running))
            .await
            .unwrap(),
        RuntimeInspection::NotFound { .. }
    ));
    assert_socket_closes(loss_address).await;
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn capabilities_and_preflight_reject_udp_and_outbound_without_mutation() {
    let directory = tempfile::tempdir().unwrap();
    let backend = Arc::new(DriverFakeBackend::default());
    let driver = fake_driver_with_backend_and_connector(
        &directory,
        backend,
        Arc::new(TestConnector::default()),
    );
    let capabilities = driver.capabilities().await.unwrap();
    assert_eq!(
        capabilities.network_modes,
        vec![NetworkMode::None, NetworkMode::Service]
    );
    assert!(capabilities.features.contains(&RuntimeFeature::ServiceTcp));
    assert!(!capabilities.features.contains(&RuntimeFeature::ServiceUdp));
    assert!(!capabilities.network_modes.contains(&NetworkMode::Outbound));

    let mut udp = service_spec("service-endpoint-udp", 1, &[("dns", 5_353)]);
    udp.network.ports[0].protocol = TransportProtocol::Udp;
    assert!(matches!(
        driver.apply(&udp, &accepted(&udp)).await,
        Err(RuntimeError::UnsupportedCapabilities(missing))
            if missing == vec!["feature:ServiceUdp"]
    ));

    let mut outbound = runtime_spec("service-endpoint-outbound", 1, RuntimeUnitClass::Service);
    outbound.network.mode = NetworkMode::Outbound;
    assert!(matches!(
        driver.apply(&outbound, &accepted(&outbound)).await,
        Err(RuntimeError::UnsupportedCapabilities(missing))
            if missing == vec!["network_mode:Outbound"]
    ));
    assert!(driver.manager.managed_records().await.unwrap().is_empty());
}
