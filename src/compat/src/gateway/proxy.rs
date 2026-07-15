use std::sync::Arc;
use std::time::Duration;

use a3s_box_core::{
    ExecutionManager, ExecutionManagerError, ExecutionPortConnector, ExecutionPortStream,
    ExecutionSessionManager,
};
use axum::body::Body;
use axum::http::header::{
    ACCESS_CONTROL_ALLOW_HEADERS, ACCESS_CONTROL_ALLOW_METHODS, ACCESS_CONTROL_ALLOW_ORIGIN,
    ACCESS_CONTROL_EXPOSE_HEADERS, ACCESS_CONTROL_MAX_AGE, CONNECTION, CONTENT_TYPE, HOST, ORIGIN,
    TE, TRAILER, TRANSFER_ENCODING, UPGRADE,
};
use axum::http::{
    HeaderMap, HeaderName, HeaderValue, Method, Request, Response, StatusCode, Uri, Version,
};
use hyper::client::conn;
use thiserror::Error;
use tokio::io::copy_bidirectional;
use tracing::debug;

use crate::control::EnvdMode;
use crate::envd::EnvdBroker;
use crate::routing::{
    EnvdHealthResolution, ParsedSandboxRoute, RouteLeaseError, RouteLeaseService, RouteParseError,
    SandboxRouteParser, ENVD_ACCESS_TOKEN_HEADER, ENVD_PORT, SANDBOX_ID_HEADER,
    SANDBOX_PORT_HEADER, TRAFFIC_ACCESS_TOKEN_HEADER,
};

const ACCESS_CONTROL_REQUEST_METHOD: &str = "access-control-request-method";
const ACCESS_CONTROL_REQUEST_HEADERS: &str = "access-control-request-headers";
const FORWARDED: &str = "forwarded";
const X_FORWARDED_FOR: &str = "x-forwarded-for";
const X_FORWARDED_HOST: &str = "x-forwarded-host";
const X_FORWARDED_PORT: &str = "x-forwarded-port";
const X_FORWARDED_PROTO: &str = "x-forwarded-proto";
const PROXY_CONNECTION: &str = "proxy-connection";
const PROXY_AUTHENTICATE: &str = "proxy-authenticate";
const PROXY_AUTHORIZATION: &str = "proxy-authorization";
const KEEP_ALIVE: &str = "keep-alive";
const EXPOSED_HEADERS: &str =
    "Grpc-Status, Grpc-Message, Grpc-Status-Details-Bin, Connect-Content-Encoding, Trailer";

#[derive(Clone)]
pub struct DataPlaneProxy {
    parser: SandboxRouteParser,
    leases: RouteLeaseService,
    envd: EnvdBroker,
    connector: Arc<dyn ExecutionPortConnector>,
    connect_timeout: Duration,
}

impl DataPlaneProxy {
    pub(crate) fn new(
        parser: SandboxRouteParser,
        leases: RouteLeaseService,
        executions: Arc<dyn ExecutionManager>,
        sessions: Arc<dyn ExecutionSessionManager>,
        connector: Arc<dyn ExecutionPortConnector>,
        connect_timeout: Duration,
    ) -> Self {
        Self {
            parser,
            leases,
            envd: EnvdBroker::new(executions, sessions),
            connector,
            connect_timeout,
        }
    }

    pub async fn handle(&self, mut request: Request<Body>) -> Response<Body> {
        let cors = request.headers().contains_key(ORIGIN);
        let route = match self.parser.parse_uri(request.uri(), request.headers()) {
            Ok(route) => route,
            Err(error) => return with_cors(error_response(ProxyFailure::Route(error)), cors),
        };
        if is_cors_preflight(&request) {
            return preflight_response(request.headers());
        }

        if is_envd_health(&request, &route) {
            let resolution = match self
                .leases
                .resolve_envd_health(&route, request.headers())
                .await
            {
                Ok(resolution) => resolution,
                Err(error) => return with_cors(error_response(ProxyFailure::Lease(error)), cors),
            };
            let response = match resolution {
                EnvdHealthResolution::Running(lease) => {
                    if lease.envd_mode() == EnvdMode::Runtime {
                        return with_cors(self.proxy_runtime(&mut request, &lease).await, cors);
                    }
                    self.envd.handle(request, &lease).await
                }
                EnvdHealthResolution::Inactive => self.envd.inactive_health(),
            };
            return with_cors(response, cors);
        }

        let lease = match self.leases.resolve(&route, request.headers()).await {
            Ok(lease) => lease,
            Err(error) => return with_cors(error_response(ProxyFailure::Lease(error)), cors),
        };
        if lease.port().get() == ENVD_PORT && lease.envd_mode() == EnvdMode::Broker {
            return with_cors(self.envd.handle(request, &lease).await, cors);
        }
        with_cors(self.proxy_runtime(&mut request, &lease).await, cors)
    }

