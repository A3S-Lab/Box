use std::fmt;

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::model::LifecycleError;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "StoredTokenRepr", into = "StoredTokenRepr")]
pub struct StoredToken {
    key_version: u32,
    ciphertext: Vec<u8>,
    digest: Vec<u8>,
}

#[derive(Serialize, Deserialize)]
struct StoredTokenRepr {
    key_version: u32,
    ciphertext: Vec<u8>,
    digest: Vec<u8>,
}

impl TryFrom<StoredTokenRepr> for StoredToken {
    type Error = LifecycleError;

    fn try_from(value: StoredTokenRepr) -> Result<Self, Self::Error> {
        Self::new(value.key_version, value.ciphertext, value.digest)
    }
}

impl From<StoredToken> for StoredTokenRepr {
    fn from(value: StoredToken) -> Self {
        Self {
            key_version: value.key_version,
            ciphertext: value.ciphertext,
            digest: value.digest,
        }
    }
}

impl StoredToken {
    pub fn new(
        key_version: u32,
        ciphertext: Vec<u8>,
        digest: Vec<u8>,
    ) -> Result<Self, LifecycleError> {
        if key_version == 0 || ciphertext.is_empty() || digest.is_empty() {
            return Err(LifecycleError::InvalidCredentialMaterial);
        }
        Ok(Self {
            key_version,
            ciphertext,
            digest,
        })
    }

    pub const fn key_version(&self) -> u32 {
        self.key_version
    }

    pub fn ciphertext(&self) -> &[u8] {
        &self.ciphertext
    }

    pub fn digest(&self) -> &[u8] {
        &self.digest
    }
}

impl fmt::Debug for StoredToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("StoredToken")
            .field("key_version", &self.key_version)
            .field("ciphertext", &"[REDACTED]")
            .field("digest", &"[REDACTED]")
            .finish()
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SandboxCredentials {
    pub envd: StoredToken,
    pub traffic: StoredToken,
}

#[derive(Clone, PartialEq, Eq)]
pub struct SecretToken(String);

impl SecretToken {
    pub fn new(value: impl Into<String>) -> TokenIssuerResult<Self> {
        let value = value.into();
        if value.is_empty() {
            return Err(TokenIssuerError::InvalidMaterial);
        }
        Ok(Self(value))
    }

    pub fn expose_secret(&self) -> &str {
        &self.0
    }
}

impl fmt::Debug for SecretToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("SecretToken([REDACTED])")
    }
}

pub struct IssuedToken {
    pub secret: SecretToken,
    pub stored: StoredToken,
}

impl fmt::Debug for IssuedToken {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("IssuedToken")
            .field("secret", &self.secret)
            .field("stored", &self.stored)
            .finish()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TokenScope {
    Envd,
    Traffic,
    Volume,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TokenIssuerError {
    #[error("token material is invalid")]
    InvalidMaterial,
    #[error("token key version {0} is not configured")]
    UnknownKeyVersion(u32),
    #[error("token provider is unavailable: {0}")]
    Unavailable(String),
}

pub type TokenIssuerResult<T> = std::result::Result<T, TokenIssuerError>;

#[async_trait]
pub trait TokenIssuer: Send + Sync {
    async fn issue(&self, scope: TokenScope) -> TokenIssuerResult<IssuedToken>;
}

#[async_trait]
pub trait TokenResolver: Send + Sync {
    async fn resolve(
        &self,
        scope: TokenScope,
        stored: &StoredToken,
    ) -> TokenIssuerResult<SecretToken>;
}

#[async_trait]
pub trait TokenVerifier: Send + Sync {
    async fn verify(
        &self,
        scope: TokenScope,
        presented: &SecretToken,
        stored: &StoredToken,
    ) -> TokenIssuerResult<bool>;
}
