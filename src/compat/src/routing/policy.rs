use std::collections::BTreeMap;
use std::fmt;

use serde::{Deserialize, Serialize};
use thiserror::Error;

use crate::control::TokenScope;

pub const ENVD_PORT: u16 = 49_983;
pub const CODE_INTERPRETER_PORT: u16 = 49_999;
pub const MCP_PORT: u16 = 50_005;
const MAX_ROUTED_PORTS: usize = 64;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(
    try_from = "BTreeMap<u16, TokenScope>",
    into = "BTreeMap<u16, TokenScope>"
)]
pub struct SandboxRoutePolicy {
    ports: BTreeMap<u16, TokenScope>,
}

impl SandboxRoutePolicy {
    pub fn new(ports: impl IntoIterator<Item = (u16, TokenScope)>) -> RoutePolicyResult<Self> {
        let mut routed = BTreeMap::new();
        for (port, scope) in ports {
            if port == 0 || routed.insert(port, scope).is_some() {
                return Err(RoutePolicyError::InvalidPort(port));
            }
        }
        let policy = Self { ports: routed };
        policy.validate()?;
        Ok(policy)
    }

    pub fn with_port(mut self, port: u16, scope: TokenScope) -> RoutePolicyResult<Self> {
        if port == 0 || self.ports.insert(port, scope).is_some() {
            return Err(RoutePolicyError::InvalidPort(port));
        }
        self.validate()?;
        Ok(self)
    }

    pub fn token_scope(&self, port: u16) -> Option<TokenScope> {
        self.ports.get(&port).copied()
    }

    pub fn ports(&self) -> impl Iterator<Item = (u16, TokenScope)> + '_ {
        self.ports.iter().map(|(port, scope)| (*port, *scope))
    }

    pub fn validate(&self) -> RoutePolicyResult<()> {
        if self.ports.len() > MAX_ROUTED_PORTS {
            return Err(RoutePolicyError::TooManyPorts);
        }
        if self.ports.get(&ENVD_PORT) != Some(&TokenScope::Envd) {
            return Err(RoutePolicyError::MissingEnvd);
        }
        if self.ports.iter().any(|(port, scope)| {
            *port == 0
                || (*scope == TokenScope::Envd && *port != ENVD_PORT)
                || *scope == TokenScope::Volume
        }) {
            return Err(RoutePolicyError::InvalidScope);
        }
        Ok(())
    }
}

impl Default for SandboxRoutePolicy {
    fn default() -> Self {
        Self {
            ports: BTreeMap::from([(ENVD_PORT, TokenScope::Envd)]),
        }
    }
}

impl fmt::Debug for SandboxRoutePolicy {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("SandboxRoutePolicy")
            .field("ports", &self.ports)
            .finish()
    }
}

impl TryFrom<BTreeMap<u16, TokenScope>> for SandboxRoutePolicy {
    type Error = RoutePolicyError;

    fn try_from(ports: BTreeMap<u16, TokenScope>) -> Result<Self, Self::Error> {
        let policy = Self { ports };
        policy.validate()?;
        Ok(policy)
    }
}

impl From<SandboxRoutePolicy> for BTreeMap<u16, TokenScope> {
    fn from(policy: SandboxRoutePolicy) -> Self {
        policy.ports
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum RoutePolicyError {
    #[error("routed port {0} is invalid or duplicated")]
    InvalidPort(u16),
    #[error("the envd compatibility port is required")]
    MissingEnvd,
    #[error("envd token scope is valid only for the envd compatibility port")]
    InvalidScope,
    #[error("too many routed ports")]
    TooManyPorts,
}

pub type RoutePolicyResult<T> = std::result::Result<T, RoutePolicyError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn policy_requires_scope_separated_envd_and_traffic_ports() {
        let policy = SandboxRoutePolicy::default()
            .with_port(CODE_INTERPRETER_PORT, TokenScope::Traffic)
            .unwrap()
            .with_port(MCP_PORT, TokenScope::Traffic)
            .unwrap();

        assert_eq!(policy.token_scope(ENVD_PORT), Some(TokenScope::Envd));
        assert_eq!(
            policy.token_scope(CODE_INTERPRETER_PORT),
            Some(TokenScope::Traffic)
        );
        assert_eq!(policy.ports().count(), 3);
        assert_eq!(
            SandboxRoutePolicy::new([(CODE_INTERPRETER_PORT, TokenScope::Traffic)]).unwrap_err(),
            RoutePolicyError::MissingEnvd
        );
        assert_eq!(
            SandboxRoutePolicy::default()
                .with_port(MCP_PORT, TokenScope::Envd)
                .unwrap_err(),
            RoutePolicyError::InvalidScope
        );
    }

    #[test]
    fn persisted_policy_revalidates_scope_invariants() {
        let invalid = format!(r#"{{"{ENVD_PORT}":"traffic"}}"#);
        assert!(serde_json::from_str::<SandboxRoutePolicy>(&invalid).is_err());

        let policy = SandboxRoutePolicy::default();
        assert_eq!(
            serde_json::from_str::<SandboxRoutePolicy>(&serde_json::to_string(&policy).unwrap())
                .unwrap(),
            policy
        );
    }
}
