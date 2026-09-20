//! Generation-fenced host endpoints for Runtime Service ports.

use std::collections::BTreeMap;
use std::net::{Ipv4Addr, SocketAddr};
use std::num::NonZeroU16;
use std::sync::Arc;
use std::time::Duration;

use a3s_box_core::{
    ExecutionGeneration, ExecutionId, ExecutionPortConnector, ExecutionPortStream, ExecutionUdpPort,
};
use a3s_runtime::contract::{
    NetworkMode, RuntimeEvidence, RuntimeObservation, RuntimeServiceEndpoint, RuntimeUnitClass,
    RuntimeUnitSpec, RuntimeUnitState, TransportProtocol,
};
use a3s_runtime::{RuntimeError, RuntimeResult};
use tokio::io::AsyncReadExt;
use tokio::net::{TcpListener, TcpStream, UdpSocket};
use tokio::sync::{Mutex, Semaphore};
use tokio::task::{JoinHandle, JoinSet};

const SERVICE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const MAX_SERVICE_CONNECTIONS: usize = 64;
const MAX_UDP_ASSOCIATIONS: usize = 64;
const UDP_IDLE_TIMEOUT: Duration = Duration::from_secs(60);
const UDP_PROBE_PAYLOAD: &[u8] = b"a3s-runtime-service-udp-probe";
/// Bounded grace for the first advertised-URL proof after bind.
///
/// `reconcile` runs within milliseconds of Running (or right after readiness).
/// Workloads that take a short time to `bind()` used to fail one immediate
/// probe and retire the Service (#611). Retained-lease probes stay single-shot.
const SERVICE_PUBLISH_PROBE_GRACE: Duration = Duration::from_secs(8);
const SERVICE_PUBLISH_PROBE_INTERVAL: Duration = Duration::from_millis(250);

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
    ports: Vec<(String, u16, TransportProtocol)>,
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

