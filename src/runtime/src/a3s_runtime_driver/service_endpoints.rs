//! Generation-fenced host endpoints for Runtime Service ports.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Duration;

use a3s_box_core::{ExecutionGeneration, ExecutionId, ExecutionPortConnector, ExecutionPortStream};
use a3s_runtime::contract::{
    NetworkMode, RuntimeEvidence, RuntimeObservation, RuntimeServiceEndpoint, RuntimeUnitClass,
    RuntimeUnitSpec, RuntimeUnitState, TransportProtocol,
};
use a3s_runtime::{RuntimeError, RuntimeResult};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::{JoinHandle, JoinSet};

const SERVICE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_SERVICE_CONNECTIONS: usize = 64;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct RuntimeEndpointKey {
    unit_id: String,
    runtime_generation: u64,
}

impl RuntimeEndpointKey {
    fn new(spec: &RuntimeUnitSpec) -> Self {
        Self {
            unit_id: spec.unit_id.clone(),
            runtime_generation: spec.generation,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct EndpointIdentity {
    execution_id: ExecutionId,
    execution_generation: ExecutionGeneration,
    spec_digest: String,
    ports: Vec<(String, u16)>,
}

struct EndpointLease {
    identity: EndpointIdentity,
    endpoints: Vec<RuntimeServiceEndpoint>,
    tasks: Vec<JoinHandle<()>>,
}

impl Drop for EndpointLease {
    fn drop(&mut self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

/// Owns only live listeners and relay tasks. Runtime observations remain the
/// authoritative endpoint publication; durable endpoint state is deliberately
/// not duplicated inside Box.
pub(super) struct ServiceEndpointOwner {
    connector: Arc<dyn ExecutionPortConnector>,
    connection_limit: Arc<Semaphore>,
    leases: Mutex<BTreeMap<RuntimeEndpointKey, EndpointLease>>,
}

impl ServiceEndpointOwner {
    pub(super) fn new(connector: Arc<dyn ExecutionPortConnector>) -> Self {
        Self {
            connector,
            connection_limit: Arc::new(Semaphore::new(MAX_SERVICE_CONNECTIONS)),
            leases: Mutex::new(BTreeMap::new()),
        }
    }

    pub(super) async fn reconcile(
        &self,
        spec: &RuntimeUnitSpec,
        execution_id: ExecutionId,
        execution_generation: ExecutionGeneration,
        mut observation: RuntimeObservation,
    ) -> RuntimeResult<RuntimeObservation> {
        let key = RuntimeEndpointKey::new(spec);
        if spec.class != RuntimeUnitClass::Service
            || spec.network.mode != NetworkMode::Service
            || observation.state != RuntimeUnitState::Running
            || spec.network.ports.is_empty()
        {
            self.leases.lock().await.remove(&key);
            observation.clear_service_endpoints();
            observation
                .validate_against(spec)
                .map_err(RuntimeError::Protocol)?;
            return Ok(observation);
        }

        let spec_digest = spec.digest().map_err(RuntimeError::Protocol)?;
        let identity = EndpointIdentity {
            execution_id: execution_id.clone(),
            execution_generation,
            spec_digest,
            ports: spec
                .network
                .ports
                .iter()
                .map(|port| (port.name.clone(), port.container_port))
                .collect(),
        };
        let mut leases = self.leases.lock().await;
        if let Some(existing) = leases.get(&key) {
            if existing.identity == identity {
                attach_endpoints(spec, &mut observation, &existing.endpoints)?;
                return Ok(observation);
            }
        }

        // Keep every listener bound until the complete endpoint set has been
        // validated. A partial bind is never published into Runtime evidence.
        let mut staged = Vec::with_capacity(spec.network.ports.len());
        for port in &spec.network.ports {
            if port.protocol != TransportProtocol::Tcp {
                return Err(RuntimeError::UnsupportedCapabilities(vec![format!(
                    "feature:Service{:?}",
                    port.protocol
                )]));
            }
            let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                .await
                .map_err(|error| {
                    RuntimeError::ProviderUnavailable(format!(
                        "Box could not bind Runtime Service port {:?}: {error}",
                        port.name
                    ))
                })?;
            let address = listener.local_addr().map_err(|error| {
                RuntimeError::ProviderUnavailable(format!(
                    "Box could not inspect Runtime Service port {:?}: {error}",
                    port.name
                ))
            })?;
            let endpoint = RuntimeServiceEndpoint::node_local_tcp(&port.name, address.port())
                .map_err(RuntimeError::Protocol)?;
            let guest_port = NonZeroU16::new(port.container_port).ok_or_else(|| {
                RuntimeError::Protocol("Runtime Service declared a zero TCP port".into())
            })?;
            staged.push((listener, endpoint, guest_port));
        }

        // Bind and serve first, then probe each advertised host URL through the
        // live listener. Publishing before that proof lets Running observations
        // advertise dead MicroVM relays (same honesty class as Box#370).
        let mut prepared = staged
            .into_iter()
            .map(|(listener, endpoint, guest_port)| {
                let task = tokio::spawn(serve_endpoint(
                    listener,
                    Arc::clone(&self.connector),
                    Arc::clone(&self.connection_limit),
                    execution_id.clone(),
                    execution_generation,
                    guest_port,
                    endpoint.port_name.clone(),
                ));
                (endpoint, task)
            })
            .collect::<Vec<_>>();

        let probe_targets = prepared
            .iter()
            .map(|(endpoint, _)| (endpoint.port_name.clone(), endpoint.socket_addr()))
            .collect::<Vec<_>>();
        for (port_name, address) in probe_targets {
            if let Err(error) = probe_advertised_tcp_endpoint(address).await {
                for (_, task) in prepared.drain(..) {
                    task.abort();
                    let _ = task.await;
                }
                observation.clear_service_endpoints();
                return Err(RuntimeError::ProviderUnavailable(format!(
                    "Box Runtime Service endpoint {port_name:?} at {address} is not reachable through the advertised host URL: {error}"
                )));
            }
        }

        let endpoints = prepared
            .iter()
            .map(|(endpoint, _)| endpoint.clone())
            .collect::<Vec<_>>();
        attach_endpoints(spec, &mut observation, &endpoints)?;
        let tasks = prepared
            .into_iter()
            .map(|(_, task)| task)
            .collect::<Vec<_>>();
        leases.insert(
            key,
            EndpointLease {
                identity,
                endpoints,
                tasks,
            },
        );
        Ok(observation)
    }

    pub(super) async fn remove_runtime(&self, unit_id: &str, runtime_generation: u64) {
        self.leases.lock().await.remove(&RuntimeEndpointKey {
            unit_id: unit_id.into(),
            runtime_generation,
        });
    }

    pub(super) async fn remove_provider(&self, execution_id: &ExecutionId) {
        self.leases
            .lock()
            .await
            .retain(|_, lease| &lease.identity.execution_id != execution_id);
    }
}

impl Drop for ServiceEndpointOwner {
    fn drop(&mut self) {
        self.leases.get_mut().clear();
    }
}

fn attach_endpoints(
    spec: &RuntimeUnitSpec,
    observation: &mut RuntimeObservation,
    endpoints: &[RuntimeServiceEndpoint],
) -> RuntimeResult<()> {
    observation.clear_service_endpoints();
    if !endpoints.is_empty() {
        let provider_build = observation.provider_build.clone().ok_or_else(|| {
            RuntimeError::Protocol(
                "Runtime Service endpoint publication requires provider build identity".into(),
            )
        })?;
        let evidence = observation.evidence.get_or_insert_with(|| RuntimeEvidence {
            provider_build: provider_build.clone(),
            spec_digest: observation.spec_digest.clone(),
            semantics_profile_digest: spec.semantics_profile_digest.clone(),
            identity_attachment_digest: spec.identity_attachment_digest.clone(),
            claims: BTreeMap::new(),
        });
        if evidence.provider_build != provider_build
            || evidence.spec_digest != observation.spec_digest
            || evidence.semantics_profile_digest != spec.semantics_profile_digest
            || evidence.identity_attachment_digest != spec.identity_attachment_digest
        {
            return Err(RuntimeError::Protocol(
                "Runtime Service endpoint evidence does not match provider identity".into(),
            ));
        }
        for endpoint in endpoints {
            endpoint
                .insert_claim(&mut evidence.claims)
                .map_err(RuntimeError::Protocol)?;
        }
    }
    observation
        .validate_against(spec)
        .map_err(RuntimeError::Protocol)
}

/// Prove the advertised host URL can accept traffic that reaches `connect_port`.
///
/// TCP accept on the host listener happens before the guest OPEN. A successful
/// `TcpStream::connect` alone is not enough: rejection closes the accepted
/// stream. Idle success keeps the connection open while `copy_bidirectional`
/// waits, so surviving a short settle without peer close is the live-URL proof
/// for non-HTTP Service ports.
async fn probe_advertised_tcp_endpoint(address: SocketAddr) -> Result<(), String> {
    tokio::time::timeout(SERVICE_CONNECT_TIMEOUT, async {
        let mut stream = TcpStream::connect(address)
            .await
            .map_err(|error| error.to_string())?;
        let settle = Duration::from_millis(500);
        let mut buf = [0_u8; 1];
        tokio::select! {
            biased;
            result = stream.read(&mut buf) => match result {
                Ok(0) => Err(
                    "peer closed before the generation-fenced guest relay opened".into(),
                ),
                Ok(_) => Ok(()),
                Err(error) => Err(error.to_string()),
            },
            _ = tokio::time::sleep(settle) => Ok(()),
        }
    })
    .await
    .map_err(|_| format!("timed out probing advertised endpoint {address}"))?
}

async fn serve_endpoint(
    listener: TcpListener,
    connector: Arc<dyn ExecutionPortConnector>,
    connection_limit: Arc<Semaphore>,
    execution_id: ExecutionId,
    execution_generation: ExecutionGeneration,
    guest_port: NonZeroU16,
    port_name: String,
) {
    let mut relays = JoinSet::new();
    loop {
        let (host_stream, peer_address) = match listener.accept().await {
            Ok(connection) => connection,
            Err(error) => {
                tracing::warn!(
                    execution_id = %execution_id,
                    port_name,
                    error = %error,
                    "Runtime Service endpoint accept failed"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        // Each listener holds at most one accepted stream while waiting for
        // the driver-wide relay budget. Idle declared ports do not reserve a
        // permit and therefore cannot starve an active endpoint.
        let permit = match Arc::clone(&connection_limit).acquire_owned().await {
            Ok(permit) => permit,
            Err(_) => return,
        };
        if let Err(error) = host_stream.set_nodelay(true) {
            tracing::warn!(
                execution_id = %execution_id,
                port_name,
                peer = %peer_address,
                error = %error,
                "Runtime Service endpoint could not configure a host stream"
            );
        }
        let connector = Arc::clone(&connector);
        let relay_execution_id = execution_id.clone();
        let relay_port_name = port_name.clone();
        relays.spawn(async move {
            let _permit = permit;
            if let Err(error) = relay_connection(
                connector.as_ref(),
                &relay_execution_id,
                execution_generation,
                guest_port,
                host_stream,
            )
            .await
            {
                tracing::warn!(
                    execution_id = %relay_execution_id,
                    port_name = relay_port_name,
                    peer = %peer_address,
                    error = %error,
                    "Runtime Service endpoint relay failed"
                );
            }
        });
        while let Some(result) = relays.try_join_next() {
            if let Err(error) = result {
                tracing::warn!(
                    execution_id = %execution_id,
                    port_name,
                    error = %error,
                    "Runtime Service endpoint relay task failed"
                );
            }
        }
    }
}

async fn relay_connection(
    connector: &dyn ExecutionPortConnector,
    execution_id: &ExecutionId,
    execution_generation: ExecutionGeneration,
    guest_port: NonZeroU16,
    mut host_stream: TcpStream,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut guest_stream: ExecutionPortStream = connector
        .connect_port(
            execution_id,
            execution_generation,
            guest_port,
            SERVICE_CONNECT_TIMEOUT,
        )
        .await?;
    tokio::io::copy_bidirectional(&mut host_stream, &mut guest_stream)
        .await
        .map_err(|error| format!("failed to relay Runtime Service TCP traffic: {error}"))?;
    Ok(())
}
