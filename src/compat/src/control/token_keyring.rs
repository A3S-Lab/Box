use std::collections::BTreeMap;
use std::fmt;

use async_trait::async_trait;
use ring::aead::{self, Aad, LessSafeKey, Nonce, UnboundKey};
use ring::hmac;
use ring::rand::{SecureRandom, SystemRandom};

use super::{
    IssuedToken, SecretToken, StoredToken, TokenIssuer, TokenIssuerError, TokenIssuerResult,
    TokenResolver, TokenScope, TokenVerifier,
};

const KEY_BYTES: usize = 32;
const NONCE_BYTES: usize = 12;
const TOKEN_BYTES: usize = 32;
const TOKEN_AAD_DOMAIN: &[u8] = b"a3s-box-e2b-token-v1";
const TOKEN_DIGEST_DOMAIN: &[u8] = b"a3s-box-e2b-token-digest-v1";

#[derive(Clone)]
pub struct TokenKeyMaterial {
    version: u32,
    encryption_key: [u8; KEY_BYTES],
    digest_key: [u8; KEY_BYTES],
}

impl TokenKeyMaterial {
    pub fn new(version: u32, encryption_key: &[u8], digest_key: &[u8]) -> TokenIssuerResult<Self> {
        if version == 0 {
            return Err(TokenIssuerError::InvalidMaterial);
        }
        let encryption_key = encryption_key
            .try_into()
            .map_err(|_| TokenIssuerError::InvalidMaterial)?;
        let digest_key = digest_key
            .try_into()
            .map_err(|_| TokenIssuerError::InvalidMaterial)?;
        Ok(Self {
            version,
            encryption_key,
            digest_key,
        })
    }

    pub const fn version(&self) -> u32 {
        self.version
    }
}

impl fmt::Debug for TokenKeyMaterial {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("TokenKeyMaterial")
            .field("version", &self.version)
            .field("encryption_key", &"[REDACTED]")
            .field("digest_key", &"[REDACTED]")
            .finish()
    }
}

pub struct RotatingTokenProvider {
    active_version: u32,
    keys: BTreeMap<u32, TokenKeyMaterial>,
    random: SystemRandom,
}

impl RotatingTokenProvider {
    pub fn new(
        active_version: u32,
        keys: impl IntoIterator<Item = TokenKeyMaterial>,
    ) -> TokenIssuerResult<Self> {
        let mut by_version = BTreeMap::new();
        for key in keys {
            let version = key.version();
            if by_version.insert(version, key).is_some() {
                return Err(TokenIssuerError::InvalidMaterial);
            }
        }
        if active_version == 0 || !by_version.contains_key(&active_version) {
            return Err(TokenIssuerError::UnknownKeyVersion(active_version));
        }
        Ok(Self {
            active_version,
            keys: by_version,
            random: SystemRandom::new(),
        })
    }

    pub const fn active_version(&self) -> u32 {
        self.active_version
    }

    fn key(&self, version: u32) -> TokenIssuerResult<&TokenKeyMaterial> {
        self.keys
            .get(&version)
            .ok_or(TokenIssuerError::UnknownKeyVersion(version))
    }

    fn store(&self, scope: TokenScope, secret: &SecretToken) -> TokenIssuerResult<StoredToken> {
        let key = self.key(self.active_version)?;
        let mut nonce_bytes = [0_u8; NONCE_BYTES];
        self.random
            .fill(&mut nonce_bytes)
            .map_err(|_| TokenIssuerError::Unavailable("secure random generation failed".into()))?;

        let mut sealed = secret.expose_secret().as_bytes().to_vec();
        encryption_key(key)?
            .seal_in_place_append_tag(
                Nonce::assume_unique_for_key(nonce_bytes),
                Aad::from(aad(key.version(), scope)),
                &mut sealed,
            )
            .map_err(|_| TokenIssuerError::Unavailable("token encryption failed".into()))?;

        let mut ciphertext = Vec::with_capacity(NONCE_BYTES + sealed.len());
        ciphertext.extend_from_slice(&nonce_bytes);
        ciphertext.extend_from_slice(&sealed);
        let digest = token_digest(key, scope, secret.expose_secret());
        StoredToken::new(key.version(), ciphertext, digest)
            .map_err(|_| TokenIssuerError::InvalidMaterial)
    }

    fn open(&self, scope: TokenScope, stored: &StoredToken) -> TokenIssuerResult<SecretToken> {
        let key = self.key(stored.key_version())?;
        let (nonce, sealed) = stored
            .ciphertext()
            .split_at_checked(NONCE_BYTES)
            .ok_or(TokenIssuerError::InvalidMaterial)?;
        let nonce: [u8; NONCE_BYTES] = nonce
            .try_into()
            .map_err(|_| TokenIssuerError::InvalidMaterial)?;
        let mut sealed = sealed.to_vec();
        let plaintext = encryption_key(key)?
            .open_in_place(
                Nonce::assume_unique_for_key(nonce),
                Aad::from(aad(key.version(), scope)),
                &mut sealed,
            )
            .map_err(|_| TokenIssuerError::InvalidMaterial)?;
        let secret =
            std::str::from_utf8(plaintext).map_err(|_| TokenIssuerError::InvalidMaterial)?;
        let secret = SecretToken::new(secret)?;
        if !verify_token_digest(key, scope, &secret, stored.digest()) {
            return Err(TokenIssuerError::InvalidMaterial);
        }
        Ok(secret)
    }
}

