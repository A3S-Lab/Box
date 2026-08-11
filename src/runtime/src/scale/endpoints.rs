//! Live host endpoint leases for Gateway-managed replica slots.

use std::{
    collections::{BTreeMap, BTreeSet},
    net::{IpAddr, Ipv4Addr, SocketAddr},
    num::NonZeroU16,
    sync::Arc,
    time::Duration,
};

use a3s_box_core::{
    scale::ScaleEndpoint, ExecutionGeneration, ExecutionId, ExecutionPortConnector,
    ExecutionPortStream,
};
use thiserror::Error;
use tokio::{
    net::{TcpListener, TcpStream},
    sync::{Mutex, Semaphore},
    task::{JoinHandle, JoinSet},
};

use super::reconciler::ScaleReconcileError;

const ENDPOINT_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_ENDPOINT_CONNECTIONS: usize = 1_024;

#[derive(Debug, Error)]
pub enum ScaleEndpointConfigError {
    #[error("invalid scale endpoint advertise host {host:?}: {message}")]
    InvalidAdvertiseHost { host: String, message: String },
}

/// Host listener and advertised address policy for live scale endpoints.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScaleEndpointConfig {
    bind_address: IpAddr,
    advertise_host: url::Host<String>,
}

impl ScaleEndpointConfig {
    pub fn new(
        bind_address: IpAddr,
        advertise_host: impl Into<String>,
    ) -> Result<Self, ScaleEndpointConfigError> {
        let advertise_host = advertise_host.into();
        let parsed = match advertise_host.parse::<IpAddr>() {
            Ok(IpAddr::V4(address)) => url::Host::Ipv4(address),
            Ok(IpAddr::V6(address)) => url::Host::Ipv6(address),
            Err(_) => url::Host::parse(&advertise_host).map_err(|error| {
                ScaleEndpointConfigError::InvalidAdvertiseHost {
                    host: advertise_host.clone(),
                    message: error.to_string(),
                }
            })?,
        };
        if matches!(&parsed, url::Host::Domain(domain) if domain.trim().is_empty()) {
            return Err(ScaleEndpointConfigError::InvalidAdvertiseHost {
                host: advertise_host,
                message: "host must not be empty".to_string(),
            });
        }
        if matches!(&parsed, url::Host::Ipv4(address) if address.is_unspecified())
            || matches!(&parsed, url::Host::Ipv6(address) if address.is_unspecified())
        {
            return Err(ScaleEndpointConfigError::InvalidAdvertiseHost {
                host: advertise_host,
                message: "an unspecified address cannot be advertised to Gateway".to_string(),
            });
        }
        let address_family_mismatch = matches!(
            (bind_address, &parsed),
            (IpAddr::V4(_), url::Host::Ipv6(_)) | (IpAddr::V6(_), url::Host::Ipv4(_))
        );
        if address_family_mismatch {
            return Err(ScaleEndpointConfigError::InvalidAdvertiseHost {
                host: advertise_host,
                message: format!(
                    "literal address family does not match endpoint bind address {bind_address}"
                ),
            });
        }
        Ok(Self {
            bind_address,
            advertise_host: parsed,
        })
    }

    pub fn loopback() -> Self {
        Self {
            bind_address: IpAddr::V4(Ipv4Addr::LOCALHOST),
            advertise_host: url::Host::Ipv4(Ipv4Addr::LOCALHOST),
        }
    }

    pub fn bind_address(&self) -> IpAddr {
        self.bind_address
    }

    pub fn advertise_host(&self) -> String {
        match &self.advertise_host {
            url::Host::Domain(host) => host.clone(),
            url::Host::Ipv4(host) => host.to_string(),
            url::Host::Ipv6(host) => host.to_string(),
        }
    }

    fn endpoint_url(&self, port: u16) -> String {
        match &self.advertise_host {
            url::Host::Ipv6(host) => format!("http://[{host}]:{port}"),
            url::Host::Domain(host) => format!("http://{host}:{port}"),
            url::Host::Ipv4(host) => format!("http://{host}:{port}"),
        }
    }
}

