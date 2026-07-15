use std::num::NonZeroU16;
use std::sync::Arc;

use a3s_box_core::{ExecutionGeneration, ExecutionId};
use axum::http::{HeaderMap, HeaderName};
use chrono::{DateTime, Utc};
use thiserror::Error;

use crate::control::{
    Clock, LifecycleState, RepositoryError, SandboxGeneration, SandboxId, SandboxRecord,
    SandboxRepository, SecretToken, TokenIssuerError, TokenScope, TokenVerifier,
};

use super::ParsedSandboxRoute;

pub const ENVD_ACCESS_TOKEN_HEADER: &str = "x-access-token";
pub const TRAFFIC_ACCESS_TOKEN_HEADER: &str = "e2b-traffic-access-token";
const MAX_TOKEN_BYTES: usize = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RouteLease {
    sandbox_id: SandboxId,
    execution_id: ExecutionId,
    sandbox_generation: SandboxGeneration,
    execution_generation: ExecutionGeneration,
    port: NonZeroU16,
    token_scope: TokenScope,
    expires_at: DateTime<Utc>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum EnvdHealthResolution {
    Running(RouteLease),
    Inactive,
}

impl RouteLease {
    pub fn sandbox_id(&self) -> &SandboxId {
        &self.sandbox_id
    }

    pub fn execution_id(&self) -> &ExecutionId {
        &self.execution_id
    }

    pub const fn sandbox_generation(&self) -> SandboxGeneration {
        self.sandbox_generation
    }

    pub const fn execution_generation(&self) -> ExecutionGeneration {
        self.execution_generation
    }

    pub const fn port(&self) -> NonZeroU16 {
        self.port
    }

    pub const fn token_scope(&self) -> TokenScope {
        self.token_scope
    }

    pub const fn expires_at(&self) -> DateTime<Utc> {
        self.expires_at
    }

    pub fn is_current(&self, record: &SandboxRecord, now: DateTime<Utc>) -> bool {
        record.state() == LifecycleState::Running
            && record.sandbox_id() == &self.sandbox_id
            && record.generation() == self.sandbox_generation
            && record.execution_id() == Some(&self.execution_id)
            && record.execution_generation() == Some(self.execution_generation)
            && record.expires_at() > now
            && record.routing().token_scope(self.port.get()) == Some(self.token_scope)
    }
}

#[derive(Clone)]
pub struct RouteLeaseService {
    repository: Arc<dyn SandboxRepository>,
    tokens: Arc<dyn TokenVerifier>,
    clock: Arc<dyn Clock>,
}

impl RouteLeaseService {
    pub fn new(
        repository: Arc<dyn SandboxRepository>,
        tokens: Arc<dyn TokenVerifier>,
        clock: Arc<dyn Clock>,
    ) -> Self {
        Self {
            repository,
            tokens,
            clock,
        }
    }

    pub async fn resolve(
        &self,
        route: &ParsedSandboxRoute,
        headers: &HeaderMap,
    ) -> RouteLeaseResult<RouteLease> {
        let record = self
            .repository
            .get(&route.sandbox_id)
            .await?
            .ok_or(RouteLeaseError::NotFound)?;
        if record.state() != LifecycleState::Running {
            return Err(RouteLeaseError::Inactive);
        }
        let now = self.clock.now();
        if record.expires_at() <= now {
            return Err(RouteLeaseError::Expired);
        }
        let token_scope = self.verify_route_token(&record, route, headers).await?;
        let execution_id = record
            .execution_id()
            .cloned()
            .ok_or(RouteLeaseError::InvalidRecord)?;
        let execution_generation = record
            .execution_generation()
            .ok_or(RouteLeaseError::InvalidRecord)?;
        Ok(RouteLease {
            sandbox_id: record.sandbox_id().clone(),
            execution_id,
            sandbox_generation: record.generation(),
            execution_generation,
            port: route.port,
            token_scope,
            expires_at: record.expires_at(),
        })
    }

    /// Resolve an authenticated envd health request without issuing a live
    /// route lease for an inactive or expired sandbox.
    ///
    /// This preserves the official client's `502 -> false` behavior after a
    /// successful kill while ensuring an invalid token cannot probe terminal
    /// sandbox state. All other data-plane requests continue to require a live
    /// [`RouteLease`].
    pub async fn resolve_envd_health(
        &self,
        route: &ParsedSandboxRoute,
        headers: &HeaderMap,
    ) -> RouteLeaseResult<EnvdHealthResolution> {
        let record = self
            .repository
            .get(&route.sandbox_id)
            .await?
            .ok_or(RouteLeaseError::NotFound)?;
        let token_scope = self.verify_route_token(&record, route, headers).await?;
        if token_scope != TokenScope::Envd {
            return Err(RouteLeaseError::PortDenied);
        }
        let now = self.clock.now();
        if record.state() != LifecycleState::Running || record.expires_at() <= now {
            return Ok(EnvdHealthResolution::Inactive);
        }
        let execution_id = record
            .execution_id()
            .cloned()
            .ok_or(RouteLeaseError::InvalidRecord)?;
        let execution_generation = record
            .execution_generation()
            .ok_or(RouteLeaseError::InvalidRecord)?;
        Ok(EnvdHealthResolution::Running(RouteLease {
            sandbox_id: record.sandbox_id().clone(),
            execution_id,
            sandbox_generation: record.generation(),
            execution_generation,
            port: route.port,
            token_scope,
            expires_at: record.expires_at(),
        }))
    }

    async fn verify_route_token(
        &self,
        record: &SandboxRecord,
        route: &ParsedSandboxRoute,
        headers: &HeaderMap,
    ) -> RouteLeaseResult<TokenScope> {
        let token_scope = record
            .routing()
            .token_scope(route.port.get())
            .ok_or(RouteLeaseError::PortDenied)?;
        let presented = presented_token(headers, token_scope)?;
        let stored = match token_scope {
            TokenScope::Envd => &record.credentials().envd,
            TokenScope::Traffic => &record.credentials().traffic,
        };
        if !self.tokens.verify(token_scope, &presented, stored).await? {
            return Err(RouteLeaseError::Unauthorized);
        }
        Ok(token_scope)
    }
}

fn presented_token(headers: &HeaderMap, scope: TokenScope) -> RouteLeaseResult<SecretToken> {
    let name = HeaderName::from_static(match scope {
        TokenScope::Envd => ENVD_ACCESS_TOKEN_HEADER,
        TokenScope::Traffic => TRAFFIC_ACCESS_TOKEN_HEADER,
    });
    let mut values = headers.get_all(name).iter();
    let value = values.next().ok_or(RouteLeaseError::MissingToken)?;
    if values.next().is_some() {
        return Err(RouteLeaseError::InvalidToken);
    }
    let value = value.to_str().map_err(|_| RouteLeaseError::InvalidToken)?;
    if value.is_empty() || value.len() > MAX_TOKEN_BYTES {
        return Err(RouteLeaseError::InvalidToken);
    }
    SecretToken::new(value).map_err(|_| RouteLeaseError::InvalidToken)
}

#[derive(Debug, Error)]
pub enum RouteLeaseError {
    #[error("sandbox route was not found")]
    NotFound,
    #[error("sandbox route is not active")]
    Inactive,
    #[error("sandbox route has expired")]
    Expired,
    #[error("sandbox port is not routed")]
    PortDenied,
    #[error("sandbox route token is missing")]
    MissingToken,
    #[error("sandbox route token is invalid")]
    InvalidToken,
    #[error("sandbox route is unauthorized")]
    Unauthorized,
    #[error("sandbox route record is invalid")]
    InvalidRecord,
    #[error(transparent)]
    Repository(#[from] RepositoryError),
    #[error(transparent)]
    Token(#[from] TokenIssuerError),
}

pub type RouteLeaseResult<T> = std::result::Result<T, RouteLeaseError>;

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    use a3s_box_core::{
        resolve_execution, BoxConfig, ExecutionIsolation, ExecutionLease, OperationId,
    };
    use axum::http::HeaderValue;
    use chrono::{Duration, TimeZone};

    use crate::control::{
        LifecyclePolicy, MemorySandboxRepository, NewSandboxRecord, OnTimeoutAction,
        RotatingTokenProvider, SandboxCredentials, SandboxRecord, SqliteSandboxRepository,
        TokenIssuer, TokenKeyMaterial,
    };
    use crate::routing::{SandboxRoutePolicy, CODE_INTERPRETER_PORT, ENVD_PORT};

    use super::*;

    struct FixedClock(DateTime<Utc>);

    impl Clock for FixedClock {
        fn now(&self) -> DateTime<Utc> {
            self.0
        }
    }

    struct Harness {
        repository: Arc<MemorySandboxRepository>,
        service: RouteLeaseService,
        envd_secret: SecretToken,
        traffic_secret: SecretToken,
        now: DateTime<Utc>,
        sandbox_id: SandboxId,
    }

    impl Harness {
        async fn new() -> Self {
            let now = Utc
                .with_ymd_and_hms(2026, 7, 15, 10, 0, 0)
                .single()
                .unwrap();
            let tokens = Arc::new(
                RotatingTokenProvider::new(
                    1,
                    [TokenKeyMaterial::new(1, &[7; 32], &[8; 32]).unwrap()],
                )
                .unwrap(),
            );
            let envd = tokens.issue(TokenScope::Envd).await.unwrap();
            let traffic = tokens.issue(TokenScope::Traffic).await.unwrap();
            let sandbox_id = SandboxId::new("sandbox-route-1").unwrap();
            let config = BoxConfig {
                isolation: ExecutionIsolation::Sandbox,
                ..BoxConfig::default()
            };
            let plan = resolve_execution(&config).unwrap();
            let routing = SandboxRoutePolicy::default()
                .with_port(CODE_INTERPRETER_PORT, TokenScope::Traffic)
                .unwrap();
            let mut record = SandboxRecord::creating(NewSandboxRecord {
                sandbox_id: sandbox_id.clone(),
                operation_id: OperationId::new("operation-route-1").unwrap(),
                owner_id: "owner-route".to_string(),
                template_id: "code-interpreter-v1".to_string(),
                plan: plan.clone(),
                resources: config.resources.clone(),
                lifecycle: LifecyclePolicy {
                    on_timeout: OnTimeoutAction::Kill,
                    auto_resume: false,
                    keep_memory_on_pause: false,
                },
                created_at: now,
                expires_at: now + Duration::minutes(5),
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
                    execution_id: ExecutionId::new("execution-route-1").unwrap(),
                    generation: ExecutionGeneration::INITIAL,
                    plan,
                    resources: config.resources,
                    started_at: now,
                })
                .unwrap();
            let repository = Arc::new(MemorySandboxRepository::default());
            repository.insert(record).await.unwrap();
            let service =
                RouteLeaseService::new(repository.clone(), tokens, Arc::new(FixedClock(now)));
            Self {
                repository,
                service,
                envd_secret: envd.secret,
                traffic_secret: traffic.secret,
                now,
                sandbox_id,
            }
        }

        fn route(&self, port: u16) -> ParsedSandboxRoute {
            ParsedSandboxRoute {
                sandbox_id: self.sandbox_id.clone(),
                port: NonZeroU16::new(port).unwrap(),
                form: super::super::RouteForm::Direct,
            }
        }
    }

    fn token_headers(name: &'static str, token: &SecretToken) -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(name, HeaderValue::from_str(token.expose_secret()).unwrap());
        headers
    }

    #[tokio::test]
    async fn resolves_scope_bound_generation_fenced_leases() {
        let harness = Harness::new().await;
        let envd = harness
            .service
            .resolve(
                &harness.route(ENVD_PORT),
                &token_headers(ENVD_ACCESS_TOKEN_HEADER, &harness.envd_secret),
            )
            .await
            .unwrap();
        assert_eq!(envd.token_scope(), TokenScope::Envd);
        assert_eq!(envd.execution_id().as_str(), "execution-route-1");
        assert!(matches!(
            harness
                .service
                .resolve_envd_health(
                    &harness.route(ENVD_PORT),
                    &token_headers(ENVD_ACCESS_TOKEN_HEADER, &harness.envd_secret),
                )
                .await
                .unwrap(),
            EnvdHealthResolution::Running(lease) if lease == envd
        ));

        let traffic = harness
            .service
            .resolve(
                &harness.route(CODE_INTERPRETER_PORT),
                &token_headers(TRAFFIC_ACCESS_TOKEN_HEADER, &harness.traffic_secret),
            )
            .await
            .unwrap();
        assert_eq!(traffic.token_scope(), TokenScope::Traffic);
        assert!(traffic.is_current(
            &harness
                .repository
                .get(&harness.sandbox_id)
                .await
                .unwrap()
                .unwrap(),
            harness.now
        ));
    }

    #[tokio::test]
    async fn rejects_swapped_tokens_unrouted_ports_and_stale_generations() {
        let harness = Harness::new().await;
        assert!(matches!(
            harness
                .service
                .resolve(
                    &harness.route(ENVD_PORT),
                    &token_headers(ENVD_ACCESS_TOKEN_HEADER, &harness.traffic_secret),
                )
                .await,
            Err(RouteLeaseError::Unauthorized)
        ));
        assert!(matches!(
            harness
                .service
                .resolve(
                    &harness.route(8080),
                    &token_headers(TRAFFIC_ACCESS_TOKEN_HEADER, &harness.traffic_secret),
                )
                .await,
            Err(RouteLeaseError::PortDenied)
        ));

        let old = harness
            .service
            .resolve(
                &harness.route(ENVD_PORT),
                &token_headers(ENVD_ACCESS_TOKEN_HEADER, &harness.envd_secret),
            )
            .await
            .unwrap();
        let mut record = harness
            .repository
            .get(&harness.sandbox_id)
            .await
            .unwrap()
            .unwrap();
        let expected = record.generation();
        record
            .replace_expiry(harness.now + Duration::minutes(10))
            .unwrap();
        harness
            .repository
            .compare_and_swap(&harness.sandbox_id, expected, record.clone())
            .await
            .unwrap();
        assert!(!old.is_current(&record, harness.now));

        let renewed = harness
            .service
            .resolve(
                &harness.route(ENVD_PORT),
                &token_headers(ENVD_ACCESS_TOKEN_HEADER, &harness.envd_secret),
            )
            .await
            .unwrap();
        assert!(renewed.sandbox_generation() > old.sandbox_generation());

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
        assert!(matches!(
            harness
                .service
                .resolve(
                    &harness.route(ENVD_PORT),
                    &token_headers(ENVD_ACCESS_TOKEN_HEADER, &harness.envd_secret),
                )
                .await,
            Err(RouteLeaseError::Inactive)
        ));
        assert_eq!(
            harness
                .service
                .resolve_envd_health(
                    &harness.route(ENVD_PORT),
                    &token_headers(ENVD_ACCESS_TOKEN_HEADER, &harness.envd_secret),
                )
                .await
                .unwrap(),
            EnvdHealthResolution::Inactive
        );
        assert!(matches!(
            harness
                .service
                .resolve_envd_health(
                    &harness.route(ENVD_PORT),
                    &token_headers(ENVD_ACCESS_TOKEN_HEADER, &harness.traffic_secret),
                )
                .await,
            Err(RouteLeaseError::Unauthorized)
        ));
    }

    #[tokio::test]
    async fn resolves_a_generation_fenced_lease_after_sqlite_restart() {
        let harness = Harness::new().await;
        let record = harness
            .repository
            .get(&harness.sandbox_id)
            .await
            .unwrap()
            .unwrap();
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("routes.db");
        let repository = SqliteSandboxRepository::open(&path).await.unwrap();
        repository.insert(record).await.unwrap();
        drop(repository);

        let repository = Arc::new(SqliteSandboxRepository::open(&path).await.unwrap());
        let tokens = Arc::new(
            RotatingTokenProvider::new(1, [TokenKeyMaterial::new(1, &[7; 32], &[8; 32]).unwrap()])
                .unwrap(),
        );
        let service = RouteLeaseService::new(repository, tokens, Arc::new(FixedClock(harness.now)));
        let lease = service
            .resolve(
                &harness.route(ENVD_PORT),
                &token_headers(ENVD_ACCESS_TOKEN_HEADER, &harness.envd_secret),
            )
            .await
            .unwrap();

        assert_eq!(lease.sandbox_id(), &harness.sandbox_id);
        assert_eq!(lease.token_scope(), TokenScope::Envd);
        assert_eq!(lease.expires_at(), harness.now + Duration::minutes(5));
    }
}
