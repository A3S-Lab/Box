//! Box-owned workload templates for Gateway-driven scaling.

use std::{collections::BTreeMap, path::Path};

use a3s_box_core::{
    compose::ComposeConfig, CreateExecutionRequest, ExecutionIsolation, ExecutionRecordPolicy,
};
use sha2::{Digest, Sha256};
use thiserror::Error;

use crate::ComposeRuntimePlan;

pub const SCALE_MANAGED_LABEL: &str = "com.a3s.scale.managed";
pub const SCALE_SERVICE_LABEL: &str = "com.a3s.scale.service";
pub const SCALE_SLOT_LABEL: &str = "com.a3s.scale.slot";
pub const SCALE_TEMPLATE_DIGEST_LABEL: &str = "com.a3s.scale.template-digest";

#[derive(Debug, Error)]
pub enum ScaleCatalogError {
    #[error("failed to read scale service catalog {path}: {source}")]
    Read {
        path: String,
        source: std::io::Error,
    },
    #[error("invalid scale service catalog: {0}")]
    Invalid(String),
}

/// Validated Compose ACL templates owned by the Box execution plane.
#[derive(Debug, Clone)]
pub struct ScaleServiceCatalog {
    plan: ComposeRuntimePlan,
    isolation: ExecutionIsolation,
}

impl ScaleServiceCatalog {
    pub fn from_acl_file(
        path: &Path,
        project_name: impl Into<String>,
        isolation: ExecutionIsolation,
    ) -> Result<Self, ScaleCatalogError> {
        if path.extension().and_then(|extension| extension.to_str()) != Some("acl") {
            return Err(ScaleCatalogError::Invalid(format!(
                "scale service catalog {} must use the .acl format",
                path.display()
            )));
        }
        let source = std::fs::read_to_string(path).map_err(|source| ScaleCatalogError::Read {
            path: path.display().to_string(),
            source,
        })?;
        Self::from_acl_str_with_base_dir(
            &source,
            project_name,
            path.parent().unwrap_or_else(|| Path::new(".")),
            isolation,
        )
    }

    pub fn from_acl_str(
        source: &str,
        project_name: impl Into<String>,
        isolation: ExecutionIsolation,
    ) -> Result<Self, ScaleCatalogError> {
        Self::from_acl_str_with_base_dir(source, project_name, Path::new("."), isolation)
    }

    fn from_acl_str_with_base_dir(
        source: &str,
        project_name: impl Into<String>,
        base_dir: &Path,
        isolation: ExecutionIsolation,
    ) -> Result<Self, ScaleCatalogError> {
        let config = ComposeConfig::from_acl_str(source)
            .map_err(|error| ScaleCatalogError::Invalid(error.to_string()))?;
        validate_stateless_templates(&config)?;
        let plan = ComposeRuntimePlan::with_base_dir(project_name, config, base_dir)
            .map_err(|error| ScaleCatalogError::Invalid(error.to_string()))?;
        Ok(Self { plan, isolation })
    }

    pub fn contains(&self, service: &str) -> bool {
        self.plan.config.services.contains_key(service)
    }

    pub fn services(&self) -> Vec<String> {
        let mut services = self
            .plan
            .config
            .services
            .keys()
            .cloned()
            .collect::<Vec<_>>();
        services.sort();
        services
    }

    pub fn create_request(
        &self,
        service: &str,
        slot: u32,
    ) -> Result<CreateExecutionRequest, ScaleCatalogError> {
        let mut config = self
            .plan
            .build_box_config(service, None)
            .map_err(|error| ScaleCatalogError::Invalid(error.to_string()))?;
        config.isolation = self.isolation;
        let encoded = serde_json::to_vec(&config).map_err(|error| {
            ScaleCatalogError::Invalid(format!(
                "failed to encode service {service:?} template: {error}"
            ))
        })?;
        let digest = format!("sha256:{:x}", Sha256::digest(encoded));
        let mut labels = BTreeMap::new();
        labels.insert(SCALE_MANAGED_LABEL.to_string(), "true".to_string());
        labels.insert(SCALE_SERVICE_LABEL.to_string(), service.to_string());
        labels.insert(SCALE_SLOT_LABEL.to_string(), slot.to_string());
        labels.insert(SCALE_TEMPLATE_DIGEST_LABEL.to_string(), digest);

        Ok(CreateExecutionRequest {
            external_sandbox_id: format!("scale-{service}-{slot}"),
            config,
            labels,
            policy: ExecutionRecordPolicy::default(),
            rootfs_snapshot_id: None,
        })
    }
}

