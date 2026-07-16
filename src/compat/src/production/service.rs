use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use a3s_box_core::{ExecutionManager, ExecutionPortConnector, ExecutionSessionManager};
use a3s_box_runtime::LocalExecutionManager;
use axum::Router;
use thiserror::Error;
use tokio::sync::watch;
use tokio::time::{self, MissedTickBehavior};
use tracing::{error, info, warn};
use url::Url;

use crate::control::{
    ControlService, ControlServiceDependencies, LifecycleMaintenanceReport, LifecycleSupervisor,
    LifecycleSupervisorDependencies, LifecycleSupervisorError, RepositoryError,
    RotatingTokenProvider, SandboxRepository, SqliteSandboxRepository, SystemClock,
    TokenIssuerError,
};
use crate::gateway::{DataPlaneGateway, DataPlaneGatewayError};
use crate::http::{
    lifecycle_router, CredentialHashError, HashedCredentialVerifier, LifecycleHttpConfig,
    LifecycleHttpState, RejectingCursorDecoder,
};
use crate::routing::{RouteLeaseService, SandboxRouteParser};

use super::{E2bCompatConfig, SupervisorConfig, UuidSandboxIdentityProvider};

/// Production service with one canonical lifecycle store and runtime manager.
pub struct E2bCompatService {
    listen: SocketAddr,
    gateway_listen: SocketAddr,
    public_url: Url,
    sandbox_domain: String,
    sandbox_public_domain: String,
    router: Router,
    gateway: DataPlaneGateway,
    supervisor: LifecycleSupervisor,
    supervisor_config: SupervisorConfig,
    route_parser: SandboxRouteParser,
    route_leases: RouteLeaseService,
}

impl E2bCompatService {
    pub async fn build(config: E2bCompatConfig) -> E2bServiceResult<Self> {
        prepare_directory(&config.runtime_home).await?;
        prepare_parent(&config.database_path).await?;
        prepare_parent(&config.runtime_state_path).await?;

        let repository = Arc::new(SqliteSandboxRepository::open(&config.database_path).await?);
        let local_executions = Arc::new(LocalExecutionManager::with_vm_backend(
            &config.runtime_state_path,
            &config.runtime_home,
        ));
        let executions: Arc<dyn ExecutionManager> = local_executions.clone();
        let sessions: Arc<dyn ExecutionSessionManager> = local_executions.clone();
        let port_connector: Arc<dyn ExecutionPortConnector> = local_executions;
        let clock = Arc::new(SystemClock);
        let tokens = Arc::new(RotatingTokenProvider::new(
            config.active_token_version,
            config.token_keys,
        )?);
        let verifier = Arc::new(HashedCredentialVerifier::new(config.credentials)?);
        let templates = Arc::new(config.templates);

        let control = Arc::new(ControlService::new(ControlServiceDependencies {
            repository: repository.clone(),
            executions: executions.clone(),
            ports: port_connector.clone(),
            clock: clock.clone(),
            identities: Arc::new(UuidSandboxIdentityProvider),
            templates,
            token_issuer: tokens.clone(),
            token_resolver: tokens.clone(),
        }));
        let supervisor = LifecycleSupervisor::new(LifecycleSupervisorDependencies {
            repository: repository.clone(),
            executions: executions.clone(),
            clock: clock.clone(),
        });
        let route_parser = SandboxRouteParser::new(config.sandbox_domain.clone());
        let route_leases =
            RouteLeaseService::new(repository as Arc<dyn SandboxRepository>, tokens, clock);
        let sandbox_domain = config.sandbox_domain.as_str().to_string();
        let sandbox_public_domain = config.sandbox_public_domain.clone();
        let router = lifecycle_router(LifecycleHttpState::new(
            control,
            verifier,
            Arc::new(RejectingCursorDecoder),
            LifecycleHttpConfig {
                domain: Some(sandbox_public_domain.clone()),
                max_json_bytes: config.max_json_bytes,
            },
        ));
        let gateway = DataPlaneGateway::build(
            config.gateway.clone(),
            route_parser.clone(),
            route_leases.clone(),
            executions,
            sessions,
            port_connector,
        )
        .await?;

        Ok(Self {
            listen: config.api_listen,
            gateway_listen: gateway.listen(),
            public_url: config.api_public_url,
            sandbox_domain,
            sandbox_public_domain,
            router,
            gateway,
            supervisor,
            supervisor_config: config.supervisor,
            route_parser,
            route_leases,
        })
    }

