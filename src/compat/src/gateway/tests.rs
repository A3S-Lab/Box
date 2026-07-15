use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::num::{NonZeroU16, NonZeroUsize};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use a3s_box_core::{
    resolve_execution, BoxConfig, ExecutionGeneration, ExecutionId, ExecutionIsolation,
    ExecutionLease, ExecutionManager, ExecutionManagerError, ExecutionManagerResult,
    ExecutionPortConnector, ExecutionPortStream, ExecutionState, ExecutionStatus, KillOutcome,
    OperationId, ReconcileOutcome,
};
use async_trait::async_trait;
use axum::body::Body;
use axum::http::header::{HOST, ORIGIN};
use axum::http::{HeaderValue, Method, Request, Response, StatusCode, Version};
use chrono::{DateTime, TimeZone, Utc};
use hyper::body::to_bytes;
use hyper::service::service_fn;
use rustls::pki_types::ServerName;
use tokio::net::{TcpListener, TcpStream};

use crate::control::{
    Clock, LifecyclePolicy, MemorySandboxRepository, NewSandboxRecord, OnTimeoutAction,
    RotatingTokenProvider, SandboxCredentials, SandboxId, SandboxRecord, SandboxRepository,
    SecretToken, TokenIssuer, TokenKeyMaterial, TokenScope,
};
use crate::routing::{
    RouteLeaseService, SandboxDomain, SandboxRouteParser, SandboxRoutePolicy,
    CODE_INTERPRETER_PORT, ENVD_ACCESS_TOKEN_HEADER, ENVD_PORT, SANDBOX_ID_HEADER,
    SANDBOX_PORT_HEADER, TRAFFIC_ACCESS_TOKEN_HEADER,
};

use super::{DataPlaneGateway, DataPlaneGatewayConfig, DataPlaneProxy};

struct FixedClock(DateTime<Utc>);

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        self.0
    }
}

#[derive(Clone)]
struct TcpConnector {
    address: SocketAddr,
    calls: Arc<Mutex<Vec<(String, u64, u16)>>>,
}

#[async_trait]
impl ExecutionPortConnector for TcpConnector {
    async fn connect_port(
        &self,
        execution_id: &ExecutionId,
        generation: ExecutionGeneration,
        port: NonZeroU16,
        _timeout: Duration,
    ) -> ExecutionManagerResult<ExecutionPortStream> {
        self.calls
            .lock()
            .unwrap()
            .push((execution_id.to_string(), generation.get(), port.get()));
        let stream = TcpStream::connect(self.address)
            .await
            .map_err(|error| ExecutionManagerError::Unavailable(error.to_string()))?;
        Ok(Box::pin(stream))
    }
}

struct RunningExecutionManager {
    status: ExecutionStatus,
}

#[async_trait]
impl ExecutionManager for RunningExecutionManager {
    async fn inspect(&self, execution_id: &ExecutionId) -> ExecutionManagerResult<ExecutionStatus> {
        if execution_id != &self.status.execution_id {
            return Err(ExecutionManagerError::NotFound(execution_id.clone()));
        }
        Ok(self.status.clone())
    }

    async fn pause(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
        _keep_memory: bool,
    ) -> ExecutionManagerResult<ExecutionLease> {
        Err(unsupported_manager_operation())
    }

    async fn resume(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<ExecutionLease> {
        Err(unsupported_manager_operation())
    }

    async fn kill(
        &self,
        _execution_id: &ExecutionId,
        _generation: ExecutionGeneration,
    ) -> ExecutionManagerResult<KillOutcome> {
        Err(unsupported_manager_operation())
    }

    async fn reconcile(
        &self,
        _operation_id: &OperationId,
    ) -> ExecutionManagerResult<ReconcileOutcome> {
        Err(unsupported_manager_operation())
    }
}

fn unsupported_manager_operation() -> ExecutionManagerError {
    ExecutionManagerError::Unavailable("unsupported test manager operation".to_string())
}

struct Harness {
    proxy: DataPlaneProxy,
    parser: SandboxRouteParser,
    leases: RouteLeaseService,
    repository: Arc<MemorySandboxRepository>,
    executions: Arc<RunningExecutionManager>,
    connector: Arc<TcpConnector>,
    sandbox_id: SandboxId,
    envd_token: SecretToken,
    traffic_token: SecretToken,
    upstream: tokio::task::JoinHandle<()>,
}

impl Harness {
    async fn new() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let upstream = tokio::spawn(async move {
            loop {
                let Ok((socket, _)) = listener.accept().await else {
                    return;
                };
                tokio::spawn(async move {
                    let service = service_fn(upstream_response);
                    let mut http = hyper::server::conn::Http::new();
                    http.http1_only(true);
                    let _ = http.serve_connection(socket, service).with_upgrades().await;
                });
            }
        });

