use std::time::Duration;

use a3s_box_core::NetworkMode;
use a3s_runtime::contract::RuntimeUnitState;
use a3s_runtime::RuntimeClient;

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
        config.network == NetworkMode::None,
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
        .await
}