enum StagedListener {
    Tcp {
        listener: TcpListener,
        endpoint: RuntimeServiceEndpoint,
        guest_port: NonZeroU16,
    },
    Udp {
        socket: Arc<UdpSocket>,
        endpoint: RuntimeServiceEndpoint,
        guest_port: NonZeroU16,
    },
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
                .map(|port| (port.name.clone(), port.container_port, port.protocol))
                .collect(),
        };
        let retained = {
            let leases = self.leases.lock().await;
            leases.get(&key).and_then(|existing| {
                (existing.identity == identity).then(|| existing.endpoints.clone())
            })
        };
        // First publish after bind gets a bounded grace (#611). Re-probing a
        // withdrawn retained lease must fail closed quickly so inspect/re-apply
        // do not burn the grace window and hit the lifecycle control timeout.
        let mut publish_with_grace = true;
        if let Some(endpoints) = retained {
            let mut reachable = true;
            for endpoint in &endpoints {
                let address = endpoint.socket_addr();
                let probe = match endpoint.protocol {
                    TransportProtocol::Tcp => probe_advertised_tcp_endpoint(address).await,
                    TransportProtocol::Udp => probe_advertised_udp_endpoint(address).await,
                };
                if let Err(error) = probe {
                    tracing::warn!(
                        unit_id = %spec.unit_id,
                        port_name = %endpoint.port_name,
                        %address,
                        protocol = endpoint.protocol.as_str(),
                        %error,
                        "Runtime Service advertised endpoint stopped answering; withdrawing lease"
                    );
                    reachable = false;
                    break;
                }
            }
            if reachable {
                let leases = self.leases.lock().await;
                if let Some(existing) = leases.get(&key) {
                    if existing.identity == identity {
                        attach_endpoints(spec, &mut observation, &existing.endpoints)?;
                        return Ok(observation);
                    }
                }
            } else {
                self.leases.lock().await.remove(&key);
                publish_with_grace = false;
            }
        }

        let mut leases = self.leases.lock().await;

        // Keep every listener bound until the complete endpoint set has been
        // validated. A partial bind is never published into Runtime evidence.
        let mut staged = Vec::with_capacity(spec.network.ports.len());
        for port in &spec.network.ports {
            let guest_port = NonZeroU16::new(port.container_port).ok_or_else(|| {
                RuntimeError::Protocol("Runtime Service declared a zero container port".into())
            })?;
            match port.protocol {
                TransportProtocol::Tcp => {
                    let listener = TcpListener::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                        .await
                        .map_err(|error| {
                            RuntimeError::ProviderUnavailable(format!(
                                "Box could not bind Runtime Service TCP port {:?}: {error}",
                                port.name
                            ))
                        })?;
                    let address = listener.local_addr().map_err(|error| {
                        RuntimeError::ProviderUnavailable(format!(
                            "Box could not inspect Runtime Service TCP port {:?}: {error}",
                            port.name
                        ))
                    })?;
                    let endpoint =
                        RuntimeServiceEndpoint::node_local_tcp(&port.name, address.port())
                            .map_err(RuntimeError::Protocol)?;
                    staged.push(StagedListener::Tcp {
                        listener,
                        endpoint,
                        guest_port,
                    });
                }
                TransportProtocol::Udp => {
                    let socket = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
                        .await
                        .map_err(|error| {
                            RuntimeError::ProviderUnavailable(format!(
                                "Box could not bind Runtime Service UDP port {:?}: {error}",
                                port.name
                            ))
                        })?;
                    let address = socket.local_addr().map_err(|error| {
                        RuntimeError::ProviderUnavailable(format!(
                            "Box could not inspect Runtime Service UDP port {:?}: {error}",
                            port.name
                        ))
                    })?;
                    let endpoint = RuntimeServiceEndpoint::new(
                        &port.name,
                        TransportProtocol::Udp,
                        std::net::IpAddr::V4(Ipv4Addr::LOCALHOST),
                        address.port(),
                    )
                    .map_err(RuntimeError::Protocol)?;
                    staged.push(StagedListener::Udp {
                        socket: Arc::new(socket),
                        endpoint,
                        guest_port,
                    });
                }
            }
        }

        // Bind and serve first, then probe each advertised host URL through the
        // live listener. Publishing before that proof lets Running observations
        // advertise dead MicroVM relays (same honesty class as Box#370).
        let mut prepared = staged
            .into_iter()
            .map(|staged| match staged {
                StagedListener::Tcp {
                    listener,
                    endpoint,
                    guest_port,
                } => {
                    let task = tokio::spawn(serve_tcp_endpoint(
                        listener,
                        Arc::clone(&self.connector),
                        Arc::clone(&self.connection_limit),
                        execution_id.clone(),
                        execution_generation,
                        guest_port,
                        endpoint.port_name.clone(),
                    ));
                    (endpoint, task)
                }
                StagedListener::Udp {
                    socket,
                    endpoint,
                    guest_port,
                } => {
                    let task = tokio::spawn(serve_udp_endpoint(
                        socket,
                        Arc::clone(&self.connector),
                        Arc::clone(&self.connection_limit),
                        execution_id.clone(),
                        execution_generation,
                        guest_port,
                        endpoint.port_name.clone(),
                    ));
                    (endpoint, task)
                }
            })
            .collect::<Vec<_>>();

        let probe_targets = prepared
            .iter()
            .map(|(endpoint, _)| {
                (
                    endpoint.port_name.clone(),
                    endpoint.protocol,
                    endpoint.socket_addr(),
                )
            })
            .collect::<Vec<_>>();
        for (port_name, protocol, address) in probe_targets {
            let probe = match (protocol, publish_with_grace) {
                (TransportProtocol::Tcp, true) => {
                    probe_advertised_tcp_endpoint_with_grace(address).await
                }
                (TransportProtocol::Tcp, false) => probe_advertised_tcp_endpoint(address).await,
                (TransportProtocol::Udp, true) => {
                    probe_advertised_udp_endpoint_with_grace(address).await
                }
                (TransportProtocol::Udp, false) => probe_advertised_udp_endpoint(address).await,
            };
            if let Err(error) = probe {
                for (_, task) in prepared.drain(..) {
                    task.abort();
                    let _ = task.await;
                }
                observation.clear_service_endpoints();
                let grace_note = if publish_with_grace {
                    format!(" after {SERVICE_PUBLISH_PROBE_GRACE:?} grace")
                } else {
                    String::new()
                };
                return Err(RuntimeError::ProviderUnavailable(format!(
                    "Box Runtime Service endpoint {port_name:?} at {address} is not reachable through the advertised host URL{grace_note}: {error}"
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
                    "peer closed before the generation-fenced guest relay opened (guest may not be listening yet)".into(),
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

async fn probe_advertised_tcp_endpoint_with_grace(address: SocketAddr) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + SERVICE_PUBLISH_PROBE_GRACE;
    loop {
        match probe_advertised_tcp_endpoint(address).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(error);
                }
                tokio::time::sleep(SERVICE_PUBLISH_PROBE_INTERVAL).await;
            }
        }
    }
}

/// Prove the advertised UDP URL can accept a datagram without immediate error.
///
/// Unlike TCP there is no accept handshake; a successful send into the live
/// socket is the host-side proof that the listener is still bound.
async fn probe_advertised_udp_endpoint(address: SocketAddr) -> Result<(), String> {
    tokio::time::timeout(SERVICE_CONNECT_TIMEOUT, async {
        let probe = UdpSocket::bind(SocketAddr::from((Ipv4Addr::LOCALHOST, 0)))
            .await
            .map_err(|error| error.to_string())?;
        probe
            .send_to(UDP_PROBE_PAYLOAD, address)
            .await
            .map_err(|error| error.to_string())?;
        // Brief settle so a bind race cannot invent reachability while the
        // serve task is still aborting after a failed sibling probe.
        tokio::time::sleep(Duration::from_millis(50)).await;
        Ok(())
    })
    .await
    .map_err(|_| format!("timed out probing advertised UDP endpoint {address}"))?
}

async fn probe_advertised_udp_endpoint_with_grace(address: SocketAddr) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + SERVICE_PUBLISH_PROBE_GRACE;
    loop {
        match probe_advertised_udp_endpoint(address).await {
            Ok(()) => return Ok(()),
            Err(error) => {
                if tokio::time::Instant::now() >= deadline {
                    return Err(error);
                }
                tokio::time::sleep(SERVICE_PUBLISH_PROBE_INTERVAL).await;
            }
        }
    }
}

