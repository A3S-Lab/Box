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

        let endpoints = staged
            .iter()
            .map(|(_, endpoint, _)| endpoint.clone())
            .collect::<Vec<_>>();
        attach_endpoints(spec, &mut observation, &endpoints)?;

        let tasks = staged
            .into_iter()
            .map(|(listener, endpoint, guest_port)| {
                tokio::spawn(serve_endpoint(
                    listener,
                    Arc::clone(&self.connector),
                    Arc::clone(&self.connection_limit),
                    execution_id.clone(),
                    execution_generation,
                    guest_port,
                    endpoint.port_name,
                ))
            })
            .collect();
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
            claims: BTreeMap::new(),
        });
        if evidence.provider_build != provider_build
            || evidence.spec_digest != observation.spec_digest
            || evidence.semantics_profile_digest != spec.semantics_profile_digest
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
