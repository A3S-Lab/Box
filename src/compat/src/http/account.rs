use std::fmt;
use std::num::NonZeroU32;
use std::str::FromStr;

use async_trait::async_trait;
use ring::pbkdf2;
use ring::rand::{SecureRandom, SystemRandom};
use serde::{Deserialize, Serialize};
use thiserror::Error;

use super::{
    AuthenticatedAccount, AuthenticationError, AuthenticationResult, CredentialScheme,
    CredentialVerifier, PresentedCredential,
};

const HASH_ALGORITHM: &str = "pbkdf2-sha256";
const DEFAULT_ITERATIONS: u32 = 210_000;
const MINIMUM_ITERATIONS: u32 = 100_000;
const SALT_BYTES: usize = 16;
const DIGEST_BYTES: usize = 32;
const MAX_CREDENTIAL_BYTES: usize = 4096;

#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(try_from = "String", into = "String")]
pub struct CredentialHash {
    iterations: NonZeroU32,
    salt: [u8; SALT_BYTES],
    digest: [u8; DIGEST_BYTES],
}

impl CredentialHash {
    pub fn generate(secret: &str) -> CredentialHashResult<Self> {
        validate_secret_length(secret)?;
        let mut salt = [0_u8; SALT_BYTES];
        SystemRandom::new()
            .fill(&mut salt)
            .map_err(|_| CredentialHashError::RandomUnavailable)?;
        Self::derive(secret, DEFAULT_ITERATIONS, &salt)
    }

    pub fn derive(secret: &str, iterations: u32, salt: &[u8]) -> CredentialHashResult<Self> {
        validate_secret_length(secret)?;
        if iterations < MINIMUM_ITERATIONS {
            return Err(CredentialHashError::InvalidIterations);
        }
        let iterations =
            NonZeroU32::new(iterations).ok_or(CredentialHashError::InvalidIterations)?;
        let salt: [u8; SALT_BYTES] = salt
            .try_into()
            .map_err(|_| CredentialHashError::InvalidSalt)?;
        let mut digest = [0_u8; DIGEST_BYTES];
        pbkdf2::derive(
            pbkdf2::PBKDF2_HMAC_SHA256,
            iterations,
            &salt,
            secret.as_bytes(),
            &mut digest,
        );
        Ok(Self {
            iterations,
            salt,
            digest,
        })
    }

    pub fn verify(&self, secret: &str) -> bool {
        if validate_secret_length(secret).is_err() {
            return false;
        }
        pbkdf2::verify(
            pbkdf2::PBKDF2_HMAC_SHA256,
            self.iterations,
            &self.salt,
            secret.as_bytes(),
            &self.digest,
        )
        .is_ok()
    }
}

impl fmt::Display for CredentialHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "{HASH_ALGORITHM}${}${}${}",
            self.iterations,
            hex::encode(self.salt),
            hex::encode(self.digest)
        )
    }
}

impl fmt::Debug for CredentialHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("CredentialHash")
            .field("algorithm", &HASH_ALGORITHM)
            .field("iterations", &self.iterations)
            .field("salt", &"[REDACTED]")
            .field("digest", &"[REDACTED]")
            .finish()
    }
}

impl FromStr for CredentialHash {
    type Err = CredentialHashError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        let mut fields = value.split('$');
        if fields.next() != Some(HASH_ALGORITHM) {
            return Err(CredentialHashError::InvalidEncoding);
        }
        let iterations = fields
            .next()
            .ok_or(CredentialHashError::InvalidEncoding)?
            .parse::<u32>()
            .map_err(|_| CredentialHashError::InvalidEncoding)?;
        if iterations < MINIMUM_ITERATIONS {
            return Err(CredentialHashError::InvalidIterations);
        }
        let iterations =
            NonZeroU32::new(iterations).ok_or(CredentialHashError::InvalidIterations)?;
        let salt = hex::decode(fields.next().ok_or(CredentialHashError::InvalidEncoding)?)
            .map_err(|_| CredentialHashError::InvalidEncoding)?;
        let digest = hex::decode(fields.next().ok_or(CredentialHashError::InvalidEncoding)?)
            .map_err(|_| CredentialHashError::InvalidEncoding)?;
        if fields.next().is_some() {
            return Err(CredentialHashError::InvalidEncoding);
        }
        Ok(Self {
            iterations,
            salt: salt
                .try_into()
                .map_err(|_| CredentialHashError::InvalidSalt)?,
            digest: digest
                .try_into()
                .map_err(|_| CredentialHashError::InvalidDigest)?,
        })
    }
}