fn validate_stateless_templates(config: &ComposeConfig) -> Result<(), ScaleCatalogError> {
    for (name, service) in &config.services {
        if !service.depends_on.services().is_empty() {
            return Err(ScaleCatalogError::Invalid(format!(
                "service {name:?} uses depends_on; independently scaled templates cannot own dependency lifecycles"
            )));
        }
        if !service.ports.is_empty() {
            return Err(ScaleCatalogError::Invalid(format!(
                "service {name:?} publishes fixed host ports; independently scaled replicas require runtime-discovered endpoints"
            )));
        }
        if !service.volumes.is_empty() {
            return Err(ScaleCatalogError::Invalid(format!(
                "service {name:?} mounts shared volumes; Gateway scaling currently accepts only stateless templates"
            )));
        }
        if !service.networks.names().is_empty() {
            return Err(ScaleCatalogError::Invalid(format!(
                "service {name:?} selects a Compose network; independently scaled replicas currently use Box's default TSI network"
            )));
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const CATALOG: &str = r#"
        service "worker" {
            image       = "ghcr.io/a3s-lab/worker:v1"
            command     = ["serve", "--port", "8080"]
            environment = { MODE = "production" }
            cpus        = 2
            mem_limit   = "768m"
        }
        service "api" {
            image = "ghcr.io/a3s-lab/api:v2"
        }
    "#;

    #[test]
    fn catalog_builds_deterministic_labeled_execution_templates() {
        let catalog = ScaleServiceCatalog::from_acl_str(
            CATALOG,
            "gateway-scale",
            ExecutionIsolation::Sandbox,
        )
        .unwrap();
        assert_eq!(catalog.services(), vec!["api", "worker"]);

        let first = catalog.create_request("worker", 3).unwrap();
        let replay = catalog.create_request("worker", 3).unwrap();
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(&replay).unwrap()
        );
        assert_eq!(first.external_sandbox_id, "scale-worker-3");
        assert_eq!(first.config.isolation, ExecutionIsolation::Sandbox);
        assert_eq!(first.config.resources.vcpus, 2);
        assert_eq!(first.config.resources.memory_mb, 768);
        assert_eq!(first.labels[SCALE_SERVICE_LABEL], "worker");
        assert_eq!(first.labels[SCALE_SLOT_LABEL], "3");
        assert!(first.labels[SCALE_TEMPLATE_DIGEST_LABEL].starts_with("sha256:"));
    }

    #[test]
    fn catalog_rejects_stateful_or_fixed_endpoint_templates() {
        for (field, expected) in [
            ("ports = [\"8080:80\"]", "fixed host ports"),
            ("volumes = [\"data:/data\"]", "shared volumes"),
            ("depends_on = [\"db\"]", "depends_on"),
            ("networks = [\"backend\"]", "Compose network"),
        ] {
            let source = format!(
                "service \"api\" {{ image = \"api:v1\"; {field} }}\nservice \"db\" {{ image = \"db:v1\" }}"
            );
            let error = ScaleServiceCatalog::from_acl_str(
                &source,
                "gateway-scale",
                ExecutionIsolation::Microvm,
            )
            .unwrap_err();
            assert!(error.to_string().contains(expected), "{error}");
        }
    }

    #[test]
    fn unknown_service_fails_without_fabricating_a_template() {
        let catalog = ScaleServiceCatalog::from_acl_str(
            CATALOG,
            "gateway-scale",
            ExecutionIsolation::Microvm,
        )
        .unwrap();
        let error = catalog.create_request("missing", 0).unwrap_err();
        assert!(error.to_string().contains("not found"));
    }

    #[test]
    fn file_catalog_requires_an_acl_extension() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("services.yaml");
        std::fs::write(&path, CATALOG).unwrap();

        let error =
            ScaleServiceCatalog::from_acl_file(&path, "gateway-scale", ExecutionIsolation::Microvm)
                .unwrap_err();

        assert!(error.to_string().contains(".acl format"), "{error}");
    }
}
