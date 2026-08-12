use std::fmt;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use a3s_box_core::{
    BoxError, CompiledEgressPolicy, EgressDecisionDestination, EgressDecisionProtocol,
    EgressHttpScheme, EgressPolicy, ExecutionGeneration, ExecutionId,
};
use thiserror::Error;
use tokio::io::AsyncWriteExt;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tokio::task::{JoinHandle, JoinSet};
use uuid::Uuid;

use super::decision_log::{EgressDecisionLog, EgressDecisionLogError, EgressRuntimeDecisionReason};
use super::http::{read_proxy_request, ProxyRequest, ProxyRequestError};
use super::resolver::{
    BoundedEgressDnsResolver, EgressDnsError, EgressDnsResolver, SystemEgressDnsResolver,
};
use super::transport::{
    copy_exact_with_idle_timeout, copy_until_eof_with_idle_timeout, send_proxy_response,
    tunnel_with_idle_timeout,
};

#[derive(Debug, Error)]
pub enum EgressProxyError {
    #[error("invalid egress proxy configuration: {0}")]
    InvalidConfig(String),
    #[error("egress policy compilation failed: {0}")]
    Policy(#[from] BoxError),
    #[error(transparent)]
    DecisionLog(#[from] EgressDecisionLogError),
    #[error("egress proxy I/O failed: {0}")]
    Io(#[from] std::io::Error),
    #[error("egress proxy task failed: {0}")]
    Task(#[from] tokio::task::JoinError),
}

#[derive(Debug, Clone)]
pub struct EgressProxyConfig {
    pub execution_id: ExecutionId,
    pub generation: ExecutionGeneration,
    pub policy: EgressPolicy,
    pub bind_address: SocketAddr,
    pub decision_log_path: PathBuf,
    pub policy_socket_path: Option<PathBuf>,
}

impl EgressProxyConfig {
    pub fn new(
        execution_id: ExecutionId,
        generation: ExecutionGeneration,
        policy: EgressPolicy,
        decision_log_path: impl Into<PathBuf>,
    ) -> Self {
        Self {
            execution_id,
            generation,
            policy,
            bind_address: SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 0),
            decision_log_path: decision_log_path.into(),
            policy_socket_path: None,
        }
    }

    pub const fn with_bind_address(mut self, bind_address: SocketAddr) -> Self {
        self.bind_address = bind_address;
        self
    }

    pub fn with_policy_socket_path(mut self, path: impl Into<PathBuf>) -> Self {
        self.policy_socket_path = Some(path.into());
        self
    }
}

pub struct EgressProxyHandle {
    local_address: SocketAddr,
    authorization_token: String,
    shutdown: watch::Sender<bool>,
    task: Option<JoinHandle<Result<(), EgressProxyError>>>,
    policy_task: Option<JoinHandle<Result<(), EgressProxyError>>>,
    policy_socket_path: Option<PathBuf>,
}

impl fmt::Debug for EgressProxyHandle {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("EgressProxyHandle")
            .field("local_address", &self.local_address)
            .field("authorization_token", &"[redacted]")
            .field(
                "running",
                &self.task.as_ref().is_some_and(|task| !task.is_finished()),
            )
            .finish()
    }
}

impl EgressProxyHandle {
    pub async fn start(config: EgressProxyConfig) -> Result<Self, EgressProxyError> {
        Self::start_with_resolver(config, Arc::new(SystemEgressDnsResolver)).await
    }

    pub async fn start_with_resolver(
        config: EgressProxyConfig,
        resolver: Arc<dyn EgressDnsResolver>,
    ) -> Result<Self, EgressProxyError> {
        if !config.bind_address.ip().is_loopback() {
            return Err(EgressProxyError::InvalidConfig(
                "the host proxy listener must bind to a loopback address".to_string(),
            ));
        }
        let policy = Arc::new(CompiledEgressPolicy::compile(&config.policy)?);
        let limits = policy.limits();
        let listener = TcpListener::bind(config.bind_address).await?;
        let local_address = listener.local_addr()?;

        #[cfg(unix)]
        let policy_listener = match config.policy_socket_path.as_deref() {
            Some(path) => Some(super::policy_channel::bind(path).await?),
            None => None,
        };
        #[cfg(not(unix))]
        if config.policy_socket_path.is_some() {
            return Err(EgressProxyError::InvalidConfig(
                "the raw egress policy channel requires a Unix host".to_string(),
            ));
        }

        let decision_log = match EgressDecisionLog::create(
            config.decision_log_path,
            config.execution_id,
            config.generation,
            limits,
        )
        .await
        {
            Ok(log) => Arc::new(log),
            Err(error) => {
                #[cfg(unix)]
                super::policy_channel::remove_socket(config.policy_socket_path.as_deref());
                return Err(error.into());
            }
        };
        let resolver = Arc::new(BoundedEgressDnsResolver::new(resolver, limits));
        let authorization_token = format!("{}{}", Uuid::new_v4().simple(), Uuid::new_v4().simple());
        let (shutdown, shutdown_rx) = watch::channel(false);
        let task_token = authorization_token.clone();
        #[cfg(unix)]
        let policy_task = policy_listener.map(|listener| {
            tokio::spawn(super::policy_channel::serve(
                listener,
                Arc::clone(&policy),
                Arc::clone(&decision_log),
                limits,
                shutdown_rx.clone(),
            ))
        });
        #[cfg(not(unix))]
        let policy_task = None;
        let task = tokio::spawn(serve(
            listener,
            policy,
            resolver,
            decision_log,
            task_token,
            shutdown_rx,
        ));

        Ok(Self {
            local_address,
            authorization_token,
            shutdown,
            task: Some(task),
            policy_task,
            policy_socket_path: config.policy_socket_path,
        })
    }

    pub const fn local_address(&self) -> SocketAddr {
        self.local_address
    }

    pub fn proxy_url(&self) -> String {
        format!("http://{}", self.local_address)
    }

    pub fn authorization_header_value(&self) -> String {
        format!("Bearer {}", self.authorization_token)
    }

    /// Return a standard HTTP proxy URL whose Basic credentials are scoped to
    /// this execution generation. The supplied address is the guest-visible
    /// netproxy gateway route, not the host listener address.
    pub fn authenticated_proxy_url(&self, guest_address: SocketAddr) -> String {
        format!("http://a3s:{}@{}", self.authorization_token, guest_address)
    }

    pub fn is_running(&self) -> bool {
        self.task.as_ref().is_some_and(|task| !task.is_finished())
            && self
                .policy_task
                .as_ref()
                .is_none_or(|task| !task.is_finished())
    }

    pub async fn stop(mut self) -> Result<(), EgressProxyError> {
        let _ = self.shutdown.send(true);
        let mut first_error = None;
        if let Some(task) = self.policy_task.take() {
            if let Err(error) = flatten_task_result(task.await) {
                first_error = Some(error);
            }
        }
        if let Some(task) = self.task.take() {
            if let Err(error) = flatten_task_result(task.await) {
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        #[cfg(unix)]
        super::policy_channel::remove_socket(self.policy_socket_path.as_deref());
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }
}

fn flatten_task_result(
    result: Result<Result<(), EgressProxyError>, tokio::task::JoinError>,
) -> Result<(), EgressProxyError> {
    match result {
        Ok(result) => result,
        Err(error) => Err(EgressProxyError::Task(error)),
    }
}

impl Drop for EgressProxyHandle {
    fn drop(&mut self) {
        let _ = self.shutdown.send(true);
        #[cfg(unix)]
        super::policy_channel::remove_socket(self.policy_socket_path.as_deref());
    }
}

async fn serve(
    listener: TcpListener,
    policy: Arc<CompiledEgressPolicy>,
    resolver: Arc<BoundedEgressDnsResolver>,
    decision_log: Arc<EgressDecisionLog>,
    authorization_token: String,
    mut shutdown: watch::Receiver<bool>,
) -> Result<(), EgressProxyError> {
    let limits = policy.limits();
    let connections = Arc::new(Semaphore::new(limits.max_concurrent_connections as usize));
    let pending = Arc::new(Semaphore::new(limits.max_pending_connections as usize));
    let mut tasks = JoinSet::new();
    let result = loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    break Ok(());
                }
            }
            accepted = listener.accept() => {
                let (mut client, _) = match accepted {
                    Ok(connection) => connection,
                    Err(error) => break Err(EgressProxyError::Io(error)),
                };
                let connection_permit = match Arc::clone(&connections).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        record_runtime_denied(
                            &decision_log,
                            EgressRuntimeDecisionReason::ConnectionLimitExceeded,
                            None,
                            None,
                            None,
                        ).await;
                        let _ = send_proxy_response(&mut client, 503, "Egress capacity exhausted").await;
                        continue;
                    }
                };
                let pending_permit = match Arc::clone(&pending).try_acquire_owned() {
                    Ok(permit) => permit,
                    Err(_) => {
                        drop(connection_permit);
                        record_runtime_denied(
                            &decision_log,
                            EgressRuntimeDecisionReason::PendingConnectionLimitExceeded,
                            None,
                            None,
                            None,
                        ).await;
                        let _ = send_proxy_response(&mut client, 503, "Egress setup capacity exhausted").await;
                        continue;
                    }
                };

                tasks.spawn(handle_connection(
                    client,
                    Arc::clone(&policy),
                    Arc::clone(&resolver),
                    Arc::clone(&decision_log),
                    authorization_token.clone(),
                    connection_permit,
                    pending_permit,
                ));
            }
            Some(completed) = tasks.join_next(), if !tasks.is_empty() => {
                if let Err(error) = completed {
                    tracing::debug!(%error, "egress proxy connection task terminated");
                }
            }
        }
    };

    tasks.abort_all();
    while tasks.join_next().await.is_some() {}
    decision_log.finish().await?;
    result
}