    async fn proxy_runtime(
        &self,
        request: &mut Request<Body>,
        lease: &crate::routing::RouteLease,
    ) -> Response<Body> {
        let stream = match self
            .connector
            .connect_port(
                lease.execution_id(),
                lease.execution_generation(),
                lease.port(),
                self.connect_timeout,
            )
            .await
        {
            Ok(stream) => stream,
            Err(error) => return error_response(ProxyFailure::Connect(error)),
        };

        match proxy_upstream(request, stream).await {
            Ok(response) => response,
            Err(error) => {
                debug!(%error, "sandbox data-plane upstream request failed");
                error_response(error)
            }
        }
    }
}

fn is_envd_health(request: &Request<Body>, route: &ParsedSandboxRoute) -> bool {
    route.port.get() == ENVD_PORT
        && request.method() == Method::GET
        && request.uri().path() == "/health"
}

async fn proxy_upstream(
    request: &mut Request<Body>,
    stream: ExecutionPortStream,
) -> Result<Response<Body>, ProxyFailure> {
    let downstream_version = request.version();
    let upgrade = is_upgrade(request.headers(), downstream_version);
    let downstream_upgrade = upgrade.then(|| hyper::upgrade::on(&mut *request));
    normalize_http1_upstream(request)?;
    sanitize_request_headers(request.headers_mut(), downstream_version, upgrade);
    // Downstream ALPN does not describe the plaintext Sandbox origin. E2B's
    // data plane translates both HTTP/1.1 and HTTP/2 clients to HTTP/1.1 here.
    *request.version_mut() = Version::HTTP_11;

    let (mut sender, connection) = conn::Builder::new().handshake(stream).await?;
    tokio::spawn(async move {
        if let Err(error) = connection.await {
            debug!(%error, "sandbox data-plane upstream connection closed");
        }
    });
    let outbound = std::mem::replace(request, Request::new(Body::empty()));
    let mut response = sender.send_request(outbound).await?;
    let switched_protocols = response.status() == StatusCode::SWITCHING_PROTOCOLS && upgrade;
    sanitize_response_headers(
        response.headers_mut(),
        downstream_version,
        switched_protocols,
    );

    if let Some(downstream_upgrade) = downstream_upgrade.filter(|_| switched_protocols) {
        let upstream_upgrade = hyper::upgrade::on(&mut response);
        tokio::spawn(async move {
            let (mut downstream, mut upstream) =
                match tokio::try_join!(downstream_upgrade, upstream_upgrade) {
                    Ok(upgrades) => upgrades,
                    Err(error) => {
                        debug!(%error, "sandbox data-plane protocol upgrade failed");
                        return;
                    }
                };
            if let Err(error) = copy_bidirectional(&mut downstream, &mut upstream).await {
                debug!(%error, "sandbox data-plane upgraded stream closed");
            }
        });
    }
    Ok(response)
}

fn normalize_http1_upstream(request: &mut Request<Body>) -> Result<(), ProxyFailure> {
    let authority = request
        .uri()
        .authority()
        .map(|value| value.as_str().to_string());
    let path_and_query = request
        .uri()
        .path_and_query()
        .map(|value| value.as_str())
        .unwrap_or("/")
        .to_string();
    if !request.headers().contains_key(HOST) {
        let authority = authority.ok_or(ProxyFailure::InvalidUpstreamUri)?;
        let host =
            HeaderValue::from_str(&authority).map_err(|_| ProxyFailure::InvalidUpstreamUri)?;
        request.headers_mut().insert(HOST, host);
    }
    *request.uri_mut() = path_and_query
        .parse::<Uri>()
        .map_err(|_| ProxyFailure::InvalidUpstreamUri)?;
    Ok(())
}

