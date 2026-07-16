use std::sync::Arc;

use async_trait::async_trait;

use crate::control::{
    ResolvedTemplate, TemplateProvider, TemplateProviderError, TemplateProviderResult,
};

use super::super::*;
use super::support::{record, template};

struct ConfiguredTemplate;

#[async_trait]
impl TemplateProvider for ConfiguredTemplate {
    async fn resolve(
        &self,
        _owner_id: &str,
        template_id: &str,
    ) -> TemplateProviderResult<ResolvedTemplate> {
        if template_id == "configured" {
            Ok(template())
        } else {
            Err(TemplateProviderError::NotFound(template_id.to_string()))
        }
    }
}

#[tokio::test]
async fn dynamic_templates_are_owner_scoped_and_fall_back_to_configuration() {
    let snapshots = Arc::new(MemorySnapshotRepository::default());
    let active = record(
        "snapshot-a",
        "owner-a",
        Some("state"),
        SnapshotState::Active,
        0,
    );
    snapshots.insert(active.clone()).await.unwrap();
    let provider = SnapshotTemplateProvider::new(Arc::new(ConfiguredTemplate), snapshots);

    let resolved = provider
        .resolve("owner-a", active.reference())
        .await
        .unwrap();
    assert_eq!(
        resolved.rootfs_snapshot_id.as_ref(),
        Some(active.content_id())
    );
    assert!(matches!(
        provider.resolve("owner-b", active.reference()).await,
        Err(TemplateProviderError::NotFound(_))
    ));
    assert!(provider
        .resolve("owner-b", "configured")
        .await
        .unwrap()
        .rootfs_snapshot_id
        .is_none());
}