async fn handle_connection(
    mut client: TcpStream,
    policy: Arc<CompiledEgressPolicy>,
    resolver: Arc<BoundedEgressDnsResolver>,
    decision_log: Arc<EgressDecisionLog>,
    authorization_token: String,
    _connection_permit: OwnedSemaphorePermit,
    pending_permit: OwnedSemaphorePermit,
) {
    let limits = policy.limits();
    let request_timeout = Duration::from_millis(u64::from(limits.connect_timeout_ms));
    let request = match read_proxy_request(&mut client, &authorization_token, request_timeout).await
    {
        Ok(request) => request,
        Err(error) => {
            let reason = match error {
                ProxyRequestError::Authentication => {
                    EgressRuntimeDecisionReason::AuthenticationFailed
                }
                ProxyRequestError::Timeout => EgressRuntimeDecisionReason::RequestHeaderTimeout,
                ProxyRequestError::HeaderTooLarge => {
                    EgressRuntimeDecisionReason::RequestHeaderTooLarge
                }
                ProxyRequestError::Malformed | ProxyRequestError::Io(_) => {
                    EgressRuntimeDecisionReason::MalformedRequest
                }
            };
            record_runtime_denied(&decision_log, reason, None, None, None).await;
            let status = if matches!(error, ProxyRequestError::Authentication) {
                407
            } else {
                400
            };
            let _ = send_proxy_response(&mut client, status, "Egress request denied").await;
            return;
        }
    };

    let scheme = match request {
        ProxyRequest::Connect { .. } => EgressHttpScheme::Https,
        ProxyRequest::Http { .. } => EgressHttpScheme::Http,
    };
    let initial = policy.evaluate_http(scheme, request.host(), request.port());
    if !initial.is_allowed() {
        let _ = decision_log.append_policy(initial).await;
        let _ = send_proxy_response(&mut client, 403, "Destination denied").await;
        return;
    }

    let addresses = match &initial.destination {
        EgressDecisionDestination::Ip { address } => vec![*address],
        EgressDecisionDestination::Hostname { hostname } => {
            match resolver.resolve(hostname, request.port()).await {
                Ok(addresses) => addresses,
                Err(error) => {
                    record_runtime_denied(
                        &decision_log,
                        dns_reason(&error),
                        Some(EgressDecisionProtocol::from(scheme)),
                        Some(initial.destination.clone()),
                        Some(request.port()),
                    )
                    .await;
                    let _ = send_proxy_response(&mut client, 502, "Destination unavailable").await;
                    return;
                }
            }
        }
        EgressDecisionDestination::ResolvedHostname { address, .. } => vec![*address],
        EgressDecisionDestination::Invalid => {
            let _ = send_proxy_response(&mut client, 403, "Destination denied").await;
            return;
        }
    };

    let connect_timeout = Duration::from_millis(u64::from(limits.connect_timeout_ms));
    let deadline = tokio::time::Instant::now() + connect_timeout;
    let mut upstream = None;
    let mut last_denial = None;
    let mut timed_out = false;
    for address in addresses {
        let evaluation =
            policy.evaluate_resolved_http(scheme, request.host(), request.port(), address);
        if !evaluation.is_allowed() {
            last_denial = Some(evaluation);
            continue;
        }
        if decision_log.append_policy(evaluation).await.is_err() {
            let _ = send_proxy_response(&mut client, 503, "Decision log exhausted").await;
            return;
        }
        let now = tokio::time::Instant::now();
        if now >= deadline {
            timed_out = true;
            break;
        }
        match tokio::time::timeout_at(
            deadline,
            TcpStream::connect(SocketAddr::new(address, request.port())),
        )
        .await
        {
            Ok(Ok(stream)) => {
                upstream = Some(stream);
                break;
            }
            Ok(Err(_)) => {}
            Err(_) => {
                timed_out = true;
                break;
            }
        }
    }

    let Some(mut upstream) = upstream else {
        if let Some(denial) = last_denial {
            let _ = decision_log.append_policy(denial).await;
            let _ = send_proxy_response(&mut client, 403, "Resolved destination denied").await;
        } else {
            record_runtime_denied(
                &decision_log,
                if timed_out {
                    EgressRuntimeDecisionReason::ConnectTimeout
                } else {
                    EgressRuntimeDecisionReason::ConnectFailed
                },
                Some(EgressDecisionProtocol::from(scheme)),
                Some(initial.destination),
                Some(request.port()),
            )
            .await;
            let _ = send_proxy_response(&mut client, 502, "Destination unavailable").await;
        }
        return;
    };
    drop(pending_permit);

    let idle_timeout = Duration::from_millis(u64::from(limits.idle_timeout_ms));
    match request {
        ProxyRequest::Connect {
            buffered_tunnel_bytes,
            ..
        } => {
            if send_proxy_response(&mut client, 200, "Connection established")
                .await
                .is_err()
            {
                return;
            }
            if !buffered_tunnel_bytes.is_empty()
                && upstream.write_all(&buffered_tunnel_bytes).await.is_err()
            {
                return;
            }
            let _ = tunnel_with_idle_timeout(client, upstream, idle_timeout).await;
        }
        ProxyRequest::Http {
            upstream_header,
            buffered_body,
            remaining_body_bytes,
            ..
        } => {
            if upstream.write_all(&upstream_header).await.is_err()
                || upstream.write_all(&buffered_body).await.is_err()
                || copy_exact_with_idle_timeout(
                    &mut client,
                    &mut upstream,
                    remaining_body_bytes,
                    idle_timeout,
                )
                .await
                .is_err()
            {
                return;
            }
            let _ = upstream.shutdown().await;
            let _ =
                copy_until_eof_with_idle_timeout(&mut upstream, &mut client, idle_timeout).await;
        }
    }
}