impl fmt::Debug for RotatingTokenProvider {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("RotatingTokenProvider")
            .field("active_version", &self.active_version)
            .field("configured_versions", &self.keys.keys())
            .finish_non_exhaustive()
    }
}

#[async_trait]
impl TokenIssuer for RotatingTokenProvider {
    async fn issue(&self, scope: TokenScope) -> TokenIssuerResult<IssuedToken> {
        let mut random = [0_u8; TOKEN_BYTES];
        self.random
            .fill(&mut random)
            .map_err(|_| TokenIssuerError::Unavailable("secure random generation failed".into()))?;
        let secret = SecretToken::new(hex::encode(random))?;
        let stored = self.store(scope, &secret)?;
        Ok(IssuedToken { secret, stored })
    }
}

#[async_trait]
impl TokenResolver for RotatingTokenProvider {
    async fn resolve(
        &self,
        scope: TokenScope,
        stored: &StoredToken,
    ) -> TokenIssuerResult<SecretToken> {
        self.open(scope, stored)
    }
}

#[async_trait]
impl TokenVerifier for RotatingTokenProvider {
    async fn verify(
        &self,
        scope: TokenScope,
        presented: &SecretToken,
        stored: &StoredToken,
    ) -> TokenIssuerResult<bool> {
        let key = self.key(stored.key_version())?;
        Ok(verify_token_digest(key, scope, presented, stored.digest()))
    }
}

fn encryption_key(key: &TokenKeyMaterial) -> TokenIssuerResult<LessSafeKey> {
    UnboundKey::new(&aead::AES_256_GCM, &key.encryption_key)
        .map(LessSafeKey::new)
        .map_err(|_| TokenIssuerError::InvalidMaterial)
}

fn aad(version: u32, scope: TokenScope) -> Vec<u8> {
    let mut value = Vec::with_capacity(TOKEN_AAD_DOMAIN.len() + 5);
    value.extend_from_slice(TOKEN_AAD_DOMAIN);
    value.extend_from_slice(&version.to_be_bytes());
    value.push(scope_marker(scope));
    value
}

fn token_digest(key: &TokenKeyMaterial, scope: TokenScope, secret: &str) -> Vec<u8> {
    let digest_key = hmac::Key::new(hmac::HMAC_SHA256, &key.digest_key);
    hmac::sign(&digest_key, &digest_input(key.version(), scope, secret))
        .as_ref()
        .to_vec()
}

fn verify_token_digest(
    key: &TokenKeyMaterial,
    scope: TokenScope,
    secret: &SecretToken,
    expected: &[u8],
) -> bool {
    let digest_key = hmac::Key::new(hmac::HMAC_SHA256, &key.digest_key);
    hmac::verify(
        &digest_key,
        &digest_input(key.version(), scope, secret.expose_secret()),
        expected,
    )
    .is_ok()
}

fn digest_input(version: u32, scope: TokenScope, secret: &str) -> Vec<u8> {
    let mut value = Vec::with_capacity(TOKEN_DIGEST_DOMAIN.len() + secret.len() + 5);
    value.extend_from_slice(TOKEN_DIGEST_DOMAIN);
    value.extend_from_slice(&version.to_be_bytes());
    value.push(scope_marker(scope));
    value.extend_from_slice(secret.as_bytes());
    value
}

