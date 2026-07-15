use std::fmt;

use async_trait::async_trait;
use axum::http::{header, HeaderMap};
use serde::{Deserialize, Serialize};
use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CredentialScheme {
    ApiKey,
    Bearer,
    Supabase,
}

#[derive(Clone, PartialEq, Eq)]
pub struct PresentedCredential {
    scheme: CredentialScheme,
    secret: String,
    owner_hint: Option<String>,
}

impl PresentedCredential {
    pub fn from_headers(headers: &HeaderMap) -> AuthenticationResult<Self> {
        if let Some(value) = headers.get("x-api-key") {
            return Ok(Self {
                scheme: CredentialScheme::ApiKey,
                secret: header_value(value)?,
                owner_hint: None,
            });
        }

        if let Some(value) = headers.get("x-supabase-token") {
            let owner_hint = headers
                .get("x-supabase-team")
                .map(header_value)
                .transpose()?;
            return Ok(Self {
                scheme: CredentialScheme::Supabase,
                secret: header_value(value)?,
                owner_hint,
            });
        }

        if let Some(value) = headers.get(header::AUTHORIZATION) {
            let authorization = header_value(value)?;
            let secret = authorization
                .strip_prefix("Bearer ")
                .filter(|secret| !secret.is_empty())
                .ok_or(AuthenticationError::Invalid)?;
            let owner_hint = headers.get("x-team-id").map(header_value).transpose()?;
            return Ok(Self {
                scheme: CredentialScheme::Bearer,
                secret: secret.to_string(),
                owner_hint,
            });
        }

        Err(AuthenticationError::Missing)
    }

    pub const fn scheme(&self) -> CredentialScheme {
        self.scheme
    }

    pub fn expose_secret(&self) -> &str {
        &self.secret
    }

    pub fn owner_hint(&self) -> Option<&str> {
        self.owner_hint.as_deref()
    }
}

impl fmt::Debug for PresentedCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("PresentedCredential")
            .field("scheme", &self.scheme)
            .field("secret", &"[REDACTED]")
            .field("owner_hint", &self.owner_hint)
            .finish()
    }
}

#[derive(Debug, Clone)]
pub struct AuthenticatedAccount {
    pub owner_id: String,
    pub client_id: String,
}

#[derive(Debug, Error)]
pub enum AuthenticationError {
    #[error("authentication credential is missing")]
    Missing,
    #[error("authentication credential is invalid")]
    Invalid,
    #[error("authentication provider is unavailable: {0}")]
    Unavailable(String),
}

pub type AuthenticationResult<T> = std::result::Result<T, AuthenticationError>;

#[async_trait]
pub trait CredentialVerifier: Send + Sync {
    async fn verify(
        &self,
        credential: &PresentedCredential,
    ) -> AuthenticationResult<AuthenticatedAccount>;
}

fn header_value(value: &axum::http::HeaderValue) -> AuthenticationResult<String> {
    value
        .to_str()
        .map(str::to_string)
        .map_err(|_| AuthenticationError::Invalid)
}