async fn serve_tcp_endpoint(
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
            if let Err(error) = relay_tcp_connection(
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

async fn relay_tcp_connection(
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

async fn serve_udp_endpoint(
    socket: Arc<UdpSocket>,
    connector: Arc<dyn ExecutionPortConnector>,
    connection_limit: Arc<Semaphore>,
    execution_id: ExecutionId,
    execution_generation: ExecutionGeneration,
    guest_port: NonZeroU16,
    port_name: String,
) {
    let associations: Arc<Mutex<BTreeMap<SocketAddr, tokio::sync::mpsc::Sender<Vec<u8>>>>> =
        Arc::new(Mutex::new(BTreeMap::new()));
    let mut buf = vec![0_u8; 65_535];
    loop {
        let (n, client) = match socket.recv_from(&mut buf).await {
            Ok(received) => received,
            Err(error) => {
                tracing::warn!(
                    execution_id = %execution_id,
                    port_name,
                    error = %error,
                    "Runtime Service UDP endpoint recv failed"
                );
                tokio::time::sleep(Duration::from_millis(100)).await;
                continue;
            }
        };
        let payload = buf[..n].to_vec();
        // Drop probe-only traffic after proving the socket is live; do not open
        // a guest association for the synthetic probe payload.
        if payload == UDP_PROBE_PAYLOAD {
            continue;
        }

        {
            let mut guard = associations.lock().await;
            guard.retain(|_, tx| !tx.is_closed());
            if let Some(tx) = guard.get(&client) {
                let _ = tx.try_send(payload);
                continue;
            }
            if guard.len() >= MAX_UDP_ASSOCIATIONS {
                tracing::warn!(
                    execution_id = %execution_id,
                    port_name,
                    limit = MAX_UDP_ASSOCIATIONS,
                    "Runtime Service UDP association limit reached"
                );
                continue;
            }

            let permit = match Arc::clone(&connection_limit).try_acquire_owned() {
                Ok(permit) => permit,
                Err(_) => {
                    tracing::warn!(
                        execution_id = %execution_id,
                        port_name,
                        "Runtime Service UDP connection budget exhausted"
                    );
                    continue;
                }
            };
            let (tx, rx) = tokio::sync::mpsc::channel::<Vec<u8>>(32);
            let _ = tx.try_send(payload);
            guard.insert(client, tx);

            let connector = Arc::clone(&connector);
            let host_socket = Arc::clone(&socket);
            let relay_execution_id = execution_id.clone();
            let relay_port_name = port_name.clone();
            let associations_for_task = Arc::clone(&associations);
            tokio::spawn(async move {
                let _permit = permit;
                let result = relay_udp_association(
                    connector.as_ref(),
                    &relay_execution_id,
                    execution_generation,
                    guest_port,
                    host_socket,
                    client,
                    rx,
                )
                .await;
                associations_for_task.lock().await.remove(&client);
                if let Err(error) = result {
                    tracing::warn!(
                        execution_id = %relay_execution_id,
                        port_name = relay_port_name,
                        peer = %client,
                        error = %error,
                        "Runtime Service UDP association failed"
                    );
                }
            });
        }
    }
}

async fn relay_udp_association(
    connector: &dyn ExecutionPortConnector,
    execution_id: &ExecutionId,
    execution_generation: ExecutionGeneration,
    guest_port: NonZeroU16,
    host_socket: Arc<UdpSocket>,
    client: SocketAddr,
    mut host_rx: tokio::sync::mpsc::Receiver<Vec<u8>>,
) -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let mut guest: ExecutionUdpPort = connector
        .connect_udp_port(
            execution_id,
            execution_generation,
            guest_port,
            SERVICE_CONNECT_TIMEOUT,
        )
        .await?;
    let mut last_activity = std::time::Instant::now();
    loop {
        if last_activity.elapsed() > UDP_IDLE_TIMEOUT {
            return Ok(());
        }
        tokio::select! {
            biased;
            host = host_rx.recv() => {
                let Some(payload) = host else {
                    return Ok(());
                };
                guest.send_datagram(&payload).await?;
                last_activity = std::time::Instant::now();
            }
            guest_datagram = guest.recv_datagram(65_535) => {
                let payload = guest_datagram?;
                host_socket.send_to(&payload, client).await?;
                last_activity = std::time::Instant::now();
            }
            _ = tokio::time::sleep(Duration::from_millis(200)) => {}
        }
    }
}