fn sanitize_request_headers(headers: &mut HeaderMap, version: Version, upgrade: bool) {
    for name in [
        ENVD_ACCESS_TOKEN_HEADER,
        TRAFFIC_ACCESS_TOKEN_HEADER,
        SANDBOX_ID_HEADER,
        SANDBOX_PORT_HEADER,
        FORWARDED,
        X_FORWARDED_FOR,
        X_FORWARDED_HOST,
        X_FORWARDED_PORT,
        X_FORWARDED_PROTO,
    ] {
        headers.remove(name);
    }
    let original_host = headers.get(HOST).cloned();
    strip_hop_headers(headers, version, upgrade);
    headers.insert(X_FORWARDED_PROTO, HeaderValue::from_static("https"));
    if let Some(host) = original_host {
        headers.insert(X_FORWARDED_HOST, host);
    }
}

fn sanitize_response_headers(headers: &mut HeaderMap, version: Version, upgrade: bool) {
    strip_hop_headers(headers, version, upgrade);
}

fn strip_hop_headers(headers: &mut HeaderMap, version: Version, upgrade: bool) {
    let nominated = connection_header_names(headers);
    for name in nominated {
        if !upgrade || (name != UPGRADE && name != CONNECTION) {
            headers.remove(name);
        }
    }
    for name in [
        HeaderName::from_static(PROXY_CONNECTION),
        HeaderName::from_static(PROXY_AUTHENTICATE),
        HeaderName::from_static(PROXY_AUTHORIZATION),
        HeaderName::from_static(KEEP_ALIVE),
        TRANSFER_ENCODING,
    ] {
        headers.remove(name);
    }
    if !upgrade || version == Version::HTTP_2 {
        headers.remove(CONNECTION);
        headers.remove(UPGRADE);
    }
    if version == Version::HTTP_2 && headers.get(TE) != Some(&HeaderValue::from_static("trailers"))
    {
        headers.remove(TE);
    }
    if version == Version::HTTP_2 {
        // HTTP/2 carries trailers without the HTTP/1.1 Trailer declaration.
        headers.remove(TRAILER);
    }
}

fn connection_header_names(headers: &HeaderMap) -> Vec<HeaderName> {
    headers
        .get_all(CONNECTION)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .flat_map(|value| value.split(','))
        .filter_map(|value| value.trim().parse::<HeaderName>().ok())
        .collect()
}

fn is_upgrade(headers: &HeaderMap, version: Version) -> bool {
    version != Version::HTTP_2
        && headers.contains_key(UPGRADE)
        && headers
            .get_all(CONNECTION)
            .iter()
            .filter_map(|value| value.to_str().ok())
            .flat_map(|value| value.split(','))
            .any(|value| value.trim().eq_ignore_ascii_case("upgrade"))
}

fn is_cors_preflight(request: &Request<Body>) -> bool {
    request.method() == Method::OPTIONS
        && request.headers().contains_key(ORIGIN)
        && request
            .headers()
            .contains_key(ACCESS_CONTROL_REQUEST_METHOD)
}

fn preflight_response(request_headers: &HeaderMap) -> Response<Body> {
    let method = request_headers
        .get(ACCESS_CONTROL_REQUEST_METHOD)
        .cloned()
        .unwrap_or_else(|| HeaderValue::from_static("GET, POST, PUT, PATCH, DELETE, OPTIONS"));
    let headers = request_headers
        .get(ACCESS_CONTROL_REQUEST_HEADERS)
        .cloned()
        .unwrap_or_else(|| {
            HeaderValue::from_static(
                "Authorization, Content-Type, X-Access-Token, E2B-Traffic-Access-Token, E2b-Sandbox-Id, E2b-Sandbox-Port, Connect-Protocol-Version, Connect-Timeout-Ms",
            )
        });
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::NO_CONTENT;
    response
        .headers_mut()
        .insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
    response
        .headers_mut()
        .insert(ACCESS_CONTROL_ALLOW_METHODS, method);
    response
        .headers_mut()
        .insert(ACCESS_CONTROL_ALLOW_HEADERS, headers);
    response
        .headers_mut()
        .insert(ACCESS_CONTROL_MAX_AGE, HeaderValue::from_static("600"));
    response
}