impl TryFrom<String> for CredentialHash {
    type Error = CredentialHashError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        value.parse()
    }
}

impl From<CredentialHash> for String {
    fn from(value: CredentialHash) -> Self {
        value.to_string()
    }
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum CredentialHashError {
    #[error("credential is empty or too large")]
    InvalidSecret,
    #[error("credential hash iteration count is below the production minimum")]
    InvalidIterations,
    #[error("credential hash salt is invalid")]
    InvalidSalt,
    #[error("credential hash digest is invalid")]
    InvalidDigest,
    #[error("credential hash encoding is invalid")]
    InvalidEncoding,
    #[error("secure random generation is unavailable")]
    RandomUnavailable,
    #[error("account identity is invalid")]
    InvalidAccount,
    #[error("compatibility API key must match e2b_[0-9a-f]+")]
    InvalidApiKey,
    #[error("at least one hashed account credential is required")]
    MissingCredentials,
}

pub type CredentialHashResult<T> = std::result::Result<T, CredentialHashError>;

#[derive(Clone)]
pub struct HashedAccountCredential {
    scheme: CredentialScheme,
    owner_id: String,
    client_id: String,
    hash: CredentialHash,
}

impl HashedAccountCredential {
    pub fn new(
        scheme: CredentialScheme,
        owner_id: impl Into<String>,
        client_id: impl Into<String>,
        hash: CredentialHash,
    ) -> CredentialHashResult<Self> {
        let owner_id = owner_id.into();
        let client_id = client_id.into();
        if owner_id.trim().is_empty() || client_id.trim().is_empty() {
            return Err(CredentialHashError::InvalidAccount);
        }
        Ok(Self {
            scheme,
            owner_id,
            client_id,
            hash,
        })
    }

    pub fn from_secret(
        scheme: CredentialScheme,
        owner_id: impl Into<String>,
        client_id: impl Into<String>,
        secret: &str,
    ) -> CredentialHashResult<Self> {
        if scheme == CredentialScheme::ApiKey && !is_compatibility_api_key(secret) {
            return Err(CredentialHashError::InvalidApiKey);
        }
        Self::new(
            scheme,
            owner_id,
            client_id,
            CredentialHash::generate(secret)?,
        )
    }

    pub const fn scheme(&self) -> CredentialScheme {
        self.scheme
    }

    pub fn owner_id(&self) -> &str {
        &self.owner_id
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    pub fn hash(&self) -> &CredentialHash {
        &self.hash
    }
}

impl fmt::Debug for HashedAccountCredential {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HashedAccountCredential")
            .field("scheme", &self.scheme)
            .field("owner_id", &self.owner_id)
            .field("client_id", &self.client_id)
            .field("hash", &self.hash)
            .finish()
    }
}

pub struct HashedCredentialVerifier {
    credentials: Vec<HashedAccountCredential>,
}

impl HashedCredentialVerifier {
    pub fn new(
        credentials: impl IntoIterator<Item = HashedAccountCredential>,
    ) -> CredentialHashResult<Self> {
        let credentials: Vec<_> = credentials.into_iter().collect();
        if credentials.is_empty() {
            return Err(CredentialHashError::MissingCredentials);
        }
        Ok(Self { credentials })
    }
}

impl fmt::Debug for HashedCredentialVerifier {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("HashedCredentialVerifier")
            .field("credential_count", &self.credentials.len())
            .finish()
    }
}

#[async_trait]
impl CredentialVerifier for HashedCredentialVerifier {
    async fn verify(
        &self,
        credential: &PresentedCredential,
    ) -> AuthenticationResult<AuthenticatedAccount> {
        if credential.scheme() == CredentialScheme::ApiKey
            && !is_compatibility_api_key(credential.expose_secret())
        {
            return Err(AuthenticationError::Invalid);
        }

        let mut account = None;
        for stored in &self.credentials {
            if stored.scheme() != credential.scheme()
                || credential
                    .owner_hint()
                    .is_some_and(|hint| hint != stored.owner_id())
                || !stored.hash().verify(credential.expose_secret())
            {
                continue;
            }
            if account.is_some() {
                return Err(AuthenticationError::Invalid);
            }
            account = Some(AuthenticatedAccount {
                owner_id: stored.owner_id().to_string(),
                client_id: stored.client_id().to_string(),
            });
        }
        account.ok_or(AuthenticationError::Invalid)
    }
}

