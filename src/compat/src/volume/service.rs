use std::sync::Arc;

use async_trait::async_trait;
use thiserror::Error;
use uuid::Uuid;

use crate::control::{
    Clock, SecretToken, TokenIssuer, TokenIssuerError, TokenResolver, TokenScope, TokenVerifier,
};

use super::{
    validate_mounts, ResolvedVolumeMount, RuntimeVolumeError, RuntimeVolumeRemoveResult,
    RuntimeVolumeStore, VolumeContentError, VolumeFilesystem, VolumeId, VolumeModelError,
    VolumeMount, VolumeMountResolver, VolumeRecord, VolumeReplaceResult, VolumeRepository,
    VolumeRepositoryError, VolumeState,
};

#[derive(Debug)]
pub struct VolumeConnection {
    pub record: VolumeRecord,
    pub token: SecretToken,
}

#[derive(Debug, Clone)]
pub struct AuthorizedVolume {
    pub record: VolumeRecord,
    pub root: std::path::PathBuf,
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct VolumeReconciliationReport {
    pub examined: usize,
    pub completed: usize,
    pub deferred: usize,
    pub failures: Vec<String>,
}

#[derive(Debug, Error)]
pub enum VolumeServiceError {
    #[error("invalid volume request: {0}")]
    InvalidRequest(String),
    #[error("volume not found")]
    NotFound,
    #[error("volume already exists")]
    Duplicate,
    #[error("volume is in use or changing state")]
    Conflict,
    #[error("volume token is invalid")]
    Forbidden,
    #[error(transparent)]
    Repository(#[from] VolumeRepositoryError),
    #[error(transparent)]
    Runtime(#[from] RuntimeVolumeError),
    #[error(transparent)]
    Credential(#[from] TokenIssuerError),
    #[error(transparent)]
    Model(#[from] VolumeModelError),
    #[error(transparent)]
    Content(#[from] VolumeContentError),
}

pub type VolumeServiceResult<T> = std::result::Result<T, VolumeServiceError>;

#[derive(Clone)]
pub struct VolumeService {
    repository: Arc<dyn VolumeRepository>,
    runtime: Arc<dyn RuntimeVolumeStore>,
    clock: Arc<dyn Clock>,
    token_issuer: Arc<dyn TokenIssuer>,
    token_resolver: Arc<dyn TokenResolver>,
    token_verifier: Arc<dyn TokenVerifier>,
    filesystem: Arc<VolumeFilesystem>,
}

pub struct VolumeServiceDependencies {
    pub repository: Arc<dyn VolumeRepository>,
    pub runtime: Arc<dyn RuntimeVolumeStore>,
    pub clock: Arc<dyn Clock>,
    pub token_issuer: Arc<dyn TokenIssuer>,
    pub token_resolver: Arc<dyn TokenResolver>,
    pub token_verifier: Arc<dyn TokenVerifier>,
    pub filesystem: Arc<VolumeFilesystem>,
}

impl VolumeService {
    pub fn new(dependencies: VolumeServiceDependencies) -> Self {
        Self {
            repository: dependencies.repository,
            runtime: dependencies.runtime,
            clock: dependencies.clock,
            token_issuer: dependencies.token_issuer,
            token_resolver: dependencies.token_resolver,
            token_verifier: dependencies.token_verifier,
            filesystem: dependencies.filesystem,
        }
    }

    pub fn filesystem(&self) -> &VolumeFilesystem {
        &self.filesystem
    }

    pub async fn create(
        &self,
        owner_id: &str,
        name: &str,
    ) -> VolumeServiceResult<VolumeConnection> {
        if owner_id.trim().is_empty() || !super::valid_volume_name(name) {
            return Err(VolumeServiceError::InvalidRequest(
                "volume name must match [A-Za-z0-9_-]+".to_string(),
            ));
        }

        let volume_id = VolumeId::new(Uuid::new_v4().to_string())?;
        let runtime_name = format!("e2b-{}", Uuid::new_v4().simple());
        let token = self.token_issuer.issue(TokenScope::Volume).await?;
        let mut record = VolumeRecord::creating(
            volume_id,
            owner_id,
            name,
            runtime_name,
            token.stored,
            self.clock.now(),
        )?;
        match self.repository.insert(record.clone()).await {
            Ok(()) => {}
            Err(VolumeRepositoryError::Duplicate) => return Err(VolumeServiceError::Duplicate),
            Err(error) => return Err(error.into()),
        }

        let runtime = match self.runtime.materialize(record.runtime_name()).await {
            Ok(runtime) => runtime,
            Err(error) => {
                let _ = self
                    .repository
                    .delete(record.volume_id(), VolumeState::Creating)
                    .await;
                return Err(error.into());
            }
        };
        if let Err(error) = self.filesystem.initialize_root(&runtime.mount_point).await {
            let _ = self.runtime.remove(record.runtime_name()).await;
            let _ = self
                .repository
                .delete(record.volume_id(), VolumeState::Creating)
                .await;
            return Err(error.into());
        }
        record.mark_active()?;
        self.replace(VolumeState::Creating, record.clone()).await?;
        Ok(VolumeConnection {
            record,
            token: token.secret,
        })
    }

    pub async fn get(
        &self,
        owner_id: &str,
        volume_id: &VolumeId,
    ) -> VolumeServiceResult<VolumeConnection> {
        let record = self.require_visible(owner_id, volume_id).await?;
        let token = self
            .token_resolver
            .resolve(TokenScope::Volume, record.token())
            .await?;
        Ok(VolumeConnection { record, token })
    }

    pub async fn list(&self, owner_id: &str) -> VolumeServiceResult<Vec<VolumeRecord>> {
        Ok(self.repository.list(owner_id).await?)
    }

    pub async fn delete(&self, owner_id: &str, volume_id: &VolumeId) -> VolumeServiceResult<()> {
        let mut record = self.require_visible(owner_id, volume_id).await?;
        record.begin_delete()?;
        self.replace(VolumeState::Active, record.clone()).await?;

        match self.runtime.remove(record.runtime_name()).await {
            Ok(RuntimeVolumeRemoveResult::Removed | RuntimeVolumeRemoveResult::NotFound) => {}
            Err(RuntimeVolumeError::InUse) => {
                self.restore_active(record).await?;
                return Err(VolumeServiceError::Conflict);
            }
            Err(error) => {
                self.restore_active(record).await?;
                return Err(error.into());
            }
        }
        self.delete_record(record.volume_id(), VolumeState::Deleting)
            .await
    }

    pub async fn authorize(
        &self,
        volume_id: &VolumeId,
        presented: &SecretToken,
    ) -> VolumeServiceResult<AuthorizedVolume> {
        let record = self
            .repository
            .get(volume_id)
            .await?
            .filter(|record| record.state() == VolumeState::Active)
            .ok_or(VolumeServiceError::NotFound)?;
        if !self
            .token_verifier
            .verify(TokenScope::Volume, presented, record.token())
            .await?
        {
            return Err(VolumeServiceError::Forbidden);
        }
        let runtime = self
            .runtime
            .get(record.runtime_name())
            .await?
            .ok_or_else(|| {
                RuntimeVolumeError::Unavailable(format!(
                    "runtime volume '{}' is missing",
                    record.runtime_name()
                ))
            })?;
        Ok(AuthorizedVolume {
            record,
            root: runtime.mount_point,
        })
    }

    pub async fn reconcile_startup(&self) -> VolumeServiceResult<VolumeReconciliationReport> {
        let mut report = VolumeReconciliationReport::default();
        for state in [VolumeState::Creating, VolumeState::Deleting] {
            for record in self.repository.list_in_state(state).await? {
                report.examined += 1;
                let result = match state {
                    VolumeState::Creating => self.reconcile_create(record).await,
                    VolumeState::Deleting => self.reconcile_delete(record).await,
                    VolumeState::Active => unreachable!(),
                };
                match result {
                    Ok(ReconciliationOutcome::Completed) => report.completed += 1,
                    Ok(ReconciliationOutcome::Deferred) => report.deferred += 1,
                    Err(error) => report.failures.push(error.to_string()),
                }
            }
        }
        Ok(report)
    }

    async fn reconcile_create(
        &self,
        mut record: VolumeRecord,
    ) -> VolumeServiceResult<ReconciliationOutcome> {
        let runtime = self.runtime.materialize(record.runtime_name()).await?;
        self.filesystem
            .initialize_root(&runtime.mount_point)
            .await?;
        record.mark_active()?;
        self.replace(VolumeState::Creating, record).await?;
        Ok(ReconciliationOutcome::Completed)
    }

    async fn reconcile_delete(
        &self,
        mut record: VolumeRecord,
    ) -> VolumeServiceResult<ReconciliationOutcome> {
        match self.runtime.remove(record.runtime_name()).await {
            Ok(RuntimeVolumeRemoveResult::Removed | RuntimeVolumeRemoveResult::NotFound) => {
                self.delete_record(record.volume_id(), VolumeState::Deleting)
                    .await?;
                Ok(ReconciliationOutcome::Completed)
            }
            Err(RuntimeVolumeError::InUse) => {
                record.abort_delete()?;
                self.replace(VolumeState::Deleting, record).await?;
                Ok(ReconciliationOutcome::Deferred)
            }
            Err(error) => Err(error.into()),
        }
    }

    async fn require_visible(
        &self,
        owner_id: &str,
        volume_id: &VolumeId,
    ) -> VolumeServiceResult<VolumeRecord> {
        self.repository
            .get(volume_id)
            .await?
            .filter(|record| record.owner_id() == owner_id && record.state() == VolumeState::Active)
            .ok_or(VolumeServiceError::NotFound)
    }

    async fn restore_active(&self, mut record: VolumeRecord) -> VolumeServiceResult<()> {
        record.abort_delete()?;
        self.replace(VolumeState::Deleting, record).await
    }

    async fn replace(
        &self,
        expected: VolumeState,
        record: VolumeRecord,
    ) -> VolumeServiceResult<()> {
        match self.repository.replace(expected, record).await? {
            VolumeReplaceResult::Updated => Ok(()),
            VolumeReplaceResult::NotFound => Err(VolumeServiceError::NotFound),
            VolumeReplaceResult::Conflict => Err(VolumeServiceError::Conflict),
        }
    }

    async fn delete_record(
        &self,
        volume_id: &VolumeId,
        expected: VolumeState,
    ) -> VolumeServiceResult<()> {
        match self.repository.delete(volume_id, expected).await? {
            VolumeReplaceResult::Updated => Ok(()),
            VolumeReplaceResult::NotFound => Err(VolumeServiceError::NotFound),
            VolumeReplaceResult::Conflict => Err(VolumeServiceError::Conflict),
        }
    }
}

#[async_trait]
impl VolumeMountResolver for VolumeService {
    async fn resolve_mounts(
        &self,
        owner_id: &str,
        mounts: &[VolumeMount],
    ) -> VolumeServiceResult<Vec<ResolvedVolumeMount>> {
        validate_mounts(mounts)?;
        let mut resolved = Vec::with_capacity(mounts.len());
        for mount in mounts {
            let record = self
                .repository
                .get_by_owner_name(owner_id, &mount.name)
                .await?
                .filter(|record| record.state() == VolumeState::Active)
                .ok_or(VolumeServiceError::NotFound)?;
            let runtime = self
                .runtime
                .get(record.runtime_name())
                .await?
                .ok_or_else(|| {
                    RuntimeVolumeError::Unavailable(format!(
                        "runtime volume '{}' is missing",
                        record.runtime_name()
                    ))
                })?;
            resolved.push(ResolvedVolumeMount {
                public: mount.clone(),
                runtime_name: record.runtime_name().to_string(),
                host_path: runtime.mount_point,
            });
        }
        Ok(resolved)
    }
}

enum ReconciliationOutcome {
    Completed,
    Deferred,
}