        let now = Utc
            .with_ymd_and_hms(2026, 7, 15, 16, 0, 0)
            .single()
            .unwrap();
        let tokens = Arc::new(
            RotatingTokenProvider::new(1, [TokenKeyMaterial::new(1, &[7; 32], &[8; 32]).unwrap()])
                .unwrap(),
        );
        let envd = tokens.issue(TokenScope::Envd).await.unwrap();
        let traffic = tokens.issue(TokenScope::Traffic).await.unwrap();
        let sandbox_id = SandboxId::new("sandbox-gateway-1").unwrap();
        let mut config = BoxConfig {
            isolation: ExecutionIsolation::Sandbox,
            image: "alpine:3.20".to_string(),
            ..BoxConfig::default()
        };
        config.network = a3s_box_core::NetworkMode::None;
        let plan = resolve_execution(&config).unwrap();
        let routing = SandboxRoutePolicy::default()
            .with_port(CODE_INTERPRETER_PORT, TokenScope::Traffic)
            .unwrap();
        let mut record = SandboxRecord::creating(NewSandboxRecord {
            sandbox_id: sandbox_id.clone(),
            operation_id: OperationId::new("operation-gateway-1").unwrap(),
            owner_id: "owner-gateway".to_string(),
            template_id: "gateway-template".to_string(),
            plan: plan.clone(),
            resources: config.resources.clone(),
            lifecycle: LifecyclePolicy {
                on_timeout: OnTimeoutAction::Kill,
                auto_resume: false,
                keep_memory_on_pause: false,
            },
            created_at: now,
            expires_at: now + chrono::Duration::minutes(5),
            metadata: BTreeMap::new(),
            envd_version: "0.1.3".to_string(),
            secure: true,
            allow_internet_access: Some(false),
            credentials: SandboxCredentials {
                envd: envd.stored,
                traffic: traffic.stored,
            },
            routing,
        })
        .unwrap();
        record
            .mark_running(ExecutionLease {
                execution_id: ExecutionId::new("execution-gateway-1").unwrap(),
                generation: ExecutionGeneration::INITIAL,
                plan: plan.clone(),
                resources: config.resources,
                started_at: now,
            })
            .unwrap();
        let executions = Arc::new(RunningExecutionManager {
            status: ExecutionStatus {
                execution_id: ExecutionId::new("execution-gateway-1").unwrap(),
                generation: ExecutionGeneration::INITIAL,
                state: ExecutionState::Running,
                plan: plan.clone(),
            },
        });
        let repository = Arc::new(MemorySandboxRepository::default());
        repository.insert(record).await.unwrap();
        let leases = RouteLeaseService::new(repository.clone(), tokens, Arc::new(FixedClock(now)));
        let connector = Arc::new(TcpConnector {
            address,
            calls: Arc::new(Mutex::new(Vec::new())),
        });
        let parser = SandboxRouteParser::new(SandboxDomain::new("box.example.com").unwrap());
        let proxy = DataPlaneProxy::new(
            parser.clone(),
            leases.clone(),
            executions.clone(),
            connector.clone(),
            Duration::from_secs(2),
        );
        Self {
            proxy,
            parser,
            leases,
            repository,
            executions,
            connector,
            sandbox_id,
            envd_token: envd.secret,
            traffic_token: traffic.secret,
            upstream,
        }
    }

    fn direct_request(
        &self,
        port: u16,
        token_header: &'static str,
        token: &SecretToken,
    ) -> Request<Body> {
        Request::builder()
            .method(Method::POST)
            .uri("/echo?value=one")
            .header(HOST, format!("{port}-{}.box.example.com", self.sandbox_id))
            .header(token_header, token.expose_secret())
            .header("x-forwarded-for", "203.0.113.5")
            .body(Body::from("hello-data-plane"))
            .unwrap()
    }

    fn call_count(&self) -> usize {
        self.connector.calls.lock().unwrap().len()
    }

    fn health_request(&self, token: &SecretToken) -> Request<Body> {
        Request::builder()
            .uri("/health")
            .header(
                HOST,
                format!("{}-{}.box.example.com", ENVD_PORT, self.sandbox_id),
            )
            .header(ENVD_ACCESS_TOKEN_HEADER, token.expose_secret())
            .body(Body::empty())
            .unwrap()
    }
}

impl Drop for Harness {
    fn drop(&mut self) {
        self.upstream.abort();
    }
}