impl Default for ScaleEndpointConfig {
    fn default() -> Self {
        Self::loopback()
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct ScaleEndpointTarget {
    pub execution_id: ExecutionId,
    pub generation: ExecutionGeneration,
    pub service: String,
    pub slot: u32,
    pub guest_port: NonZeroU16,
}

struct EndpointLease {
    target: ScaleEndpointTarget,
    endpoint: ScaleEndpoint,
    task: JoinHandle<()>,
}

impl Drop for EndpointLease {
    fn drop(&mut self) {
        self.task.abort();
    }
}

pub(super) struct ScaleEndpointOwner {
    config: ScaleEndpointConfig,
    connector: Arc<dyn ExecutionPortConnector>,
    connection_limit: Arc<Semaphore>,
    leases: Mutex<BTreeMap<String, EndpointLease>>,
}

impl ScaleEndpointOwner {
    pub fn new(config: ScaleEndpointConfig, connector: Arc<dyn ExecutionPortConnector>) -> Self {
        Self {
            config,
            connector,
            connection_limit: Arc::new(Semaphore::new(MAX_ENDPOINT_CONNECTIONS)),
            leases: Mutex::new(BTreeMap::new()),
        }
    }

    pub async fn reconcile_service(
        &self,
        service: &str,
        targets: &[ScaleEndpointTarget],
    ) -> Result<Vec<ScaleEndpoint>, ScaleReconcileError> {
        validate_targets(service, targets)?;
        let desired = targets
            .iter()
            .map(|target| (target.execution_id.as_str().to_string(), target))
            .collect::<BTreeMap<_, _>>();
        let mut leases = self.leases.lock().await;
        leases.retain(|execution_id, lease| {
            lease.target.service != service
                || (!lease.task.is_finished()
                    && desired
                        .get(execution_id)
                        .is_some_and(|target| lease.target == **target))
        });

        for target in targets {
            if leases.contains_key(target.execution_id.as_str()) {
                continue;
            }
            let listener = TcpListener::bind(SocketAddr::new(self.config.bind_address, 0))
                .await
                .map_err(|error| {
                    ScaleReconcileError::Lifecycle(format!(
                        "failed to bind service endpoint for {} slot {}: {error}",
                        target.service, target.slot
                    ))
                })?;
            let address = listener.local_addr().map_err(|error| {
                ScaleReconcileError::Lifecycle(format!(
                    "failed to inspect service endpoint for {} slot {}: {error}",
                    target.service, target.slot
                ))
            })?;

            // A running execution is not a ready HTTP replica until its declared
            // guest port accepts a generation-fenced connection.
            self.connector
                .connect_port(
                    &target.execution_id,
                    target.generation,
                    target.guest_port,
                    ENDPOINT_CONNECT_TIMEOUT,
                )
                .await
                .map_err(|error| {
                    ScaleReconcileError::Lifecycle(format!(
                        "service endpoint for {} slot {} is not ready: {error}",
                        target.service, target.slot
                    ))
                })?;

            let endpoint = ScaleEndpoint {
                instance_id: target.execution_id.as_str().to_string(),
                slot: target.slot,
                url: self.config.endpoint_url(address.port()),
            };
            let task = tokio::spawn(serve_endpoint(
                listener,
                Arc::clone(&self.connector),
                Arc::clone(&self.connection_limit),
                target.clone(),
            ));
            leases.insert(
                target.execution_id.as_str().to_string(),
                EndpointLease {
                    target: target.clone(),
                    endpoint,
                    task,
                },
            );
        }

        let mut endpoints = leases
            .values()
            .filter(|lease| lease.target.service == service)
            .map(|lease| lease.endpoint.clone())
            .collect::<Vec<_>>();
        endpoints.sort_by(|left, right| {
            left.slot
                .cmp(&right.slot)
                .then_with(|| left.instance_id.cmp(&right.instance_id))
        });
        Ok(endpoints)
    }

    pub async fn remove(&self, execution_id: &ExecutionId) {
        self.leases.lock().await.remove(execution_id.as_str());
    }
}

fn validate_targets(
    service: &str,
    targets: &[ScaleEndpointTarget],
) -> Result<(), ScaleReconcileError> {
    let mut executions = BTreeSet::new();
    let mut service_slots = BTreeSet::new();
    for target in targets {
        if target.service != service {
            return Err(ScaleReconcileError::Lifecycle(format!(
                "endpoint target for service {:?} was reconciled in scope {service:?}",
                target.service
            )));
        }
        if !executions.insert(target.execution_id.as_str()) {
            return Err(ScaleReconcileError::Lifecycle(format!(
                "duplicate endpoint target for execution {}",
                target.execution_id
            )));
        }
        if !service_slots.insert((target.service.as_str(), target.slot)) {
            return Err(ScaleReconcileError::Lifecycle(format!(
                "duplicate endpoint target for service {:?} slot {}",
                target.service, target.slot
            )));
        }
    }
    Ok(())
}

async fn serve_endpoint(
    listener: TcpListener,
    connector: Arc<dyn ExecutionPortConnector>,
    connection_limit: Arc<Semaphore>,
    target: ScaleEndpointTarget,
) {
    let mut relays = JoinSet::new();
    loop {
        let (host_stream, peer) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                tracing::warn!(
                    execution_id = %target.execution_id,
                    service = target.service,
                    slot = target.slot,
                    %error,
                    "Scale endpoint accept failed"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let permit = match Arc::clone(&connection_limit).acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => return,
        };
        let connector = Arc::clone(&connector);
        let relay_target = target.clone();
        relays.spawn(async move {
            let _permit = permit;
            if let Err(error) =
                relay_connection(connector.as_ref(), &relay_target, host_stream).await
            {
                tracing::warn!(
                    execution_id = %relay_target.execution_id,
                    service = relay_target.service,
                    slot = relay_target.slot,
                    %peer,
                    %error,
                    "Scale endpoint relay failed"
                );
            }
        });
        while let Some(result) = relays.try_join_next() {
            if let Err(error) = result {
                tracing::warn!(
                    execution_id = %target.execution_id,
                    service = target.service,
                    slot = target.slot,
                    %error,
                    "Scale endpoint relay task failed"
                );
            }
        }
    }
}

async fn relay_connection(
    connector: &dyn ExecutionPortConnector,
    target: &ScaleEndpointTarget,
    mut host_stream: TcpStream,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut guest_stream: ExecutionPortStream = connector
        .connect_port(
            &target.execution_id,
            target.generation,
            target.guest_port,
            ENDPOINT_CONNECT_TIMEOUT,
        )
        .await?;
    tokio::io::copy_bidirectional(&mut host_stream, &mut guest_stream)
        .await
        .map_err(|error| format!("failed to relay scale endpoint traffic: {error}"))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use a3s_box_core::ExecutionManagerResult;
    use async_trait::async_trait;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    struct EchoConnector;

    #[async_trait]
    impl ExecutionPortConnector for EchoConnector {
        async fn connect_port(
            &self,
            _execution_id: &ExecutionId,
            _generation: ExecutionGeneration,
            _port: NonZeroU16,
            _timeout: Duration,
        ) -> ExecutionManagerResult<ExecutionPortStream> {
            let (client, mut server) = tokio::io::duplex(1_024);
            tokio::spawn(async move {
                let mut buffer = [0_u8; 1_024];
                loop {
                    let count = match server.read(&mut buffer).await {
                        Ok(0) | Err(_) => return,
                        Ok(count) => count,
                    };
                    if server.write_all(&buffer[..count]).await.is_err() {
                        return;
                    }
                }
            });
            Ok(Box::pin(client))
        }
    }

    fn target() -> ScaleEndpointTarget {
        ScaleEndpointTarget {
            execution_id: ExecutionId::new("scale-api-0").unwrap(),
            generation: ExecutionGeneration::INITIAL,
            service: "api".to_string(),
            slot: 0,
            guest_port: NonZeroU16::new(8080).unwrap(),
        }
    }

    #[tokio::test]
    async fn endpoint_is_stable_and_relays_to_the_exact_generation() {
        let owner =
            ScaleEndpointOwner::new(ScaleEndpointConfig::loopback(), Arc::new(EchoConnector));
        let first = owner.reconcile_service("api", &[target()]).await.unwrap();
        let replay = owner.reconcile_service("api", &[target()]).await.unwrap();
        assert_eq!(replay, first);
        assert_eq!(first[0].slot, 0);

        let worker = ScaleEndpointTarget {
            execution_id: ExecutionId::new("scale-worker-0").unwrap(),
            service: "worker".to_string(),
            ..target()
        };
        assert_eq!(
            owner
                .reconcile_service("worker", &[worker])
                .await
                .unwrap()
                .len(),
            1
        );
        assert_eq!(
            owner.reconcile_service("api", &[target()]).await.unwrap(),
            first
        );

        let address = first[0]
            .url
            .strip_prefix("http://")
            .unwrap()
            .parse::<SocketAddr>()
            .unwrap();
        let mut stream = TcpStream::connect(address).await.unwrap();
        stream.write_all(b"ready").await.unwrap();
        let mut reply = [0_u8; 5];
        stream.read_exact(&mut reply).await.unwrap();
        assert_eq!(&reply, b"ready");

        assert!(owner
            .reconcile_service("api", &[])
            .await
            .unwrap()
            .is_empty());
    }

    #[test]
    fn endpoint_config_validates_and_formats_advertised_hosts() {
        let ipv6 = ScaleEndpointConfig::new(IpAddr::V6("::".parse().unwrap()), "::1").unwrap();
        assert_eq!(ipv6.endpoint_url(8080), "http://[::1]:8080");
        assert!(ScaleEndpointConfig::new(IpAddr::V4(Ipv4Addr::LOCALHOST), "bad host").is_err());
        assert!(ScaleEndpointConfig::new(
            IpAddr::V4(Ipv4Addr::UNSPECIFIED),
            Ipv4Addr::UNSPECIFIED.to_string(),
        )
        .is_err());
        assert!(ScaleEndpointConfig::new(IpAddr::V4(Ipv4Addr::UNSPECIFIED), "::1").is_err());
    }
}