fn validate_secret_length(secret: &str) -> CredentialHashResult<()> {
    if secret.is_empty() || secret.len() > MAX_CREDENTIAL_BYTES {
        Err(CredentialHashError::InvalidSecret)
    } else {
        Ok(())
    }
}

fn is_compatibility_api_key(value: &str) -> bool {
    value.strip_prefix("e2b_").is_some_and(|suffix| {
        !suffix.is_empty()
            && suffix
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    })
}

#[cfg(test)]
mod tests {
    use axum::http::{header, HeaderMap, HeaderValue};

    use super::*;

    fn deterministic_hash(secret: &str, marker: u8) -> CredentialHash {
        CredentialHash::derive(secret, MINIMUM_ITERATIONS, &[marker; SALT_BYTES]).unwrap()
    }

    fn credential(headers: HeaderMap) -> PresentedCredential {
        PresentedCredential::from_headers(&headers).unwrap()
    }

    #[test]
    fn encoded_hash_round_trips_without_exposing_the_secret() {
        let hash = deterministic_hash("e2b_a1b2c3", 7);
        let encoded = hash.to_string();

        assert!(!encoded.contains("e2b_a1b2c3"));
        let parsed: CredentialHash = encoded.parse().unwrap();
        assert!(parsed.verify("e2b_a1b2c3"));
        assert!(!parsed.verify("e2b_deadbeef"));
        assert_eq!(
            serde_json::from_str::<CredentialHash>(&serde_json::to_string(&hash).unwrap()).unwrap(),
            hash
        );
    }

    #[test]
    fn hashes_use_salts_and_enforce_production_cost() {
        let first = deterministic_hash("e2b_a1b2c3", 1);
        let second = deterministic_hash("e2b_a1b2c3", 2);

        assert_ne!(first, second);
        assert_eq!(
            CredentialHash::derive("e2b_a1b2c3", MINIMUM_ITERATIONS - 1, &[0; SALT_BYTES])
                .unwrap_err(),
            CredentialHashError::InvalidIterations
        );
        assert!("pbkdf2-sha256$99999$00000000000000000000000000000000$0000000000000000000000000000000000000000000000000000000000000000"
            .parse::<CredentialHash>()
            .is_err());
    }

    #[tokio::test]
    async fn verifier_authenticates_api_keys_and_bearer_tokens_by_hash() {
        let verifier = HashedCredentialVerifier::new([
            HashedAccountCredential::new(
                CredentialScheme::ApiKey,
                "owner-api",
                "client-api",
                deterministic_hash("e2b_a1b2c3", 3),
            )
            .unwrap(),
            HashedAccountCredential::new(
                CredentialScheme::Bearer,
                "owner-bearer",
                "client-bearer",
                deterministic_hash("bearer-secret", 4),
            )
            .unwrap(),
        ])
        .unwrap();

        let api_key = credential(HeaderMap::from_iter([(
            "x-api-key".parse().unwrap(),
            HeaderValue::from_static("e2b_a1b2c3"),
        )]));
        let api_account = verifier.verify(&api_key).await.unwrap();
        assert_eq!(api_account.owner_id, "owner-api");
        assert_eq!(api_account.client_id, "client-api");

        let bearer = credential(HeaderMap::from_iter([(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer bearer-secret"),
        )]));
        let bearer_account = verifier.verify(&bearer).await.unwrap();
        assert_eq!(bearer_account.owner_id, "owner-bearer");
    }