fn with_cors(mut response: Response<Body>, enabled: bool) -> Response<Body> {
    if enabled {
        response
            .headers_mut()
            .insert(ACCESS_CONTROL_ALLOW_ORIGIN, HeaderValue::from_static("*"));
        response.headers_mut().insert(
            ACCESS_CONTROL_EXPOSE_HEADERS,
            HeaderValue::from_static(EXPOSED_HEADERS),
        );
    }
    response
}

fn error_response(error: ProxyFailure) -> Response<Body> {
    let (status, code, message) = error.public_error();
    let body = serde_json::json!({ "code": code, "message": message }).to_string();
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static("application/json"));
    response
}

#[derive(Debug, Error)]
enum ProxyFailure {
    #[error(transparent)]
    Route(#[from] RouteParseError),
    #[error(transparent)]
    Lease(#[from] RouteLeaseError),
    #[error(transparent)]
    Connect(#[from] ExecutionManagerError),
    #[error("sandbox upstream URI is invalid")]
    InvalidUpstreamUri,
    #[error("sandbox upstream HTTP transport failed: {0}")]
    Upstream(#[from] hyper::Error),
}

impl ProxyFailure {
    fn public_error(&self) -> (StatusCode, &'static str, &'static str) {
        match self {
            Self::Route(RouteParseError::UnsupportedHost) => (
                StatusCode::NOT_FOUND,
                "ROUTE_NOT_FOUND",
                "Sandbox route not found",
            ),
            Self::Route(_) => (
                StatusCode::BAD_REQUEST,
                "INVALID_ROUTE",
                "Sandbox route is invalid",
            ),
            Self::Lease(
                RouteLeaseError::MissingToken
                | RouteLeaseError::InvalidToken
                | RouteLeaseError::Unauthorized,
            ) => (
                StatusCode::UNAUTHORIZED,
                "UNAUTHORIZED",
                "Sandbox access token is invalid",
            ),
            Self::Lease(
                RouteLeaseError::NotFound
                | RouteLeaseError::Inactive
                | RouteLeaseError::Expired
                | RouteLeaseError::PortDenied,
            ) => (
                StatusCode::NOT_FOUND,
                "ROUTE_NOT_FOUND",
                "Sandbox route not found",
            ),
            Self::Lease(RouteLeaseError::InvalidRecord) => (
                StatusCode::BAD_GATEWAY,
                "INVALID_SANDBOX_STATE",
                "Sandbox runtime state is invalid",
            ),
            Self::Lease(RouteLeaseError::Repository(_) | RouteLeaseError::Token(_)) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "ROUTE_UNAVAILABLE",
                "Sandbox route is temporarily unavailable",
            ),
            Self::Connect(ExecutionManagerError::InvalidRequest(_)) => (
                StatusCode::BAD_REQUEST,
                "INVALID_UPSTREAM",
                "Sandbox upstream request is invalid",
            ),
            Self::Connect(ExecutionManagerError::NotFound(_)) => (
                StatusCode::NOT_FOUND,
                "RUNTIME_NOT_FOUND",
                "Sandbox runtime not found",
            ),
            Self::Connect(ExecutionManagerError::Conflict { .. }) => (
                StatusCode::CONFLICT,
                "STALE_RUNTIME",
                "Sandbox runtime generation changed",
            ),
            Self::Connect(ExecutionManagerError::Unavailable(_)) => (
                StatusCode::SERVICE_UNAVAILABLE,
                "UPSTREAM_UNAVAILABLE",
                "Sandbox upstream is unavailable",
            ),
            Self::Connect(ExecutionManagerError::Internal(_))
            | Self::InvalidUpstreamUri
            | Self::Upstream(_) => (
                StatusCode::BAD_GATEWAY,
                "UPSTREAM_FAILURE",
                "Sandbox upstream request failed",
            ),
        }
    }
}
