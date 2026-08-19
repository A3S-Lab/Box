//! Compose adapter for Box's single transient Secret materialization boundary.

use a3s_box_core::compose::{ComposeConfig, ServiceConfig};
use a3s_box_core::config::BoxConfig;

#[cfg(target_os = "linux")]
const SECRET_SCOPE: &str = "compose";

fn uses_secrets(config: &ComposeConfig) -> bool {
    config
        .services
        .values()
        .any(|service| !service.secret_environment.is_empty())
}

#[cfg(target_os = "linux")]
fn material_from_process_environment(
    source: &str,
) -> Result<a3s_box_runtime::BoxSecretMaterial, Box<dyn std::error::Error>> {
    let value = std::env::var(source).map_err(|_| {
        format!(
            "Compose Secret source environment variable {source:?} is unset or is not valid UTF-8"
        )
    })?;
    a3s_box_runtime::BoxSecretMaterial::new(value.into_bytes()).map_err(|error| error.into())
}

#[cfg(target_os = "linux")]
async fn scoped_store(
) -> Result<a3s_box_runtime::BoxTransientSecretStore, Box<dyn std::error::Error>> {
    let root = a3s_box_core::dirs_home().join("runtime-secrets");
    Ok(a3s_box_runtime::BoxTransientSecretStore::new(root)
        .private_scope(SECRET_SCOPE)
        .await?)
}

/// Fail before network, state, or VM mutation when Secret prerequisites are
/// unavailable. Values are immediately wrapped in zeroizing memory and dropped.
pub(super) async fn preflight(config: &ComposeConfig) -> Result<(), Box<dyn std::error::Error>> {
    if !uses_secrets(config) {
        return Ok(());
    }

    #[cfg(not(target_os = "linux"))]
    {
        Err("Compose transient Secret environment projection requires a private Linux tmpfs".into())
    }

    #[cfg(target_os = "linux")]
    {
        let _store = scoped_store().await?;
        let sources = config
            .services
            .values()
            .flat_map(|service| service.secret_environment.values())
            .collect::<std::collections::BTreeSet<_>>();
        for source in sources {
            let _material = material_from_process_environment(source)?;
        }
        Ok(())
    }
}

#[cfg(target_os = "linux")]
pub(super) struct ComposeSecretLease {
    store: a3s_box_runtime::BoxTransientSecretStore,
    identity: String,
    armed: bool,
}

#[cfg(target_os = "linux")]
impl ComposeSecretLease {
    pub(super) fn identity(&self) -> &str {
        &self.identity
    }

    pub(super) fn configure_vm(
        &self,
        manager: &mut a3s_box_runtime::VmManager,
    ) -> Result<(), Box<dyn std::error::Error>> {
        self.store.configure_vm(manager)?;
        Ok(())
    }

    /// Transfer cleanup ownership to the persisted box record.
    pub(super) fn persist(&mut self) {
        self.armed = false;
    }
}

#[cfg(target_os = "linux")]
impl Drop for ComposeSecretLease {
    fn drop(&mut self) {
        if self.armed {
            if let Err(error) = self.store.cleanup_identity_sync(&self.identity) {
                tracing::warn!(
                    error = %error,
                    "Failed to clean unregistered Compose transient Secret material"
                );
            }
        }
    }
}

#[cfg(not(target_os = "linux"))]
pub(super) struct ComposeSecretLease;

#[cfg(not(target_os = "linux"))]
impl ComposeSecretLease {
    pub(super) fn identity(&self) -> &str {
        unreachable!("Secret preflight rejects non-Linux Compose execution")
    }

    pub(super) fn configure_vm(
        &self,
        _manager: &mut a3s_box_runtime::VmManager,
    ) -> Result<(), Box<dyn std::error::Error>> {
        unreachable!("Secret preflight rejects non-Linux Compose execution")
    }

    pub(super) fn persist(&mut self) {
        unreachable!("Secret preflight rejects non-Linux Compose execution")
    }
}

/// Resolve and project one service immediately before its box lifecycle begins.
pub(super) async fn project_service(
    service: &ServiceConfig,
    box_id: &str,
    box_config: &mut BoxConfig,
) -> Result<Option<ComposeSecretLease>, Box<dyn std::error::Error>> {
    if service.secret_environment.is_empty() {
        return Ok(None);
    }

    #[cfg(not(target_os = "linux"))]
    {
        let _ = (box_id, box_config);
        Err("Compose transient Secret environment projection requires Linux".into())
    }

    #[cfg(target_os = "linux")]
    {
        use sha2::{Digest, Sha256};

        let store = scoped_store().await?;
        let identity = format!("sha256:{}", hex::encode(Sha256::digest(box_id.as_bytes())));
        let mut references = service.secret_environment.iter().collect::<Vec<_>>();
        references.sort_by(|left, right| left.0.cmp(right.0));
        let mut bindings = Vec::with_capacity(references.len());
        for (target, source) in references {
            bindings.push((target.clone(), material_from_process_environment(source)?));
        }
        let projection = store.materialize_environment(&identity, bindings).await?;
        box_config.volumes.extend(projection.volumes);
        box_config.extra_env.push((
            a3s_box_core::secret::SECRET_ENVIRONMENT_MANIFEST.to_string(),
            projection.manifest,
        ));
        Ok(Some(ComposeSecretLease {
            store,
            identity,
            armed: true,
        }))
    }
}

#[cfg(target_os = "linux")]
pub(crate) fn cleanup_persisted(identity: &str) -> Result<(), Box<dyn std::error::Error>> {
    let root = a3s_box_core::dirs_home()
        .join("runtime-secrets")
        .join(SECRET_SCOPE);
    a3s_box_runtime::BoxTransientSecretStore::new(root).cleanup_identity_sync(identity)?;
    Ok(())
}

#[cfg(not(target_os = "linux"))]
pub(crate) fn cleanup_persisted(_identity: &str) -> Result<(), Box<dyn std::error::Error>> {
    Ok(())
}
