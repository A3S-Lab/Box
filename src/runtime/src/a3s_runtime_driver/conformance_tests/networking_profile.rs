use std::time::Duration;

use a3s_box_core::NetworkMode as BoxNetworkMode;
use a3s_runtime::contract::{
    NetworkMode, RuntimePort, RuntimeServiceEndpoint, RuntimeUnitState, TransportProtocol,
};
use a3s_runtime::RuntimeClient;
use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::fixture::BoxRuntimeConformanceFixture;
use super::{require, Result};

pub(super) async fn run(
    fixture: &BoxRuntimeConformanceFixture,
    client: &dyn RuntimeClient,
) -> Result<()> {
    let listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
        .await
        .map_err(|error| super::external("bind loopback network oracle", error))?;
    let port = listener
        .local_addr()
        .map_err(|error| super::external("read loopback network oracle address", error))?
        .port();
    let request = fixture.cases.task(
        "network-none",
        &format!(
            "if wget -q -T 1 -O /dev/null http://127.0.0.1:{port}; then printf 'unexpected-network-access\\n' >&2; exit 41; else printf 'r17-network-none-denied\\n'; fi"
        ),
        10_000,
    );
    let observation = client.apply(&request).await?;
    require(
        observation.state == RuntimeUnitState::Succeeded,
        "NetworkMode::None workload reached the host loopback listener",
    )?;
    let record = fixture.record_for(&request.spec).await?;
    let config = &record
        .managed_execution
        .as_ref()
        .ok_or_else(|| super::protocol("network fixture lost managed metadata"))?
        .request
        .config;
    require(
        config.network == BoxNetworkMode::None,
        "NetworkMode::None was not preserved in provider configuration",
    )?;
    require(
        tokio::time::timeout(Duration::from_millis(200), listener.accept())
            .await
            .is_err(),
        "NetworkMode::None connected to a host loopback service",
    )?;
    fixture
        .remove_unit(client, &request.spec, "network-none")
        .await?;

    let capabilities = client.capabilities().await?;
    if capabilities.network_modes.contains(&NetworkMode::Outbound) {
        // MicroVM/TSI: guest 127.0.0.1 connects are proxied onto the host stack.
        let outbound_listener = tokio::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0))
            .await
            .map_err(|error| super::external("bind outbound network oracle", error))?;
        let outbound_port = outbound_listener
            .local_addr()
            .map_err(|error| super::external("read outbound network oracle address", error))?
            .port();
        let accept = tokio::spawn(async move {
            let (mut stream, _) = outbound_listener.accept().await?;
            let mut buf = [0u8; 64];
            let _ = stream.read(&mut buf).await?;
            stream
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 2\r\nConnection: close\r\n\r\nok")
                .await?;
            Ok::<_, std::io::Error>(())
        });
        let mut outbound = fixture.cases.task(
            "network-outbound",
            &format!(
                "wget -q -T 5 -O /dev/null http://127.0.0.1:{outbound_port} && printf 'r17-network-outbound-ok\\n'"
            ),
            15_000,
        );
        outbound.spec.network.mode = NetworkMode::Outbound;
        let outbound_observation = client.apply(&outbound).await?;
        require(
            outbound_observation.state == RuntimeUnitState::Succeeded,
            "NetworkMode::Outbound workload could not reach the host loopback listener",
        )?;
        let outbound_record = fixture.record_for(&outbound.spec).await?;
        let outbound_config = &outbound_record
            .managed_execution
            .as_ref()
            .ok_or_else(|| super::protocol("outbound fixture lost managed metadata"))?
            .request
            .config;
        require(
            outbound_config.network == BoxNetworkMode::Tsi,
            "NetworkMode::Outbound was not mapped to TSI egress",
        )?;
        accept
            .await
            .map_err(|error| super::external("join outbound oracle accept", error))?
            .map_err(|error| super::external("outbound oracle accept", error))?;
        fixture
            .remove_unit(client, &outbound.spec, "network-outbound")
            .await?;
    } else {
        // Sandbox: Outbound is not advertised; apply must fail closed.
        let mut outbound = fixture.cases.task(
            "network-outbound-unsupported",
            "printf 'r17-network-outbound-should-reject\\n'",
            5_000,
        );
        outbound.spec.network.mode = NetworkMode::Outbound;
        require(
            client.apply(&outbound).await.is_err(),
            "Sandbox provider accepted NetworkMode::Outbound without egress",
        )?;
    }

    let script = "while :; do { printf 'HTTP/1.1 200 OK\\r\\nContent-Length: 15\\r\\nConnection: close\\r\\n\\r\\nr17-service-tcp'; } | nc -l -p 18080; done";
    let mut first = fixture.cases.service("network-service-first", script);
    first.spec.network.mode = NetworkMode::Service;
    first.spec.network.ports = vec![RuntimePort {
        name: "http".into(),
        container_port: 18_080,
        protocol: TransportProtocol::Tcp,
    }];
    let first_observation = client.apply(&first).await?;
    require(
        first_observation.state == RuntimeUnitState::Running,
        "first Runtime TCP Service did not reach running state",
    )?;
    let first_endpoint = RuntimeServiceEndpoint::from_observation(&first_observation, "http")
        .map_err(super::protocol)?;

    // The same guest socket in a different private Sandbox must receive a
    // distinct host endpoint instead of colliding in a global registry.
    let mut second = fixture.cases.service("network-service-second", script);
    second.spec.network.mode = NetworkMode::Service;
    second.spec.network.ports = first.spec.network.ports.clone();
    let second_observation = client.apply(&second).await?;
    require(
        second_observation.state == RuntimeUnitState::Running,
        "second Runtime TCP Service did not reach running state",
    )?;
    let second_endpoint = RuntimeServiceEndpoint::from_observation(&second_observation, "http")
        .map_err(super::protocol)?;
    require(
        first_endpoint.protocol == TransportProtocol::Tcp
            && first_endpoint.address.is_loopback()
            && second_endpoint.protocol == TransportProtocol::Tcp
            && second_endpoint.address.is_loopback(),
        "Runtime TCP Service endpoints were not exact node-local loopback publications",
    )?;
    require(
        first_endpoint.socket_addr() != second_endpoint.socket_addr(),
        "two Runtime Services published the same host socket",
    )?;
    require_service_response(first_endpoint.socket_addr()).await?;
    require_service_response(second_endpoint.socket_addr()).await?;

    let first_address = first_endpoint.socket_addr();
    let second_address = second_endpoint.socket_addr();
    fixture
        .remove_unit(client, &first.spec, "network-service-first")
        .await?;
    fixture
        .remove_unit(client, &second.spec, "network-service-second")
        .await?;
    require_endpoint_closed(first_address).await?;
    require_endpoint_closed(second_address).await
}