    #[tokio::test]
    async fn verifier_rejects_invalid_lexical_form_hints_and_ambiguous_keys() {
        assert_eq!(
            HashedAccountCredential::from_secret(
                CredentialScheme::ApiKey,
                "owner",
                "client",
                "e2b_UPPER",
            )
            .unwrap_err(),
            CredentialHashError::InvalidApiKey
        );

        let shared = "shared-bearer";
        let verifier = HashedCredentialVerifier::new([
            HashedAccountCredential::new(
                CredentialScheme::Bearer,
                "owner-a",
                "client-a",
                deterministic_hash(shared, 5),
            )
            .unwrap(),
            HashedAccountCredential::new(
                CredentialScheme::Bearer,
                "owner-b",
                "client-b",
                deterministic_hash(shared, 6),
            )
            .unwrap(),
        ])
        .unwrap();
        let ambiguous = credential(HeaderMap::from_iter([(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer shared-bearer"),
        )]));
        assert!(matches!(
            verifier.verify(&ambiguous).await,
            Err(AuthenticationError::Invalid)
        ));

        let hinted = credential(HeaderMap::from_iter([
            (
                header::AUTHORIZATION,
                HeaderValue::from_static("Bearer shared-bearer"),
            ),
            (
                "x-team-id".parse().unwrap(),
                HeaderValue::from_static("owner-a"),
            ),
        ]));
        assert_eq!(verifier.verify(&hinted).await.unwrap().owner_id, "owner-a");
    }

    #[tokio::test]
    async fn supabase_owner_hint_selects_one_hashed_account() {
        let shared = "supabase-secret";
        let verifier = HashedCredentialVerifier::new([
            HashedAccountCredential::new(
                CredentialScheme::Supabase,
                "team-a",
                "client-a",
                deterministic_hash(shared, 7),
            )
            .unwrap(),
            HashedAccountCredential::new(
                CredentialScheme::Supabase,
                "team-b",
                "client-b",
                deterministic_hash(shared, 8),
            )
            .unwrap(),
        ])
        .unwrap();

        let hinted = credential(HeaderMap::from_iter([
            (
                "x-supabase-token".parse().unwrap(),
                HeaderValue::from_static("supabase-secret"),
            ),
            (
                "x-supabase-team".parse().unwrap(),
                HeaderValue::from_static("team-b"),
            ),
        ]));
        let account = verifier.verify(&hinted).await.unwrap();
        assert_eq!(account.owner_id, "team-b");
        assert_eq!(account.client_id, "client-b");

        let ambiguous = credential(HeaderMap::from_iter([(
            "x-supabase-token".parse().unwrap(),
            HeaderValue::from_static("supabase-secret"),
        )]));
        assert!(matches!(
            verifier.verify(&ambiguous).await,
            Err(AuthenticationError::Invalid)
        ));
    }

    #[test]
    fn malformed_hashes_and_empty_provider_configuration_are_rejected() {
        assert_eq!(
            CredentialHash::generate("").unwrap_err(),
            CredentialHashError::InvalidSecret
        );
        assert_eq!(
            CredentialHash::generate(&"x".repeat(MAX_CREDENTIAL_BYTES + 1)).unwrap_err(),
            CredentialHashError::InvalidSecret
        );
        for malformed in [
            "",
            "sha256$100000$00000000000000000000000000000000$0000000000000000000000000000000000000000000000000000000000000000",
            "pbkdf2-sha256$100000$zz$00",
            "pbkdf2-sha256$100000$00$0000000000000000000000000000000000000000000000000000000000000000",
            "pbkdf2-sha256$100000$00000000000000000000000000000000$00",
            "pbkdf2-sha256$100000$00000000000000000000000000000000$0000000000000000000000000000000000000000000000000000000000000000$extra",
        ] {
            assert!(malformed.parse::<CredentialHash>().is_err(), "{malformed}");
        }
        assert_eq!(
            HashedAccountCredential::new(
                CredentialScheme::Bearer,
                " ",
                "client",
                deterministic_hash("secret", 10),
            )
            .unwrap_err(),
            CredentialHashError::InvalidAccount
        );
        assert_eq!(
            HashedCredentialVerifier::new(std::iter::empty()).unwrap_err(),
            CredentialHashError::MissingCredentials
        );
    }

    #[test]
    fn debug_output_redacts_hash_material() {
        let hash = deterministic_hash("e2b_a1b2c3", 9);
        let debug = format!("{hash:?}");
        assert!(debug.contains("REDACTED"));
        assert!(!debug.contains(&hex::encode([9_u8; SALT_BYTES])));
    }
}