fn dns_reason(error: &EgressDnsError) -> EgressRuntimeDecisionReason {
    match error {
        EgressDnsError::QueryBudgetExhausted => {
            EgressRuntimeDecisionReason::DnsQueryBudgetExhausted
        }
        EgressDnsError::CacheBudgetExhausted => {
            EgressRuntimeDecisionReason::DnsCacheBudgetExhausted
        }
        EgressDnsError::AnswerBudgetExceeded => {
            EgressRuntimeDecisionReason::DnsAnswerBudgetExceeded
        }
        EgressDnsError::Timeout => EgressRuntimeDecisionReason::DnsTimeout,
        EgressDnsError::NoAddresses | EgressDnsError::Resolve(_) => {
            EgressRuntimeDecisionReason::DnsResolutionFailed
        }
    }
}

async fn record_runtime_denied(
    decision_log: &EgressDecisionLog,
    reason: EgressRuntimeDecisionReason,
    protocol: Option<EgressDecisionProtocol>,
    destination: Option<EgressDecisionDestination>,
    port: Option<u16>,
) {
    let _ = decision_log
        .append_runtime_denied(reason, protocol, destination, port)
        .await;
}

#[cfg(all(test, unix))]
mod lifecycle_tests {
    use a3s_box_core::{EgressPolicy, ExecutionGeneration, ExecutionId};

    use super::*;

    #[tokio::test]
    async fn policy_task_failure_is_visible_and_stop_still_cleans_every_listener() {
        let directory = tempfile::tempdir().unwrap();
        let socket_path = directory.path().join("policy.sock");
        let proxy = EgressProxyHandle::start(
            EgressProxyConfig::new(
                ExecutionId::new("failed-policy-task").unwrap(),
                ExecutionGeneration::INITIAL,
                EgressPolicy::DenyAll,
                directory.path().join("decisions.jsonl"),
            )
            .with_policy_socket_path(&socket_path),
        )
        .await
        .unwrap();
        let http_address = proxy.local_address();

        proxy.policy_task.as_ref().unwrap().abort();
        tokio::time::timeout(Duration::from_secs(2), async {
            while !proxy.policy_task.as_ref().unwrap().is_finished() {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();

        assert!(!proxy.is_running());
        assert!(matches!(proxy.stop().await, Err(EgressProxyError::Task(_))));
        assert!(!socket_path.exists());
        assert!(TcpStream::connect(http_address).await.is_err());
    }
}