async fn upstream_response(
    request: Request<Body>,
) -> Result<Response<Body>, std::convert::Infallible> {
    let method = request.method().to_string();
    let uri = request.uri().to_string();
    let version = format!("{:?}", request.version());
    let headers = request.headers().clone();
    let body = to_bytes(request.into_body()).await.unwrap();
    let value = serde_json::json!({
        "method": method,
        "uri": uri,
        "version": version,
        "host": headers.get(HOST).and_then(|value| value.to_str().ok()),
        "body": String::from_utf8_lossy(&body),
        "envdTokenForwarded": headers.contains_key(ENVD_ACCESS_TOKEN_HEADER),
        "trafficTokenForwarded": headers.contains_key(TRAFFIC_ACCESS_TOKEN_HEADER),
        "sandboxIdForwarded": headers.contains_key(SANDBOX_ID_HEADER),
        "sandboxPortForwarded": headers.contains_key(SANDBOX_PORT_HEADER),
        "forwardedProto": headers.get("x-forwarded-proto").and_then(|value| value.to_str().ok()),
        "forwardedHost": headers.get("x-forwarded-host").and_then(|value| value.to_str().ok()),
        "forwardedFor": headers.get("x-forwarded-for").and_then(|value| value.to_str().ok()),
    });
    let mut response = Response::new(Body::from(value.to_string()));
    response
        .headers_mut()
        .insert("content-type", HeaderValue::from_static("application/json"));
    Ok(response)
}

#[tokio::test]
async fn translates_downstream_http2_to_plaintext_http1_upstream() {
    let harness = Harness::new().await;
    let mut request = harness.direct_request(
        CODE_INTERPRETER_PORT,
        TRAFFIC_ACCESS_TOKEN_HEADER,
        &harness.traffic_token,
    );
    let authority = request.headers_mut().remove(HOST).unwrap();
    *request.uri_mut() = format!("https://{}/echo?value=one", authority.to_str().unwrap())
        .parse()
        .unwrap();
    *request.version_mut() = Version::HTTP_2;

    let response = harness.proxy.handle(request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body()).await.unwrap()).unwrap();
    assert_eq!(body["version"], "HTTP/1.1");
    assert_eq!(body["host"], authority.to_str().unwrap());
    assert_eq!(harness.call_count(), 1);
}

#[tokio::test]
async fn authenticated_direct_route_proxies_stream_and_strips_edge_credentials() {
    let harness = Harness::new().await;
    let request = harness.direct_request(
        CODE_INTERPRETER_PORT,
        TRAFFIC_ACCESS_TOKEN_HEADER,
        &harness.traffic_token,
    );
    let response = harness.proxy.handle(request).await;
    assert_eq!(response.status(), StatusCode::OK);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(response.into_body()).await.unwrap()).unwrap();
    assert_eq!(body["method"], "POST");
    assert_eq!(body["uri"], "/echo?value=one");
    assert_eq!(body["body"], "hello-data-plane");
    assert_eq!(body["envdTokenForwarded"], false);
    assert_eq!(body["trafficTokenForwarded"], false);
    assert_eq!(body["sandboxIdForwarded"], false);
    assert_eq!(body["sandboxPortForwarded"], false);
    assert_eq!(body["forwardedProto"], "https");
    assert!(body["forwardedHost"]
        .as_str()
        .unwrap()
        .starts_with("49999-sandbox-gateway-1"));
    assert!(body["forwardedFor"].is_null());
    assert_eq!(harness.call_count(), 1);
}

