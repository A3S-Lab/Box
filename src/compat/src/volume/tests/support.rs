use std::collections::BTreeMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use chrono::{DateTime, Duration, TimeZone, Utc};
use sha2::{Digest, Sha256};
use tempfile::{tempdir, TempDir};

use crate::control::{
    Clock, IssuedToken, SecretToken, StoredToken, TokenIssuer, TokenIssuerError, TokenIssuerResult,
    TokenResolver, TokenScope, TokenVerifier,
};

use super::super::*;

pub fn test_time(second: i64) -> DateTime<Utc> {
    Utc.with_ymd_and_hms(2026, 7, 16, 12, 0, 0)
        .single()
        .unwrap()
        + Duration::seconds(second)
}

pub fn stored_token(secret: &str) -> StoredToken {
    let ciphertext = secret.as_bytes().to_vec();
    StoredToken::new(1, ciphertext.clone(), Sha256::digest(ciphertext).to_vec()).unwrap()
}

pub fn record(
    id: &str,
    owner: &str,
    name: &str,
    runtime_name: &str,
    state: VolumeState,
    second: i64,
) -> VolumeRecord {
    let mut record = VolumeRecord::creating(
        VolumeId::new(id).unwrap(),
        owner,
        name,
        runtime_name,
        stored_token(&format!("token-{id}")),
        test_time(second),
    )
    .unwrap();
    if matches!(state, VolumeState::Active | VolumeState::Deleting) {
        record.mark_active().unwrap();
    }
    if state == VolumeState::Deleting {
        record.begin_delete().unwrap();
    }
    record
}

#[derive(Debug)]
pub struct FixedClock;

impl Clock for FixedClock {
    fn now(&self) -> DateTime<Utc> {
        test_time(0)
    }
}

#[derive(Debug, Default)]
pub struct TestTokens {
    sequence: AtomicU64,
}

#[async_trait]
impl TokenIssuer for TestTokens {
    async fn issue(&self, scope: TokenScope) -> TokenIssuerResult<IssuedToken> {
        if scope != TokenScope::Volume {
            return Err(TokenIssuerError::InvalidMaterial);
        }
        let sequence = self.sequence.fetch_add(1, Ordering::Relaxed) + 1;
        let value = format!("volume-token-{sequence}");
        Ok(IssuedToken {
            secret: SecretToken::new(&value)?,
            stored: stored_token(&value),
        })
    }
}

#[async_trait]
impl TokenResolver for TestTokens {
    async fn resolve(
        &self,
        scope: TokenScope,
        stored: &StoredToken,
    ) -> TokenIssuerResult<SecretToken> {
        if scope != TokenScope::Volume {
            return Err(TokenIssuerError::InvalidMaterial);
        }
        let value = std::str::from_utf8(stored.ciphertext())
            .map_err(|_| TokenIssuerError::InvalidMaterial)?;
        SecretToken::new(value)
    }
}

#[async_trait]
impl TokenVerifier for TestTokens {
    async fn verify(
        &self,
        scope: TokenScope,
        presented: &SecretToken,
        stored: &StoredToken,
    ) -> TokenIssuerResult<bool> {
        if scope != TokenScope::Volume {
            return Ok(false);
        }
        let digest = Sha256::digest(presented.expose_secret().as_bytes());
        Ok(digest[..] == stored.digest()[..])
    }
}

#[derive(Debug)]
pub struct TestRuntime {
    root: std::path::PathBuf,
    volumes: Mutex<BTreeMap<String, RuntimeVolume>>,
}

impl TestRuntime {
    fn new(root: std::path::PathBuf) -> Self {
        Self {
            root,
            volumes: Mutex::new(BTreeMap::new()),
        }
    }

    pub fn set_in_use(&self, name: &str, in_use: bool) {
        let mut volumes = self.volumes.lock().unwrap();
        let volume = volumes.get_mut(name).expect("runtime volume must exist");
        volume.in_use_by = if in_use {
            vec!["sandbox-1".to_string()]
        } else {
            Vec::new()
        };
    }
}

#[async_trait]
impl RuntimeVolumeStore for TestRuntime {
    async fn materialize(&self, name: &str) -> RuntimeVolumeResult<RuntimeVolume> {
        let mut volumes = self.volumes.lock().unwrap();
        if let Some(volume) = volumes.get(name) {
            return Ok(volume.clone());
        }
        let mount_point = self.root.join(name);
        std::fs::create_dir_all(&mount_point).map_err(|error| {
            RuntimeVolumeError::Unavailable(format!("create test volume: {error}"))
        })?;
        let volume = RuntimeVolume {
            name: name.to_string(),
            mount_point,
            in_use_by: Vec::new(),
        };
        volumes.insert(name.to_string(), volume.clone());
        Ok(volume)
    }

    async fn get(&self, name: &str) -> RuntimeVolumeResult<Option<RuntimeVolume>> {
        Ok(self.volumes.lock().unwrap().get(name).cloned())
    }

    async fn remove(&self, name: &str) -> RuntimeVolumeResult<RuntimeVolumeRemoveResult> {
        let removed = {
            let mut volumes = self.volumes.lock().unwrap();
            if volumes
                .get(name)
                .is_some_and(|volume| !volume.in_use_by.is_empty())
            {
                return Err(RuntimeVolumeError::InUse);
            }
            volumes.remove(name)
        };
        let path = self.root.join(name);
        if path.exists() {
            std::fs::remove_dir_all(&path).map_err(|error| {
                RuntimeVolumeError::Unavailable(format!("remove test volume: {error}"))
            })?;
        }
        Ok(if removed.is_some() {
            RuntimeVolumeRemoveResult::Removed
        } else {
            RuntimeVolumeRemoveResult::NotFound
        })
    }
}

pub struct ServiceHarness {
    pub service: VolumeService,
    pub repository: Arc<MemoryVolumeRepository>,
    pub runtime: Arc<TestRuntime>,
    _directory: TempDir,
}

impl ServiceHarness {
    pub fn new() -> Self {
        let directory = tempdir().unwrap();
        let repository = Arc::new(MemoryVolumeRepository::default());
        let runtime = Arc::new(TestRuntime::new(directory.path().join("volumes")));
        let tokens = Arc::new(TestTokens::default());
        let service = VolumeService::new(VolumeServiceDependencies {
            repository: repository.clone(),
            runtime: runtime.clone(),
            clock: Arc::new(FixedClock),
            token_issuer: tokens.clone(),
            token_resolver: tokens.clone(),
            token_verifier: tokens,
            filesystem: Arc::new(VolumeFilesystem::new(Arc::new(
                IdentityVolumeIdMapper::current(),
            ))),
        });
        Self {
            service,
            repository,
            runtime,
            _directory: directory,
        }
    }
}
