use std::sync::Arc;

use async_trait::async_trait;

use crate::control::{
    ResolvedTemplate, TemplateProvider, TemplateProviderError, TemplateProviderResult,
};

use super::{SnapshotRepository, SnapshotRepositoryError, SnapshotState};

#[derive(Clone)]
pub struct SnapshotTemplateProvider {
    configured: Arc<dyn TemplateProvider>,
    snapshots: Arc<dyn SnapshotRepository>,
}

impl SnapshotTemplateProvider {
    pub fn new(
        configured: Arc<dyn TemplateProvider>,
        snapshots: Arc<dyn SnapshotRepository>,
    ) -> Self {
        Self {
            configured,
            snapshots,
        }
    }
}

#[async_trait]
impl TemplateProvider for SnapshotTemplateProvider {
    async fn resolve(
        &self,
        owner_id: &str,
        template_id: &str,
    ) -> TemplateProviderResult<ResolvedTemplate> {
        let normalized = if template_id.contains(':') {
            template_id.to_string()
        } else {
            format!("{template_id}:default")
        };
        let snapshot = self
            .snapshots
            .get_by_reference(owner_id, &normalized)
            .await
            .map_err(map_repository_error)?;
        if let Some(snapshot) = snapshot.filter(|record| record.state() == SnapshotState::Active) {
            return Ok(snapshot.template().clone());
        }
        self.configured.resolve(owner_id, template_id).await
    }
}

fn map_repository_error(error: SnapshotRepositoryError) -> TemplateProviderError {
    match error {
        SnapshotRepositoryError::Unavailable(message) => {
            TemplateProviderError::Unavailable(message)
        }
        SnapshotRepositoryError::Corrupt(message) => TemplateProviderError::Unavailable(message),
        SnapshotRepositoryError::Duplicate => TemplateProviderError::Unavailable(
            "snapshot repository returned an impossible duplicate read".to_string(),
        ),
    }
}