#[tokio::test]
async fn token_scope_is_checked_before_opening_an_upstream_connection() {
    let harness = Harness::new().await;
    let swapped = harness.direct_request(
        CODE_INTERPRETER_PORT,
        TRAFFIC_ACCESS_TOKEN_HEADER,
        &harness.envd_token,
    );
    assert_eq!(
        harness.proxy.handle(swapped).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(harness.call_count(), 0);

    let valid = harness.direct_request(
        CODE_INTERPRETER_PORT,
        TRAFFIC_ACCESS_TOKEN_HEADER,
        &harness.traffic_token,
    );
    assert_eq!(harness.proxy.handle(valid).await.status(), StatusCode::OK);
    assert_eq!(harness.call_count(), 1);
}

#[tokio::test]
async fn shared_routes_and_browser_preflight_use_the_same_validated_parser() {
    let harness = Harness::new().await;
    let preflight = Request::builder()
        .method(Method::OPTIONS)
        .uri("/health")
        .header(HOST, "sandbox.box.example.com")
        .header(SANDBOX_ID_HEADER, harness.sandbox_id.as_str())
        .header(SANDBOX_PORT_HEADER, ENVD_PORT.to_string())
        .header(ORIGIN, "https://app.example.com")
        .header("access-control-request-method", "GET")
        .header("access-control-request-headers", "X-Access-Token")
        .body(Body::empty())
        .unwrap();
    let response = harness.proxy.handle(preflight).await;
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    assert_eq!(response.headers()["access-control-allow-origin"], "*");
    assert_eq!(harness.call_count(), 0);

    let shared = Request::builder()
        .uri("/health")
        .header(HOST, "sandbox.box.example.com")
        .header(SANDBOX_ID_HEADER, harness.sandbox_id.as_str())
        .header(SANDBOX_PORT_HEADER, ENVD_PORT.to_string())
        .header(ENVD_ACCESS_TOKEN_HEADER, harness.envd_token.expose_secret())
        .body(Body::empty())
        .unwrap();
    assert_eq!(
        harness.proxy.handle(shared).await.status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(harness.call_count(), 0);
}

#[tokio::test]
async fn authenticated_terminal_health_returns_false_without_reopening_traffic_routes() {
    let harness = Harness::new().await;
    let mut record = harness
        .repository
        .get(&harness.sandbox_id)
        .await
        .unwrap()
        .unwrap();
    let expected = record.generation();
    record.begin_kill().unwrap();
    harness
        .repository
        .compare_and_swap(&harness.sandbox_id, expected, record.clone())
        .await
        .unwrap();
    let expected = record.generation();
    record.mark_killed().unwrap();
    harness
        .repository
        .compare_and_swap(&harness.sandbox_id, expected, record)
        .await
        .unwrap();

    let inactive = harness
        .proxy
        .handle(harness.health_request(&harness.envd_token))
        .await;
    assert_eq!(inactive.status(), StatusCode::BAD_GATEWAY);
    let body: serde_json::Value =
        serde_json::from_slice(&to_bytes(inactive.into_body()).await.unwrap()).unwrap();
    assert_eq!(body["code"], "SANDBOX_NOT_RUNNING");

    let unauthorized = harness
        .proxy
        .handle(harness.health_request(&harness.traffic_token))
        .await;
    assert_eq!(unauthorized.status(), StatusCode::UNAUTHORIZED);

    let traffic = harness.direct_request(
        CODE_INTERPRETER_PORT,
        TRAFFIC_ACCESS_TOKEN_HEADER,
        &harness.traffic_token,
    );
    assert_eq!(
        harness.proxy.handle(traffic).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(harness.call_count(), 0);
}

#[tokio::test]
async fn wildcard_tls_listener_serves_an_authenticated_route() {
    let harness = Harness::new().await;
    let temporary = tempfile::tempdir().unwrap();
    let rcgen::CertifiedKey { cert, key_pair } = rcgen::generate_simple_self_signed(vec![
        "*.box.example.com".to_string(),
        "sandbox.box.example.com".to_string(),
    ])
    .unwrap();
    let certificate_path = temporary.path().join("certificate.pem");
    let private_key_path = temporary.path().join("private-key.pem");
    std::fs::write(&certificate_path, cert.pem()).unwrap();
    std::fs::write(&private_key_path, key_pair.serialize_pem()).unwrap();

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let config = DataPlaneGatewayConfig {
        listen: address,
        certificate_path,
        private_key_path,
        max_connections: NonZeroUsize::new(16).unwrap(),
        handshake_timeout: Duration::from_secs(2),
        connect_timeout: Duration::from_secs(2),
        drain_timeout: Duration::from_secs(2),
    };
    let gateway = DataPlaneGateway::build(
        config,
        harness.parser.clone(),
        harness.leases.clone(),
        harness.executions.clone(),
        harness.connector.clone(),
    )
    .await
    .unwrap();
    let (shutdown_sender, shutdown_receiver) = tokio::sync::watch::channel(false);
    let gateway_task = tokio::spawn(gateway.serve(listener, shutdown_receiver));

    let _ = rustls::crypto::ring::default_provider().install_default();
    let mut roots = rustls::RootCertStore::empty();
    roots.add(cert.der().clone()).unwrap();
    let mut client_config = rustls::ClientConfig::builder()
        .with_root_certificates(roots)
        .with_no_client_auth();
    client_config.alpn_protocols = vec![b"http/1.1".to_vec()];
    let connector = tokio_rustls::TlsConnector::from(Arc::new(client_config));
    let host = format!("{}-{}.box.example.com", ENVD_PORT, harness.sandbox_id);
    let tcp = TcpStream::connect(address).await.unwrap();
    let tls = connector
        .connect(ServerName::try_from(host.clone()).unwrap(), tcp)
        .await
        .unwrap();
    let (mut sender, connection) = hyper::client::conn::handshake(tls).await.unwrap();
    let client_task = tokio::spawn(connection);
    let request = Request::builder()
        .uri("/health")
        .header(HOST, host)
        .header(ENVD_ACCESS_TOKEN_HEADER, harness.envd_token.expose_secret())
        .body(Body::empty())
        .unwrap();
    let response = sender.send_request(request).await.unwrap();
    assert_eq!(response.status(), StatusCode::NO_CONTENT);
    let _ = to_bytes(response.into_body()).await.unwrap();
    drop(sender);
    client_task.await.unwrap().unwrap();
    shutdown_sender.send(true).unwrap();
    gateway_task.await.unwrap().unwrap();
}