    pub fn listen(&self) -> SocketAddr {
        self.listen
    }

    pub fn public_url(&self) -> &Url {
        &self.public_url
    }

    pub fn gateway_listen(&self) -> SocketAddr {
        self.gateway_listen
    }

    pub fn sandbox_domain(&self) -> &str {
        &self.sandbox_domain
    }

    pub fn sandbox_public_domain(&self) -> &str {
        &self.sandbox_public_domain
    }

    pub fn router(&self) -> Router {
        self.router.clone()
    }

    pub fn route_parser(&self) -> &SandboxRouteParser {
        &self.route_parser
    }

    pub fn route_leases(&self) -> &RouteLeaseService {
        &self.route_leases
    }

    pub async fn reconcile_startup(&self) -> E2bServiceResult<LifecycleMaintenanceReport> {
        Ok(self
            .supervisor
            .reconcile_startup(self.supervisor_config.reconciliation_page_size())
            .await?)
    }

    pub async fn serve(self) -> E2bServiceResult<()> {
        let listener = tokio::net::TcpListener::bind(self.listen)
            .await
            .map_err(|source| E2bServiceError::Bind {
                address: self.listen,
                source,
            })?;
        let local_address = listener
            .local_addr()
            .map_err(|source| E2bServiceError::Bind {
                address: self.listen,
                source,
            })?;
        let gateway_listener = tokio::net::TcpListener::bind(self.gateway_listen)
            .await
            .map_err(|source| E2bServiceError::Bind {
                address: self.gateway_listen,
                source,
            })?;
        let listener = listener
            .into_std()
            .map_err(|source| E2bServiceError::Bind {
                address: self.listen,
                source,
            })?;
        let (shutdown_sender, shutdown_receiver) = watch::channel(false);
        let supervisor = self.supervisor.clone();
        let supervisor_config = self.supervisor_config;
        let mut maintenance = tokio::spawn(run_maintenance(
            supervisor,
            supervisor_config,
            shutdown_receiver.clone(),
        ));
        let mut gateway = Box::pin(
            self.gateway
                .serve(gateway_listener, shutdown_receiver.clone()),
        );
        let mut server = Box::pin(
            axum::Server::from_tcp(listener)
                .map_err(E2bServiceError::Listener)?
                .serve(self.router.into_make_service())
                .with_graceful_shutdown(wait_for_shutdown(shutdown_receiver)),
        );

        info!(
            listen = %local_address,
            public_url = %self.public_url,
            sandbox_domain = %self.sandbox_domain,
            sandbox_public_domain = %self.sandbox_public_domain,
            gateway_listen = %self.gateway_listen,
            "E2B compatibility service started"
        );

        let termination = tokio::select! {
            signal = shutdown_signal() => {
                Termination::Signal(signal)
            }
            server_result = &mut server => {
                Termination::Control(server_result)
            }
            gateway_result = &mut gateway => {
                Termination::Gateway(gateway_result)
            }
            maintenance_result = &mut maintenance => {
                Termination::Maintenance(maintenance_result)
            }
        };
        request_shutdown(&shutdown_sender);
        match termination {
            Termination::Signal(signal) => {
                if signal.is_ok() {
                    info!("shutdown signal received");
                }
                let control = server.await;
                let gateway_result = gateway.await;
                let maintenance_result = maintenance.await;
                signal?;
                control.map_err(E2bServiceError::Server)?;
                gateway_result?;
                join_maintenance(maintenance_result)?;
            }
            Termination::Control(control) => {
                let gateway_result = gateway.await;
                let maintenance_result = maintenance.await;
                control.map_err(E2bServiceError::Server)?;
                gateway_result?;
                join_maintenance(maintenance_result)?;
            }
            Termination::Gateway(gateway_result) => {
                let control = server.await;
                let maintenance_result = maintenance.await;
                gateway_result?;
                control.map_err(E2bServiceError::Server)?;
                join_maintenance(maintenance_result)?;
            }
            Termination::Maintenance(maintenance_result) => {
                let control = server.await;
                let gateway_result = gateway.await;
                join_maintenance(maintenance_result)?;
                control.map_err(E2bServiceError::Server)?;
                gateway_result?;
            }
        }
        info!("E2B compatibility service stopped");
        Ok(())
    }
}