async fn require_service_response(address: std::net::SocketAddr) -> Result<()> {
    let mut last_failure = "no connection attempt completed".to_string();
    for _ in 0..30 {
        match tokio::time::timeout(Duration::from_millis(500), service_request(address)).await {
            Ok(Ok(response)) if response.ends_with(b"r17-service-tcp") => return Ok(()),
            Ok(Ok(response)) => {
                last_failure = format!("unexpected {} byte response", response.len());
            }
            Ok(Err(error)) => last_failure = error.to_string(),
            Err(_) => last_failure = "request timed out".into(),
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    Err(super::protocol(format!(
        "Runtime TCP Service endpoint {address} did not relay traffic: {last_failure}"
    )))
}

async fn service_request(address: std::net::SocketAddr) -> std::io::Result<Vec<u8>> {
    let mut stream = tokio::net::TcpStream::connect(address).await?;
    stream
        .write_all(b"GET / HTTP/1.1\r\nHost: runtime\r\nConnection: close\r\n\r\n")
        .await?;
    let mut response = Vec::new();
    stream.read_to_end(&mut response).await?;
    Ok(response)
}

async fn require_endpoint_closed(address: std::net::SocketAddr) -> Result<()> {
    for _ in 0..50 {
        if tokio::net::TcpStream::connect(address).await.is_err() {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    Err(super::protocol(format!(
        "Runtime TCP Service endpoint {address} remained open after removal"
    )))
}