const fn scope_marker(scope: TokenScope) -> u8 {
    match scope {
        TokenScope::Envd => 1,
        TokenScope::Traffic => 2,
        TokenScope::Volume => 3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn key(version: u32, marker: u8) -> TokenKeyMaterial {
        TokenKeyMaterial::new(version, &[marker; KEY_BYTES], &[marker + 1; KEY_BYTES]).unwrap()
    }

    #[tokio::test]
    async fn issued_tokens_are_encrypted_hashed_and_scope_bound() {
        let provider = RotatingTokenProvider::new(1, [key(1, 7)]).unwrap();
        let issued = provider.issue(TokenScope::Envd).await.unwrap();

        assert_eq!(issued.stored.key_version(), 1);
        assert_ne!(
            issued.stored.ciphertext(),
            issued.secret.expose_secret().as_bytes()
        );
        assert!(!issued
            .stored
            .ciphertext()
            .windows(issued.secret.expose_secret().len())
            .any(|window| window == issued.secret.expose_secret().as_bytes()));
        assert_eq!(issued.stored.digest().len(), 32);
        assert_eq!(
            provider
                .resolve(TokenScope::Envd, &issued.stored)
                .await
                .unwrap()
                .expose_secret(),
            issued.secret.expose_secret()
        );
        assert!(provider
            .verify(TokenScope::Envd, &issued.secret, &issued.stored)
            .await
            .unwrap());
        assert!(!provider
            .verify(TokenScope::Traffic, &issued.secret, &issued.stored)
            .await
            .unwrap());
        assert!(matches!(
            provider.resolve(TokenScope::Traffic, &issued.stored).await,
            Err(TokenIssuerError::InvalidMaterial)
        ));
    }

    #[tokio::test]
    async fn key_rotation_issues_with_active_key_and_resolves_retained_versions() {
        let first = RotatingTokenProvider::new(1, [key(1, 11)]).unwrap();
        let old = first.issue(TokenScope::Traffic).await.unwrap();

        let rotated = RotatingTokenProvider::new(2, [key(1, 11), key(2, 21)]).unwrap();
        let current = rotated.issue(TokenScope::Traffic).await.unwrap();

        assert_eq!(current.stored.key_version(), 2);
        assert_eq!(
            rotated
                .resolve(TokenScope::Traffic, &old.stored)
                .await
                .unwrap()
                .expose_secret(),
            old.secret.expose_secret()
        );
        assert_eq!(
            rotated
                .resolve(TokenScope::Traffic, &current.stored)
                .await
                .unwrap()
                .expose_secret(),
            current.secret.expose_secret()
        );

        let retired = RotatingTokenProvider::new(2, [key(2, 21)]).unwrap();
        assert!(matches!(
            retired.resolve(TokenScope::Traffic, &old.stored).await,
            Err(TokenIssuerError::UnknownKeyVersion(1))
        ));
    }

    #[tokio::test]
    async fn tampered_ciphertext_and_digest_are_rejected() {
        let provider = RotatingTokenProvider::new(3, [key(3, 31)]).unwrap();
        let issued = provider.issue(TokenScope::Envd).await.unwrap();

        let mut ciphertext = issued.stored.ciphertext().to_vec();
        ciphertext[NONCE_BYTES] ^= 1;
        let tampered_ciphertext =
            StoredToken::new(3, ciphertext, issued.stored.digest().to_vec()).unwrap();
        assert!(matches!(
            provider
                .resolve(TokenScope::Envd, &tampered_ciphertext)
                .await,
            Err(TokenIssuerError::InvalidMaterial)
        ));

        let mut digest = issued.stored.digest().to_vec();
        digest[0] ^= 1;
        let tampered_digest =
            StoredToken::new(3, issued.stored.ciphertext().to_vec(), digest).unwrap();
        assert!(!provider
            .verify(TokenScope::Envd, &issued.secret, &tampered_digest)
            .await
            .unwrap());
        assert!(matches!(
            provider.resolve(TokenScope::Envd, &tampered_digest).await,
            Err(TokenIssuerError::InvalidMaterial)
        ));
    }

    #[tokio::test]
    async fn stored_tokens_round_trip_and_randomize_repeated_plaintext() {
        let provider = RotatingTokenProvider::new(5, [key(5, 51)]).unwrap();
        let secret = SecretToken::new("fixed-token-material").unwrap();
        let first = provider.store(TokenScope::Traffic, &secret).unwrap();
        let second = provider.store(TokenScope::Traffic, &secret).unwrap();

        assert_ne!(first.ciphertext(), second.ciphertext());
        assert_eq!(first.digest(), second.digest());

        let encoded = serde_json::to_vec(&first).unwrap();
        assert!(!encoded
            .windows(secret.expose_secret().len())
            .any(|window| window == secret.expose_secret().as_bytes()));
        let decoded: StoredToken = serde_json::from_slice(&encoded).unwrap();
        assert_eq!(decoded, first);
        assert_eq!(
            provider
                .resolve(TokenScope::Traffic, &decoded)
                .await
                .unwrap()
                .expose_secret(),
            secret.expose_secret()
        );
    }

    #[test]
    fn invalid_keyring_configuration_fails_closed() {
        assert!(matches!(
            TokenKeyMaterial::new(0, &[1; KEY_BYTES], &[2; KEY_BYTES]),
            Err(TokenIssuerError::InvalidMaterial)
        ));
        assert!(matches!(
            TokenKeyMaterial::new(1, &[1; KEY_BYTES - 1], &[2; KEY_BYTES]),
            Err(TokenIssuerError::InvalidMaterial)
        ));
        assert!(matches!(
            RotatingTokenProvider::new(1, std::iter::empty()),
            Err(TokenIssuerError::UnknownKeyVersion(1))
        ));
        assert!(matches!(
            RotatingTokenProvider::new(2, [key(1, 1)]),
            Err(TokenIssuerError::UnknownKeyVersion(2))
        ));
        assert!(matches!(
            RotatingTokenProvider::new(1, [key(1, 1), key(1, 2)]),
            Err(TokenIssuerError::InvalidMaterial)
        ));
    }

    #[test]
    fn keyring_debug_output_redacts_key_material() {
        let material = key(4, 41);
        let provider = RotatingTokenProvider::new(4, [material.clone()]).unwrap();

        assert!(!format!("{material:?}").contains(&hex::encode([41_u8; KEY_BYTES])));
        let debug = format!("{provider:?}");
        assert!(debug.contains("configured_versions"));
        assert!(!debug.contains(&hex::encode([41_u8; KEY_BYTES])));
    }
}