enum Termination {
    Signal(E2bServiceResult<()>),
    Control(Result<(), hyper::Error>),
    Gateway(Result<(), DataPlaneGatewayError>),
    Maintenance(Result<E2bServiceResult<()>, tokio::task::JoinError>),
}

async fn run_maintenance(
    supervisor: LifecycleSupervisor,
    config: SupervisorConfig,
    mut shutdown: watch::Receiver<bool>,
) -> E2bServiceResult<()> {
    let report = supervisor
        .reconcile_startup(config.reconciliation_page_size())
        .await?;
    log_report("startup reconciliation", &report);

    let start = time::Instant::now() + config.interval();
    let mut interval = time::interval_at(start, config.interval());
    interval.set_missed_tick_behavior(MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() {
                    return Ok(());
                }
            }
            _ = interval.tick() => {
                let report = supervisor.reap_expired(config.batch_size()).await?;
                log_report("expiry maintenance", &report);
            }
        }
    }
}

fn log_report(operation: &str, report: &LifecycleMaintenanceReport) {
    if report.failures.is_empty() {
        info!(
            operation,
            examined = report.examined,
            completed = report.completed,
            deferred = report.deferred,
            "lifecycle maintenance completed"
        );
        return;
    }
    warn!(
        operation,
        examined = report.examined,
        completed = report.completed,
        deferred = report.deferred,
        failures = report.failures.len(),
        "lifecycle maintenance completed with isolated record failures"
    );
    for failure in &report.failures {
        error!(
            operation,
            sandbox_id = %failure.sandbox_id,
            message = %failure.message,
            "lifecycle record maintenance failed"
        );
    }
}

async fn prepare_parent(path: &Path) -> E2bServiceResult<()> {
    let parent = path.parent().ok_or_else(|| E2bServiceError::InvalidPath {
        path: path.to_path_buf(),
    })?;
    prepare_directory(parent).await
}

async fn prepare_directory(path: &Path) -> E2bServiceResult<()> {
    tokio::fs::create_dir_all(path)
        .await
        .map_err(|source| E2bServiceError::CreateDirectory {
            path: path.to_path_buf(),
            source,
        })
}

async fn wait_for_shutdown(mut shutdown: watch::Receiver<bool>) {
    while !*shutdown.borrow() {
        if shutdown.changed().await.is_err() {
            return;
        }
    }
}

fn request_shutdown(sender: &watch::Sender<bool>) {
    let _ = sender.send(true);
}

fn join_maintenance(
    result: Result<E2bServiceResult<()>, tokio::task::JoinError>,
) -> E2bServiceResult<()> {
    result.map_err(E2bServiceError::MaintenanceTask)?
}

#[cfg(unix)]
async fn shutdown_signal() -> E2bServiceResult<()> {
    use tokio::signal::unix::{signal, SignalKind};

    let mut terminate = signal(SignalKind::terminate()).map_err(E2bServiceError::Signal)?;
    tokio::select! {
        result = tokio::signal::ctrl_c() => result.map_err(E2bServiceError::Signal)?,
        _ = terminate.recv() => {},
    }
    Ok(())
}

#[cfg(not(unix))]
async fn shutdown_signal() -> E2bServiceResult<()> {
    tokio::signal::ctrl_c()
        .await
        .map_err(E2bServiceError::Signal)
}

#[derive(Debug, Error)]
pub enum E2bServiceError {
    #[error("invalid service state path: {path}")]
    InvalidPath { path: PathBuf },
    #[error("failed to create service directory {path}: {source}")]
    CreateDirectory {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to bind E2B compatibility listener {address}: {source}")]
    Bind {
        address: SocketAddr,
        #[source]
        source: std::io::Error,
    },
    #[error("failed to create E2B compatibility listener: {0}")]
    Listener(#[source] hyper::Error),
    #[error("E2B compatibility HTTP server failed: {0}")]
    Server(#[source] hyper::Error),
    #[error("failed to install or receive the shutdown signal: {0}")]
    Signal(#[source] std::io::Error),
    #[error("lifecycle maintenance task failed: {0}")]
    MaintenanceTask(#[source] tokio::task::JoinError),
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    Credential(#[from] CredentialHashError),
    #[error(transparent)]
    Token(#[from] TokenIssuerError),
    #[error(transparent)]
    Supervisor(#[from] LifecycleSupervisorError),
    #[error(transparent)]
    Gateway(#[from] DataPlaneGatewayError),
}

pub type E2bServiceResult<T> = std::result::Result<T, E2bServiceError>;
